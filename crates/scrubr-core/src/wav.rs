//! Streaming WAV reads through hound. Samples come out as interleaved `f32` in -1.0..=1.0.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use hound::{SampleFormat, WavReader};

use crate::Error;

#[derive(Clone, Debug)]
pub struct WavInfo {
    pub path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub float: bool,
    /// Length in frames (one sample per channel).
    pub frames: u64,
}

impl WavInfo {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }
}

pub struct WavFile {
    reader: WavReader<BufReader<File>>,
    info: WavInfo,
}

impl WavFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let file = File::open(path).map_err(hound::Error::IoError)?;
        let reader = WavReader::new(BufReader::with_capacity(1 << 20, file))?;
        let spec = reader.spec();
        if spec.channels == 0 {
            return Err(Error::Unsupported("a WAV file with no channels".into()));
        }
        let info = WavInfo {
            path: path.to_path_buf(),
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            bits: spec.bits_per_sample,
            float: spec.sample_format == SampleFormat::Float,
            frames: reader.duration() as u64,
        };
        Ok(WavFile { reader, info })
    }

    pub fn info(&self) -> &WavInfo {
        &self.info
    }

    /// Moves the read position to `frame`, clamped to the end of the file.
    pub fn seek(&mut self, frame: u64) -> Result<(), Error> {
        let frame = frame.min(self.info.frames);
        self.reader.seek(frame as u32).map_err(hound::Error::IoError)?;
        Ok(())
    }

    /// Reads up to `frames` frames into `out` (cleared first) as interleaved `f32`. Returns the
    /// number of whole frames read; 0 means the end of the file.
    pub fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> Result<usize, Error> {
        out.clear();
        let channels = self.info.channels as usize;
        let want = frames * channels;
        if self.info.float {
            for s in self.reader.samples::<f32>().take(want) {
                out.push(s?);
            }
        } else {
            let scale = 1.0 / (1u64 << (self.info.bits.max(1) - 1)) as f32;
            for s in self.reader.samples::<i32>().take(want) {
                out.push(s? as f32 * scale);
            }
        }
        // A truncated file can end mid-frame; drop the partial frame.
        out.truncate(out.len() - out.len() % channels);
        Ok(out.len() / channels)
    }
}
