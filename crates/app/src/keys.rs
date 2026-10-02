//! API keys: where they are read from and where Settings saves them.
//!
//! Lookup order for each service's key:
//! 1. The environment (`ASSEMBLYAI_API_KEY`, `DEEPGRAM_API_KEY`), for
//!    power users and scripts. It always wins.
//! 2. The desktop keyring (Secret Service: GNOME Keyring, KWallet, ...),
//!    service `f9-talk`, one entry per variable name. Settings saves here.
//! 3. `~/.config/F9_talk/secrets.env` (mode 600), the pre-0.8 store and
//!    the fallback when no keyring is running. Existing keys there keep
//!    working with nothing retyped.
//!
//! Placeholders starting `PASTE_` count as no key. Key values are never
//! logged. `F9_TALK_NO_KEYRING=1` skips the keyring (tests, headless).

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tracing::debug;

use crate::config::Backend;

pub const SECRETS_FILE: &str = "secrets.env";
const KEYRING_SERVICE: &str = "f9-talk";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Environment,
    Keyring,
    File,
}

impl KeySource {
    pub fn describe(self) -> &'static str {
        match self {
            KeySource::Environment => "set by an environment variable",
            KeySource::Keyring => "saved in your desktop keyring",
            KeySource::File => "saved in secrets.env",
        }
    }
}

pub struct KeyStore {
    dir: Option<PathBuf>,
    use_keyring: bool,
}

impl KeyStore {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            use_keyring: std::env::var_os("F9_TALK_NO_KEYRING").is_none(),
        }
    }

    /// A store that never touches the desktop keyring (tests).
    #[cfg(test)]
    pub fn without_keyring(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            use_keyring: false,
        }
    }

    fn secrets_path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(SECRETS_FILE))
    }

    /// The key f9-talk will use, and where it came from.
    pub fn get(&self, backend: Backend) -> Option<(String, KeySource)> {
        if let Ok(v) = std::env::var(backend.key_var()) {
            if is_real_key(&v) {
                return Some((v, KeySource::Environment));
            }
        }
        self.get_stored(backend)
    }

    /// The saved key (keyring, then file), ignoring the environment: what
    /// the Settings window edits.
    pub fn get_stored(&self, backend: Backend) -> Option<(String, KeySource)> {
        if self.use_keyring {
            match keyring_entry(backend).and_then(|e| e.get_password()) {
                Ok(v) if is_real_key(&v) => return Some((v, KeySource::Keyring)),
                Ok(_) | Err(keyring::Error::NoEntry) => {}
                Err(e) => debug!("keyring read for {}: {e}", backend.key_var()),
            }
        }
        let path = self.secrets_path()?;
        let text = fs::read_to_string(path).ok()?;
        parse_secrets(&text)
            .remove(backend.key_var())
            .map(|v| (v, KeySource::File))
    }

    /// Save `value` as the key for `backend` (empty removes it). Goes to
    /// the keyring when one is running, else to `secrets.env` (mode 600).
    /// A key moved into the keyring is removed from `secrets.env`, so
    /// there is one stored copy. Returns where it went.
    pub fn set(&self, backend: Backend, value: &str) -> Result<Option<KeySource>, String> {
        let value = value.trim();
        let var = backend.key_var();
        if value.is_empty() {
            if self.use_keyring {
                if let Ok(entry) = keyring_entry(backend) {
                    match entry.delete_credential() {
                        Ok(()) | Err(keyring::Error::NoEntry) => {}
                        Err(e) => debug!("keyring delete for {var}: {e}"),
                    }
                }
            }
            self.write_file_key(var, None)?;
            return Ok(None);
        }
        if self.use_keyring {
            match keyring_entry(backend).and_then(|e| e.set_password(value)) {
                Ok(()) => {
                    self.write_file_key(var, None)?;
                    return Ok(Some(KeySource::Keyring));
                }
                Err(e) => debug!("keyring unavailable for {var} ({e}); using {SECRETS_FILE}"),
            }
        }
        self.write_file_key(var, Some(value))?;
        Ok(Some(KeySource::File))
    }

    /// Replace (or with `None`, remove) `var` in `secrets.env`, keeping
    /// every other line, and keep the file private.
    fn write_file_key(&self, var: &str, value: Option<&str>) -> Result<(), String> {
        let Some(path) = self.secrets_path() else {
            return Err("no config directory".into());
        };
        let old = fs::read_to_string(&path).unwrap_or_default();
        if value.is_none() && !old.lines().any(|l| line_var(l) == Some(var)) {
            return Ok(());
        }
        let new = replace_secret(&old, var, value);
        write_private(&path, &new).map_err(|e| format!("write {}: {e}", path.display()))
    }
}

