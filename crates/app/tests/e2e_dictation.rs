//! End-to-end: a recorded English clip goes through the real binary
//! (`f9-talk simulate`) and the real cloud service at real-time pace, the
//! way the mic loop feeds it while F9 is held; then the text that would be
//! typed is checked, and the release-to-text time is measured.
//!
//! These call paid APIs, so they are ignored by default. Run with:
//!
//! ```text
//! ASSEMBLYAI_API_KEY=... cargo test -p f9-talk --test e2e_dictation -- --ignored --nocapture
//! ```
//!
//! (The Deepgram test also needs DEEPGRAM_API_KEY.) The binary runs with
//! an empty XDG_CONFIG_HOME, so it uses the default settings and key terms
//! and never reads or writes the real ~/.config/F9_talk.

use std::path::PathBuf;
use std::process::Command;

const DICTATION: &str = "Can you push the fix to GitHub and tell Codex to open a pull request? \
     The Terraform report goes to the DevOps team on Thursday, and Claude should switch the \
     Kubernetes cluster from Deepgram to AssemblyAI.";

/// The key terms the test writes into its own keyterms.txt (the shipped
/// default boosts nothing).
const KEYTERMS: &[&str] = &[
    "GitHub",
    "Codex",
    "Terraform",
    "DevOps",
    "Claude",
    "Kubernetes",
    "Deepgram",
    "AssemblyAI",
    ".env",
    "em dash",
];

