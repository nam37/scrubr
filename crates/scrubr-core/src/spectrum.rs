//! Spectrograms of the timeline: how much sound there is at each pitch, moment by moment.
//!
//! A waveform shows how loud; this shows what kind of sound. A hum is a level line low down, hiss
//! a haze at the top, a click a thin upright line, speech a stack of wavering bands. Frequency
//! runs up the image on a logarithmic scale, so the octave from 50 to 100 Hz gets as much room
//! as the one from 5 to 10 kHz, and a mains hum is as easy to see as a whistle.
//!
//! The channels are mixed to one before measuring. Each column shows the most that sounded at
//! each pitch during the time it covers, so a click is not averaged away when the view is wide.

use std::path::Path;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex32;
use tiny_skia::Pixmap;

use crate::error::{Error, Result, io_at};
use crate::image::{JOIN, RULE, draw_text, fill, tick_label, tick_step};
use crate::project::{Paths, Project};
use crate::render::{Renderer, walk};

/// The height of the time ruler and the width of the frequency scale, in pixels.
pub const RULER: u32 = 24;
pub const GUTTER: u32 = 66;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub width: u32,
    pub height: u32,
    /// Timeline range in frames. The default is the whole timeline.
    pub range: Option<(u64, u64)>,
    /// This track alone, by its index in the timeline's tracks. The default is the mix.
    pub track: Option<usize>,
    /// The lowest and highest frequencies drawn. The default top is half the sample rate, or
    /// 20 kHz if that is lower.
    pub min_hz: f64,
    pub max_hz: Option<f64>,
    /// The levels drawn as black and as the brightest colour, in dBFS. A full-scale sine is 0.
    pub floor_db: f64,
    pub ceiling_db: f64,
}

impl Default for Options {
    fn default() -> Self {
        Options { width: 1600, height: 480, range: None, track: None, min_hz: 40.0, max_hz: None, floor_db: -100.0, ceiling_db: -10.0 }
    }
}

/// Levels by time and frequency.
#[derive(Clone, Debug)]
pub struct Grid {
    pub start: u64,
    pub end: u64,
    /// Timeline frames per column. Column `x` starts at frame `start + x * frames_per_column`.
    pub frames_per_column: f64,
    pub min_hz: f64,
    pub max_hz: f64,
    /// The frames each measurement looks at. Longer tells pitches apart better and moments worse.
    pub window: usize,
    /// For each column, the level of each row in dBFS. Row 0 is `max_hz`, the last is `min_hz`.
    pub db: Vec<Vec<f32>>,
}

impl Grid {
    pub fn rows(&self) -> usize {
        self.db.first().map_or(0, Vec::len)
    }

    /// The frequency at the middle of a row.
    pub fn hz_of_row(&self, row: f64) -> f64 {
        hz_of_row(self.min_hz, self.max_hz, self.rows(), row)
    }

    /// The row a frequency falls in.
    pub fn row_of_hz(&self, hz: f64) -> f64 {
        let rows = self.rows().max(2) as f64;
        (hz / self.max_hz).ln() / (self.min_hz / self.max_hz).ln() * (rows - 1.0)
    }
}

fn hz_of_row(min_hz: f64, max_hz: f64, rows: usize, row: f64) -> f64 {
    max_hz * (min_hz / max_hz).powf(row / (rows.max(2) as f64 - 1.0))
}

/// The window for a view, in frames. About 85 ms when there is room, which tells a 50 Hz hum
/// from a 60 Hz one; shorter close in, where a column is a millisecond or two and the moment
/// matters more than the pitch.
fn window_for(frames_per_column: f64, rate: f64) -> usize {
    let wide = ((rate * 0.085) as usize).next_power_of_two().max(256);
    let seconds = frames_per_column / rate;
    if seconds >= 0.008 {
        wide
    } else if seconds >= 0.002 {
        wide / 2
    } else {
        wide / 4
    }
}

