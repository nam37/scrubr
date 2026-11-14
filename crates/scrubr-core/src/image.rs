//! Waveform images of the timeline, drawn on the CPU with tiny-skia so they work headless.
//!
//! One lane per channel under a time ruler. Each pixel column shows the min and max of the audio
//! it covers, as the edits leave it. Wide views read the peak cache; views close enough to see
//! samples render the audio itself.

use std::path::Path;

use tiny_skia::{Color, Paint, Pixmap, Rect, Transform};

use crate::analysis::CLIP_LEVEL;
use crate::error::{Error, Result, io_at};
use crate::peaks::{BASE, MinMax, Peaks};
use crate::project::{Paths, Project};
use crate::render::{Renderer, walk};
use crate::source;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub width: u32,
    pub height: u32,
    /// Timeline range in frames. The default is the whole timeline.
    pub range: Option<(u64, u64)>,
    /// Draw this track alone, by its index in the timeline's tracks. The default is the mix.
    pub track: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Options { width: 1600, height: 480, range: None, track: None }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub width: u32,
    pub height: u32,
    pub start: u64,
    pub end: u64,
    /// Timeline frames per pixel column. Column `x` starts at frame `start + x * frames_per_pixel`.
    pub frames_per_pixel: f64,
    /// True when the columns came from the peak cache, false when the audio itself was rendered.
    pub from_peaks: bool,
}

/// Below this many frames per column the audio is rendered, so crossfades and fades are exact.
const PEAKS_FROM: f64 = (BASE * 4) as f64;
const RULER: u32 = 24;

const BACKGROUND: [u8; 3] = [0xFF, 0xFF, 0xFF];
const LANE: [u8; 3] = [0xF4, 0xF5, 0xF7];
pub(crate) const RULE: [u8; 3] = [0xD8, 0xDC, 0xE2];
const WAVE: [u8; 3] = [0x1A, 0x1D, 0x23];
const CLIP: [u8; 3] = [0xD0, 0x2B, 0x2B];
pub(crate) const JOIN: [u8; 3] = [0xA3, 0x5C, 0x00];
const TEXT: [u8; 3] = [0x59, 0x61, 0x70];

pub(crate) fn fill(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, [r, g, b]: [u8; 3]) {
    let Some(rect) = Rect::from_xywh(x, y, w.max(0.0), h.max(0.0)) else { return };
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(r, g, b, 255));
    paint.anti_alias = false;
    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
}

/// 5x7 glyphs for the ruler: digits, colon and point. Each row is five bits, left to right.
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        ':' => [0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000],
        '.' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110],
        _ => [0; 7],
    }
}

/// Draws `text` with its top-left corner at `(x, y)`, two pixels per glyph dot.
pub(crate) fn draw_text(pixmap: &mut Pixmap, text: &str, x: f32, y: f32) {
    for (i, c) in text.chars().enumerate() {
        for (row, bits) in glyph(c).iter().enumerate() {
            for col in 0..5 {
                if bits & (0b10000 >> col) != 0 {
                    fill(pixmap, x + (i * 12 + col * 2) as f32, y + (row * 2) as f32, 2.0, 2.0, TEXT);
                }
            }
        }
    }
}

/// `1:02:03`, `12:03` or, when ticks are closer than a second, `12:03.250`.
pub(crate) fn tick_label(seconds: f64, decimals: usize) -> String {
    let whole = seconds.floor() as u64;
    let (h, m, s) = (whole / 3600, whole / 60 % 60, whole % 60);
    let mut out = if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") };
    if decimals > 0 {
        let frac = format!("{:.*}", decimals, seconds - whole as f64);
        out.push_str(&frac[1..]);
    }
    out
}

/// The tick spacing, in seconds, that puts labels at least `min_px` apart.
pub(crate) fn tick_step(seconds_per_px: f64, min_px: f64) -> (f64, usize) {
    const STEPS: [(f64, usize); 21] = [
        (0.001, 3), (0.002, 3), (0.005, 3), (0.01, 2), (0.02, 2), (0.05, 2), (0.1, 1), (0.2, 1), (0.5, 1),
        (1.0, 0), (2.0, 0), (5.0, 0), (10.0, 0), (15.0, 0), (30.0, 0), (60.0, 0), (120.0, 0), (300.0, 0),
        (600.0, 0), (1800.0, 0), (3600.0, 0),
    ];
    let want = seconds_per_px * min_px;
    STEPS.iter().copied().find(|(step, _)| *step >= want).unwrap_or_else(|| {
        let hours = (want / 3600.0).ceil();
        (hours * 3600.0, 0)
    })
}

