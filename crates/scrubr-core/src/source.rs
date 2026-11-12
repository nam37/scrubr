//! Bringing audio files into a project.
//!
//! A WAV file at the project's sample rate is read in place. Anything else (another rate, or a
//! compressed format) is decoded once, resampled if needed, and written as a float WAV into the
//! project's cache folder; the source then reads from that copy. The copy is rebuilt from the
//! original when it is missing. Everything streams: no file is held in memory.

use std::fs;
use std::path::Path;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::error::{Error, Result, io_at};
use crate::peaks::{PeakBuilder, Peaks};
use crate::project::{Paths, Source, file_mtime, hash_file};
use crate::wav::WavFile;

const CHUNK_FRAMES: usize = 1 << 15;

/// A stream of decoded audio: interleaved `f32` frames at the file's own rate.
trait Frames {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> usize;
    /// Replaces `out` with the next frames. An empty `out` is the end.
    fn next(&mut self, out: &mut Vec<f32>) -> Result<()>;
}

impl Frames for WavFile {
    fn sample_rate(&self) -> u32 {
        self.info().sample_rate
    }

    fn channels(&self) -> usize {
        self.info().channels as usize
    }

    fn next(&mut self, out: &mut Vec<f32>) -> Result<()> {
        self.read(CHUNK_FRAMES, out).map(|_| ())
    }
}

/// Any format symphonia reads: FLAC, MP3, OGG Vorbis and the WAV variants hound does not.
struct Decoded {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track: u32,
    rate: u32,
    channels: usize,
    bits: u16,
    buf: Option<SampleBuffer<f32>>,
}

fn decode_error(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::codec("decode_failed", format!("Could not decode {}: {e}", path.display()))
        .hint("scrubr reads WAV, FLAC, MP3 and OGG Vorbis. Convert other formats to WAV first.")
}

impl Decoded {
    fn open(path: &Path) -> Result<Decoded> {
        let file = fs::File::open(path).map_err(|e| io_at(path, e))?;
        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format_opts = FormatOptions { enable_gapless: true, ..Default::default() };
        let probed = symphonia::default::get_probe()
            .format(&hint, stream, &format_opts, &MetadataOptions::default())
            .map_err(|e| decode_error(path, e))?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| decode_error(path, "it has no audio track"))?;
        let params = track.codec_params.clone();
        let decoder = symphonia::default::get_codecs().make(&params, &DecoderOptions::default()).map_err(|e| decode_error(path, e))?;
        let rate = params.sample_rate.ok_or_else(|| decode_error(path, "it does not state a sample rate"))?;
        let mut decoded = Decoded {
            track: track.id,
            format,
            decoder,
            rate,
            channels: params.channels.map_or(0, |c| c.count()),
            bits: params.bits_per_sample.unwrap_or(16) as u16,
            buf: None,
        };
        if decoded.channels == 0 {
            // Some formats only reveal the channel layout with the first packet.
            let mut first = Vec::new();
            decoded.decode_next(path, &mut first)?;
            return Err(decode_error(path, "it does not state its channel count"));
        }
        Ok(decoded)
    }

    fn decode_next(&mut self, path: &Path, out: &mut Vec<f32>) -> Result<()> {
        out.clear();
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(SymError::ResetRequired) => return Ok(()),
                Err(e) => return Err(decode_error(path, e)),
            };
            if packet.track_id() != self.track {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(audio) => {
                    let spec = *audio.spec();
                    if spec.channels.count() != self.channels || spec.rate != self.rate {
                        return Err(decode_error(path, "its format changes partway through"));
                    }
                    let buf = self.buf.get_or_insert_with(|| SampleBuffer::<f32>::new(audio.capacity() as u64, spec));
                    if buf.capacity() < audio.capacity() * self.channels {
                        *buf = SampleBuffer::<f32>::new(audio.capacity() as u64, spec);
                    }
                    buf.copy_interleaved_ref(audio);
                    if !buf.samples().is_empty() {
                        out.extend_from_slice(buf.samples());
                        return Ok(());
                    }
                }
                // A damaged packet is skipped, as players do.
                Err(SymError::DecodeError(_)) => continue,
                Err(e) => return Err(decode_error(path, e)),
            }
        }
    }
}

struct DecodedFile<'a> {
    path: &'a Path,
    inner: Decoded,
}

impl Frames for DecodedFile<'_> {
    fn sample_rate(&self) -> u32 {
        self.inner.rate
    }

    fn channels(&self) -> usize {
        self.inner.channels
    }

    fn next(&mut self, out: &mut Vec<f32>) -> Result<()> {
        self.inner.decode_next(self.path, out)
    }
}

/// What a file is, read from its header.
#[derive(Clone, Debug)]
pub struct Probe {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub float: bool,
    /// Length in frames at the file's own rate, when the header gives it.
    pub frames: Option<u64>,
    /// A WAV that can be read in place, without a converted copy.
    pub wav: bool,
}