/// Measures timeline range `range` into `columns` by `rows` levels.
#[allow(clippy::too_many_arguments)]
pub fn measure(project: &Project, paths: &Paths, track: Option<usize>, range: Option<(u64, u64)>, columns: usize, rows: usize, min_hz: f64, max_hz: Option<f64>) -> Result<Grid> {
    let mut renderer = Renderer::of(project, paths, track)?;
    let total = renderer.total();
    if total == 0 {
        return Err(Error::usage("empty_timeline", "The timeline is empty, so there is nothing to draw."));
    }
    let (start, end) = range.unwrap_or((0, total));
    crate::edit::check_range(project, paths, renderer.timeline(), (start, end))?;
    let rate = project.sample_rate as f64;
    let max_hz = max_hz.unwrap_or(20_000.0).min(rate / 2.0);
    if !(min_hz >= 1.0 && min_hz < max_hz) {
        return Err(Error::usage("bad_frequency", format!("Frequencies from {min_hz} to {max_hz} Hz cannot be drawn."))
            .hint(format!("Give a lowest frequency of 1 Hz or more and a highest above it, up to {} Hz, half the sample rate.", rate / 2.0)));
    }
    let frames_per_column = (end - start) as f64 / columns as f64;
    let window = window_for(frames_per_column, rate);
    let hop = (window / 2).min((frames_per_column as usize).max(1));
    let bin_hz = rate / window as f64;
    // Each row reads the strongest of the bins it spans, or lies between two when it is
    // narrower than a bin, as the low rows are.
    let spans: Vec<(usize, usize, f32)> = (0..rows)
        .map(|y| {
            let (lo, hi) = (hz_of_row(min_hz, max_hz, rows, y as f64 + 0.5) / bin_hz, hz_of_row(min_hz, max_hz, rows, y as f64 - 0.5) / bin_hz);
            let last = window / 2 - 1;
            if hi - lo >= 1.0 {
                ((lo.ceil() as usize).min(last), (hi.floor() as usize).min(last), -1.0)
            } else {
                let at = hz_of_row(min_hz, max_hz, rows, y as f64) / bin_hz;
                let a = (at.floor() as usize).min(last);
                (a, (a + 1).min(last), (at - at.floor()) as f32)
            }
        })
        .collect();
    let shape: Vec<f32> = (0..window).map(|n| 0.5 - 0.5 * (std::f32::consts::TAU * n as f32 / window as f32).cos()).collect();
    // So that a sine at full scale reads 0 dBFS.
    let scale = 2.0 / shape.iter().sum::<f32>();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(window);
    let mut bins = vec![Complex32::default(); window];
    let mut power = vec![0f32; window / 2];
    let mut grid = vec![vec![-1f32; rows]; columns];

    let channels = renderer.channels();
    let half = (window / 2) as u64;
    let (from, to) = (start.saturating_sub(half), (end + half).min(total));
    // The first window is centred on the start of the range: silence stands in for audio
    // before the timeline begins.
    let mut pending: Vec<f32> = vec![0.0; (half - (start - from)) as usize];
    let mut base = start as i64 - half as i64;
    let mut take = |pending: &mut Vec<f32>, base: &mut i64| {
        while pending.len() >= window {
            let centre = *base + half as i64;
            if centre >= start as i64 && centre < end as i64 {
                for (bin, (&s, &w)) in bins.iter_mut().zip(pending.iter().zip(&shape)) {
                    *bin = Complex32::new(s * w, 0.0);
                }
                fft.process(&mut bins);
                for (p, bin) in power.iter_mut().zip(&bins) {
                    *p = bin.norm_sqr() * scale * scale;
                }
                let x = (((centre - start as i64) as f64 / frames_per_column) as usize).min(columns - 1);
                for (cell, &(a, b, part)) in grid[x].iter_mut().zip(&spans) {
                    let level = if part >= 0.0 { power[a] + (power[b] - power[a]) * part } else { power[a..=b.max(a)].iter().copied().fold(0.0, f32::max) };
                    *cell = cell.max(level);
                }
            }
            pending.drain(..hop);
            *base += hop as i64;
        }
    };
    walk(&mut renderer, from, to, |_, block| {
        pending.extend(block.chunks_exact(channels).map(|frame| frame.iter().sum::<f32>() / channels as f32));
        take(&mut pending, &mut base);
        Ok(())
    })?;
    pending.extend(std::iter::repeat_n(0.0, window));
    take(&mut pending, &mut base);

    // A column no window was centred in shows what the one before it does.
    let mut db: Vec<Vec<f32>> = Vec::with_capacity(columns);
    for column in grid {
        if column.first().is_some_and(|&v| v < 0.0) {
            db.push(db.last().cloned().unwrap_or_else(|| vec![-160.0; rows]));
        } else {
            db.push(column.into_iter().map(|p| (10.0 * p.max(1e-16).log10()).max(-160.0)).collect());
        }
    }
    Ok(Grid { start, end, frames_per_column, min_hz, max_hz, window, db })
}

/// What [`render_png`] drew, for turning a pixel back into a time and a frequency.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub width: u32,
    pub height: u32,
    pub start: u64,
    pub end: u64,
    /// The plot is the image without the ruler above it and the scale to its left.
    pub plot_left: u32,
    pub plot_top: u32,
    pub plot_width: u32,
    pub plot_height: u32,
    /// Timeline frames per pixel column of the plot.
    pub frames_per_pixel: f64,
    /// The frequency at the bottom row of the plot and at the top row. Between them the scale
    /// is logarithmic: `top_hz * (bottom_hz / top_hz) ^ (row / (plot_height - 1))`.
    pub bottom_hz: f64,
    pub top_hz: f64,
    pub floor_db: f64,
    pub ceiling_db: f64,
    pub window: usize,
}