fn keyring_entry(backend: Backend) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, backend.key_var())
}

pub fn is_real_key(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty() && !v.starts_with("PASTE_")
}

/// `KEY=value` lines of a secrets file; the first real value of each key
/// wins, comments and placeholders are skipped.
pub fn parse_secrets(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim().trim_start_matches("export ").trim().to_string();
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if is_real_key(&v) {
                out.entry(k).or_insert(v);
            }
        }
    }
    out
}

fn line_var(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let (k, _) = line.split_once('=')?;
    Some(k.trim().trim_start_matches("export ").trim())
}

/// `text` with every line for `var` dropped and, with `Some(value)`, one
/// new `var=value` line where the first one was (or at the end).
fn replace_secret(text: &str, var: &str, value: Option<&str>) -> String {
    let mut out = String::new();
    let mut placed = false;
    for line in text.lines() {
        if line_var(line) == Some(var) {
            if let (Some(v), false) = (value, placed) {
                out.push_str(&format!("{var}={v}\n"));
                placed = true;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if let (Some(v), false) = (value, placed) {
        out.push_str(&format!("{var}={v}\n"));
    }
    out
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_and_comments_are_not_keys() {
        let m = parse_secrets(
            "# comment\nASSEMBLYAI_API_KEY=PASTE_YOUR_ASSEMBLYAI_KEY_HERE\n\
             export DEEPGRAM_API_KEY=\"dg-real\"\nASSEMBLYAI_API_KEY=aai-real\n",
        );
        assert_eq!(
            m.get("ASSEMBLYAI_API_KEY").map(String::as_str),
            Some("aai-real")
        );
        assert_eq!(
            m.get("DEEPGRAM_API_KEY").map(String::as_str),
            Some("dg-real")
        );
    }

    #[test]
    fn replacing_a_key_keeps_the_other_lines() {
        let old =
            "# keys\nDEEPGRAM_API_KEY=dg\nASSEMBLYAI_API_KEY=PASTE_X\nASSEMBLYAI_API_KEY=old\n";
        assert_eq!(
            replace_secret(old, "ASSEMBLYAI_API_KEY", Some("new")),
            "# keys\nDEEPGRAM_API_KEY=dg\nASSEMBLYAI_API_KEY=new\n"
        );
        assert_eq!(
            replace_secret(old, "ASSEMBLYAI_API_KEY", None),
            "# keys\nDEEPGRAM_API_KEY=dg\n"
        );
        assert_eq!(
            replace_secret("", "DEEPGRAM_API_KEY", Some("dg")),
            "DEEPGRAM_API_KEY=dg\n"
        );
    }

    fn temp_store(tag: &str) -> (KeyStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("f9-talk-keys-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = KeyStore {
            dir: Some(dir.clone()),
            use_keyring: false,
        };
        (store, dir)
    }

    #[test]
    fn without_a_keyring_keys_go_to_a_private_file() {
        let (store, dir) = temp_store("file");
        // A pre-0.8 file with a placeholder line keeps working.
        fs::write(
            dir.join(SECRETS_FILE),
            "DEEPGRAM_API_KEY=dg-old\nASSEMBLYAI_API_KEY=PASTE_YOUR_ASSEMBLYAI_KEY_HERE\n",
        )
        .unwrap();
        assert_eq!(
            store.get_stored(Backend::Deepgram),
            Some(("dg-old".into(), KeySource::File))
        );
        assert_eq!(store.get_stored(Backend::AssemblyAi), None);

        assert_eq!(
            store.set(Backend::AssemblyAi, " aai-new "),
            Ok(Some(KeySource::File))
        );
        assert_eq!(
            store.get_stored(Backend::AssemblyAi),
            Some(("aai-new".into(), KeySource::File))
        );
        let path = dir.join(SECRETS_FILE);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("DEEPGRAM_API_KEY=dg-old"));

        assert_eq!(store.set(Backend::AssemblyAi, ""), Ok(None));
        assert_eq!(store.get_stored(Backend::AssemblyAi), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[ignore = "uses the desktop keyring (Secret Service); run on a desktop session"]
    fn the_desktop_keyring_round_trips() {
        let entry = keyring::Entry::new("f9-talk-test", "F9_TALK_TEST_KEY").unwrap();
        entry.set_password("round-trip").unwrap();
        assert_eq!(entry.get_password().unwrap(), "round-trip");
        entry.delete_credential().unwrap();
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
    }
}
