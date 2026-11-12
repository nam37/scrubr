//! Rendering the timeline to a file: WAV through hound, FLAC through flacenc.
//!
//! Both encoders are fed block by block from the streaming renderer and write block by block,
//! single-threaded, so the same project version always gives the same bytes on one platform.

use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use flacenc::bitsink::MemSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::error::Verify;
use flacenc::source::{Context, Fill, FrameBuf};

use crate::error::{Error, Result, io_at};
use crate::project::{Paths, Project, verify_sources};
use crate::render::{Renderer, walk};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Wav,
    Flac,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Wav => "wav",
            Format::Flac => "flac",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    Int(u16),
    /// 32-bit float, WAV only.
    Float,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Sample format. The default follows the project's first source, capped at 24 bits for FLAC.
    pub depth: Option<Depth>,
    /// Timeline range in frames. The default is the whole timeline.
    pub range: Option<(u64, u64)>,
    /// Set to the number of frames written as the export goes, for a caller on another thread
    /// to show progress.
    pub progress: Option<Arc<AtomicU64>>,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub path: PathBuf,
    pub format: Format,
    pub depth: Depth,
    pub frames: u64,
    pub bytes: u64,
    /// Samples that were over full scale and had to be clamped. Zero for float output.
    pub clipped_samples: u64,
}

const FLAC_BLOCK: usize = 4096;

fn encode_failed(path: &Path, e: impl std::fmt::Debug) -> Error {
    Error::codec("encode_failed", format!("Could not encode {}: {e:?}", path.display()))
}

/// The output format, from the file name.
pub fn format_for(path: &Path) -> Result<Format> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "wav" => Ok(Format::Wav),
        "flac" => Ok(Format::Flac),
        "mp3" => Err(Error::codec("format_not_supported", "MP3 export is not in this build; it is planned for milestone M4.")
            .hint("Export to .flac or .wav instead.")),
        _ => Err(Error::codec("format_not_supported", format!("scrubr cannot tell the export format from \"{}\".", path.display()))
            .hint("Name the output with a .wav or .flac extension.")),
    }
}

/// Converts interleaved floats to integers of `bits`, rounding to nearest and clamping at full
/// scale. Returns how many samples had to be clamped.
fn quantize(samples: &[f32], bits: u16, out: &mut Vec<i32>) -> u64 {
    let scale = (1i64 << (bits - 1)) as f64;
    let (min, max) = (-scale, scale - 1.0);
    let mut clipped = 0;
    out.clear();
    out.extend(samples.iter().map(|&s| {
        let v = (s as f64 * scale).round();
        if v > max || v < min {
            clipped += 1;
        }
        v.clamp(min, max) as i32
    }));
    clipped
}