/// Black through purple, red and orange to pale yellow: dark is quiet, bright is loud.
fn colour(t: f32) -> [u8; 3] {
    const STOPS: [[f32; 3]; 9] = [
        [0.0, 0.0, 4.0],
        [31.0, 12.0, 72.0],
        [85.0, 15.0, 109.0],
        [136.0, 34.0, 106.0],
        [186.0, 54.0, 85.0],
        [227.0, 89.0, 51.0],
        [249.0, 140.0, 10.0],
        [249.0, 201.0, 50.0],
        [252.0, 255.0, 164.0],
    ];
    let at = t.clamp(0.0, 1.0) * (STOPS.len() - 1) as f32;
    let i = (at as usize).min(STOPS.len() - 2);
    let part = at - i as f32;
    std::array::from_fn(|c| (STOPS[i][c] + (STOPS[i + 1][c] - STOPS[i][c]) * part).round() as u8)
}

/// Draws a spectrogram of the timeline at head, or a range of it, to a PNG at `out`.
pub fn render_png(project: &Project, paths: &Paths, out: &Path, options: &Options) -> Result<Report> {
    let (width, height) = (options.width, options.height);
    if !(GUTTER + 64..=16_384).contains(&width) || !(RULER + 64..=8192).contains(&height) {
        return Err(Error::usage("bad_size", format!("A {width}x{height} image is out of range for a spectrogram."))
            .hint(format!("Use a width of {} to 16384 and a height of {} to 8192.", GUTTER + 64, RULER + 64)));
    }
    let (plot_w, plot_h) = (width - GUTTER, height - RULER);
    let grid = measure(project, paths, options.track, options.range, plot_w as usize, plot_h as usize, options.min_hz, options.max_hz)?;
    let mut pixmap = Pixmap::new(width, height).ok_or_else(|| Error::usage("bad_size", "The image size is not valid."))?;
    fill(&mut pixmap, 0.0, 0.0, width as f32, height as f32, [0xFF, 0xFF, 0xFF]);

    let span = (options.ceiling_db - options.floor_db).max(1.0) as f32;
    let data = pixmap.data_mut();
    for (x, column) in grid.db.iter().enumerate() {
        for (y, &db) in column.iter().enumerate() {
            let [r, g, b] = colour((db - options.floor_db as f32) / span);
            let at = ((y + RULER as usize) * width as usize + x + GUTTER as usize) * 4;
            data[at..at + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }

    // The time ruler, as on a waveform image.
    let rate = project.sample_rate as f64;
    let (start, end) = (grid.start, grid.end);
    let (step, decimals) = tick_step(grid.frames_per_column / rate, 110.0);
    for n in (start as f64 / rate / step).ceil() as u64..=(end as f64 / rate / step).floor() as u64 {
        let seconds = n as f64 * step;
        let x = GUTTER as f32 + ((seconds * rate - start as f64) / grid.frames_per_column).floor() as f32;
        fill(&mut pixmap, x, 16.0, 1.0, 8.0, RULE);
        let label = tick_label(seconds, decimals);
        if x + 3.0 + label.len() as f32 * 12.0 <= width as f32 {
            draw_text(&mut pixmap, &label, x + 3.0, 2.0);
        }
    }
    // The frequency scale, in Hz.
    let mut last = f32::NEG_INFINITY;
    for hz in [20_000.0, 10_000.0, 5000.0, 2000.0, 1000.0, 500.0, 200.0, 100.0, 50.0, 20.0, 10.0, 5.0, 2.0] {
        if hz > grid.max_hz || hz < grid.min_hz {
            continue;
        }
        let y = RULER as f32 + grid.row_of_hz(hz).round() as f32;
        if y - last < 20.0 || y + 8.0 > height as f32 {
            continue;
        }
        last = y;
        let label = format!("{hz:.0}");
        fill(&mut pixmap, GUTTER as f32 - 5.0, y, 5.0, 1.0, RULE);
        draw_text(&mut pixmap, &label, GUTTER as f32 - 8.0 - label.len() as f32 * 12.0, (y - 7.0).max(RULER as f32));
    }
    // A mark under the ruler wherever two pieces of audio meet.
    let renderer = Renderer::of(project, paths, options.track)?;
    for &join in renderer.timeline().joins() {
        if join > start && join < end {
            let x = GUTTER as f32 + ((join - start) as f64 / grid.frames_per_column).floor() as f32;
            fill(&mut pixmap, x - 1.0, RULER as f32 - 6.0, 3.0, 6.0, JOIN);
        }
    }

    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| io_at(dir, e))?;
    }
    pixmap.save_png(out).map_err(|e| Error::codec("encode_failed", format!("Could not write {}: {e}", out.display())))?;
    Ok(Report {
        width,
        height,
        start,
        end,
        plot_left: GUTTER,
        plot_top: RULER,
        plot_width: plot_w,
        plot_height: plot_h,
        frames_per_pixel: grid.frames_per_column,
        bottom_hz: grid.min_hz,
        top_hz: grid.max_hz,
        floor_db: options.floor_db,
        ceiling_db: options.ceiling_db,
        window: grid.window,
    })
}
