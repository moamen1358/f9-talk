//! `f9-talk settings`: the Settings window (egui).
//!
//! Runs as its own short-lived process, so it never shares a thread with
//! the dictation loop or the Wayland indicator. It edits:
//! - the service (AssemblyAI Universal-3.6 Pro or Deepgram Nova-3),
//! - each service's API key (masked, with show/hide and a "Test key"
//!   button that makes one free request),
//! - the language, the key terms and the warm-connection seconds.
//!
//! Save writes `config.toml` and `keyterms.txt`, stores changed keys in
//! the desktop keyring (else `secrets.env`, mode 600), then tells the
//! running app to reload (or starts it when it is not running).
//!
//! Opened by: right-clicking the red dot, the apps-menu "Settings"
//! action, launching F9 Talk while it already runs, and automatically
//! on first run when no key is set.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui;
use egui::{Color32, RichText};

use crate::config::{self, Backend, Settings, LANGUAGES};
use crate::keys::{KeySource, KeyStore};
use crate::{ipc, keycheck};

const ASSEMBLYAI_SIGNUP: &str = "https://www.assemblyai.com/dashboard/signup";
const DEEPGRAM_SIGNUP: &str = "https://console.deepgram.com/signup";
const BRAND_RED: Color32 = Color32::from_rgb(0xd9, 0x2b, 0x2b);
const OK_GREEN: Color32 = Color32::from_rgb(0x2e, 0x9e, 0x4f);

#[derive(clap::Args, Debug, Clone, Default)]
pub struct SettingsArgs {
    /// Opened because no API key is set yet: show the getting-started note.
    #[arg(long)]
    pub first_run: bool,
    /// Save a PNG of the window to this path and exit (for docs).
    #[arg(long, hide = true)]
    pub screenshot: Option<PathBuf>,
}

