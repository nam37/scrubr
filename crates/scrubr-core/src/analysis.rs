//! Measurements of the rendered timeline: how an agent hears.

use ebur128::{EbuR128, Mode};

use crate::error::{Error, Result};
use crate::render::{Renderer, walk};

/// A sample at or above this magnitude counts as clipped. It is just under full scale for
/// 16-bit audio, the coarsest format in common use.
pub const CLIP_LEVEL: f32 = 0.9999;

/// The most clipped ranges a report lists. The count is always complete.
pub const MAX_CLIP_RANGES: usize = 200;

/// Level in dBFS for a linear magnitude. Silence is reported as this floor, since JSON has no
/// minus infinity.
pub const FLOOR_DB: f64 = -120.0;

pub fn to_db(linear: f64) -> f64 {
    if linear <= 0.0 { FLOOR_DB } else { (20.0 * linear.log10()).max(FLOOR_DB) }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub loudness: bool,
    pub peaks: bool,
    /// Threshold in dBFS and the shortest run, in frames, that counts.
    pub silence: Option<(f64, u64)>,
    /// Window length in frames.
    pub envelope: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loudness {
    /// Integrated loudness in LUFS, or `None` when the audio is too quiet or short to measure.
    pub integrated_lufs: Option<f64>,
    pub range_lu: f64,
    pub true_peak_dbtp: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PeakReport {
    /// Sample peak per channel, in dBFS.
    pub peak_db: Vec<f64>,
    pub clipped_samples: u64,
    pub clipped_range_count: usize,
    /// Timeline ranges, in frames, that hold clipped samples. Ranges closer than 10 ms are one.
    pub clipped_ranges: Vec<(u64, u64)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub loudness: Option<Loudness>,
    pub peaks: Option<PeakReport>,
    /// Timeline ranges, in frames, where every channel stays under the threshold.
    pub silence: Option<Vec<(u64, u64)>>,
    /// RMS level in dBFS of each window, over all channels.
    pub envelope: Option<Vec<f64>>,
}

/// The length of quiet [`quietest`] looks for, in seconds.
pub const QUIET_SECONDS: f64 = 0.1;

/// How many steps the quiet window moves through in its own length.
const QUIET_STEPS: usize = 10;

/// The quietest stretch of a range, in timeline frames: where a cut is least likely to be heard.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quiet {
    pub start: u64,
    pub end: u64,
    /// RMS level in dBFS of the quietest [`QUIET_SECONDS`] in the range, over all channels.
    pub rms_db: f64,
}

impl Quiet {
    /// The middle of the stretch: the place to cut.
    pub fn at(&self) -> u64 {
        self.start + (self.end - self.start) / 2
    }
}

/// Finds the quietest [`QUIET_SECONDS`] of timeline range `t0..t1`, then widens it over the
/// audio around it that is no more than 3 dB louder, so the stretch covers a whole pause and its
/// middle leaves room on both sides. No threshold is involved, so it works the same on a quiet
/// recording and a loud one. A range shorter than the window comes back whole.
pub fn quietest(renderer: &mut Renderer, t0: u64, t1: u64, sample_rate: u32) -> Result<Quiet> {
    let ch = renderer.channels();
    let t1 = t1.min(renderer.total());
    let hop = ((sample_rate as f64 * QUIET_SECONDS).round() as u64 / QUIET_STEPS as u64).max(1);
    let mut hops: Vec<f64> = Vec::new();
    let mut energy = 0f64;
    let mut in_hop = 0u64;
    let mut all = 0f64;
    walk(renderer, t0, t1, |_, block| {
        for frame in block.chunks_exact(ch) {
            energy += frame.iter().map(|&s| s as f64 * s as f64).sum::<f64>();
            in_hop += 1;
            if in_hop == hop {
                hops.push(energy);
                all += energy;
                energy = 0.0;
                in_hop = 0;
            }
        }
        Ok(())
    })?;
    let level = |energy: f64, frames: u64| to_db((energy / (frames * ch as u64).max(1) as f64).sqrt());
    if hops.len() <= QUIET_STEPS {
        return Ok(Quiet { start: t0, end: t1, rms_db: level(all + energy, t1.saturating_sub(t0)) });
    }
    // The energy of each window of QUIET_STEPS hops, one hop apart.
    let mut sums = Vec::with_capacity(hops.len() - QUIET_STEPS + 1);
    let mut sum: f64 = hops[..QUIET_STEPS].iter().sum();
    sums.push(sum);
    for i in QUIET_STEPS..hops.len() {
        // Recomputed rather than updated in place, so rounding cannot leave silence above zero.
        sum = hops[i + 1 - QUIET_STEPS..=i].iter().sum();
        sums.push(sum);
    }
    let best = (0..sums.len()).min_by(|&a, &b| sums[a].total_cmp(&sums[b])).unwrap_or(0);
    let window = hop * QUIET_STEPS as u64;
    // Twice the energy is 3 dB. The small constant lets digital silence match itself.
    let limit = sums[best] * 2.0 + 1e-12 * (window * ch as u64) as f64;
    let mut first = best;
    while first > 0 && sums[first - 1] <= limit {
        first -= 1;
    }
    let mut last = best;
    while last + 1 < sums.len() && sums[last + 1] <= limit {
        last += 1;
    }
    Ok(Quiet { start: t0 + first as u64 * hop, end: t0 + last as u64 * hop + window, rms_db: level(sums[best], window) })
}

fn meter_failed(e: ebur128::Error) -> Error {
    Error::codec("analysis_failed", format!("The loudness meter failed: {e}"))
}

/// Measures timeline range `t0..t1` in one streaming pass.
pub fn analyze(renderer: &mut Renderer, t0: u64, t1: u64, options: &Options, sample_rate: u32) -> Result<Report> {
    let ch = renderer.channels();
    let mut meter = match options.loudness {
        true => Some(EbuR128::new(ch as u32, sample_rate, Mode::I | Mode::LRA | Mode::TRUE_PEAK).map_err(meter_failed)?),
        false => None,
    };
    let mut peak = vec![0f32; ch];
    let mut clipped_samples = 0u64;
    let mut clipped: Vec<(u64, u64)> = Vec::new();
    let mut clipped_count = 0usize;
    let mut clip_run: Option<(u64, u64)> = None;
    let clip_gap = (sample_rate / 100) as u64;

    let threshold = options.silence.map(|(db, _)| 10f64.powf(db / 20.0) as f32);
    let min_silence = options.silence.map_or(0, |(_, frames)| frames.max(1));
    let mut silence: Vec<(u64, u64)> = Vec::new();
    let mut quiet_since: Option<u64> = None;

    let window = options.envelope.unwrap_or(0);
    let mut envelope: Vec<f64> = Vec::new();
    let mut energy = 0f64;
    let mut in_window = 0u64;

    walk(renderer, t0, t1, |at, block| {
        if let Some(meter) = &mut meter {
            meter.add_frames_f32(block).map_err(meter_failed)?;
        }
        if !options.peaks && threshold.is_none() && window == 0 {
            return Ok(());
        }
        for (i, frame) in block.chunks_exact(ch).enumerate() {
            let t = at + i as u64;
            let mut loudest = 0f32;
            for (c, &s) in frame.iter().enumerate() {
                let mag = s.abs();
                loudest = loudest.max(mag);
                if options.peaks {
                    peak[c] = peak[c].max(mag);
                    if mag >= CLIP_LEVEL {
                        clipped_samples += 1;
                    }
                }
                if window > 0 {
                    energy += s as f64 * s as f64;
                }
            }
            if options.peaks && loudest >= CLIP_LEVEL {
                match &mut clip_run {
                    Some(run) if t <= run.1 + clip_gap => run.1 = t + 1,
                    run => {
                        if let Some(done) = run.replace((t, t + 1)) {
                            clipped_count += 1;
                            if clipped.len() < MAX_CLIP_RANGES {
                                clipped.push(done);
                            }
                        }
                    }
                }
            }
            if let Some(threshold) = threshold {
                if loudest < threshold {
                    quiet_since.get_or_insert(t);
                } else if let Some(since) = quiet_since.take()
                    && t - since >= min_silence
                {
                    silence.push((since, t));
                }
            }
            if window > 0 {
                in_window += 1;
                if in_window == window {
                    envelope.push(to_db((energy / (window * ch as u64) as f64).sqrt()));
                    energy = 0.0;
                    in_window = 0;
                }
            }
        }
        Ok(())
    })?;

    let end = t1.min(renderer.total());
    if let Some(since) = quiet_since
        && end - since >= min_silence
    {
        silence.push((since, end));
    }
    if let Some(done) = clip_run {
        clipped_count += 1;
        if clipped.len() < MAX_CLIP_RANGES {
            clipped.push(done);
        }
    }
    if window > 0 && in_window > 0 {
        envelope.push(to_db((energy / (in_window * ch as u64) as f64).sqrt()));
    }

    let loudness = match meter {
        Some(meter) => {
            let integrated = meter.loudness_global().map_err(meter_failed)?;
            let mut true_peak = 0f64;
            for c in 0..ch as u32 {
                true_peak = true_peak.max(meter.true_peak(c).map_err(meter_failed)?);
            }
            Some(Loudness {
                integrated_lufs: integrated.is_finite().then_some(integrated),
                range_lu: meter.loudness_range().map_err(meter_failed)?,
                true_peak_dbtp: to_db(true_peak),
            })
        }
        None => None,
    };
    Ok(Report {
        loudness,
        peaks: options.peaks.then(|| PeakReport {
            peak_db: peak.iter().map(|&p| to_db(p as f64)).collect(),
            clipped_samples,
            clipped_range_count: clipped_count,
            clipped_ranges: clipped,
        }),
        silence: threshold.map(|_| silence),
        envelope: (window > 0).then_some(envelope),
    })
}
