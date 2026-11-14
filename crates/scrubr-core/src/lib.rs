//! scrubr's engine: the project file and its edit log, the timeline computed from it, streaming
//! decode and render, analysis, waveform images and export. No UI dependencies.
//!
//! Audio always streams. Sources are read in blocks and never held whole in memory.

pub mod analysis;
pub mod edit;
pub mod error;
pub mod export;
pub mod image;
pub mod peaks;
pub mod phrase;
pub mod project;
pub mod render;
pub mod source;
pub mod timeline;
pub mod transcribe;
pub mod wav;
pub mod words;

use std::path::Path;

pub use error::{Error, Kind, Result};
pub use peaks::{MinMax, PeakBuilder, Peaks};
pub use project::{Paths, Project};
pub use timeline::Timeline;
pub use wav::{WavFile, WavInfo};

/// Frames decoded per read while building peaks. Audio streams through this buffer; the file is
/// never held in memory.
const BUILD_CHUNK_FRAMES: usize = 1 << 16;

/// Streams `path` once and builds its peak pyramid. `progress` hears 0.0..=1.0.
pub fn build_peaks(path: impl AsRef<Path>, mut progress: impl FnMut(f32)) -> Result<(WavInfo, Peaks)> {
    let mut wav = WavFile::open(path)?;
    let info = wav.info().clone();
    let mut builder = PeakBuilder::new(info.channels as usize, info.frames);
    let mut buf = Vec::with_capacity(BUILD_CHUNK_FRAMES * info.channels as usize);
    let mut done = 0u64;
    loop {
        let n = wav.read(BUILD_CHUNK_FRAMES, &mut buf)?;
        if n == 0 {
            break;
        }
        builder.push(&buf);
        done += n as u64;
        progress(done as f32 / info.frames.max(1) as f32);
    }
    Ok((info, builder.finish()))
}