/// One min/max per pixel column per channel, from the peak cache.
fn columns_from_peaks(project: &Project, paths: &Paths, renderer: &Renderer, start: u64, frames_per_px: f64, width: usize) -> Result<Vec<Vec<MinMax>>> {
    let tl = renderer.timeline();
    let channels = project.channels as usize;
    let mut peaks: Vec<Option<Peaks>> = (0..project.sources.len()).map(|_| None).collect();
    let mut columns = vec![vec![MinMax::default(); width]; channels];
    // `x` indexes one column in every channel's lane.
    #[allow(clippy::needless_range_loop)]
    for x in 0..width {
        let t0 = start + (x as f64 * frames_per_px) as u64;
        let t1 = (start + ((x + 1) as f64 * frames_per_px) as u64).max(t0 + 1);
        let mut any = false;
        let mut acc = vec![MinMax { min: f32::INFINITY, max: f32::NEG_INFINITY }; channels];
        for span in tl.resolve(t0, t1) {
            if peaks[span.src].is_none() {
                peaks[span.src] = Some(source::peaks(paths, &project.sources[span.src], project.sample_rate)?);
            }
            let p = peaks[span.src].as_ref().expect("just loaded");
            let gain = tl.gain_at(span.src, span.start + span.frames() / 2);
            for (c, acc) in acc.iter_mut().enumerate() {
                // Mono sources fan out, and a mono project shows the widest of its channels.
                let m = if p.channels() == channels {
                    p.range(c, span.start, span.end)
                } else {
                    (0..p.channels()).map(|sc| p.range(sc, span.start, span.end)).fold(MinMax { min: 0.0, max: 0.0 }, |a, b| MinMax {
                        min: a.min.min(b.min),
                        max: a.max.max(b.max),
                    })
                };
                acc.min = acc.min.min(m.min * gain);
                acc.max = acc.max.max(m.max * gain);
            }
            any = true;
        }
        if any {
            for (c, acc) in acc.into_iter().enumerate() {
                columns[c][x] = acc;
            }
        }
    }
    Ok(columns)
}

/// One min/max per pixel column per channel, from the rendered audio.
fn columns_from_audio(renderer: &mut Renderer, start: u64, end: u64, frames_per_px: f64, width: usize) -> Result<Vec<Vec<MinMax>>> {
    let channels = renderer.channels();
    let mut columns = vec![vec![MinMax { min: f32::INFINITY, max: f32::NEG_INFINITY }; width]; channels];
    walk(renderer, start, end, |at, block| {
        for (i, frame) in block.chunks_exact(channels).enumerate() {
            let x = (((at - start) + i as u64) as f64 / frames_per_px) as usize;
            let x = x.min(width - 1);
            for (c, &s) in frame.iter().enumerate() {
                let m = &mut columns[c][x];
                m.min = m.min.min(s);
                m.max = m.max.max(s);
            }
        }
        Ok(())
    })?;
    // Zoomed past one frame per column, a column with no frame repeats the one before it.
    for lane in &mut columns {
        let mut prev = MinMax::default();
        for m in lane.iter_mut() {
            if m.min > m.max {
                *m = prev;
            }
            prev = *m;
        }
    }
    Ok(columns)
}

