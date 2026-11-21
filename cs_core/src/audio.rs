//! Audio: the game's clips, decoded and mixed.
//!
//! Two thirds of `data.vfs.mp3` — 12.6 MB of the 19 — is **Ogg Vorbis**: 23
//! stereo tracks of about a minute (the music) and 62 mono clips (the effects),
//! all at 22 050 Hz. The original never decoded any of it itself: the guest
//! looked a clip up in the VFS and handed Java the *byte range*
//! (`putMusicClip(clipId, vfsOffset, length)`), and Java pointed a `MediaPlayer`
//! at the asset file descriptor with that offset and length.
//!
//! Here the same byte range is decoded with `lewton` and mixed in software. The
//! mixer is deliberately independent of any output API: it just fills an
//! interleaved stereo `i16` buffer on demand, so the Android side can drive it
//! from its audio callback and the headless probe can render the exact same mix
//! to a WAV file for checking.
//!
//! It lives behind a mutex because the guest and the audio device are on
//! different threads, and every operation is short: `put` decodes (off the
//! audio thread), `render` only walks the playing clips.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// WIPI clip states (`co.kr.eamobile.constant.WIPIConstant`).
pub const MEDIA_NOT_EXIST: i32 = -1;
pub const MEDIA_STOPPED: i32 = 0;
pub const MEDIA_PAUSED: i32 = 1;
pub const MEDIA_PLAYING: i32 = 2;

/// Which WIPI volume control a clip answers to. The game exposes three, and
/// they are genuinely separate settings in its options screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bus {
    Media,
    Music,
    Sound,
}

struct Clip {
    /// Decoded interleaved samples, always stereo. Mono sources are duplicated
    /// at decode time so the mixer has one layout to deal with.
    pcm: Arc<Vec<i16>>,
    rate: u32,
    bus: Bus,
    /// Playback cursor in output frames, as 16.16 fixed point, so a clip
    /// recorded at 22 050 Hz can feed a 48 000 Hz device without per-sample
    /// floating point.
    pos: u64,
    step: u64,
    playing: bool,
    paused: bool,
    looping: bool,
    /// The bytes are still being decoded on the worker thread. The clip exists,
    /// answers `play`/`stop`/`status` normally and simply makes no sound yet.
    pending: bool,
}

pub struct Mixer {
    out_rate: u32,
    clips: HashMap<i64, Clip>,
    /// 0..=100, the WIPI scale.
    vol: [i32; 3],
    /// Set while the app is backgrounded: `render` emits silence and clips stop
    /// advancing, so music does not keep playing behind the lock screen.
    pub muted: bool,
    pub decoded_clips: u64,
    pub decode_failures: u64,
}

fn bus_index(b: Bus) -> usize {
    match b {
        Bus::Media => 0,
        Bus::Music => 1,
        Bus::Sound => 2,
    }
}

impl Mixer {
    pub fn new(out_rate: u32) -> Self {
        Self {
            out_rate: out_rate.max(8000),
            clips: HashMap::new(),
            vol: [100, 100, 100],
            muted: false,
            decoded_clips: 0,
            decode_failures: 0,
        }
    }

