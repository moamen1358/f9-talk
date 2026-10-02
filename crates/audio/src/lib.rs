//! cpal-based microphone streamer with resampling + channel down-mix.
//!
//! - cpal opens the system default input device at its **native** sample
//!   rate / format (most consumer hardware: 44.1 / 48 kHz F32, 1 or 2 ch).
//! - Each callback we down-mix to mono, linearly resample to 16 kHz, and
//!   convert to int16 - the format every STT backend in the repo expects.
//! - 25 ms frames (800 bytes) pushed into a bounded `mpsc::channel(64)`
//!   (≈1.6 s headroom). On overflow the audio thread drops oldest with a
//!   counted warn-log - it must never block.
//! - On stream error (device disappear, callback panic, EOF) the spawner
//!   relaunches with exponential backoff (1 s → 30 s cap).

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use f9_talk_core::{FRAME_BYTES, FRAME_CHANNEL_CAPACITY, SAMPLE_RATE_HZ};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct Frame {
    pub bytes: Vec<u8>,
}

pub type RmsHandle = Arc<Mutex<f32>>;

pub fn spawn() -> anyhow::Result<(
    mpsc::Receiver<Frame>,
    RmsHandle,
    tokio::task::JoinHandle<()>,
)> {
    let (tx, rx) = mpsc::channel::<Frame>(FRAME_CHANNEL_CAPACITY);
    let rms = Arc::new(Mutex::new(0.0_f32));
    let dropped = Arc::new(AtomicU64::new(0));
    let task = tokio::spawn(stream_loop(tx, rms.clone(), dropped));
    Ok((rx, rms, task))
}

async fn stream_loop(tx: mpsc::Sender<Frame>, rms_handle: RmsHandle, dropped: Arc<AtomicU64>) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);
    loop {
        match build_and_run_stream(&tx, &rms_handle, &dropped) {
            Ok(()) => {
                info!("mic stream closed normally");
                return;
            }
            Err(e) => {
                error!("mic stream error: {e}; reopening in {:.1?}", backoff);
            }
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Linear-interpolation resampler. Tracks a fractional read cursor + the
/// last seen input sample so resampling stays continuous across cpal
/// callback boundaries (no clicks at chunk seams).
#[derive(Debug)]
struct Resampler {
    in_rate: f64,
    out_rate: f64,
    cursor: f64,
    last_sample: f32,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate: in_rate as f64,
            out_rate: out_rate as f64,
            cursor: 0.0,
            last_sample: 0.0,
        }
    }

    /// Resample mono f32 input to mono f32 at out_rate.
    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        let step = self.in_rate / self.out_rate;
        // Estimate output length so we don't grow the Vec mid-loop.
        let est = ((input.len() as f64 - self.cursor) / step).max(0.0).ceil() as usize;
        let mut out = Vec::with_capacity(est);

        while self.cursor < input.len() as f64 {
            let i_floor = self.cursor.floor() as isize;
            let frac = (self.cursor - i_floor as f64) as f32;
            let s0 = if i_floor < 0 {
                self.last_sample
            } else {
                input[i_floor as usize]
            };
            let s1 = if (i_floor + 1) as usize >= input.len() {
                // We don't have the next sample yet; reuse the current
                // one (frac rarely reaches close to 1 for 44.1→16 ratios).
                s0
            } else {
                input[(i_floor + 1) as usize]
            };
            out.push(s0 * (1.0 - frac) + s1 * frac);
            self.cursor += step;
        }
        // Carry the fractional remainder for the next batch (so a sample
        // at position 1023.7 in this batch picks up at -0.3 in the next).
        self.cursor -= input.len() as f64;
        self.last_sample = *input.last().unwrap_or(&0.0);
        out
    }
}

/// Per-stream state shared with the cpal callback.
struct StreamState {
    resampler: Mutex<Resampler>,
    channels: u16,
    pending: Mutex<Vec<u8>>, // accumulator for partial 800 B frames
}