pub fn probe(path: &Path) -> Result<Probe> {
    if !path.exists() {
        return Err(Error::not_found("file_not_found", format!("{} does not exist.", path.display())).hint("Check the path to the audio file."));
    }
    if let Ok(wav) = WavFile::open(path) {
        let info = wav.info();
        return Ok(Probe { sample_rate: info.sample_rate, channels: info.channels, bits: info.bits, float: info.float, frames: Some(info.frames), wav: true });
    }
    let decoded = Decoded::open(path)?;
    Ok(Probe {
        sample_rate: decoded.rate,
        channels: decoded.channels as u16,
        bits: decoded.bits,
        float: false,
        frames: decoded.format.tracks().iter().find(|t| t.id == decoded.track).and_then(|t| t.codec_params.n_frames),
        wav: false,
    })
}

fn open_frames<'a>(path: &'a Path, wav: bool) -> Result<Box<dyn Frames + 'a>> {
    if wav {
        Ok(Box::new(WavFile::open(path)?))
    } else {
        Ok(Box::new(DecodedFile { path, inner: Decoded::open(path)? }))
    }
}

/// Streams `input` through a resampler to `rate`, handing interleaved frames to `sink`. The
/// output is trimmed of the resampler's delay and is `ceil(frames * rate / input rate)` long.
fn resample(input: &mut dyn Frames, rate: u32, sink: &mut dyn FnMut(&[f32]) -> Result<()>) -> Result<()> {
    let channels = input.channels();
    let in_rate = input.sample_rate();
    let failed = |e: &dyn std::fmt::Display| Error::codec("resample_failed", format!("Could not resample from {in_rate} Hz to {rate} Hz: {e}"));
    let mut resampler = Fft::<f32>::new(in_rate as usize, rate as usize, 1024, channels, FixedSync::Input).map_err(|e| failed(&e))?;
    let chunk = resampler.input_frames_next();
    let mut out = vec![0f32; resampler.output_frames_max() * channels];
    let mut skip = resampler.output_delay();
    let mut pending: Vec<f32> = Vec::new();
    let mut read: Vec<f32> = Vec::new();
    let mut frames_in = 0u64;
    let mut frames_out = 0u64;
    let mut ended = false;
    // `limit` is known once the input has ended; until then everything produced is wanted.
    let mut emit = |out: &[f32], produced: usize, skip: &mut usize, frames_out: &mut u64, limit: Option<u64>| -> Result<()> {
        let drop = (*skip).min(produced);
        *skip -= drop;
        let mut keep = produced - drop;
        if let Some(limit) = limit {
            keep = keep.min((limit - *frames_out) as usize);
        }
        *frames_out += keep as u64;
        sink(&out[drop * channels..(drop + keep) * channels])
    };
    while !ended {
        while pending.len() < chunk * channels {
            input.next(&mut read)?;
            if read.is_empty() {
                ended = true;
                break;
            }
            pending.extend_from_slice(&read);
        }
        let mut at = 0;
        while pending.len() - at >= chunk * channels {
            let buf_in = InterleavedSlice::new(&pending[at..at + chunk * channels], channels, chunk).map_err(|e| failed(&e))?;
            let frames = out.len() / channels;
            let mut buf_out = InterleavedSlice::new_mut(&mut out, channels, frames).map_err(|e| failed(&e))?;
            let (used, produced) = resampler.process_into_buffer(&buf_in, &mut buf_out, None).map_err(|e| failed(&e))?;
            at += used * channels;
            frames_in += used as u64;
            emit(&out, produced, &mut skip, &mut frames_out, None)?;
        }
        pending.drain(..at);
    }
    let left = pending.len() / channels;
    frames_in += left as u64;
    let limit = (frames_in * rate as u64).div_ceil(in_rate as u64);
    // The last, short chunk, then silence until the resampler has given back everything it holds.
    pending.resize(chunk * channels, 0.0);
    let mut partial = Some(left);
    while frames_out < limit {
        let indexing = Indexing::new().partial_len(partial.take().unwrap_or(0));
        let buf_in = InterleavedSlice::new(&pending, channels, chunk).map_err(|e| failed(&e))?;
        let frames = out.len() / channels;
        let mut buf_out = InterleavedSlice::new_mut(&mut out, channels, frames).map_err(|e| failed(&e))?;
        let (_, produced) = resampler.process_into_buffer(&buf_in, &mut buf_out, Some(&indexing)).map_err(|e| failed(&e))?;
        emit(&out, produced, &mut skip, &mut frames_out, Some(limit))?;
    }
    Ok(())
}

/// Decodes `file` and writes it to `out` as a 32-bit float WAV at `rate`. Returns the frames
/// written.
fn conform(file: &Path, probe: &Probe, rate: u32, out: &Path) -> Result<u64> {
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).map_err(|e| io_at(dir, e))?;
    }
    let mut frames = open_frames(file, probe.wav)?;
    let channels = frames.channels();
    let spec = hound::WavSpec { channels: channels as u16, sample_rate: rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let tmp = out.with_extension("wav.part");
    let mut writer = hound::WavWriter::create(&tmp, spec)?;
    let mut written = 0u64;
    let mut write = |samples: &[f32]| -> Result<()> {
        for &s in samples {
            writer.write_sample(s)?;
        }
        written += (samples.len() / channels) as u64;
        Ok(())
    };
    if frames.sample_rate() == rate {
        let mut buf = Vec::new();
        loop {
            frames.next(&mut buf)?;
            if buf.is_empty() {
                break;
            }
            write(&buf)?;
        }
    } else {
        resample(frames.as_mut(), rate, &mut write)?;
    }
    writer.finalize()?;
    fs::rename(&tmp, out).map_err(|e| io_at(out, e))?;
    Ok(written)
}

