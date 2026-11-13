//! Streams the timeline as audio: reads each segment from its source, applies the level edits,
//! and crossfades the joins.
//!
//! A frame's value depends only on its timeline position, never on how the caller chunks its
//! reads, so analysis, images and exports of one project version all see the same samples.
//!
//! A crossfade is centred on its join and does not change the timeline's length. Each side
//! fades against the audio that follows or precedes it in its own source, which for a cut is the
//! first and last few milliseconds of what was removed. Where a source has no such audio (its
//! own start or end), that side fades to or from silence instead.
//!
//! With more than one track, the audio at a frame of a main-track source is a mix: the main
//! track's own audio and every clip attached to that frame, each with its level edits, its
//! level and its pan, summed. Crossfades work on that mix, so every track is faded across a
//! join together. A project with one track at its default level takes the plain path, and its
//! samples are what they were before tracks existed.

use std::path::PathBuf;

use crate::error::Result;
use crate::project::{Paths, Project};
use crate::source::ensure_audio;
use crate::timeline::Timeline;
use crate::wav::WavFile;

/// Reads one source as interleaved frames in the project's channel layout.
struct SrcReader {
    wav: WavFile,
    /// The frame the next sequential read returns.
    pos: u64,
    frames: u64,
    channels: usize,
    raw: Vec<f32>,
}

impl SrcReader {
    fn open(path: &PathBuf, buffer: usize) -> Result<SrcReader> {
        let wav = WavFile::open_buffered(path, buffer)?;
        let frames = wav.info().frames;
        let channels = wav.info().channels as usize;
        Ok(SrcReader { wav, pos: 0, frames, channels, raw: Vec::new() })
    }

    /// Fills `out` with frames `start..start + out.len() / channels` of the source. Frames
    /// outside the file are silent.
    fn read(&mut self, start: u64, channels: usize, out: &mut [f32]) -> Result<()> {
        let want = out.len() / channels;
        let have = (self.frames.saturating_sub(start) as usize).min(want);
        if have > 0 {
            if self.pos != start {
                self.wav.seek(start)?;
            }
            let got = self.wav.read(have, &mut self.raw)?;
            self.pos = start + got as u64;
            map_channels(&self.raw, self.channels, &mut out[..got * channels], channels);
            out[got * channels..].fill(0.0);
        } else {
            out.fill(0.0);
        }
        Ok(())
    }
}

/// Mono fans out to every channel, anything folds down to mono by averaging, and other layouts
/// keep the channels they share.
fn map_channels(src: &[f32], src_ch: usize, dst: &mut [f32], dst_ch: usize) {
    if src_ch == dst_ch {
        dst.copy_from_slice(src);
        return;
    }
    for (from, to) in src.chunks_exact(src_ch).zip(dst.chunks_exact_mut(dst_ch)) {
        if src_ch == 1 {
            to.fill(from[0]);
        } else if dst_ch == 1 {
            to[0] = from.iter().sum::<f32>() / src_ch as f32;
        } else {
            for (c, s) in to.iter_mut().enumerate() {
                *s = from.get(c).copied().unwrap_or(0.0);
            }
        }
    }
}

/// The crossfade around the boundary before a segment: `before` frames on the earlier side and
/// `after` on the later one.
#[derive(Clone, Copy, Debug, Default)]
struct Window {
    before: u64,
    after: u64,
    /// The earlier segment's source has audio past its end to fade out across the whole window.
    tail: bool,
    /// The later segment's source has audio before its start to fade in across the whole window.
    lead: bool,
}

pub struct Renderer {
    tl: Timeline,
    channels: usize,
    files: Vec<PathBuf>,
    main: Vec<Option<SrcReader>>,
    /// A second reader per source for the short reads a crossfade needs, so they do not disturb
    /// the main reader's position or its large buffer.
    side: Vec<Option<SrcReader>>,
    /// One window per boundary: before segment 0, between segments, after the last.
    windows: Vec<Window>,
    extra: Vec<f32>,
    /// What each track contributes to the left and the right, when there is mixing to do.
    mix: Option<Vec<[f32; 2]>>,
    scratch: Vec<f32>,
}

/// Scales interleaved frames by a gain for the left channel and one for the right. Audio that is
/// not stereo has no sides, and takes the left gain.
fn scale(buf: &mut [f32], channels: usize, gain: [f32; 2]) {
    if gain == [1.0, 1.0] {
        return;
    }
    if channels == 2 {
        for frame in buf.as_chunks_mut::<2>().0 {
            frame[0] *= gain[0];
            frame[1] *= gain[1];
        }
    } else {
        buf.iter_mut().for_each(|s| *s *= gain[0]);
    }
}

impl Renderer {
    /// A renderer for the project's timeline at head.
    pub fn new(project: &Project, paths: &Paths) -> Result<Renderer> {
        Self::with_timeline(project, paths, Timeline::build(project)?)
    }

