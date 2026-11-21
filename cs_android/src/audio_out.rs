//! Pushing the mixer's output at an audio device.
//!
//! `cs_core::audio::Mixer` produces interleaved stereo `i16` on demand and
//! knows nothing about any audio API; this is the twenty lines that connect it
//! to one. cpal's Android backend talks to AAudio directly (no `oboe`, so no
//! C++ build in the cargo-ndk cross-compile), which is the reason it is cpal
//! here rather than something that would need cmake.
//!
//! Failure is not fatal: a device that will not open leaves `Audio::stream` as
//! `None` and the game runs silent, which is exactly what it did before audio
//! existed at all.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cs_core::audio::Mixer;
use std::sync::{Arc, Mutex};

pub struct Audio {
    pub mixer: Arc<Mutex<Mixer>>,
    /// Dropping this stops playback, so it has to outlive the app.
    _stream: Option<cpal::Stream>,
}

impl Audio {
    /// Open the default output and start pulling from a mixer built at that
    /// device's own sample rate, so the mixer resamples once instead of the
    /// device resampling every buffer.
    pub fn start() -> Self {
        match Self::try_start() {
            Ok(a) => a,
            Err(e) => {
                log::warn!("audio unavailable, running silent: {e}");
                Self { mixer: Mixer::shared(22_050), _stream: None }
            }
        }
    }

    fn try_start() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no default output device")?;
        let cfg = device.default_output_config().map_err(|e| e.to_string())?;
        // cpal 0.17 returns a plain `u32` here, not the `SampleRate` newtype
        // older versions used.
        let rate: u32 = cfg.sample_rate();
        let channels = cfg.channels() as usize;
        log::info!("audio device: {rate} Hz, {channels} ch, {:?}", cfg.sample_format());

        let mixer = Mixer::shared(rate);
        let m = mixer.clone();
        // Scratch for the stereo mix, grown to whatever the device asks for and
        // then reused: this runs on the audio callback thread, where an
        // allocation per buffer is exactly the thing that causes glitches.
        let mut scratch: Vec<i16> = Vec::new();

        let err = |e| log::error!("audio stream error: {e}");
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::I16 => device.build_output_stream(
                &cfg.into(),
                move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    render(&m, &mut scratch, out.len() / channels, channels, |i, v| out[i] = v);
                },
                err,
                None,
            ),
            cpal::SampleFormat::F32 => device.build_output_stream(
                &cfg.into(),
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    render(&m, &mut scratch, out.len() / channels, channels, |i, v| {
                        out[i] = v as f32 / 32768.0
                    });
                },
                err,
                None,
            ),
            other => return Err(format!("unsupported sample format {other:?}")),
        }
        .map_err(|e| e.to_string())?;

        stream.play().map_err(|e| e.to_string())?;
        Ok(Self { mixer, _stream: Some(stream) })
    }
}

/// Render `frames` stereo frames and fan them out to a device layout of
/// `channels` (mono takes the left channel; more than two duplicates the pair
/// and leaves the rest silent).
fn render(
    mixer: &Arc<Mutex<Mixer>>,
    scratch: &mut Vec<i16>,
    frames: usize,
    channels: usize,
    mut put: impl FnMut(usize, i16),
) {
    scratch.clear();
    scratch.resize(frames * 2, 0);
    if let Ok(mut m) = mixer.lock() {
        m.render(scratch);
    }
    for f in 0..frames {
        let (l, r) = (scratch[f * 2], scratch[f * 2 + 1]);
        for c in 0..channels {
            let v = match c {
                0 => l,
                1 => r,
                _ => 0,
            };
            put(f * channels + c, v);
        }
    }
}