pub fn run(args: &SettingsArgs) -> Result<()> {
    let Ok(_lock) = ipc::acquire_settings_lock() else {
        eprintln!("f9-talk: the Settings window is already open.");
        return Ok(());
    };
    let dir = config::config_dir().context("no config directory")?;
    let app = SettingsApp::load(dir, args.clone());
    let viewport = egui::ViewportBuilder::default()
        .with_title("F9 Talk Settings")
        .with_app_id("f9-talk")
        .with_inner_size([560.0, 720.0])
        .with_min_inner_size([480.0, 520.0]);
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "F9 Talk Settings",
        options,
        Box::new(move |cc| {
            style(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("settings window: {e}"))
}

/// Widest the settings column grows; a tiling window manager may make
/// the window much wider, and the column stays centred.
const COLUMN_MAX: f32 = 640.0;

fn style(ctx: &egui::Context) {
    use egui::{FontFamily::Proportional, FontId, TextStyle};
    ctx.style_mut(|s| {
        for (style, size) in [
            (TextStyle::Body, 15.0),
            (TextStyle::Button, 15.0),
            (TextStyle::Small, 13.0),
            (TextStyle::Heading, 22.0),
        ] {
            s.text_styles.insert(style, FontId::new(size, Proportional));
        }
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.visuals.selection.bg_fill = BRAND_RED;
        s.visuals.hyperlink_color = Color32::from_rgb(0x4a, 0x9e, 0xf0);
    });
}

enum Check {
    Idle,
    Running(mpsc::Receiver<Result<(), String>>),
    Works,
    Failed(String),
}

struct KeyField {
    backend: Backend,
    value: String,
    /// What is stored now (to save only real changes).
    saved: String,
    source: Option<KeySource>,
    env_set: bool,
    show: bool,
    check: Check,
}

impl KeyField {
    fn load(store: &KeyStore, backend: Backend) -> Self {
        let (saved, source) = match store.get_stored(backend) {
            Some((v, s)) => (v, Some(s)),
            None => (String::new(), None),
        };
        let env_set = std::env::var(backend.key_var()).is_ok_and(|v| crate::keys::is_real_key(&v));
        Self {
            backend,
            value: saved.clone(),
            saved,
            source,
            env_set,
            show: false,
            check: Check::Idle,
        }
    }

    fn usable(&self) -> bool {
        self.env_set || !self.value.trim().is_empty()
    }
}

enum Status {
    None,
    Info(String),
    Error(String),
}

struct SettingsApp {
    dir: PathBuf,
    store: KeyStore,
    args: SettingsArgs,
    settings: Settings,
    keys: [KeyField; 2],
    keyterms: String,
    status: Status,
    frames: u32,
    shot_requested: bool,
    /// Where the settings column was drawn (for `--screenshot` cropping).
    column: Option<egui::Rect>,
    /// Runs after a successful save: tell the running app (tests swap it).
    after_save: fn(&str) -> Status,
}

impl SettingsApp {
    fn load(dir: PathBuf, args: SettingsArgs) -> Self {
        let store = KeyStore::new(Some(dir.clone()));
        let settings = config::load_settings(Some(&dir));
        let keyterms = config::load_keyterms(Some(&dir)).join("\n");
        let keys = [
            KeyField::load(&store, Backend::AssemblyAi),
            KeyField::load(&store, Backend::Deepgram),
        ];
        Self {
            dir,
            store,
            args,
            settings,
            keys,
            keyterms,
            status: Status::None,
            frames: 0,
            shot_requested: false,
            column: None,
            after_save: reload_running_app,
        }
    }

    fn field(&self, b: Backend) -> &KeyField {
        &self.keys[b as usize]
    }

    fn save(&mut self) {
        let chosen = self.settings.backend;
        if !self.field(chosen).usable() {
            self.status = Status::Error(format!(
                "Add a {} key first (or choose the other service).",
                short_name(chosen)
            ));
            return;
        }
        for i in 0..self.keys.len() {
            let f = &self.keys[i];
            if f.value.trim() == f.saved.trim() {
                continue;
            }
            match self.store.set(f.backend, &f.value) {
                Ok(source) => {
                    let f = &mut self.keys[i];
                    f.saved = f.value.trim().to_string();
                    f.value = f.saved.clone();
                    f.source = source;
                }
                Err(e) => {
                    self.status = Status::Error(format!("Could not save the key: {e}"));
                    return;
                }
            }
        }
        let terms: Vec<String> = self.keyterms.lines().map(str::to_string).collect();
        if let Err(e) = config::save_settings(&self.dir, &self.settings)
            .and_then(|_| config::save_keyterms(&self.dir, &terms))
        {
            self.status = Status::Error(format!("Could not save the settings: {e}"));
            return;
        }
        self.status = (self.after_save)(self.settings.backend.label());
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("F9").size(26.0).strong().color(BRAND_RED));
            ui.label(RichText::new("Talk").size(26.0).strong());
        });
        ui.label("Hold F9, speak, release: your words are typed where the cursor is.");
        let no_key = !self.keys.iter().any(KeyField::usable);
        if self.args.first_run || no_key {
            ui.add_space(4.0);
            egui::Frame::group(ui.style())
                .stroke(egui::Stroke::new(1.0_f32, BRAND_RED))
                .inner_margin(10.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Add an API key to start").strong());
                    ui.label(
                        "F9 Talk uses your own speech-to-text key. Both services give free \
                         credit to start: $50 at AssemblyAI (about 110 hours of streaming) \
                         and $200 at Deepgram.",
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.hyperlink_to("Get a free AssemblyAI key", ASSEMBLYAI_SIGNUP);
                        ui.label("or");
                        ui.hyperlink_to("Get a free Deepgram key", DEEPGRAM_SIGNUP);
                    });
                });
        }
    }

    fn service(&mut self, ui: &mut egui::Ui) {
        section(ui, "Speech-to-text service");
        ui.radio_value(
            &mut self.settings.backend,
            Backend::AssemblyAi,
            "AssemblyAI Universal-3.6 Pro  (recommended)",
        );
        ui.radio_value(
            &mut self.settings.backend,
            Backend::Deepgram,
            "Deepgram Nova-3",
        );
    }

    fn key_fields(&mut self, ui: &mut egui::Ui) {
        for i in 0..self.keys.len() {
            let backend = self.keys[i].backend;
            section(ui, &format!("{} API key", short_name(backend)));
            let f = &mut self.keys[i];
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut f.value)
                        .password(!f.show)
                        .hint_text("paste your key")
                        .margin(egui::vec2(6.0, 6.0))
                        .desired_width(ui.available_width() - 170.0),
                );
                if ui.button(if f.show { "Hide" } else { "Show" }).clicked() {
                    f.show = !f.show;
                }
                let testing = matches!(f.check, Check::Running(_));
                if ui
                    .add_enabled(!testing, egui::Button::new("Test key"))
                    .clicked()
                {
                    let (tx, rx) = mpsc::channel();
                    let key = f.value.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(keycheck::check_key(backend, &key));
                    });
                    f.check = Check::Running(rx);
                }
            });
            if let Check::Running(rx) = &f.check {
                if let Ok(result) = rx.try_recv() {
                    f.check = match result {
                        Ok(()) => Check::Works,
                        Err(e) => Check::Failed(e),
                    };
                }
            }
            match &f.check {
                Check::Idle => {}
                Check::Running(_) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Testing...");
                    });
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                Check::Works => {
                    ui.label(RichText::new("Works").color(OK_GREEN).strong());
                }
                Check::Failed(e) => {
                    ui.label(RichText::new(format!("Does not work: {e}")).color(BRAND_RED));
                }
            }
            let mut note = match f.source {
                Some(s) if f.value.trim() == f.saved.trim() => format!("Key {}.", s.describe()),
                _ if f.value.trim() != f.saved.trim() => "Not saved yet.".to_string(),
                _ => String::new(),
            };
            if f.env_set {
                note.push_str(&format!(
                    " {} is set in the environment and is used instead.",
                    backend.key_var()
                ));
            }
            if !note.is_empty() {
                ui.label(RichText::new(note.trim()).small().weak());
            }
        }
    }

    fn language(&mut self, ui: &mut egui::Ui) {
        section(ui, "Language you speak");
        let current = LANGUAGES
            .iter()
            .find(|(c, _)| *c == self.settings.language)
            .map(|(_, n)| *n)
            .unwrap_or("English");
        egui::ComboBox::from_id_salt("language")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for (code, name) in LANGUAGES {
                    ui.selectable_value(&mut self.settings.language, (*code).to_string(), *name);
                }
            });
    }

    fn key_terms(&mut self, ui: &mut egui::Ui) {
        section(ui, "Key terms");
        ui.label(
            RichText::new(
                "Names, products and jargon to always spell right, one per line \
                 (for example: GitHub, Kubernetes, .env). Up to 100.",
            )
            .small()
            .weak(),
        );
        ui.add(
            egui::TextEdit::multiline(&mut self.keyterms)
                .desired_rows(6)
                .desired_width(f32::INFINITY)
                .hint_text("GitHub\nKubernetes\n.env"),
        );
    }

    fn warm(&mut self, ui: &mut egui::Ui) {
        section(ui, "Connection");
        ui.horizontal(|ui| {
            ui.label("Keep the AssemblyAI connection open for");
            ui.add(
                egui::DragValue::new(&mut self.settings.assemblyai_warm_seconds)
                    .range(0..=3600)
                    .suffix(" s"),
            );
            ui.label("after each dictation");
        });
        ui.label(
            RichText::new(
                "AssemblyAI bills every second the connection is open, so it closes when \
                 idle and reopens on the next F9 press without losing words. 0 keeps it \
                 always open (about $0.45 per hour).",
            )
            .small()
            .weak(),
        );
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let save = egui::Button::new(RichText::new("Save").strong().color(Color32::WHITE))
                .fill(BRAND_RED)
                .min_size(egui::vec2(90.0, 30.0));
            if ui.add(save).clicked() {
                self.save();
            }
            if ui
                .add(egui::Button::new("Close").min_size(egui::vec2(90.0, 30.0)))
                .clicked()
            {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        match &self.status {
            Status::None => {}
            Status::Info(m) => {
                ui.label(RichText::new(m).color(OK_GREEN));
            }
            Status::Error(m) => {
                ui.label(RichText::new(m).color(BRAND_RED));
            }
        }
    }

    /// `--screenshot`: once the layout has settled, ask for a frame
    /// capture, save it as PNG and close.
    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.args.screenshot.clone() else {
            return;
        };
        self.frames += 1;
        if self.frames == 5 && !self.shot_requested {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            let [w, h] = image.size;
            let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
            // Crop to the settings column plus a margin, so a window that
            // a tiling compositor stretched still gives a tidy picture.
            let ppp = ctx.pixels_per_point();
            let crop = |img: image::RgbaImage| match self.column {
                Some(r) => {
                    let r = r.expand(16.0);
                    let x = ((r.min.x * ppp).max(0.0) as u32).min(w as u32);
                    let y = ((r.min.y * ppp).max(0.0) as u32).min(h as u32);
                    let cw = ((r.width() * ppp) as u32).min(w as u32 - x);
                    let ch = ((r.height() * ppp) as u32).min(h as u32 - y);
                    image::imageops::crop_imm(&img, x, y, cw, ch).to_image()
                }
                None => img,
            };
            match image::RgbaImage::from_raw(w as u32, h as u32, rgba)
                .map(|img| crop(img).save(&path))
            {
                Some(Ok(())) => println!("saved {}", path.display()),
                Some(Err(e)) => eprintln!("screenshot: {e}"),
                None => eprintln!("screenshot: bad image size"),
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let width = ui.available_width();
                let column = width.min(COLUMN_MAX);
                ui.horizontal(|ui| {
                    ui.add_space((width - column) / 2.0);
                    let r = ui.vertical(|ui| {
                        ui.set_width(column);
                        ui.add_space(4.0);
                        self.header(ui);
                        card(ui, |ui| self.service(ui));
                        card(ui, |ui| self.key_fields(ui));
                        card(ui, |ui| {
                            self.language(ui);
                            self.key_terms(ui);
                        });
                        card(ui, |ui| self.warm(ui));
                        self.footer(ui);
                    });
                    self.column = Some(r.response.rect);
                });
            });
        });
        self.screenshot(ctx);
    }
}

