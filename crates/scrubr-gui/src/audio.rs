//! cpal playback, streamed from disk: a WAV file, or a project's timeline with its edits applied.
//!
//! Three threads share a handful of atomics:
//! - the **decoder** reads the source, converts to stereo at the device's rate and fills a
//!   lock-free ring buffer;
//! - the **audio callback** drains the ring into the device and counts the frames it played;
//! - the **UI** plays, pauses, seeks and reads the position. It never blocks on either.
//!
//! The stream runs for the player's whole life and plays silence while paused, so the callback
//! is always there to acknowledge a seek.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};
use scrubr_core::project::{Paths, Project};
use scrubr_core::render::Renderer;
use scrubr_core::source::FileAudio;
use scrubr_core::WavFile;

/// Source frames decoded per read.
const CHUNK: usize = 4096;

/// What a player streams: audio at one rate that can be read from any frame.
pub trait Source: Send + 'static {
    fn frames(&self) -> u64;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> usize;
    fn seek(&mut self, frame: u64);
    /// Replaces `out` with up to `frames` interleaved frames from the current position and
    /// returns how many it read. Zero is the end.
    fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> usize;
}

impl Source for WavFile {
    fn frames(&self) -> u64 {
        self.info().frames
    }

    fn sample_rate(&self) -> u32 {
        self.info().sample_rate
    }

    fn channels(&self) -> usize {
        self.info().channels as usize
    }

    fn seek(&mut self, frame: u64) {
        let _ = WavFile::seek(self, frame);
    }

    fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> usize {
        WavFile::read(self, frames, out).unwrap_or(0)
    }
}

/// A project's timeline at head, rendered as it plays: cuts, level edits and crossfades included.
struct TimelineSource {
    renderer: Renderer,
    rate: u32,
    pos: u64,
}

impl Source for TimelineSource {
    fn frames(&self) -> u64 {
        self.renderer.total()
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> usize {
        self.renderer.channels()
    }

    fn seek(&mut self, frame: u64) {
        self.pos = frame.min(self.renderer.total());
    }

    fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> usize {
        let n = (self.renderer.total().saturating_sub(self.pos) as usize).min(frames);
        if n == 0 || self.renderer.read(self.pos, n, out).is_err() {
            out.clear();
            return 0;
        }
        self.pos += n as u64;
        n
    }
}

/// A file played as it is, before it joins a project: any format the core reads.
struct FileSource {
    audio: FileAudio,
    /// The length, or a very large number when the file does not state one.
    frames: u64,
}

impl Source for FileSource {
    fn frames(&self) -> u64 {
        self.frames
    }

    fn sample_rate(&self) -> u32 {
        self.audio.sample_rate()
    }

    fn channels(&self) -> usize {
        self.audio.channels()
    }

    fn seek(&mut self, frame: u64) {
        // A compressed file is read from its start: go back there and read up to the frame.
        if self.audio.rewind().is_err() {
            return;
        }
        let (mut left, mut skipped) = (frame, Vec::new());
        while left > 0 {
            match self.audio.read((left as usize).min(CHUNK), &mut skipped) {
                Ok(n) if n > 0 => left -= n as u64,
                _ => break,
            }
        }
    }

    fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> usize {
        self.audio.read(frames, out).unwrap_or(0)
    }
}

struct Shared {
    playing: AtomicBool,
    quit: AtomicBool,
    /// Frame the UI asked to seek to, or -1. The decoder takes it.
    seek_to: AtomicI64,
    /// The same frame, held until the decoder has finished the seek, so `position` reports the
    /// target at once.
    pending: AtomicI64,
    /// Set by the decoder around a seek: the callback empties the ring and plays silence.
    flushing: AtomicBool,
    flushed: AtomicBool,
    /// Source frame the audio in the ring starts from.
    base: AtomicU64,
    /// Device frames played since `base`.
    played: AtomicU64,
    /// Device frames handed to the device's queue since `base`.
    fed: AtomicU64,
    /// The decoder reached the end of the file.
    ended: AtomicBool,
    /// Callbacks that ran out of audio while playing before the end of the file.
    underruns: AtomicU64,
    gain: AtomicU32,
    /// The highest sample magnitude played on the left and the right since the UI last took
    /// them, as `f32` bits, before the output gain. Magnitudes are never negative, so their
    /// bits order the same way they do.
    level: [AtomicU32; 2],
}

pub struct Player {
    shared: Arc<Shared>,
    frames: u64,
    src_rate: u32,
    pub dev_rate: u32,
    pub dev_channels: u16,
    pub device: String,
}

impl Player {
    /// Opens the default output device and starts streaming `path`, paused at frame 0.
    pub fn open(path: &Path, gain: f32) -> Result<Player, String> {
        Self::start(Box::new(WavFile::open(path).map_err(|e| e.to_string())?), gain)
    }

