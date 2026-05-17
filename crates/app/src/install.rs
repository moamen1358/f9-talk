//! `f9-talk install` / `uninstall` subcommands.
//!
//! Single source of truth for desktop integration across all three
//! install paths (.deb, AppImage, cargo `curl|sh`). The .deb postinst
//! still owns the system-wide bits; this module is what AppImage and
//! cargo users invoke to get the same setup.
//!
//! Layout:
//!   --user   → ~/.local/share/applications/f9-talk.desktop
//!              ~/.config/autostart/f9-talk.desktop
//!              ~/.config/F9_talk/secrets.env  (chmod 0600, seeded only if missing)
//!   --system → /etc/udev/rules.d/99-f9-talk.rules  (needs root)
//!              usermod -aG input <invoking-user>
//!              udevadm control --reload-rules && udevadm trigger /dev/uinput
//!
//! Idempotent: re-running overwrites the .desktop files (so Exec=
//! tracks the current binary path) and is a no-op for the secrets stub
//! if one already exists.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};

#[derive(clap::Args, Debug, Clone)]
pub struct InstallArgs {
    /// Per-user integration (apps menu, autostart, secrets stub). Default if no flag given.
    #[arg(long)]
    pub user: bool,
    /// System-wide integration (udev rule, add user to `input` group). Requires root.
    #[arg(long)]
    pub system: bool,
    /// Both --user and --system. Re-exec with sudo for the system part if needed.
    #[arg(long, conflicts_with_all = ["user", "system"])]
    pub all: bool,
}

impl InstallArgs {
    fn want_user(&self) -> bool {
        // Default to user-only when no flag is given.
        self.all || self.user || !self.system
    }
    fn want_system(&self) -> bool {
        self.all || self.system
    }
}

pub fn run(args: &InstallArgs) -> Result<()> {
    if args.want_user() {
        install_user()?;
    }
    if args.want_system() {
        install_system()?;
    }
    println!("\nf9-talk: install complete. Log out and back in once for the input-group + udev rule to take effect.");
    Ok(())
}

pub fn uninstall(args: &InstallArgs) -> Result<()> {
    if args.want_user() {
        uninstall_user()?;
    }
    if args.want_system() {
        uninstall_system()?;
    }
    println!("\nf9-talk: uninstall complete. Your secrets.env was left in place.");
    Ok(())
}

// ---------- per-user ----------

fn install_user() -> Result<()> {
    let exec = launch_command()?;
    let apps = xdg_data_home().join("applications");
    let autostart = xdg_config_home().join("autostart");
    let secrets_dir = xdg_config_home().join("F9_talk");
    fs::create_dir_all(&apps).with_context(|| format!("mkdir {apps:?}"))?;
    fs::create_dir_all(&autostart).with_context(|| format!("mkdir {autostart:?}"))?;
    fs::create_dir_all(&secrets_dir).with_context(|| format!("mkdir {secrets_dir:?}"))?;

    let desktop_path = apps.join("f9-talk.desktop");
    fs::write(&desktop_path, apps_desktop(&exec))
        .with_context(|| format!("write {desktop_path:?}"))?;
    println!("  ✓ wrote {}", desktop_path.display());

    let autostart_path = autostart.join("f9-talk.desktop");
    fs::write(&autostart_path, autostart_desktop(&exec))
        .with_context(|| format!("write {autostart_path:?}"))?;
    println!("  ✓ wrote {}", autostart_path.display());

    let secrets_path = secrets_dir.join("secrets.env");
    if !secrets_path.exists() {
        fs::write(&secrets_path, SECRETS_STUB)
            .with_context(|| format!("write {secrets_path:?}"))?;
        fs::set_permissions(&secrets_path, fs::Permissions::from_mode(0o600))?;
        println!("  ✓ seeded {} (chmod 600 — paste your Deepgram key)", secrets_path.display());
    } else {
        println!("  · kept {} (already exists)", secrets_path.display());
    }

    // Best-effort: refresh the freedesktop apps DB so the entry appears
    // without a logout. Ignore failures — non-fatal.
    let _ = Command::new("update-desktop-database").arg(&apps).status();
    Ok(())
}

fn uninstall_user() -> Result<()> {
    for p in [
        xdg_data_home().join("applications/f9-talk.desktop"),
        xdg_config_home().join("autostart/f9-talk.desktop"),
    ] {
        match fs::remove_file(&p) {
            Ok(_) => println!("  ✓ removed {}", p.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow!("remove {p:?}: {e}")),
        }
    }
    let _ = Command::new("update-desktop-database")
        .arg(xdg_data_home().join("applications"))
        .status();
    Ok(())
}

// ---------- system-wide ----------