    pub fn shared(out_rate: u32) -> Arc<Mutex<Mixer>> {
        Arc::new(Mutex::new(Mixer::new(out_rate)))
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// Decode `data` (Ogg Vorbis) and register it under `id`, replacing
    /// whatever was there. Returns false if it is not something we can decode.
    pub fn put(&mut self, id: i64, data: &[u8], bus: Bus) -> bool {
        let Some((pcm, rate)) = decode_ogg(data) else {
            self.decode_failures += 1;
            log::warn!(
                "clip {id}: not decodable ({} bytes, magic {:02x?})",
                data.len(),
                &data[..data.len().min(4)]
            );
            return false;
        };
        let step = ((rate as u64) << 16) / self.out_rate as u64;
        self.clips.insert(
            id,
            Clip {
                pcm: Arc::new(pcm),
                rate,
                bus,
                pos: 0,
                step,
                playing: false,
                paused: false,
                looping: false,
                pending: false,
            },
        );
        self.decoded_clips += 1;
        true
    }

    /// Register `id` as a clip whose samples are still being decoded.
    ///
    /// Decoding a one-minute Vorbis stream takes 55-140 ms on a desktop and
    /// two to four times that on a phone, and the guest asks for one at the
    /// moment a dialogue ends — on the frame thread, holding this very mutex,
    /// which also stalls the audio callback. So the bytes go to a worker and
    /// the clip exists immediately: `play` arms it, `status` answers, and the
    /// first samples arrive when they arrive. That is what the original did
    /// too — `MediaPlayer.setDataSource` was asynchronous.
    pub fn put_pending(&mut self, id: i64, bus: Bus) {
        self.clips.insert(
            id,
            Clip {
                pcm: Arc::new(Vec::new()),
                rate: self.out_rate,
                bus,
                pos: 0,
                step: 1 << 16,
                playing: false,
                paused: false,
                looping: false,
                pending: true,
            },
        );
    }

    /// Hand a pending clip its samples, keeping whatever the guest asked of it
    /// in the meantime. Ignored if the clip was freed or replaced while the
    /// worker was busy.
    pub fn finish(&mut self, id: i64, pcm: Vec<i16>, rate: u32) {
        let out_rate = self.out_rate;
        let Some(c) = self.clips.get_mut(&id).filter(|c| c.pending) else { return };
        c.pcm = Arc::new(pcm);
        c.rate = rate;
        c.step = ((rate as u64) << 16) / out_rate as u64;
        c.pos = 0;
        c.pending = false;
        self.decoded_clips += 1;
    }

    /// The worker could not decode it. Drop the placeholder so the guest sees
    /// the same `MEDIA_NOT_EXIST` a synchronous failure would have given it.
    pub fn fail(&mut self, id: i64) {
        if self.clips.get(&id).is_some_and(|c| c.pending) {
            self.clips.remove(&id);
            self.decode_failures += 1;
        }
    }

    pub fn play(&mut self, id: i64, looping: bool) -> bool {
        // One music track at a time, exactly like the single `MediaPlayer` the
        // original used for the music bus. Without this a screen transition
        // that starts the next track before freeing the last one plays both.
        let bus = self.clips.get(&id).map(|c| c.bus);
        if bus == Some(Bus::Music) {
            for (other, c) in self.clips.iter_mut() {
                if *other != id && c.bus == Bus::Music {
                    c.playing = false;
                    c.pos = 0;
                }
            }
        }
        match self.clips.get_mut(&id) {
            Some(c) => {
                c.playing = true;
                c.paused = false;
                c.looping = looping;
                c.pos = 0;
                true
            }
            None => false,
        }
    }

    /// Put a clip back the way a fresh `put` would have left it, without
    /// re-decoding. False if it is not loaded, so the caller falls back to the
    /// real thing.
    pub fn reset(&mut self, id: i64) -> bool {
        match self.clips.get_mut(&id) {
            Some(c) => {
                c.playing = false;
                c.paused = false;
                c.looping = false;
                c.pos = 0;
                true
            }
            None => false,
        }
    }

    pub fn stop(&mut self, id: i64) -> bool {
        match self.clips.get_mut(&id) {
            Some(c) => {
                c.playing = false;
                c.paused = false;
                c.pos = 0;
                true
            }
            None => false,
        }
    }

    pub fn pause(&mut self, id: i64) -> bool {
        match self.clips.get_mut(&id) {
            Some(c) if c.playing => {
                c.paused = true;
                true
            }
            _ => false,
        }
    }

    pub fn resume(&mut self, id: i64) -> bool {
        match self.clips.get_mut(&id) {
            Some(c) if c.playing => {
                c.paused = false;
                true
            }
            _ => false,
        }
    }

    pub fn free(&mut self, id: i64) -> bool {
        self.clips.remove(&id).is_some()
    }

    pub fn status(&self, id: i64) -> i32 {
        match self.clips.get(&id) {
            None => MEDIA_NOT_EXIST,
            Some(c) if c.paused => MEDIA_PAUSED,
            Some(c) if c.playing => MEDIA_PLAYING,
            Some(_) => MEDIA_STOPPED,
        }
    }

    pub fn set_volume(&mut self, bus: Bus, v: i32) {
        self.vol[bus_index(bus)] = v.clamp(0, 100);
    }

    pub fn volume(&self, bus: Bus) -> i32 {
        self.vol[bus_index(bus)]
    }

    /// Fill `out` (interleaved stereo) with the current mix.
    pub fn render(&mut self, out: &mut [i16]) {
        out.fill(0);
        if self.muted {
            return;
        }
        let frames = out.len() / 2;
        let vol = self.vol;

        for clip in self.clips.values_mut() {
            if !clip.playing || clip.paused {
                continue;
            }
            let gain = vol[bus_index(clip.bus)];
            let pcm = clip.pcm.clone();
            let total = (pcm.len() / 2) as u64;
            if total == 0 {
                // A pending clip keeps its `playing` flag: the worker is about
                // to hand it its samples, and clearing it here would turn "not
                // decoded yet" into "finished".
                clip.playing &= clip.pending;
                continue;
            }

            for f in 0..frames {
                let idx = clip.pos >> 16;
                if idx >= total {
                    if clip.looping {
                        clip.pos = 0;
                    } else {
                        clip.playing = false;
                        clip.pos = 0;
                        break;
                    }
                }
                let i = ((clip.pos >> 16) as usize) * 2;
                // Accumulate in i32 and clamp once: two or three clips at full
                // scale would otherwise wrap and click.
                let l = out[f * 2] as i32 + (pcm[i] as i32 * gain / 100);
                let r = out[f * 2 + 1] as i32 + (pcm[i + 1] as i32 * gain / 100);
                out[f * 2] = l.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                out[f * 2 + 1] = r.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                clip.pos += clip.step;
            }
        }
    }

    /// Diagnostics: how many clips are loaded and how many are sounding.
    pub fn stats(&self) -> (usize, usize) {
        (self.clips.len(), self.clips.values().filter(|c| c.playing && !c.paused).count())
    }

    /// Total decoded bytes held, so a leak shows up as a number rather than as
    /// an out-of-memory kill.
    pub fn bytes(&self) -> usize {
        self.clips.values().map(|c| c.pcm.len() * 2).sum()
    }

    #[allow(dead_code)]
    fn rate_of(&self, id: i64) -> Option<u32> {
        self.clips.get(&id).map(|c| c.rate)
    }
}

/// A worker thread that decodes clips and publishes them into the mixer.
///
/// One thread, one queue, decoding in submission order — the guest never needs
/// two clips at once, and doing them in order keeps the behaviour predictable.
/// It holds the mixer lock only to publish, never across a decode, which is the
/// point: the audio callback wants that lock every few milliseconds.
pub struct Decoder {
    tx: std::sync::mpsc::Sender<(i64, Vec<u8>, Bus)>,
}

impl Decoder {
    pub fn start(mixer: Arc<Mutex<Mixer>>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(i64, Vec<u8>, Bus)>();
        std::thread::Builder::new()
            .name("cs-audio-decode".into())
            .spawn(move || {
                for (id, data, _bus) in rx {
                    match decode_ogg(&data) {
                        Some((pcm, rate)) => {
                            if let Ok(mut m) = mixer.lock() {
                                m.finish(id, pcm, rate);
                            }
                        }
                        None => {
                            log::warn!("clip {id}: not decodable ({} bytes)", data.len());
                            if let Ok(mut m) = mixer.lock() {
                                m.fail(id);
                            }
                        }
                    }
                }
            })
            .ok();
        Self { tx }
    }

