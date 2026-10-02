//! User-editable settings in `~/.config/F9_talk/`:
//! - `config.toml`: which speech-to-text backend, and its timing knobs.
//! - `keyterms.txt`: names and jargon to boost, one per line.
//!
//! Both are seeded with commented defaults (never overwritten) by
//! `f9-talk install --user` and on every normal start, so an existing
//! install gets them on its first run of this version. A file that fails
//! to parse falls back to the defaults with a warning; it never stops
//! dictation.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tracing::warn;

pub const CONFIG_FILE: &str = "config.toml";
pub const KEYTERMS_FILE: &str = "keyterms.txt";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[serde(alias = "assembly", alias = "assembly-ai")]
    AssemblyAi,
    Deepgram,
}

impl Backend {
    pub fn key_var(self) -> &'static str {
        match self {
            Backend::AssemblyAi => "ASSEMBLYAI_API_KEY",
            Backend::Deepgram => "DEEPGRAM_API_KEY",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Backend::AssemblyAi => "AssemblyAI Universal-3.6 Pro",
            Backend::Deepgram => "Deepgram Nova-3",
        }
    }

    pub fn other(self) -> Backend {
        match self {
            Backend::AssemblyAi => Backend::Deepgram,
            Backend::Deepgram => Backend::AssemblyAi,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub backend: Backend,
    pub assemblyai_warm_seconds: u64,
    pub finalize_timeout_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            backend: Backend::AssemblyAi,
            assemblyai_warm_seconds: 60,
            finalize_timeout_ms: 4000,
        }
    }
}

impl Settings {
    /// The safety-net wait for the final text on release.
    pub fn finalize_timeout(&self) -> Duration {
        Duration::from_millis(self.finalize_timeout_ms.clamp(500, 30_000))
    }
}

pub fn config_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("F9_talk"))
}

/// Read `config.toml`, falling back to defaults when it is missing or bad.
pub fn load_settings(dir: Option<&Path>) -> Settings {
    let Some(path) = dir.map(|d| d.join(CONFIG_FILE)) else {
        return Settings::default();
    };
    match fs::read_to_string(&path) {
        Ok(text) => parse_settings(&text).unwrap_or_else(|e| {
            warn!("{}: {e}; using the default settings", path.display());
            Settings::default()
        }),
        Err(_) => Settings::default(),
    }
}

pub fn parse_settings(text: &str) -> Result<Settings, String> {
    toml::from_str(text).map_err(|e| e.message().to_string())
}

/// Read `keyterms.txt`; the built-in list when the file is missing.
pub fn load_keyterms(dir: Option<&Path>) -> Vec<String> {
    match dir.map(|d| fs::read_to_string(d.join(KEYTERMS_FILE))) {
        Some(Ok(text)) => parse_keyterms(&text),
        _ => parse_keyterms(KEYTERMS_TEMPLATE),
    }
}

/// One term per line; blank lines and `#` comments are skipped.
pub fn parse_keyterms(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Write the commented default `config.toml` and `keyterms.txt` into
/// `dir` when they do not exist yet. Returns the files it created.
pub fn seed_user_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    fs::create_dir_all(dir)?;
    let mut created = Vec::new();
    for (name, body) in [
        (CONFIG_FILE, CONFIG_TEMPLATE),
        (KEYTERMS_FILE, KEYTERMS_TEMPLATE),
    ] {
        let path = dir.join(name);
        if !path.exists() {
            fs::write(&path, body)?;
            created.push(path);
        }
    }
    Ok(created)
}

pub const CONFIG_TEMPLATE: &str = r#"# f9-talk settings. After editing, quit f9-talk (hover the red dot, click
# the x) and start it again from the apps menu.

# Speech-to-text service:
#   "assemblyai"  AssemblyAI Universal-3.6 Pro Realtime (default)
#   "deepgram"    Deepgram Nova-3
# Each needs its key in secrets.env next to this file
# (ASSEMBLYAI_API_KEY or DEEPGRAM_API_KEY).
backend = "assemblyai"

# AssemblyAI bills every second its connection is open, idle or not, so
# f9-talk closes it this many seconds after your last dictation and opens
# it again on the next F9 press. No words are lost while it reconnects:
# your audio is kept and sent the moment the connection opens.
# 0 keeps it open all the time (about $0.45 for every hour f9-talk runs).
assemblyai_warm_seconds = 60

# Longest wait, in milliseconds, for the final text after you release F9.
# It normally arrives in 0.1 to 0.3 s; this is only a safety net.
finalize_timeout_ms = 4000
"#;

pub const KEYTERMS_TEMPLATE: &str =
    "# Words f9-talk should always get right: names, products, jargon.
# One per line (short phrases are fine), at most 100. Lines starting with
# # are skipped. After editing, quit f9-talk (click the red dot) and start
# it again. Examples:
# GitHub
# Kubernetes
# .env
# em dash
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_the_defaults() {
        assert_eq!(
            parse_settings(CONFIG_TEMPLATE).unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn one_line_switches_to_deepgram() {
        let s = parse_settings("backend = \"deepgram\"\n").unwrap();
        assert_eq!(s.backend, Backend::Deepgram);
        assert_eq!(s.assemblyai_warm_seconds, 60);
    }

    #[test]
    fn bad_values_are_errors_not_panics() {
        assert!(parse_settings("backend = \"whisper\"").is_err());
        assert!(parse_settings("backend = ").is_err());
    }

    #[test]
    fn timeout_is_clamped() {
        let s = Settings {
            finalize_timeout_ms: 10,
            ..Settings::default()
        };
        assert_eq!(s.finalize_timeout(), Duration::from_millis(500));
    }

    #[test]
    fn keyterms_skip_comments_and_blanks() {
        let terms = parse_keyterms("# heading\n\n Kubernetes \nPostgreSQL\n  # indented comment\n");
        assert_eq!(terms, ["Kubernetes", "PostgreSQL"]);
        // The shipped default boosts nothing; the examples are comments.
        assert!(parse_keyterms(KEYTERMS_TEMPLATE).is_empty());
    }

    #[test]
    fn seeding_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("f9-talk-seed-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let created = seed_user_files(&dir).unwrap();
        assert_eq!(created.len(), 2);
        fs::write(dir.join(KEYTERMS_FILE), "Only\n").unwrap();
        assert!(seed_user_files(&dir).unwrap().is_empty());
        assert_eq!(load_keyterms(Some(&dir)), ["Only"]);
        assert_eq!(load_settings(Some(&dir)), Settings::default());
        fs::remove_dir_all(&dir).unwrap();
    }
}