    /// Opens the default output device over a project's timeline at head, paused at frame 0.
    /// The player holds the timeline it was given: open another after an edit.
    pub fn timeline(project: &Project, paths: &Paths, gain: f32) -> Result<Player, String> {
        let renderer = Renderer::new(project, paths).map_err(|e| e.to_string())?;
        Self::start(Box::new(TimelineSource { renderer, rate: project.sample_rate, pos: 0 }), gain)
    }

    /// Opens the default output device over an audio file in any format the core reads, paused
    /// at its start: for hearing a file before adding it to a project.
    pub fn file(path: &Path, gain: f32) -> Result<Player, String> {
        let audio = FileAudio::open(path).map_err(|e| e.to_string())?;
        let frames = audio.frames().unwrap_or(u64::MAX / 4);
        Self::start(Box::new(FileSource { audio, frames }), gain)
    }

    fn start(wav: Box<dyn Source>, gain: f32) -> Result<Player, String> {
        let frames = wav.frames();
        let src_rate = wav.sample_rate();
        let shared = Arc::new(Shared {
            playing: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            seek_to: AtomicI64::new(-1),
            pending: AtomicI64::new(-1),
            flushing: AtomicBool::new(false),
            flushed: AtomicBool::new(false),
            base: AtomicU64::new(0),
            played: AtomicU64::new(0),
            fed: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            gain: AtomicU32::new(gain.to_bits()),
            level: [AtomicU32::new(0), AtomicU32::new(0)],
        });

        // A cpal stream is not `Send` on every platform, so it lives on the thread that made it.
        let (ready_tx, ready_rx) = mpsc::channel();
        let for_stream = shared.clone();
        thread::Builder::new()
            .name("scrubr-audio".into())
            .spawn(move || match start_stream(wav, for_stream.clone()) {
                Ok((stream, info)) => {
                    let _ = ready_tx.send(Ok(info));
                    while !for_stream.quit.load(Relaxed) {
                        thread::park_timeout(Duration::from_millis(200));
                    }
                    drop(stream);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| e.to_string())?;
        let (device, dev_rate, dev_channels) =
            ready_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "the audio device did not start".to_string())??;
        Ok(Player { shared, frames, src_rate, dev_rate, dev_channels, device })
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Relaxed)
    }

    pub fn play(&self) {
        if self.position() >= self.frames {
            self.seek(0);
        }
        self.shared.playing.store(true, Relaxed);
    }

    pub fn pause(&self) {
        self.shared.playing.store(false, Relaxed);
    }

    pub fn toggle(&self) {
        if self.is_playing() { self.pause() } else { self.play() }
    }

    pub fn seek(&self, frame: u64) {
        let frame = frame.min(self.frames) as i64;
        self.shared.pending.store(frame, Release);
        self.shared.seek_to.store(frame, Release);
    }

    /// The playhead in source frames.
    pub fn position(&self) -> u64 {
        let pending = self.shared.pending.load(Acquire);
        if pending >= 0 {
            return pending as u64;
        }
        let played = self.shared.played.load(Relaxed) as f64 * self.src_rate as f64 / self.dev_rate as f64;
        (self.shared.base.load(Relaxed) + played as u64).min(self.frames)
    }

