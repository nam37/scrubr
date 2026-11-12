//! The min/max peak pyramid a waveform is drawn from.
//!
//! Level 0 holds one min/max pair per channel for every [`BASE`] frames. Each level above it
//! merges [`FACTOR`] buckets of the level below. Drawing a column of the waveform picks the
//! coarsest level that is still at least as fine as the column, so a column reads at most a
//! handful of buckets whatever the zoom. Views finer than level 0 read samples from the file.

/// Frames per bucket at level 0.
pub const BASE: u64 = 64;
/// Buckets merged per level.
pub const FACTOR: u64 = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MinMax {
    pub min: f32,
    pub max: f32,
}

struct Level {
    frames_per_bucket: u64,
    buckets: usize,
    /// `[bucket][channel][min, max]`, quantized to i16.
    data: Vec<i16>,
}

pub struct Peaks {
    channels: usize,
    frames: u64,
    levels: Vec<Level>,
}

const Q: f32 = 32767.0;

fn quantize_min(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * Q).floor() as i16
}

fn quantize_max(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * Q).ceil() as i16
}

impl Peaks {
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Bytes held by every level.
    pub fn bytes(&self) -> usize {
        self.levels.iter().map(|l| l.data.len() * 2).sum()
    }

    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// Fills `out` with one min/max per column for `channel`, where column `i` covers frames
    /// `start + i * frames_per_col .. start + (i + 1) * frames_per_col`. Columns outside the
    /// file are silent. Returns false, leaving `out` empty, when the view is finer than level 0
    /// and the caller should read samples instead.
    pub fn columns(&self, channel: usize, start: f64, frames_per_col: f64, cols: usize, out: &mut Vec<MinMax>) -> bool {
        out.clear();
        if frames_per_col < BASE as f64 || channel >= self.channels {
            return false;
        }
        let level = self
            .levels
            .iter()
            .rev()
            .find(|l| l.frames_per_bucket as f64 <= frames_per_col)
            .unwrap_or(&self.levels[0]);
        let fpb = level.frames_per_bucket as f64;
        let stride = self.channels * 2;
        out.reserve(cols);
        for i in 0..cols {
            let a = start + i as f64 * frames_per_col;
            let b = a + frames_per_col;
            let lo = (a / fpb).floor().max(0.0) as usize;
            let hi = ((b / fpb).ceil().max(0.0) as usize).min(level.buckets);
            if lo >= hi {
                out.push(MinMax::default());
                continue;
            }
            let mut min = i16::MAX;
            let mut max = i16::MIN;
            for bucket in lo..hi {
                let at = bucket * stride + channel * 2;
                min = min.min(level.data[at]);
                max = max.max(level.data[at + 1]);
            }
            out.push(MinMax { min: min as f32 / Q, max: max as f32 / Q });
        }
        true
    }
}

/// Builds a [`Peaks`] from interleaved samples pushed in any chunk sizes.
pub struct PeakBuilder {
    channels: usize,
    frames: u64,
    in_bucket: u64,
    cur: Vec<MinMax>,
    level0: Vec<i16>,
}

const EMPTY: MinMax = MinMax { min: f32::INFINITY, max: f32::NEG_INFINITY };

impl PeakBuilder {
    pub fn new(channels: usize, expected_frames: u64) -> Self {
        let channels = channels.max(1);
        let buckets = expected_frames.div_ceil(BASE) as usize;
        PeakBuilder {
            channels,
            frames: 0,
            in_bucket: 0,
            cur: vec![EMPTY; channels],
            level0: Vec::with_capacity(buckets * channels * 2),
        }
    }

    /// Adds whole frames of interleaved samples.
    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(self.channels) {
            for (c, &s) in frame.iter().enumerate() {
                let m = &mut self.cur[c];
                m.min = m.min.min(s);
                m.max = m.max.max(s);
            }
            self.in_bucket += 1;
            if self.in_bucket == BASE {
                self.flush();
            }
        }
        self.frames += (interleaved.len() / self.channels) as u64;
    }

    fn flush(&mut self) {
        for m in &mut self.cur {
            self.level0.push(quantize_min(m.min));
            self.level0.push(quantize_max(m.max));
            *m = EMPTY;
        }
        self.in_bucket = 0;
    }

    pub fn finish(mut self) -> Peaks {
        if self.in_bucket > 0 {
            self.flush();
        }
        let stride = self.channels * 2;
        let mut levels = vec![Level { frames_per_bucket: BASE, buckets: self.level0.len() / stride, data: self.level0 }];
        while levels.last().unwrap().buckets > 1 {
            let below = levels.last().unwrap();
            let buckets = below.buckets.div_ceil(FACTOR as usize);
            let mut data = Vec::with_capacity(buckets * stride);
            for bucket in 0..buckets {
                let from = bucket * FACTOR as usize;
                let to = (from + FACTOR as usize).min(below.buckets);
                for c in 0..self.channels {
                    let mut min = i16::MAX;
                    let mut max = i16::MIN;
                    for b in from..to {
                        let at = b * stride + c * 2;
                        min = min.min(below.data[at]);
                        max = max.max(below.data[at + 1]);
                    }
                    data.push(min);
                    data.push(max);
                }
            }
            levels.push(Level { frames_per_bucket: below.frames_per_bucket * FACTOR, buckets, data });
        }
        Peaks { channels: self.channels, frames: self.frames, levels }
    }
}