fn install_system() -> Result<()> {
    if !is_root() {
        return Err(anyhow!(
            "--system needs root. Re-run with:\n    sudo {} install --system",
            current_exe_display()
        ));
    }
    let rule_path = Path::new("/etc/udev/rules.d/99-f9-talk.rules");
    fs::write(rule_path, UDEV_RULE).with_context(|| format!("write {rule_path:?}"))?;
    println!("  ✓ wrote {}", rule_path.display());

    let target_user = std::env::var("SUDO_USER").ok();
    if let Some(user) = target_user.as_deref() {
        // Add to input group. Best-effort.
        let status = Command::new("usermod").args(["-aG", "input", user]).status();
        match status {
            Ok(s) if s.success() => println!("  ✓ added {user} to the 'input' group"),
            Ok(s) => println!("  · usermod exited {s} — check manually"),
            Err(e) => println!("  · usermod failed: {e}"),
        }
    } else {
        println!("  · SUDO_USER not set — skipping `usermod -aG input`. Run it manually for your user.");
    }

    let _ = Command::new("udevadm").args(["control", "--reload-rules"]).status();
    let _ = Command::new("udevadm")
        .args(["trigger", "--type=devices", "--action=add", "--subsystem-match=misc", "--sysname-match=uinput"])
        .status();
    println!("  ✓ reloaded udev rules");

    // Belt-and-suspenders: directly fix /dev/uinput perms for the
    // current boot so the user doesn't need to reboot or replug
    // anything for f9-talk to start working. The udev rule handles
    // every subsequent boot.
    if Path::new("/dev/uinput").exists() {
        let _ = Command::new("chgrp").args(["input", "/dev/uinput"]).status();
        let _ = Command::new("chmod").args(["0660", "/dev/uinput"]).status();
        println!("  ✓ /dev/uinput is now group=input mode=0660");
    }
    Ok(())
}

fn uninstall_system() -> Result<()> {
    if !is_root() {
        return Err(anyhow!(
            "--system needs root. Re-run with:\n    sudo {} uninstall --system",
            current_exe_display()
        ));
    }
    let rule_path = Path::new("/etc/udev/rules.d/99-f9-talk.rules");
    match fs::remove_file(rule_path) {
        Ok(_) => println!("  ✓ removed {}", rule_path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow!("remove {rule_path:?}: {e}")),
    }
    let _ = Command::new("udevadm").args(["control", "--reload-rules"]).status();
    Ok(())
}

// ---------- helpers ----------

fn xdg_data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".local/share"))
}

fn xdg_config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

fn is_root() -> bool {
    // SAFETY: getuid is always-safe; it just reads a process attribute.
    unsafe { libc::getuid() == 0 }
}

/// Pick the `Exec=` line for the .desktop files.
///
/// Priority:
///   1. `$APPIMAGE` env var (set by the AppImage runtime) — points at the .AppImage on disk.
///   2. The bare name `f9-talk` if our current_exe resolves to something on $PATH.
///   3. The absolute path of current_exe (cargo / hand-built binaries).
fn launch_command() -> Result<String> {
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        let s = appimage.to_string_lossy().to_string();
        return Ok(format!("{s} --backend cloud"));
    }
    let exe = std::env::current_exe().context("resolving current_exe")?;
    if exe_is_on_path(&exe) {
        return Ok("f9-talk --backend cloud".to_string());
    }
    Ok(format!("{} --backend cloud", exe.display()))
}

fn exe_is_on_path(exe: &Path) -> bool {
    let Some(path_env) = std::env::var_os("PATH") else {
        return false;
    };
    let Some(file_name) = exe.file_name() else {
        return false;
    };
    for dir in std::env::split_paths(&path_env) {
        if dir.join(file_name) == *exe {
            return true;
        }
    }
    false
}

fn current_exe_display() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "f9-talk".to_string())
}

// ---------- embedded asset text ----------

fn apps_desktop(exec: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=F9 Talk\n\
         GenericName=Dictation\n\
         Comment=Hold F9 to speak; text appears at your cursor\n\
         Exec={exec}\n\
         Icon=f9-talk\n\
         Categories=Utility;Accessibility;\n\
         Keywords=dictation;speech;voice;stt;\n\
         StartupNotify=false\n"
    )
}

fn autostart_desktop(exec: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=F9 Talk\n\
         Comment=Hold-to-talk dictation — auto-starts on login\n\
         Exec={exec}\n\
         Icon=f9-talk\n\
         X-GNOME-Autostart-enabled=true\n\
         X-GNOME-Autostart-Delay=5\n\
         Hidden=false\n\
         NoDisplay=false\n"
    )
}

const SECRETS_STUB: &str = "# f9-talk secrets — loaded at startup.
# Get a key at https://console.deepgram.com/  (free tier available).
# Paste the value, save, then run `f9-talk` (or log in if autostart is enabled).

DEEPGRAM_API_KEY=PASTE_YOUR_DEEPGRAM_KEY_HERE
";

const UDEV_RULE: &str = "# Allow members of the `input` group to write to /dev/uinput.
# Installed by `f9-talk install --system` or the .deb postinst.
# After install, run:
#   sudo udevadm control --reload-rules && sudo udevadm trigger
# (or reboot) for the rule to take effect.
#
# Note: `==` is the match operator, `=` is assign. MODE/GROUP MUST use
# single `=` or the rule silently no-ops and /dev/uinput stays root:root.
KERNEL==\"uinput\", MODE=\"0660\", GROUP=\"input\"
";