    /// True once the file has played to its end.
    pub fn finished(&self) -> bool {
        self.shared.ended.load(Relaxed) && self.position() >= self.frames.saturating_sub(self.src_rate as u64 / 50)
    }

    /// True once everything the source held has been played. Unlike [`Player::finished`] it does
    /// not need the source to have said how long it is.
    pub fn done(&self) -> bool {
        self.shared.ended.load(Relaxed) && self.shared.pending.load(Acquire) < 0 && self.shared.played.load(Relaxed) >= self.shared.fed.load(Relaxed)
    }

    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Relaxed)
    }

    /// The highest sample magnitude played on the left and the right since the last call, and
    /// zero while nothing plays. It is the timeline's own level: the output gain does not
    /// change it.
    pub fn take_levels(&self) -> [f32; 2] {
        [0, 1].map(|c| f32::from_bits(self.shared.level[c].swap(0, Relaxed)))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.quit.store(true, Relaxed);
    }
}

fn start_stream(wav: Box<dyn Source>, shared: Arc<Shared>) -> Result<(cpal::Stream, (String, u32, u16)), String> {
    let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
    let name = device.id().map(|id| id.to_string()).unwrap_or_else(|_| "default output".into());
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let dev_rate = config.sample_rate;
    let dev_channels = config.channels;

    // Half a second of stereo at the device's rate.
    let (producer, consumer) = RingBuffer::<f32>::new(dev_rate as usize);
    let for_decoder = shared.clone();
    thread::Builder::new()
        .name("scrubr-decode".into())
        .spawn(move || decode(wav, producer, for_decoder, dev_rate))
        .map_err(|e| e.to_string())?;

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, consumer, shared),
        SampleFormat::I16 => build::<i16>(&device, config, consumer, shared),
        SampleFormat::I32 => build::<i32>(&device, config, consumer, shared),
        other => Err(format!("the output device wants {other} samples, which the spike does not write")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, (name, dev_rate, dev_channels)))
}

fn build<T>(device: &cpal::Device, config: StreamConfig, mut ring: Consumer<f32>, shared: Arc<Shared>) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let channels = config.channels as usize;
    let silence = T::from_sample(0.0f32);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                if shared.flushing.load(Acquire) {
                    let n = ring.slots();
                    if let Ok(chunk) = ring.read_chunk(n) {
                        chunk.commit_all();
                    }
                    data.fill(silence);
                    shared.flushed.store(true, Release);
                    return;
                }
                if !shared.playing.load(Relaxed) {
                    data.fill(silence);
                    return;
                }
                let gain = f32::from_bits(shared.gain.load(Relaxed));
                let want = data.len() / channels;
                let have = (ring.slots() / 2).min(want);
                let mut out = data.chunks_exact_mut(channels);
                let mut loudest = [0f32; 2];
                if let Ok(chunk) = ring.read_chunk(have * 2) {
                    let (a, b) = chunk.as_slices();
                    let mut samples = a.iter().chain(b.iter());
                    for frame in out.by_ref().take(have) {
                        let l = samples.next().copied().unwrap_or(0.0);
                        let r = samples.next().copied().unwrap_or(0.0);
                        loudest = [loudest[0].max(l.abs()), loudest[1].max(r.abs())];
                        let (l, r) = (l * gain, r * gain);
                        for (c, s) in frame.iter_mut().enumerate() {
                            *s = match c {
                                0 => T::from_sample(l),
                                1 => T::from_sample(r),
                                _ => silence,
                            };
                        }
                        // A mono device hears both channels.
                        if channels == 1 {
                            frame[0] = T::from_sample((l + r) * 0.5);
                        }
                    }
                    chunk.commit_all();
                }
                for frame in out {
                    frame.fill(silence);
                }
                shared.played.fetch_add(have as u64, Relaxed);
                for (level, loudest) in shared.level.iter().zip(loudest) {
                    if loudest.is_finite() {
                        level.fetch_max(loudest.to_bits(), Relaxed);
                    }
                }
                if have < want && !shared.ended.load(Relaxed) {
                    shared.underruns.fetch_add(1, Relaxed);
                }
            },
            |err| eprintln!("audio stream: {err}"),
            None,
        )
        .map_err(|e| e.to_string())
}