/// Renders the timeline at head to `out`. Every source is checked by full hash first. The file
/// appears under its final name only once it is complete.
pub fn export(project: &Project, paths: &Paths, out: &Path, options: &Options) -> Result<Report> {
    let format = format_for(out)?;
    verify_sources(project, paths, true)?;
    let out_abs = std::path::absolute(out).unwrap_or_else(|_| out.to_path_buf());
    for source in &project.sources {
        let file = paths.source_file(source);
        if std::path::absolute(&file).unwrap_or(file) == out_abs {
            return Err(Error::usage("would_overwrite_source", format!("{} is a source of this project. Sources are never written.", out.display()))
                .hint("Export to a different file name."));
        }
    }
    let first = project.sources.first();
    let depth = options.depth.unwrap_or_else(|| match first {
        Some(s) if s.float && format == Format::Wav => Depth::Float,
        Some(s) if s.float => Depth::Int(24),
        Some(s) => Depth::Int(s.bits.clamp(16, if format == Format::Flac { 24 } else { 32 })),
        None => Depth::Int(24),
    });
    match (format, depth) {
        (Format::Flac, Depth::Float) | (Format::Flac, Depth::Int(32)) => {
            return Err(Error::usage("bad_depth", "FLAC export supports 16 and 24 bits.").hint("Use --bits 16 or --bits 24, or export to .wav."));
        }
        (_, Depth::Int(bits)) if ![16, 24, 32].contains(&bits) => {
            return Err(Error::usage("bad_depth", format!("{bits}-bit output is not supported.")).hint("Use --bits 16, 24, 32 or 32f."));
        }
        _ => {}
    }

    let mut renderer = Renderer::new(project, paths)?;
    let total = renderer.total();
    let (t0, t1) = options.range.unwrap_or((0, total));
    if t1 > total || t0 >= t1 {
        if total == 0 {
            return Err(Error::usage("empty_timeline", "The timeline is empty, so there is nothing to export.")
                .hint(format!("Run `scrubr history {}` and undo the edit that removed everything.", paths.shown.display())));
        }
        crate::edit::check_range(project, paths, renderer.timeline(), (t0, t1))?;
    }

    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| io_at(dir, e))?;
    }
    let mut part = out.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let written = match format {
        Format::Wav => write_wav(project, &mut renderer, &part, depth, t0, t1, options.progress.as_deref()),
        Format::Flac => write_flac(project, &mut renderer, &part, depth, t0, t1, options.progress.as_deref()),
    };
    let clipped_samples = match written {
        Ok(clipped) => clipped,
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Err(e);
        }
    };
    fs::rename(&part, out).map_err(|e| io_at(out, e))?;
    let bytes = fs::metadata(out).map_err(|e| io_at(out, e))?.len();
    Ok(Report { path: out.to_path_buf(), format, depth, frames: t1 - t0, bytes, clipped_samples })
}

/// [`walk`], telling `progress` how many frames are done after each block.
fn walk_counted(renderer: &mut Renderer, t0: u64, t1: u64, progress: Option<&AtomicU64>, mut each: impl FnMut(&[f32]) -> Result<()>) -> Result<()> {
    let channels = renderer.channels().max(1);
    walk(renderer, t0, t1, |at, block| {
        each(block)?;
        if let Some(progress) = progress {
            progress.store(at - t0 + (block.len() / channels) as u64, Relaxed);
        }
        Ok(())
    })
}