    pub fn with_timeline(project: &Project, paths: &Paths, tl: Timeline) -> Result<Renderer> {
        Self::build(project, paths, tl, None)
    }

    /// A renderer for the mix at head, or for one track alone when `track` is given.
    pub fn of(project: &Project, paths: &Paths, track: Option<usize>) -> Result<Renderer> {
        Self::build(project, paths, Timeline::build(project)?, track)
    }

    /// A renderer that plays one track alone, at its own level and pan, whatever is muted or
    /// soloed: for measuring a track, or hearing it by itself.
    pub fn for_track(project: &Project, paths: &Paths, tl: Timeline, track: usize) -> Result<Renderer> {
        Self::build(project, paths, tl, Some(track))
    }

    fn build(project: &Project, paths: &Paths, tl: Timeline, only: Option<usize>) -> Result<Renderer> {
        let mut used = vec![false; project.sources.len()];
        for seg in tl.segs() {
            used[seg.src] = true;
            tl.attached(seg.src).iter().for_each(|a| used[a.clip] = true);
        }
        let mix = (only.is_some() || tl.needs_mix()).then(|| tl.mix_gains(project.channels as usize, only));
        for (source, used) in project.sources.iter().zip(&used) {
            if *used {
                ensure_audio(paths, source, project.sample_rate)?;
            }
        }
        let segs = tl.segs();
        let mut windows = vec![Window::default(); segs.len() + 1];
        for (i, window) in windows.iter_mut().enumerate() {
            let a = i.checked_sub(1).map(|i| segs[i]);
            let b = segs.get(i).copied();
            *window = match (a, b) {
                (None, Some(b)) => Window { before: 0, after: (b.xf_in as u64).min(b.frames() / 2), tail: false, lead: false },
                (Some(a), None) => Window { before: (a.xf_out as u64).min(a.frames() / 2), after: 0, tail: false, lead: false },
                (Some(a), Some(b)) => {
                    let xf = a.xf_out.max(b.xf_in) as u64;
                    let before = (xf / 2).min(a.frames() / 2);
                    let after = (xf - xf / 2).min(b.frames() / 2);
                    Window { before, after, tail: after > 0 && a.end + after <= tl.source_frames(a.src), lead: before > 0 && b.start >= before }
                }
                (None, None) => Window::default(),
            };
        }
        Ok(Renderer {
            channels: project.channels as usize,
            files: project.sources.iter().map(|s| paths.audio_file(s)).collect(),
            main: project.sources.iter().map(|_| None).collect(),
            side: project.sources.iter().map(|_| None).collect(),
            windows,
            tl,
            extra: Vec::new(),
            mix,
            scratch: Vec::new(),
        })
    }