/// Linear interpolation from the file's rate to the device's. Good enough to hear a spike;
/// the real engine resamples with rubato.
struct Resampler {
    /// Source frames per output frame.
    step: f64,
    /// Read position relative to the current chunk's first frame; -1.0 is `prev`.
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    fn new(src_rate: u32, dev_rate: u32) -> Self {
        Resampler { step: src_rate as f64 / dev_rate as f64, pos: 0.0, prev: [0.0; 2] }
    }

    fn reset(&mut self) {
        self.pos = 0.0;
        self.prev = [0.0; 2];
    }

    /// `input` and `out` are interleaved stereo.
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len() / 2;
        if n == 0 {
            return;
        }
        let at = |i: isize| if i < 0 { self.prev } else { [input[2 * i as usize], input[2 * i as usize + 1]] };
        while self.pos < n as f64 - 1.0 {
            let i = self.pos.floor();
            let t = (self.pos - i) as f32;
            let (a, b) = (at(i as isize), at(i as isize + 1));
            out.push(a[0] + (b[0] - a[0]) * t);
            out.push(a[1] + (b[1] - a[1]) * t);
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev = [input[2 * n - 2], input[2 * n - 1]];
    }
}

fn decode(mut wav: Box<dyn Source>, mut ring: Producer<f32>, shared: Arc<Shared>, dev_rate: u32) {
    let channels = wav.channels().max(1);
    let src_rate = wav.sample_rate();
    let same_rate = src_rate == dev_rate;
    let mut resampler = Resampler::new(src_rate, dev_rate);
    let mut raw = Vec::with_capacity(CHUNK * channels);
    let mut stereo = Vec::with_capacity(CHUNK * 2);
    let mut out = Vec::with_capacity(CHUNK * 4);
    // The most samples one chunk can turn into, so a chunk is only decoded when it will fit.
    let worst = ((CHUNK as f64 / resampler.step).ceil() as usize + 2) * 2;

    while !shared.quit.load(Relaxed) {
        let target = shared.seek_to.swap(-1, Acquire);
        if target >= 0 {
            // Stop the callback reading, wait for it to empty the ring, then restart from the
            // target. If the device has stalled the wait gives up and the seek still lands.
            shared.flushed.store(false, Release);
            shared.flushing.store(true, Release);
            let since = Instant::now();
            while !shared.flushed.load(Acquire) && since.elapsed() < Duration::from_millis(250) {
                thread::sleep(Duration::from_micros(500));
            }
            wav.seek(target as u64);
            resampler.reset();
            shared.base.store(target as u64, Release);
            shared.played.store(0, Release);
            shared.fed.store(0, Release);
            shared.ended.store(false, Release);
            shared.flushing.store(false, Release);
            // Leave `pending` alone if the UI has already asked for another seek.
            let _ = shared.pending.compare_exchange(target, -1, Release, Relaxed);
            continue;
        }
        if shared.ended.load(Relaxed) || ring.slots() < worst {
            thread::sleep(Duration::from_millis(2));
            continue;
        }
        if wav.read(CHUNK, &mut raw) == 0 {
            shared.ended.store(true, Release);
            continue;
        }
        stereo.clear();
        for frame in raw.chunks_exact(channels) {
            stereo.push(frame[0]);
            stereo.push(if channels > 1 { frame[1] } else { frame[0] });
        }
        let samples: &[f32] = if same_rate {
            &stereo
        } else {
            out.clear();
            resampler.process(&stereo, &mut out);
            &out
        };
        if let Ok(mut chunk) = ring.write_chunk(samples.len()) {
            let (a, b) = chunk.as_mut_slices();
            a.copy_from_slice(&samples[..a.len()]);
            b.copy_from_slice(&samples[a.len()..]);
            chunk.commit_all();
            shared.fed.fetch_add(samples.len() as u64 / 2, Relaxed);
        }
    }
}