struct Outcome {
    text: String,
    release_to_text_ms: u64,
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn simulate(clip: &str, args: &[&str], key_var: &str) -> Outcome {
    assert!(
        std::env::var(key_var).is_ok_and(|v| !v.is_empty()),
        "{key_var} must be set to run this end-to-end test"
    );
    let config_home =
        std::env::temp_dir().join(format!("f9-talk-e2e-{}-{clip}", std::process::id()));
    let settings_dir = config_home.join("F9_talk");
    std::fs::create_dir_all(&settings_dir).unwrap();
    std::fs::write(settings_dir.join("keyterms.txt"), KEYTERMS.join("\n")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_f9-talk"))
        .arg("simulate")
        .arg(fixture(clip))
        .args(args)
        .env("XDG_CONFIG_HOME", &config_home)
        .output()
        .expect("run f9-talk simulate");
    let _ = std::fs::remove_dir_all(&config_home);
    assert!(
        out.status.success(),
        "simulate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Log lines share stdout; the result is the JSON line.
    let stdout = String::from_utf8(out.stdout).unwrap();
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.starts_with(r#"{"backend""#))
        .expect("no result line");
    eprintln!("{clip} {args:?}: {line}");
    let json: serde_json::Value = serde_json::from_str(line).unwrap();
    Outcome {
        text: json["text"].as_str().unwrap().to_string(),
        release_to_text_ms: json["release_to_text_ms"].as_u64().unwrap(),
    }
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Word error rate of `hyp` against `reference` (Levenshtein over words).
fn wer(reference: &str, hyp: &str) -> f64 {
    let (r, h) = (words(reference), words(hyp));
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut cur = vec![i + 1; h.len() + 1];
        for (j, hw) in h.iter().enumerate() {
            let sub = prev[j] + usize::from(rw != hw);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[h.len()] as f64 / r.len().max(1) as f64
}

fn check_dictation(o: &Outcome) {
    // Every key term is spelled exactly as in keyterms.txt.
    for term in &KEYTERMS[..8] {
        assert!(o.text.contains(term), "{term:?} missing from {:?}", o.text);
    }
    // The whole sentence, to the last word: no cut-off tail.
    assert!(
        words(&o.text).ends_with(&["to".into(), "assemblyai".into()]),
        "tail cut: {:?}",
        o.text
    );
    let e = wer(DICTATION, &o.text);
    assert!(e <= 0.10, "WER {e:.2} too high: {:?}", o.text);
    // Formatted: capitalised, punctuated.
    assert!(o.text.starts_with("Can you"), "{:?}", o.text);
    assert!(o.text.contains('?'), "{:?}", o.text);
}

#[test]
#[ignore = "calls the AssemblyAI API; needs ASSEMBLYAI_API_KEY"]
fn assemblyai_warm_session_types_the_whole_dictation() {
    let o = simulate(
        "dictation-en.wav",
        &["--backend", "assemblyai", "--warm-ms", "2000"],
        "ASSEMBLYAI_API_KEY",
    );
    check_dictation(&o);
    assert!(
        o.release_to_text_ms < 1500,
        "slow: {} ms",
        o.release_to_text_ms
    );
}

#[test]
#[ignore = "calls the AssemblyAI API; needs ASSEMBLYAI_API_KEY"]
fn assemblyai_press_while_connecting_loses_no_first_word() {
    // F9 pressed the instant the app starts: the session is still opening,
    // so the audio is buffered and sent when it opens.
    let o = simulate(
        "dictation-en.wav",
        &["--backend", "assemblyai", "--warm-ms", "0"],
        "ASSEMBLYAI_API_KEY",
    );
    check_dictation(&o);
}

#[test]
#[ignore = "calls the AssemblyAI API; needs ASSEMBLYAI_API_KEY"]
fn assemblyai_short_press_is_not_cut() {
    let o = simulate(
        "short-en.wav",
        &["--backend", "assemblyai", "--warm-ms", "0"],
        "ASSEMBLYAI_API_KEY",
    );
    // The TTS voice slurs "ship it" (heard as "ChipIt" or "Chip hit"), so
    // only check that the press came back whole and formatted.
    assert!(o.text.starts_with("Yes,"), "{:?}", o.text);
    assert!(words(&o.text).len() >= 2, "{:?}", o.text);
    assert!(o.text.ends_with('.'), "{:?}", o.text);
    assert!(
        o.release_to_text_ms < 1500,
        "slow: {} ms",
        o.release_to_text_ms
    );
}

#[test]
#[ignore = "calls the AssemblyAI API; needs ASSEMBLYAI_API_KEY"]
fn assemblyai_pause_mid_sentence_stays_one_sentence() {
    // A 1.6 s pause after "The tokenizing model". With the server's
    // default turn silence this came back as two punctuated fragments,
    // "The tokenizing model. Works correctly, ...".
    let o = simulate(
        "pause-en.wav",
        &["--backend", "assemblyai", "--warm-ms", "2000"],
        "ASSEMBLYAI_API_KEY",
    );
    assert!(
        o.text.starts_with("The tokenizing model works correctly"),
        "split at the pause: {:?}",
        o.text
    );
    assert_eq!(o.text.matches('.').count(), 1, "{:?}", o.text);
    assert!(o.text.contains("GitHub"), "{:?}", o.text);
}

#[test]
#[ignore = "calls the AssemblyAI API; needs ASSEMBLYAI_API_KEY"]
fn assemblyai_spells_key_terms() {
    // Without key terms these come out as "dot EMV" and "in dashes".
    let o = simulate(
        "env-en.wav",
        &["--backend", "assemblyai", "--warm-ms", "2000"],
        "ASSEMBLYAI_API_KEY",
    );
    for term in [".env", "em dash", "Terraform"] {
        assert!(o.text.contains(term), "{term:?} missing from {:?}", o.text);
    }
}

#[test]
#[ignore = "calls the Deepgram API; needs DEEPGRAM_API_KEY"]
fn deepgram_fallback_types_the_whole_dictation() {
    let o = simulate(
        "dictation-en.wav",
        &["--backend", "deepgram"],
        "DEEPGRAM_API_KEY",
    );
    // The point here is no cut-off tail and the keyterm request being
    // accepted (the old `keywords` param was an HTTP 400).
    assert!(
        words(&o.text).ends_with(&["to".into(), "assemblyai".into()]),
        "tail cut: {:?}",
        o.text
    );
    assert!(wer(DICTATION, &o.text) <= 0.15, "{:?}", o.text);
}

#[test]
fn wer_counts_word_edits() {
    assert_eq!(wer("a b c d", "a b c d"), 0.0);
    assert_eq!(wer("a b c d", "a x c"), 0.5);
    assert_eq!(wer("Hello, world.", "hello world"), 0.0);
}