    pub fn timeline(&self) -> &Timeline {
        &self.tl
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Length of the timeline in frames.
    pub fn total(&self) -> u64 {
        self.tl.total()
    }

    fn reader<'a>(slots: &'a mut [Option<SrcReader>], files: &[PathBuf], src: usize, buffer: usize) -> Result<&'a mut SrcReader> {
        if slots[src].is_none() {
            slots[src] = Some(SrcReader::open(&files[src], buffer)?);
        }
        Ok(slots[src].as_mut().expect("just opened"))
    }

    /// Fills `buf` with what is heard at frames `first..` of main-track source `src`, before any
    /// crossfade: that source's audio with its level edits and, when there is mixing to do, every
    /// track's audio at those frames, each with its own edits, level and pan, summed.
    fn fill(&mut self, side: bool, src: usize, first: u64, buf: &mut [f32]) -> Result<()> {
        let Renderer { tl, channels, files, main, side: aside, mix, scratch, .. } = self;
        let ch = *channels;
        let (slots, buffer) = if side { (aside, 1 << 14) } else { (main, 1 << 20) };
        if tl.is_silence(src) {
            buf.fill(0.0);
        } else {
            Self::reader(slots, files, src, buffer)?.read(first, ch, buf)?;
        }
        let Some(mix) = mix else {
            tl.apply_gain(src, first, buf, ch);
            return Ok(());
        };
        tl.apply_gain_for(0, src, first, buf, ch);
        scale(buf, ch, mix[0]);
        let end = first + (buf.len() / ch) as u64;
        for i in 0..tl.attached(src).len() {
            let a = tl.attached(src)[i];
            let gain = mix[a.track];
            if a.end <= first || a.start >= end || gain == [0.0, 0.0] {
                continue;
            }
            let (from, to) = (a.start.max(first), a.end.min(end));
            scratch.clear();
            scratch.resize((to - from) as usize * ch, 0.0);
            Self::reader(slots, files, a.clip, buffer)?.read(a.clip_start + (from - a.start), ch, scratch)?;
            tl.apply_gain_for(a.track, src, from, scratch, ch);
            scale(scratch, ch, gain);
            let at = (from - first) as usize * ch;
            for (out, s) in buf[at..at + scratch.len()].iter_mut().zip(scratch.iter()) {
                *out += *s;
            }
        }
        Ok(())
    }

    /// Replaces `out` with `frames` interleaved frames starting at timeline position `t0`.
    /// Positions past the end of the timeline are silent.
    pub fn read(&mut self, t0: u64, frames: usize, out: &mut Vec<f32>) -> Result<()> {
        let ch = self.channels;
        out.clear();
        out.resize(frames * ch, 0.0);
        let t1 = (t0 + frames as u64).min(self.tl.total());
        if t0 >= t1 {
            return Ok(());
        }
        let last = self.tl.segs().len() - 1;
        let mut i = self.tl.seg_at(t0);
        let mut t = t0;
        while t < t1 {
            let seg = self.tl.segs()[i];
            let seg_t = self.tl.seg_start(i);
            let n = seg.frames();
            let k0 = t - seg_t;
            let k1 = (t1 - seg_t).min(n);
            let dst = &mut out[(t - t0) as usize * ch..(seg_t + k1 - t0) as usize * ch];
            self.fill(false, seg.src, seg.start + k0, dst)?;

            // The join, or timeline start, this segment begins with.
            let w = self.windows[i];
            if k0 < w.after {
                let to = k1.min(w.after);
                let part = &mut dst[..(to - k0) as usize * ch];
                if i == 0 {
                    for (j, frame) in part.chunks_exact_mut(ch).enumerate() {
                        let gain = (((k0 + j as u64) as f64 + 0.5) / w.after as f64) as f32;
                        frame.iter_mut().for_each(|s| *s *= gain);
                    }
                } else {
                    let a = self.tl.segs()[i - 1];
                    let mut extra = std::mem::take(&mut self.extra);
                    extra.clear();
                    extra.resize(part.len(), 0.0);
                    if w.tail {
                        self.fill(true, a.src, a.end + k0, &mut extra)?;
                    }
                    self.extra = extra;
                    let span = (w.before + w.after) as f64;
                    for (j, (frame, tail)) in part.chunks_exact_mut(ch).zip(self.extra.chunks_exact(ch)).enumerate() {
                        let d = (k0 + j as u64) as f64;
                        let through = (d + w.before as f64 + 0.5) / span;
                        let fade_in = (if w.lead { through } else { (d + 0.5) / w.after as f64 }) as f32;
                        let fade_out = (if w.tail { 1.0 - through } else { 0.0 }) as f32;
                        for (s, t) in frame.iter_mut().zip(tail) {
                            *s = *s * fade_in + *t * fade_out;
                        }
                    }
                }
            }

            // The join, or timeline end, this segment ends with.
            let w = self.windows[i + 1];
            if w.before > 0 && k1 > n - w.before {
                let from = k0.max(n - w.before);
                let part = &mut dst[(from - k0) as usize * ch..];
                if i == last {
                    for (j, frame) in part.chunks_exact_mut(ch).enumerate() {
                        let left = (n - (from + j as u64)) as f64;
                        let gain = ((left - 0.5) / w.before as f64) as f32;
                        frame.iter_mut().for_each(|s| *s *= gain);
                    }
                } else {
                    let b = self.tl.segs()[i + 1];
                    let mut extra = std::mem::take(&mut self.extra);
                    extra.clear();
                    extra.resize(part.len(), 0.0);
                    if w.lead {
                        self.fill(true, b.src, b.start - (n - from), &mut extra)?;
                    }
                    self.extra = extra;
                    let span = (w.before + w.after) as f64;
                    for (j, (frame, lead)) in part.chunks_exact_mut(ch).zip(self.extra.chunks_exact(ch)).enumerate() {
                        // Frames into the window, which starts `before` frames ahead of the join.
                        let p = (from + j as u64 + w.before - n) as f64 + 0.5;
                        let fade_out = 1.0 - if w.tail { p / span } else { p / w.before as f64 };
                        let fade_in = if w.lead { p / span } else { 0.0 };
                        for (s, l) in frame.iter_mut().zip(lead) {
                            *s = *s * fade_out as f32 + *l * fade_in as f32;
                        }
                    }
                }
            }
            t = seg_t + k1;
            i += 1;
        }
        Ok(())
    }
}

/// Frames per read when a caller just wants to walk a range.
pub const BLOCK_FRAMES: usize = 1 << 15;

/// Renders timeline range `t0..t1` block by block. `each` gets the timeline position of the
/// block's first frame and its interleaved samples.
pub fn walk(renderer: &mut Renderer, t0: u64, t1: u64, mut each: impl FnMut(u64, &[f32]) -> Result<()>) -> Result<()> {
    let t1 = t1.min(renderer.total());
    let mut buf = Vec::new();
    let mut t = t0;
    while t < t1 {
        let frames = ((t1 - t) as usize).min(BLOCK_FRAMES);
        renderer.read(t, frames, &mut buf)?;
        each(t, &buf)?;
        t += frames as u64;
    }
    Ok(())
}