/// One min/max per column straight from samples, for views finer than level 0. `samples` is
/// interleaved and its first frame is frame `first` of the file; columns are laid out as in
/// [`Peaks::columns`]. A column with no sample in it repeats the nearest sample, so a view
/// zoomed past one frame per column still draws a continuous line.
pub fn columns_from_samples(
    samples: &[f32],
    channels: usize,
    channel: usize,
    first: u64,
    start: f64,
    frames_per_col: f64,
    cols: usize,
    out: &mut Vec<MinMax>,
) {
    out.clear();
    let channels = channels.max(1);
    let frames = samples.len() / channels;
    for i in 0..cols {
        let a = start + i as f64 * frames_per_col - first as f64;
        let b = a + frames_per_col;
        if b <= 0.0 || a >= frames as f64 || frames == 0 {
            out.push(MinMax::default());
            continue;
        }
        let lo = (a.floor().max(0.0) as usize).min(frames - 1);
        let hi = (b.ceil() as usize).clamp(lo + 1, frames);
        let mut m = EMPTY;
        for f in lo..hi {
            let s = samples[f * channels + channel];
            m.min = m.min.min(s);
            m.max = m.max.max(s);
        }
        out.push(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mono ramp whose value at frame `f` is `f / len`, so a range's min and max are its ends.
    fn ramp(len: usize) -> Peaks {
        let samples: Vec<f32> = (0..len).map(|f| f as f32 / len as f32).collect();
        let mut b = PeakBuilder::new(1, len as u64);
        // Uneven chunks, to cross bucket boundaries mid-push.
        for chunk in samples.chunks(1000) {
            b.push(chunk);
        }
        b.finish()
    }

    #[test]
    fn levels_shrink_by_the_factor_down_to_one_bucket() {
        let p = ramp(100_000);
        assert_eq!(p.frames(), 100_000);
        assert_eq!(p.levels[0].buckets, 100_000usize.div_ceil(BASE as usize));
        for pair in p.levels.windows(2) {
            assert_eq!(pair[1].frames_per_bucket, pair[0].frames_per_bucket * FACTOR);
            assert_eq!(pair[1].buckets, pair[0].buckets.div_ceil(FACTOR as usize));
        }
        assert_eq!(p.levels.last().unwrap().buckets, 1);
    }

    #[test]
    fn columns_bracket_the_true_range_at_every_zoom() {
        let len = 100_000usize;
        let p = ramp(len);
        let mut out = Vec::new();
        for fpc in [64.0, 100.0, 256.0, 999.5, 4096.0, 50_000.0] {
            let cols = (len as f64 / fpc).floor() as usize;
            assert!(p.columns(0, 0.0, fpc, cols, &mut out));
            assert_eq!(out.len(), cols);
            for (i, m) in out.iter().enumerate() {
                let lo = (i as f64 * fpc) as f32 / len as f32;
                let hi = ((i + 1) as f64 * fpc - 1.0) as f32 / len as f32;
                // Never narrower than the truth, and within one coarse bucket of it on each side.
                let slack = 2.0 * fpc as f32 / len as f32 + 1e-3;
                assert!(m.min <= lo + 1e-3 && m.min >= lo - slack, "fpc {fpc} col {i}: min {} vs {lo}", m.min);
                assert!(m.max >= hi - 1e-3 && m.max <= hi + slack, "fpc {fpc} col {i}: max {} vs {hi}", m.max);
            }
        }
    }

    #[test]
    fn views_finer_than_level_zero_ask_for_samples() {
        let p = ramp(10_000);
        let mut out = vec![MinMax::default()];
        assert!(!p.columns(0, 0.0, 8.0, 100, &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn columns_outside_the_file_are_silent() {
        let p = ramp(10_000);
        let mut out = Vec::new();
        assert!(p.columns(0, -640.0, 64.0, 20, &mut out));
        assert_eq!(out[0], MinMax::default());
        assert_ne!(out[15], MinMax::default());
        assert!(p.columns(0, 9_000.0, 64.0, 40, &mut out));
        assert_eq!(*out.last().unwrap(), MinMax::default());
    }

    #[test]
    fn sample_columns_cover_each_frame_and_stretch_past_one_per_column() {
        let samples: Vec<f32> = (0..16).map(|f| f as f32 / 16.0).collect();
        let mut out = Vec::new();
        columns_from_samples(&samples, 1, 0, 100, 100.0, 4.0, 4, &mut out);
        assert_eq!(out[0], MinMax { min: 0.0, max: 3.0 / 16.0 });
        assert_eq!(out[3], MinMax { min: 12.0 / 16.0, max: 15.0 / 16.0 });
        // Four columns per frame: each frame's value repeats across its columns.
        columns_from_samples(&samples, 1, 0, 100, 100.0, 0.25, 8, &mut out);
        assert_eq!(out[0], out[3]);
        assert_eq!(out[4], MinMax { min: 1.0 / 16.0, max: 1.0 / 16.0 });
    }
}