fn build_and_run_stream(
    tx: &mpsc::Sender<Frame>,
    rms_handle: &RmsHandle,
    dropped: &Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("no default input device"))?;
    let device_id = device.id().ok();

    let supported = device.default_input_config()?;
    let in_rate: u32 = supported.sample_rate();
    let channels = supported.channels();
    info!(
        "mic: device={device_id:?} native_rate={} Hz channels={} format={:?} → resample to {} Hz mono s16le",
        in_rate, channels, supported.sample_format(), SAMPLE_RATE_HZ
    );

    let config = supported.config();
    let state = Arc::new(StreamState {
        resampler: Mutex::new(Resampler::new(in_rate, SAMPLE_RATE_HZ)),
        channels,
        pending: Mutex::new(Vec::with_capacity(FRAME_BYTES * 4)),
    });

    let tx_cb = tx.clone();
    let rms_cb = rms_handle.clone();
    let dropped_cb = dropped.clone();
    let state_cb = state.clone();

    let err_cb = |e| {
        error!("cpal stream error callback: {e}");
    };

    let stream = match supported.sample_format() {
        SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| {
                let mono: Vec<f32> = downmix_i16(data, state_cb.channels);
                forward(&mono, &state_cb, &tx_cb, &rms_cb, &dropped_cb);
            },
            err_cb,
            None,
        )?,
        SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| {
                let mono: Vec<f32> = downmix_f32(data, state_cb.channels);
                forward(&mono, &state_cb, &tx_cb, &rms_cb, &dropped_cb);
            },
            err_cb,
            None,
        )?,
        other => anyhow::bail!("unsupported cpal sample format {other:?}"),
    };
    stream.play()?;
    info!("mic stream live (target {} Hz mono s16le)", SAMPLE_RATE_HZ);

    // Park until receiver is dropped.
    while !tx.is_closed() {
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

fn downmix_i16(data: &[i16], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
    }
    let mut out = Vec::with_capacity(data.len() / ch);
    for frame in data.chunks_exact(ch) {
        let sum: f32 = frame.iter().map(|s| *s as f32 / i16::MAX as f32).sum();
        out.push(sum / ch as f32);
    }
    out
}

fn downmix_f32(data: &[f32], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len() / ch);
    for frame in data.chunks_exact(ch) {
        let sum: f32 = frame.iter().sum();
        out.push(sum / ch as f32);
    }
    out
}

fn forward(
    mono_in: &[f32],
    state: &Arc<StreamState>,
    tx: &mpsc::Sender<Frame>,
    rms_handle: &RmsHandle,
    dropped: &AtomicU64,
) {
    if mono_in.is_empty() {
        return;
    }
    update_rms(mono_in, rms_handle);

    let resampled = state.resampler.lock().process(mono_in);

    // f32 [-1,1] → int16 little-endian
    let mut new_bytes = Vec::with_capacity(resampled.len() * 2);
    for s in &resampled {
        let pcm = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        new_bytes.extend_from_slice(&pcm.to_le_bytes());
    }

    // Stitch with whatever bytes were left over from the previous callback.
    let mut pending = state.pending.lock();
    pending.extend_from_slice(&new_bytes);

    while pending.len() >= FRAME_BYTES {
        let frame_bytes: Vec<u8> = pending.drain(..FRAME_BYTES).collect();
        match tx.try_send(Frame { bytes: frame_bytes }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                let n = dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    warn!("mic frame channel full: dropped {n} frames total");
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                pending.clear();
                return;
            }
        }
    }
}

fn update_rms(mono: &[f32], rms_handle: &RmsHandle) {
    if mono.is_empty() {
        return;
    }
    let sum_sq: f64 = mono.iter().map(|s| (*s as f64).powi(2)).sum();
    let rms = (sum_sq / mono.len() as f64).sqrt();
    *rms_handle.lock() = (rms as f32).clamp(0.0, 1.0);
}