    /// Returns false if the worker is gone, so the caller can fall back to
    /// decoding inline rather than losing the clip.
    pub fn submit(&self, id: i64, data: Vec<u8>, bus: Bus) -> bool {
        self.tx.send((id, data, bus)).is_ok()
    }
}

/// Decode Ogg Vorbis to interleaved stereo `i16`. Mono is duplicated to both
/// channels here so the mixer only ever sees one layout.
pub fn decode_ogg(data: &[u8]) -> Option<(Vec<i16>, u32)> {
    use lewton::inside_ogg::OggStreamReader;

    if data.len() < 4 || &data[..4] != b"OggS" {
        return None;
    }
    let mut r = OggStreamReader::new(std::io::Cursor::new(data)).ok()?;
    let channels = r.ident_hdr.audio_channels as usize;
    let rate = r.ident_hdr.audio_sample_rate;
    let mut out: Vec<i16> = Vec::new();

    while let Ok(Some(packet)) = r.read_dec_packet_itl() {
        match channels {
            0 => return None,
            1 => {
                out.reserve(packet.len() * 2);
                for s in packet {
                    out.push(s);
                    out.push(s);
                }
            }
            2 => out.extend_from_slice(&packet),
            n => {
                // Nothing in this game is multichannel; fold anyway rather than
                // dropping the clip.
                out.reserve(packet.len() / n * 2);
                for frame in packet.chunks_exact(n) {
                    out.push(frame[0]);
                    out.push(frame[1]);
                }
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    Some((out, rate))
}