fn write_wav(project: &Project, renderer: &mut Renderer, path: &Path, depth: Depth, t0: u64, t1: u64, progress: Option<&AtomicU64>) -> Result<u64> {
    let spec = hound::WavSpec {
        channels: project.channels,
        sample_rate: project.sample_rate,
        bits_per_sample: match depth {
            Depth::Int(bits) => bits,
            Depth::Float => 32,
        },
        sample_format: if depth == Depth::Float { hound::SampleFormat::Float } else { hound::SampleFormat::Int },
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    let mut ints = Vec::new();
    let mut clipped = 0;
    walk_counted(renderer, t0, t1, progress, |block| {
        match depth {
            Depth::Float => {
                for &s in block {
                    writer.write_sample(s)?;
                }
            }
            Depth::Int(bits) => {
                clipped += quantize(block, bits, &mut ints);
                for &s in &ints {
                    writer.write_sample(s)?;
                }
            }
        }
        Ok(())
    })?;
    writer.finalize()?;
    Ok(clipped)
}

fn write_flac(project: &Project, renderer: &mut Renderer, path: &Path, depth: Depth, t0: u64, t1: u64, progress: Option<&AtomicU64>) -> Result<u64> {
    let Depth::Int(bits) = depth else { unreachable!("float FLAC is refused before this") };
    let channels = project.channels as usize;
    let failed = |e: &dyn std::fmt::Debug| encode_failed(path, e);
    let config = flacenc::config::Encoder::default().into_verified().map_err(|(_, e)| failed(&e))?;
    // flacenc 0.5 has no 5-bit Rice parameters, so on loud, noisy 24-bit audio its predicted
    // frames can come out far larger than the samples they hold. Such a frame is encoded again
    // with prediction off, which stores the samples as they are.
    let mut plain = flacenc::config::Encoder::default();
    plain.subframe_coding.use_fixed = false;
    plain.subframe_coding.use_lpc = false;
    plain.stereo_coding.use_leftside = false;
    plain.stereo_coding.use_rightside = false;
    plain.stereo_coding.use_midside = false;
    let plain = plain.into_verified().map_err(|(_, e)| failed(&e))?;
    let mut info = StreamInfo::new(project.sample_rate as usize, channels, bits as usize).map_err(|e| failed(&e))?;
    info.set_block_sizes(FLAC_BLOCK, FLAC_BLOCK).map_err(|e| failed(&e))?;

    let header = |info: &StreamInfo| -> Vec<u8> {
        let mut sink = MemSink::<u8>::new();
        Stream::with_stream_info(info.clone()).write(&mut sink).expect("writing to memory does not fail");
        sink.into_inner()
    };
    let mut file = BufWriter::with_capacity(1 << 20, File::create(path).map_err(|e| io_at(path, e))?);
    // The stream header holds the frame sizes, the length and the MD5 of the audio, which are
    // only known at the end. Write it now to reserve its place and again once they are known.
    let placeholder = header(&info);
    file.write_all(&placeholder).map_err(|e| io_at(path, e))?;

    let mut md5 = Context::new(bits as usize, channels);
    let mut frame_buf = FrameBuf::with_size(channels, FLAC_BLOCK).map_err(|e| failed(&e))?;
    let mut sink = MemSink::<u8>::new();
    let mut ints: Vec<i32> = Vec::new();
    let mut pending: Vec<i32> = Vec::new();
    let mut number = 0usize;
    let mut clipped = 0;
    let mut encode = |samples: &[i32], info: &mut StreamInfo, file: &mut BufWriter<File>| -> Result<()> {
        frame_buf.fill_interleaved(samples).map_err(|e| failed(&e))?;
        md5.fill_interleaved(samples).map_err(|e| failed(&e))?;
        let mut frame = flacenc::encode_fixed_size_frame(&config, &frame_buf, number, info).map_err(|e| failed(&e))?;
        if frame.count_bits() > samples.len() * bits as usize + 256 {
            frame = flacenc::encode_fixed_size_frame(&plain, &frame_buf, number, info).map_err(|e| failed(&e))?;
        }
        info.update_frame_info(&frame);
        sink.clear();
        frame.write(&mut sink).map_err(|e| failed(&e))?;
        file.write_all(sink.as_slice()).map_err(|e| io_at(path, e))?;
        number += 1;
        Ok(())
    };
    let block = FLAC_BLOCK * channels;
    walk_counted(renderer, t0, t1, progress, |samples| {
        clipped += quantize(samples, bits, &mut ints);
        pending.extend_from_slice(&ints);
        let mut at = 0;
        while pending.len() - at >= block {
            encode(&pending[at..at + block], &mut info, &mut file)?;
            at += block;
        }
        pending.drain(..at);
        Ok(())
    })?;
    if !pending.is_empty() {
        encode(&pending, &mut info, &mut file)?;
    }
    // Counting frames narrowed the minimum block size to the short last block. The format's
    // minimum leaves the last block out, and decoders read "minimum equals maximum" as the sign
    // of a fixed block size, which this stream has.
    info.set_block_sizes(FLAC_BLOCK, FLAC_BLOCK).map_err(|e| failed(&e))?;
    info.set_md5_digest(&md5.md5_digest());
    info.set_total_samples(md5.total_samples());
    let finished = header(&info);
    debug_assert_eq!(finished.len(), placeholder.len());
    file.seek(SeekFrom::Start(0)).map_err(|e| io_at(path, e))?;
    file.write_all(&finished).map_err(|e| io_at(path, e))?;
    file.flush().map_err(|e| io_at(path, e))?;
    Ok(clipped)
}
