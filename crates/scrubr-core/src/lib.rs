//! scrubr's engine. M0 holds only what the Rune spike needs: streaming WAV reads and the
//! min/max peak pyramid the waveform is drawn from.

pub mod peaks;
pub mod wav;

use std::fmt;
use std::path::Path;

pub use peaks::{MinMax, PeakBuilder, Peaks};
pub use wav::{WavFile, WavInfo};

#[derive(Debug)]
pub enum Error {
    Wav(hound::Error),
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Wav(e) => write!(f, "could not read the WAV file: {e}"),
            Error::Unsupported(what) => write!(f, "unsupported audio: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<hound::Error> for Error {
    fn from(e: hound::Error) -> Self {
        Error::Wav(e)
    }
}

/// Frames decoded per read while building peaks. Audio streams through this buffer; the file is
/// never held in memory.
const BUILD_CHUNK_FRAMES: usize = 1 << 16;

/// Streams `path` once and builds its peak pyramid. `progress` hears 0.0..=1.0.
pub fn build_peaks(path: impl AsRef<Path>, mut progress: impl FnMut(f32)) -> Result<(WavInfo, Peaks), Error> {
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