/// Draws the timeline at head, or a range of it, to a PNG at `out`.
pub fn render_png(project: &Project, paths: &Paths, out: &Path, options: &Options) -> Result<Report> {
    let (width, height) = (options.width, options.height);
    let channels = project.channels as u32;
    if !(64..=16_384).contains(&width) || height < RULER + 16 * channels || height > 8192 {
        return Err(Error::usage("bad_size", format!("A {width}x{height} image is out of range."))
            .hint("Use a width of 64 to 16384 and a height of at least 24 plus 16 per channel."));
    }
    let mut renderer = Renderer::of(project, paths, options.track)?;
    let total = renderer.total();
    let (start, end) = options.range.unwrap_or((0, total));
    if total == 0 {
        return Err(Error::usage("empty_timeline", "The timeline is empty, so there is nothing to draw."));
    }
    crate::edit::check_range(project, paths, renderer.timeline(), (start, end))?;

    let frames_per_px = (end - start) as f64 / width as f64;
    // The peak cache holds each file on its own. A mix, or one track of several, has to be
    // drawn from the audio.
    let from_peaks = frames_per_px >= PEAKS_FROM && options.track.is_none() && !renderer.timeline().needs_mix();
    let columns = if from_peaks {
        columns_from_peaks(project, paths, &renderer, start, frames_per_px, width as usize)?
    } else {
        columns_from_audio(&mut renderer, start, end, frames_per_px, width as usize)?
    };

    let mut pixmap = Pixmap::new(width, height).ok_or_else(|| Error::usage("bad_size", "The image size is not valid."))?;
    fill(&mut pixmap, 0.0, 0.0, width as f32, height as f32, BACKGROUND);

    // Ruler.
    let rate = project.sample_rate as f64;
    let seconds_per_px = frames_per_px / rate;
    let (step, decimals) = tick_step(seconds_per_px, 110.0);
    let first_tick = (start as f64 / rate / step).ceil() as u64;
    let last_tick = (end as f64 / rate / step).floor() as u64;
    for n in first_tick..=last_tick {
        let seconds = n as f64 * step;
        let x = ((seconds * rate - start as f64) / frames_per_px).floor() as f32;
        fill(&mut pixmap, x, 16.0, 1.0, (height - 16) as f32, RULE);
        let label = tick_label(seconds, decimals);
        if x + 3.0 + label.len() as f32 * 12.0 <= width as f32 {
            draw_text(&mut pixmap, &label, x + 3.0, 2.0);
        }
    }

    // Lanes.
    let lane_h = (height - RULER) as f32 / channels as f32;
    for (c, lane) in columns.iter().enumerate() {
        let top = RULER as f32 + c as f32 * lane_h;
        let (pad, mid) = (2.0, top + lane_h / 2.0);
        let half = lane_h / 2.0 - pad;
        fill(&mut pixmap, 0.0, top + 1.0, width as f32, lane_h - 2.0, LANE);
        // Ticks run through the lanes, under the waveform.
        for n in first_tick..=last_tick {
            let x = ((n as f64 * step * rate - start as f64) / frames_per_px).floor() as f32;
            fill(&mut pixmap, x, top + 1.0, 1.0, lane_h - 2.0, RULE);
        }
        fill(&mut pixmap, 0.0, mid, width as f32, 1.0, RULE);
        for (x, m) in lane.iter().enumerate() {
            let hot = m.max >= CLIP_LEVEL || m.min <= -CLIP_LEVEL;
            let y0 = mid - m.max.clamp(-1.0, 1.0) * half;
            let y1 = mid - m.min.clamp(-1.0, 1.0) * half;
            fill(&mut pixmap, x as f32, y0.floor(), 1.0, (y1 - y0).max(1.0).ceil(), if hot { CLIP } else { WAVE });
        }
    }

    // A mark under the ruler wherever two pieces of audio meet.
    for &join in renderer.timeline().joins() {
        if join > start && join < end {
            let x = ((join - start) as f64 / frames_per_px).floor() as f32;
            fill(&mut pixmap, x - 1.0, RULER as f32 - 6.0, 3.0, 6.0, JOIN);
        }
    }

    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| io_at(dir, e))?;
    }
    pixmap.save_png(out).map_err(|e| Error::codec("encode_failed", format!("Could not write {}: {e}", out.display())))?;
    Ok(Report { width, height, start, end, frames_per_pixel: frames_per_px, from_peaks })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_labels_read_as_timecode() {
        assert_eq!(tick_label(723.0, 0), "12:03");
        assert_eq!(tick_label(3723.25, 2), "1:02:03.25");
        assert_eq!(tick_label(0.5, 1), "0:00.5");
        assert_eq!(tick_step(1.0, 110.0), (120.0, 0));
        assert_eq!(tick_step(0.0001, 110.0), (0.02, 2));
    }
}
