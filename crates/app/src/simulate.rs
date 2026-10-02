//! `f9-talk simulate <clip.wav>`: one dictation without a microphone or
//! keyboard. The clip is streamed through the configured backend in 25 ms
//! frames at real-time pace (exactly what the mic loop sends while F9 is
//! held), then released through the same `end_session` path, and the
//! text that would have been typed (after `tidy_transcript`) is printed
//! as one JSON line:
//!
//! ```text
//! {"backend":"assemblyai","text":"...","press_ms":12340,"release_to_text_ms":231}
//! ```
//!
//! Used by the end-to-end test and handy for checking a recorded clip.
//! Nothing is typed and no settings files are written.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use f9_talk_core::FRAME_BYTES;
use f9_talk_stt::BackendEvent;
use tokio::sync::mpsc;

use crate::config::{self, Backend};

#[derive(clap::Args, Debug, Clone)]
pub struct SimulateArgs {
    /// 16 kHz mono 16-bit PCM WAV (convert with:
    /// ffmpeg -i in.m4a -ar 16000 -ac 1 -c:a pcm_s16le out.wav).
    pub wav: PathBuf,
    /// Override the backend from config.toml: assemblyai or deepgram.
    #[arg(long)]
    pub backend: Option<String>,
    /// Wait this many ms after start-up before "pressing F9", so the
    /// session is already open (0 = press at once, while it connects).
    #[arg(long, default_value_t = 0)]
    pub warm_ms: u64,
}

pub fn run(args: &SimulateArgs) -> Result<()> {
    let pcm = read_wav_16k_mono(&args.wav)?;
    let dir = config::config_dir();
    let mut settings = config::load_settings(dir.as_deref());
    if let Some(name) = &args.backend {
        settings.backend = match name.as_str() {
            "assemblyai" => Backend::AssemblyAi,
            "deepgram" => Backend::Deepgram,
            other => bail!("unknown backend {other:?} (assemblyai or deepgram)"),
        };
    }
    let keyterms = config::load_keyterms(dir.as_deref());
    let secrets = crate::load_secrets();
    let (backend, _) = crate::build_backend(&settings, &secrets, keyterms)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let (event_tx, mut event_rx) = mpsc::channel::<BackendEvent>(64);
        backend
            .start(event_tx)
            .await
            .map_err(|e| anyhow::anyhow!("backend start: {e}"))?;
        tokio::spawn(async move {
            while let Some(evt) = event_rx.recv().await {
                tracing::warn!("backend event: {evt:?}");
            }
        });
        tokio::time::sleep(Duration::from_millis(args.warm_ms)).await;

        // Hold F9 for the clip's length: one 25 ms frame per tick.
        let press_at = Instant::now();
        backend.begin_session().await;
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        for frame in pcm.chunks(FRAME_BYTES) {
            tick.tick().await;
            backend.send_audio(frame).await;
        }
        tick.tick().await;

        let release_at = Instant::now();
        let result = backend.end_session(settings.finalize_timeout()).await;
        let release_to_text = release_at.elapsed();
        backend.stop().await;
        // Let the backend close its session cleanly (billing stops).
        tokio::time::sleep(Duration::from_millis(300)).await;

        println!(
            "{}",
            serde_json::json!({
                "backend": backend.name(),
                "text": crate::tidy_transcript(&result.transcript),
                "press_ms": release_at.duration_since(press_at).as_millis() as u64,
                "release_to_text_ms": release_to_text.as_millis() as u64,
            })
        );
        Ok(())
    })
}

/// The PCM payload of a 16 kHz mono s16le WAV file.
fn read_wav_16k_mono(path: &Path) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    parse_wav_16k_mono(&bytes).with_context(|| format!("{}", path.display()))
}

fn parse_wav_16k_mono(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a WAV file");
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at =
        |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let mut pos = 12;
    let mut format_ok = false;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_at(pos + 4) as usize;
        let body = pos + 8;
        let end = body.saturating_add(len).min(bytes.len());
        match id {
            b"fmt " if len >= 16 => {
                let (fmt, channels, rate, bits) = (
                    u16_at(body),
                    u16_at(body + 2),
                    u32_at(body + 4),
                    u16_at(body + 14),
                );
                if fmt != 1 || channels != 1 || rate != 16_000 || bits != 16 {
                    bail!(
                        "need 16 kHz mono 16-bit PCM, got format {fmt}, {channels} ch, {rate} Hz, {bits} bit \
                         (convert with: ffmpeg -i in -ar 16000 -ac 1 -c:a pcm_s16le out.wav)"
                    );
                }
                format_ok = true;
            }
            b"data" => {
                if !format_ok {
                    bail!("data chunk before fmt chunk");
                }
                return Ok(bytes[body..end].to_vec());
            }
            _ => {}
        }
        pos = body + len + (len & 1);
    }
    bail!("no data chunk")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
        out.extend_from_slice(&(2 * channels).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn reads_16k_mono_pcm() {
        let pcm = parse_wav_16k_mono(&wav(16_000, 1, &[1, -2, 3])).unwrap();
        assert_eq!(pcm, [1, 0, 254, 255, 3, 0]);
    }

    #[test]
    fn rejects_other_formats() {
        assert!(parse_wav_16k_mono(&wav(44_100, 1, &[0])).is_err());
        assert!(parse_wav_16k_mono(&wav(16_000, 2, &[0, 0])).is_err());
        assert!(parse_wav_16k_mono(b"not a wav").is_err());
    }
}