/// Streams `audio` once and writes its peak pyramid to `to`.
fn write_peaks(audio: &Path, to: &Path) -> Result<Peaks> {
    let mut wav = WavFile::open(audio)?;
    let info = wav.info().clone();
    let mut builder = PeakBuilder::new(info.channels as usize, info.frames);
    let mut buf = Vec::new();
    while wav.read(CHUNK_FRAMES, &mut buf)? > 0 {
        builder.push(&buf);
    }
    let peaks = builder.finish();
    if let Some(dir) = to.parent() {
        fs::create_dir_all(dir).map_err(|e| io_at(dir, e))?;
    }
    peaks.save(to).map_err(|e| io_at(to, e))?;
    Ok(peaks)
}

/// Makes `file` a source of a project at `rate` (or at the file's own rate when the project has
/// none yet): hashes it, converts it if it cannot be read in place, and builds its peak cache.
pub fn import(paths: &Paths, id: &str, file: &Path, rate: Option<u32>) -> Result<Source> {
    let probe = probe(file)?;
    let rate = rate.unwrap_or(probe.sample_rate);
    let meta = fs::metadata(file).map_err(|e| io_at(file, e))?;
    let mut source = Source {
        id: id.to_string(),
        path: paths.stored_path(file),
        frames: 0,
        bytes: meta.len(),
        mtime: file_mtime(&meta),
        hash: String::new(),
        sample_rate: probe.sample_rate,
        channels: probe.channels,
        bits: probe.bits,
        float: probe.float,
        conformed: !probe.wav || probe.sample_rate != rate,
    };
    let audio = paths.audio_file(&source);
    let peaks = paths.peaks_file(&source);
    // The hash and the decode both read the whole file; run them side by side.
    let (hash, frames) = std::thread::scope(|scope| {
        let hash = scope.spawn(|| hash_file(file));
        let frames = (|| -> Result<u64> {
            if source.conformed {
                conform(file, &probe, rate, &audio)?;
            }
            Ok(write_peaks(&audio, &peaks)?.frames())
        })();
        (hash.join().expect("the hashing thread does not panic"), frames)
    });
    source.hash = hash?;
    source.frames = frames?;
    Ok(source)
}

/// What [`import`] would make of `file`, without hashing, converting or writing anything.
pub fn import_preview(paths: &Paths, id: &str, file: &Path, rate: Option<u32>) -> Result<Source> {
    let probe = probe(file)?;
    let rate = rate.unwrap_or(probe.sample_rate);
    let meta = fs::metadata(file).map_err(|e| io_at(file, e))?;
    let own = probe.frames.unwrap_or(0);
    Ok(Source {
        id: id.to_string(),
        path: paths.stored_path(file),
        frames: (own * rate as u64).div_ceil(probe.sample_rate as u64),
        bytes: meta.len(),
        mtime: file_mtime(&meta),
        hash: String::new(),
        sample_rate: probe.sample_rate,
        channels: probe.channels,
        bits: probe.bits,
        float: probe.float,
        conformed: !probe.wav || probe.sample_rate != rate,
    })
}

/// Makes sure the file a source's audio is read from exists, rebuilding a missing converted copy
/// from the original.
pub fn ensure_audio(paths: &Paths, source: &Source, rate: u32) -> Result<()> {
    if !source.conformed {
        return Ok(());
    }
    let audio = paths.audio_file(source);
    if let Ok(wav) = WavFile::open(&audio)
        && wav.info().frames == source.frames
        && wav.info().sample_rate == rate
    {
        return Ok(());
    }
    let file = paths.source_file(source);
    let written = conform(&file, &probe(&file)?, rate, &audio)?;
    if written != source.frames {
        return Err(Error::codec(
            "decode_failed",
            format!("Rebuilding the converted copy of {} gave {written} frames, and the project recorded {}.", file.display(), source.frames),
        ));
    }
    Ok(())
}

/// A source's peak pyramid, from the cache folder, rebuilt when missing or stale.
pub fn peaks(paths: &Paths, source: &Source, rate: u32) -> Result<Peaks> {
    let cached = paths.peaks_file(source);
    if let Ok(peaks) = Peaks::load(&cached)
        && peaks.frames() == source.frames
    {
        return Ok(peaks);
    }
    ensure_audio(paths, source, rate)?;
    write_peaks(&paths.audio_file(source), &cached)
}

/// Removes a discarded source's files from the cache folder.
pub fn forget(paths: &Paths, id: &str) {
    let _ = fs::remove_file(paths.cache.join(format!("{id}.wav")));
    let _ = fs::remove_file(paths.cache.join(format!("{id}.peaks")));
}