/// A softly filled, rounded panel around one group of settings.
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(8.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(title).strong().size(16.0));
}

/// Tell the running app to reload, or start it when it is not running.
fn reload_running_app(service: &str) -> Status {
    if ipc::notify_reload() {
        Status::Info(format!(
            "Saved. F9 Talk now uses {service}: hold F9 and speak."
        ))
    } else {
        match crate::launch_self(&[]) {
            Ok(()) => Status::Info(format!("Saved. Starting F9 Talk with {service}...")),
            Err(e) => Status::Error(format!("Saved, but F9 Talk did not start: {e}")),
        }
    }
}

fn short_name(b: Backend) -> &'static str {
    match b {
        Backend::AssemblyAi => "AssemblyAI",
        Backend::Deepgram => "Deepgram",
    }
}

/// When the Settings window was last opened by the app itself, to avoid
/// stacking windows from repeated F9 presses without a key.
pub struct OpenGate {
    last: Option<Instant>,
}

impl OpenGate {
    pub fn new() -> Self {
        Self { last: None }
    }

    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(5))
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved_ok(_: &str) -> Status {
        Status::Info("saved".into())
    }

    fn app_in(tag: &str) -> (SettingsApp, PathBuf) {
        let dir = std::env::temp_dir().join(format!("f9-talk-ui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = SettingsApp::load(dir.clone(), SettingsArgs::default());
        app.store = KeyStore::without_keyring(Some(dir.clone()));
        app.after_save = saved_ok;
        for f in &mut app.keys {
            f.env_set = false;
            f.value.clear();
            f.saved.clear();
        }
        (app, dir)
    }

    #[test]
    fn save_writes_settings_terms_and_the_key() {
        let (mut app, dir) = app_in("save");
        app.keys[Backend::AssemblyAi as usize].value = " aai-test-key ".into();
        app.settings.language = "de".into();
        app.settings.assemblyai_warm_seconds = 30;
        app.keyterms = "Kubernetes\n\n.env".into();
        app.save();
        assert!(matches!(app.status, Status::Info(_)));

        let s = config::load_settings(Some(&dir));
        assert_eq!(s.language, "de");
        assert_eq!(s.assemblyai_warm_seconds, 30);
        assert_eq!(config::load_keyterms(Some(&dir)), ["Kubernetes", ".env"]);
        assert_eq!(
            app.store.get_stored(Backend::AssemblyAi),
            Some(("aai-test-key".into(), KeySource::File))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_refuses_a_service_without_a_key() {
        let (mut app, dir) = app_in("nokey");
        app.settings.backend = Backend::Deepgram;
        app.keys[Backend::AssemblyAi as usize].value = "aai".into();
        app.save();
        assert!(matches!(&app.status, Status::Error(m) if m.contains("Deepgram")));
        assert!(!dir.join(config::CONFIG_FILE).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The new-user path, driven through the same code the window's
    /// buttons call (no GUI input): an empty settings folder, the key put
    /// into the field, "Test key", then "Save". Keyring off, so the key
    /// lands only in `$F9_FRESH_DIR/secrets.env`, which the caller shreds.
    #[test]
    #[ignore = "calls AssemblyAI; needs ASSEMBLYAI_API_KEY and F9_FRESH_DIR"]
    fn fresh_user_tests_and_saves_a_key() {
        let dir = PathBuf::from(std::env::var("F9_FRESH_DIR").expect("F9_FRESH_DIR"));
        assert!(
            !dir.join(config::CONFIG_FILE).exists(),
            "not a fresh folder"
        );
        let key = std::env::var("ASSEMBLYAI_API_KEY").expect("key in env");
        let mut app = SettingsApp::load(
            dir.clone(),
            SettingsArgs {
                first_run: true,
                ..Default::default()
            },
        );
        app.store = KeyStore::without_keyring(Some(dir.clone()));
        app.after_save = saved_ok;
        let aai = Backend::AssemblyAi as usize;
        app.keys[aai].env_set = false;
        assert!(app.keys[aai].value.is_empty(), "a fresh user has no key");

        // A wrong key first: the exact service error is shown.
        assert!(keycheck::check_key(Backend::AssemblyAi, "not-a-real-key")
            .unwrap_err()
            .starts_with("HTTP 4"));
        // Paste the key, Test key: works.
        app.keys[aai].value = key;
        assert_eq!(
            keycheck::check_key(Backend::AssemblyAi, &app.keys[aai].value),
            Ok(())
        );
        // Save.
        app.save();
        assert!(matches!(app.status, Status::Info(_)));
        assert_eq!(config::load_settings(Some(&dir)), Settings::default());
        assert_eq!(
            app.store.get_stored(Backend::AssemblyAi).map(|(_, s)| s),
            Some(KeySource::File)
        );
    }
}
