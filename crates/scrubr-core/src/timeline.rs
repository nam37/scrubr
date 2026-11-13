//! The timeline, computed by replaying the edit log up to head.
//!
//! A timeline is an ordered list of segments, each a frame range of one source. Every source
//! frame appears at most once, and the segments of one source stay in source order, so a source
//! frame and a timeline position map to each other one to one while the frame is visible.
//! Level edits (gain, fade, mute, normalize) do not change the segments; they become a gain that
//! is a function of the source frame, applied when audio is rendered.
//!
//! The segments are the main track, and they are time. Every other track's audio is attached to
//! frames of the main track's sources, so wherever those frames are on the timeline, the attached
//! audio is too: one cut takes the same moment out of every track. A level edit can name one
//! track, and then it is a gain on that track's audio at those frames.

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::project::{FadeDir, MAIN_TRACK, Op, Project, Span};

/// A piece of one source on the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    /// Index into the project's sources.
    pub src: usize,
    pub start: u64,
    pub end: u64,
    /// Crossfade length, in frames, at the join this segment starts with and the one it ends
    /// with. Zero is a hard join. At the two ends of the timeline it is a short fade from or to
    /// silence, set when an edit trimmed that end.
    pub xf_in: u32,
    pub xf_out: u32,
}

impl Seg {
    /// Length in frames.
    pub fn frames(&self) -> u64 {
        self.end - self.start
    }
}

/// A frame range of one source, by source index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SrcSpan {
    pub src: usize,
    pub start: u64,
    pub end: u64,
}

impl SrcSpan {
    /// Length in frames.
    pub fn frames(&self) -> u64 {
        self.end - self.start
    }
}

/// A track as the log leaves it: its name and its place in the mix.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackState {
    pub id: String,
    pub name: String,
    pub gain_db: f64,
    /// From -1.0, full left, to 1.0, full right. Zero leaves the track as it is.
    pub pan: f64,
    pub mute: bool,
    pub solo: bool,
    /// Taken out of the project. It keeps its place in the list so indexes stay put.
    pub removed: bool,
}

/// A piece of a clip: frames of source `clip`, from `clip_start`, attached to frames
/// `start..end` of one of the main track's sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attach {
    pub start: u64,
    pub end: u64,
    /// Index into [`Timeline::tracks`].
    pub track: usize,
    /// Index into the project's sources.
    pub clip: usize,
    pub clip_start: u64,
}

#[derive(Clone, Copy, Debug)]
enum ShapeKind {
    Gain(f32),
    FadeIn,
    FadeOut,
    /// Silence, with a ramp of this many frames inside each end.
    Mute(u64),
}

/// One span of a level edit: source frames `start..end`, which sit `offset` frames into the
/// edit's span list.
#[derive(Clone, Copy, Debug)]
struct Piece {
    start: u64,
    end: u64,
    offset: u64,
}

/// A level edit over spans.
#[derive(Clone, Debug)]
struct Shape {
    kind: ShapeKind,
    /// The one track it changes, or every track.
    track: Option<usize>,
    total: u64,
    /// Per source index, sorted by start.
    pieces: Vec<Vec<Piece>>,
}

impl Shape {
    /// The gain `at` frames into the edit's span list.
    fn gain(&self, at: u64) -> f32 {
        match self.kind {
            ShapeKind::Gain(g) => g,
            ShapeKind::FadeIn => ((at as f64 + 0.5) / self.total as f64) as f32,
            ShapeKind::FadeOut => (1.0 - (at as f64 + 0.5) / self.total as f64) as f32,
            ShapeKind::Mute(ramp) => {
                let ramp = ramp.min(self.total / 2);
                let from_end = self.total - 1 - at;
                if at < ramp {
                    (1.0 - (at + 1) as f64 / ramp as f64) as f32
                } else if from_end < ramp {
                    (1.0 - (from_end + 1) as f64 / ramp as f64) as f32
                } else {
                    0.0
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Timeline {
    segs: Vec<Seg>,
    /// Timeline position of each segment's first frame, then the total length.
    starts: Vec<u64>,
    /// Whole-timeline gain in dB, the sum of every gain and normalize edit without spans.
    global_db: f64,
    /// The whole-timeline gain of each track as a factor: `global_db` and the track's own.
    globals: Vec<f32>,
    /// Whole-timeline gain in dB of each track alone.
    track_db: Vec<f64>,
    shapes: Vec<Shape>,
    source_ids: Vec<String>,
    source_frames: Vec<u64>,
    silence: Vec<bool>,
    tracks: Vec<TrackState>,
    /// Per main-track source, the clips attached to its frames, sorted by start.
    attached: Vec<Vec<Attach>>,
}

fn invalid(what: String) -> Error {
    Error::codec("project_invalid", format!("The project's edit log does not replay: {what}"))
}

impl Timeline {
    /// The timeline at head.
    pub fn build(project: &Project) -> Result<Timeline> {
        Self::build_with(project, &[])
    }

    /// The timeline at head with `extra` operations applied after it. A batch uses this to
    /// measure the result of its earlier operations.
    pub fn build_with(project: &Project, extra: &[Op]) -> Result<Timeline> {
        let mut tl = Timeline {
            segs: Vec::new(),
            starts: Vec::new(),
            global_db: 0.0,
            globals: Vec::new(),
            track_db: vec![0.0],
            shapes: Vec::new(),
            source_ids: project.sources.iter().map(|s| s.id.clone()).collect(),
            source_frames: project.sources.iter().map(|s| s.frames).collect(),
            silence: project.sources.iter().map(|s| s.silence).collect(),
            tracks: vec![TrackState { id: MAIN_TRACK.to_string(), name: "main".to_string(), gain_db: 0.0, pan: 0.0, mute: false, solo: false, removed: false }],
            attached: vec![Vec::new(); project.sources.len()],
        };
        let index: HashMap<&str, usize> = project.sources.iter().enumerate().map(|(i, s)| (s.id.as_str(), i)).collect();
        for op in project.applied_ops().into_iter().chain(extra) {
            tl.apply(op, &index)?;
        }
        tl.finish();
        Ok(tl)
    }

    fn finish(&mut self) {
        self.starts.clear();
        let mut at = 0;
        for seg in &self.segs {
            self.starts.push(at);
            at += seg.frames();
        }
        self.starts.push(at);
        self.globals = self.track_db.iter().map(|db| 10f64.powf((self.global_db + db) / 20.0) as f32).collect();
        for list in &mut self.attached {
            list.sort_by_key(|a| (a.start, a.track));
        }
    }

    /// The index of the track with this id. An edit can name a track that a later revert took
    /// out of the log; such an edit then does nothing.
    fn track_by_id(&self, id: &str) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }

    fn src_spans(&self, spans: &[Span], index: &HashMap<&str, usize>) -> Result<Vec<SrcSpan>> {
        spans
            .iter()
            .map(|Span(id, start, end)| {
                let src = *index.get(id.as_str()).ok_or_else(|| invalid(format!("an edit names source {id}, which the project does not list")))?;
                if start > end {
                    return Err(invalid(format!("span [{id}, {start}, {end}] ends before it starts")));
                }
                Ok(SrcSpan { src, start: *start, end: *end })
            })
            .collect()
    }

    fn apply(&mut self, op: &Op, index: &HashMap<&str, usize>) -> Result<()> {
        let source = |id: &str| index.get(id).copied().ok_or_else(|| invalid(format!("an edit names source {id}, which the project does not list")));
        match op {
            Op::Import { sources } => {
                for id in sources {
                    let src = source(id)?;
                    self.segs.push(Seg { src, start: 0, end: self.source_frames[src], xf_in: 0, xf_out: 0 });
                }
            }
            Op::Cut { spans, crossfade } | Op::Keep { spans, crossfade } => {
                let spans = self.src_spans(spans, index)?;
                self.remove(&spans, *crossfade);
            }
            Op::Gain { gain_db, spans: None, track } | Op::Normalize { gain_db, track, .. } => match track {
                None => self.global_db += gain_db,
                Some(id) => {
                    if let Some(t) = self.track_by_id(id) {
                        self.track_db[t] += gain_db;
                    }
                }
            },
            Op::Gain { gain_db, spans: Some(spans), track } => {
                let spans = self.src_spans(spans, index)?;
                self.shape(ShapeKind::Gain(10f64.powf(gain_db / 20.0) as f32), &spans, track);
            }
            Op::Fade { dir, spans, track } => {
                let spans = self.src_spans(spans, index)?;
                self.shape(if *dir == FadeDir::In { ShapeKind::FadeIn } else { ShapeKind::FadeOut }, &spans, track);
            }
            Op::Mute { spans, crossfade, track } => {
                let spans = self.src_spans(spans, index)?;
                self.shape(ShapeKind::Mute(*crossfade as u64), &spans, track);
            }
            Op::Track { track, name } => {
                if self.track_by_id(track).is_none() {
                    self.tracks.push(TrackState { id: track.clone(), name: name.clone(), gain_db: 0.0, pan: 0.0, mute: false, solo: false, removed: false });
                    self.track_db.push(0.0);
                }
            }
            Op::Clip { track, source: id, at } => {
                let clip = source(id)?;
                let spans = self.src_spans(at, index)?;
                if let Some(track) = self.track_by_id(track) {
                    let mut clip_start = 0;
                    for span in spans.iter().filter(|s| s.frames() > 0) {
                        self.attached[span.src].push(Attach { start: span.start, end: span.end, track, clip, clip_start });
                        clip_start += span.frames();
                    }
                }
            }
            Op::TrackSet { track, name, gain_db, pan, mute, solo } => {
                if let Some(t) = self.track_by_id(track) {
                    let state = &mut self.tracks[t];
                    if let Some(name) = name {
                        state.name = name.clone();
                    }
                    state.gain_db = gain_db.unwrap_or(state.gain_db);
                    state.pan = pan.unwrap_or(state.pan);
                    state.mute = mute.unwrap_or(state.mute);
                    state.solo = solo.unwrap_or(state.solo);
                }
            }
            Op::TrackRemove { track } => {
                if let Some(t) = self.track_by_id(track) {
                    self.tracks[t].removed = true;
                }
            }
            Op::Insert { source: id, at, crossfade } => {
                let src = source(id)?;
                let anchor = match at {
                    Some((id, frame)) => Some((source(id)?, *frame)),
                    None => None,
                };
                self.insert(src, anchor, *crossfade);
            }
            Op::Batch { ops } => {
                for op in ops {
                    self.apply(op, index)?;
                }
            }
            Op::Revert { .. } => {}
        }
        Ok(())
    }

    fn shape(&mut self, kind: ShapeKind, spans: &[SrcSpan], track: &Option<String>) {
        let track = match track {
            None => None,
            Some(id) => match self.track_by_id(id) {
                Some(t) => Some(t),
                // The track is no longer in the log, so there is nothing for the edit to change.
                None => return,
            },
        };
        let mut pieces = vec![Vec::new(); self.source_frames.len()];
        let mut offset = 0;
        for span in spans {
            if span.frames() > 0 {
                pieces[span.src].push(Piece { start: span.start, end: span.end, offset });
                offset += span.frames();
            }
        }
        if offset == 0 {
            return;
        }
        for list in &mut pieces {
            list.sort_by_key(|p| p.start);
        }
        self.shapes.push(Shape { kind, track, total: offset, pieces });
    }

    /// Takes `spans` out of the timeline. Each run of removed audio leaves a join, or a trimmed
    /// end, that gets `xf`.
    fn remove(&mut self, spans: &[SrcSpan], xf: u32) {
        let mut cuts: Vec<Vec<(u64, u64)>> = vec![Vec::new(); self.source_frames.len()];
        for span in spans {
            if span.frames() > 0 {
                cuts[span.src].push((span.start, span.end));
            }
        }
        for list in &mut cuts {
            list.sort_unstable();
        }
        let mut out: Vec<Seg> = Vec::with_capacity(self.segs.len() + spans.len());
        let mut gap = false;
        for seg in &self.segs {
            let list = &cuts[seg.src];
            let mut at = seg.start;
            let mut i = list.partition_point(|c| c.1 <= seg.start);
            let mut emit = |from: u64, to: u64, gap: &mut bool| {
                let mut piece = Seg {
                    src: seg.src,
                    start: from,
                    end: to,
                    xf_in: if from == seg.start { seg.xf_in } else { 0 },
                    xf_out: if to == seg.end { seg.xf_out } else { 0 },
                };
                if *gap {
                    piece.xf_in = xf;
                    if let Some(last) = out.last_mut() {
                        last.xf_out = xf;
                    }
                    *gap = false;
                }
                out.push(piece);
            };
            while i < list.len() && list[i].0 < seg.end {
                let (from, to) = list[i];
                if from > at {
                    emit(at, from, &mut gap);
                }
                if to > at {
                    gap = true;
                    at = to.min(seg.end);
                }
                i += 1;
            }
            if at < seg.end {
                emit(at, seg.end, &mut gap);
            }
        }
        if gap && let Some(last) = out.last_mut() {
            last.xf_out = xf;
        }
        self.segs = out;
    }

    /// Places source `src`, whole, at `anchor`: just before that frame of that source. An anchor
    /// in audio that is no longer on the timeline lands where that audio used to be.
    fn insert(&mut self, src: usize, anchor: Option<(usize, u64)>, xf: u32) {
        let at = match anchor {
            None => self.segs.len(),
            Some((a_src, frame)) => match self.segs.iter().position(|s| s.src == a_src && s.end > frame) {
                Some(i) if self.segs[i].start < frame => {
                    let whole = self.segs[i];
                    self.segs[i] = Seg { end: frame, xf_out: 0, ..whole };
                    self.segs.insert(i + 1, Seg { start: frame, xf_in: 0, ..whole });
                    i + 1
                }
                Some(i) => i,
                None => self.segs.iter().rposition(|s| s.src == a_src).map_or(self.segs.len(), |i| i + 1),
            },
        };
        let mut seg = Seg { src, start: 0, end: self.source_frames[src], xf_in: 0, xf_out: 0 };
        if at > 0 {
            self.segs[at - 1].xf_out = xf;
            seg.xf_in = xf;
        }
        if at < self.segs.len() {
            self.segs[at].xf_in = xf;
            seg.xf_out = xf;
        }
        self.segs.insert(at, seg);
    }

    // ---- queries ----------------------------------------------------------------------------

    /// Length in frames.
    pub fn total(&self) -> u64 {
        *self.starts.last().unwrap_or(&0)
    }

    pub fn segs(&self) -> &[Seg] {
        &self.segs
    }

    /// Timeline position of segment `i`'s first frame; `i == segs().len()` gives the total.
    pub fn seg_start(&self, i: usize) -> u64 {
        self.starts[i]
    }

    /// The segment that holds timeline position `t`, which must be before the end.
    pub fn seg_at(&self, t: u64) -> usize {
        self.starts.partition_point(|&s| s <= t) - 1
    }

    pub fn global_gain_db(&self) -> f64 {
        self.global_db
    }

    /// Timeline positions where two segments meet.
    pub fn joins(&self) -> &[u64] {
        if self.segs.len() < 2 { &[] } else { &self.starts[1..self.segs.len()] }
    }

    /// The source spans under timeline range `t0..t1`, in timeline order.
    pub fn resolve(&self, t0: u64, t1: u64) -> Vec<SrcSpan> {
        let t1 = t1.min(self.total());
        let mut out = Vec::new();
        if t0 >= t1 {
            return out;
        }
        let mut i = self.seg_at(t0);
        while i < self.segs.len() && self.starts[i] < t1 {
            let seg = self.segs[i];
            let from = seg.start + t0.saturating_sub(self.starts[i]);
            let to = seg.start + (t1 - self.starts[i]).min(seg.frames());
            if from < to {
                out.push(SrcSpan { src: seg.src, start: from, end: to });
            }
            i += 1;
        }
        out
    }

    /// The source spans outside timeline range `t0..t1`.
    pub fn resolve_outside(&self, t0: u64, t1: u64) -> Vec<SrcSpan> {
        let mut out = self.resolve(0, t0);
        out.extend(self.resolve(t1, self.total()));
        out
    }

    /// Timeline position `t` as an insertion point in source coordinates: just before the frame
    /// that sits at `t`, or after the last frame when `t` is the end. `None` on an empty timeline.
    pub fn anchor(&self, t: u64) -> Option<(usize, u64)> {
        let last = self.segs.last()?;
        if t >= self.total() {
            return Some((last.src, last.end));
        }
        let i = self.seg_at(t);
        Some((self.segs[i].src, self.segs[i].start + (t - self.starts[i])))
    }

    /// Where source frames `start..end` of `src` sit on the timeline now, as timeline ranges in
    /// order. Hidden frames are left out; ranges that touch are merged.
    pub fn locate(&self, src: usize, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = Vec::new();
        for (i, seg) in self.segs.iter().enumerate() {
            if seg.src != src {
                continue;
            }
            let from = start.max(seg.start);
            let to = end.min(seg.end);
            if from >= to {
                continue;
            }
            let range = (self.starts[i] + (from - seg.start), self.starts[i] + (to - seg.start));
            match out.last_mut() {
                Some(last) if last.1 == range.0 => last.1 = range.1,
                _ => out.push(range),
            }
        }
        out
    }

    /// The timeline position of source frame `frame`, or of the first visible frame of that
    /// source after it. `None` when nothing of the source from that frame on is visible.
    pub fn position_from(&self, src: usize, frame: u64) -> Option<u64> {
        self.segs
            .iter()
            .enumerate()
            .find(|(_, s)| s.src == src && s.end > frame)
            .map(|(i, s)| self.starts[i] + frame.saturating_sub(s.start))
    }

    pub fn source_id(&self, src: usize) -> &str {
        &self.source_ids[src]
    }

    pub fn source_frames(&self, src: usize) -> u64 {
        self.source_frames[src]
    }

    pub fn source_count(&self) -> usize {
        self.source_frames.len()
    }

    /// True for a source that is a stretch of silence: time with no main audio.
    pub fn is_silence(&self, src: usize) -> bool {
        self.silence[src]
    }

    // ---- tracks -------------------------------------------------------------------------------

    /// Every track the log has made, the main one first. A removed track stays in the list,
    /// marked, so indexes stay put.
    pub fn tracks(&self) -> &[TrackState] {
        &self.tracks
    }

    /// The index of the track with this id or name, if it is still in the project.
    pub fn find_track(&self, key: &str) -> Option<usize> {
        self.tracks.iter().position(|t| !t.removed && (t.id == key || t.name == key))
    }

    /// The clips attached to frames of main-track source `src`, sorted by where they start.
    pub fn attached(&self, src: usize) -> &[Attach] {
        &self.attached[src]
    }

    /// Where each clip of a track sits on the timeline now: its source and the timeline ranges
    /// it is heard over, in order. A clip whose time has all been cut has no ranges.
    pub fn clips(&self, track: usize) -> Vec<(usize, Vec<(u64, u64)>)> {
        let mut out: Vec<(usize, Vec<(u64, u64)>)> = Vec::new();
        for (src, list) in self.attached.iter().enumerate() {
            for a in list.iter().filter(|a| a.track == track) {
                let ranges = self.locate(src, a.start, a.end);
                match out.iter_mut().find(|(clip, _)| *clip == a.clip) {
                    Some((_, all)) => all.extend(ranges),
                    None => out.push((a.clip, ranges)),
                }
            }
        }
        for (_, ranges) in &mut out {
            // A clip laid across joins is attached in pieces; where they still touch it is one stretch.
            ranges.sort_unstable();
            let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
            for &range in ranges.iter() {
                match merged.last_mut() {
                    Some(last) if last.1 == range.0 => last.1 = range.1,
                    _ => merged.push(range),
                }
            }
            *ranges = merged;
        }
        out.sort_by_key(|(clip, ranges)| (ranges.first().map_or(u64::MAX, |r| r.0), *clip));
        out
    }

    /// True when rendering has more to do than play the main track as it is: there is another
    /// track with audio, or the main track's place in the mix has been changed.
    pub fn needs_mix(&self) -> bool {
        let main = &self.tracks[0];
        let others = self.attached.iter().flatten().any(|a| !self.tracks[a.track].removed);
        others || main.gain_db != 0.0 || main.pan != 0.0 || main.mute || main.removed || self.tracks.iter().any(|t| t.solo && !t.removed)
    }

    /// What each track contributes to a mix of `channels` channels, as a gain for the left and
    /// for the right: its level, its pan, and silence if it is muted, removed, or not one of
    /// the soloed tracks when any are. With `only`, that track is heard alone at its own level
    /// and pan, whatever is muted or soloed.
    ///
    /// Pan is a balance: at the centre both sides are untouched, and moving to one side turns
    /// the other down, to silence at the end. It needs two channels and does nothing otherwise.
    pub fn mix_gains(&self, channels: usize, only: Option<usize>) -> Vec<[f32; 2]> {
        let any_solo = self.tracks.iter().any(|t| t.solo && !t.removed);
        self.tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let heard = match only {
                    Some(only) => i == only,
                    None => !t.removed && !t.mute && (!any_solo || t.solo),
                };
                if !heard {
                    return [0.0, 0.0];
                }
                let level = 10f64.powf(t.gain_db / 20.0);
                let pan = if channels == 2 { t.pan.clamp(-1.0, 1.0) } else { 0.0 };
                [(level * (1.0 - pan).min(1.0)) as f32, (level * (1.0 + pan).min(1.0)) as f32]
            })
            .collect()
    }

    /// Source spans as the project stores them.
    pub fn stored(&self, spans: &[SrcSpan]) -> Vec<Span> {
        spans.iter().map(|s| Span(self.source_ids[s.src].clone(), s.start, s.end)).collect()
    }

    // ---- level --------------------------------------------------------------------------------

    /// The gain every level edit gives the main track at one source frame.
    pub fn gain_at(&self, src: usize, frame: u64) -> f32 {
        self.gain_for(0, src, frame)
    }

    /// The gain every level edit gives one track's audio at a frame of main-track source `src`:
    /// the edits made to every track, and those made to this one.
    pub fn gain_for(&self, track: usize, src: usize, frame: u64) -> f32 {
        let mut gain = self.globals[track];
        for shape in self.shapes.iter().filter(|s| s.track.is_none_or(|t| t == track)) {
            let list = &shape.pieces[src];
            let i = list.partition_point(|p| p.end <= frame);
            if let Some(p) = list.get(i)
                && p.start <= frame
            {
                gain *= shape.gain(p.offset + (frame - p.start));
            }
        }
        gain
    }

    /// Applies every level edit to `buf`, which holds interleaved frames of `src` starting at
    /// source frame `first`. The result for a frame does not depend on how the audio is chunked.
    pub fn apply_gain(&self, src: usize, first: u64, buf: &mut [f32], channels: usize) {
        self.apply_gain_for(0, src, first, buf, channels)
    }

    /// [`Timeline::apply_gain`] for one track's audio at frames of main-track source `src`.
    pub fn apply_gain_for(&self, track: usize, src: usize, first: u64, buf: &mut [f32], channels: usize) {
        let end = first + (buf.len() / channels) as u64;
        let global = self.globals[track];
        if global != 1.0 {
            for s in buf.iter_mut() {
                *s *= global;
            }
        }
        for shape in self.shapes.iter().filter(|s| s.track.is_none_or(|t| t == track)) {
            let list = &shape.pieces[src];
            let mut i = list.partition_point(|p| p.end <= first);
            while let Some(p) = list.get(i) {
                if p.start >= end {
                    break;
                }
                for frame in p.start.max(first)..p.end.min(end) {
                    let gain = shape.gain(p.offset + (frame - p.start));
                    let at = (frame - first) as usize * channels;
                    for s in &mut buf[at..at + channels] {
                        *s *= gain;
                    }
                }
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Edit, Source, Who};

    pub(crate) fn project(frames: &[u64]) -> Project {
        let mut p = Project::new(48_000, 1);
        let who = Who { author: "test".into(), session: None };
        let mut ids = Vec::new();
        for &n in frames {
            let id = p.next_source_id();
            p.sources.push(Source {
                id: id.clone(),
                path: format!("{id}.wav"),
                frames: n,
                bytes: 0,
                mtime: String::new(),
                hash: String::new(),
                sample_rate: 48_000,
                channels: 1,
                bits: 16,
                float: false,
                conformed: false,
                silence: false,
            });
            ids.push(id);
        }
        p.push_edit(Op::Import { sources: ids }, None, &who);
        p
    }

    fn who() -> Who {
        Who { author: "test".into(), session: None }
    }

    fn cut(p: &mut Project, t0: u64, t1: u64, xf: u32) -> String {
        let tl = Timeline::build(p).unwrap();
        let spans = tl.stored(&tl.resolve(t0, t1));
        p.push_edit(Op::Cut { spans, crossfade: xf }, None, &who()).0
    }

    fn shape(tl: &Timeline) -> Vec<(usize, u64, u64)> {
        tl.segs().iter().map(|s| (s.src, s.start, s.end)).collect()
    }

    #[test]
    fn a_range_resolves_to_source_spans_and_back() {
        let mut p = project(&[1000]);
        cut(&mut p, 200, 300, 0); // source 200..300
        cut(&mut p, 500, 600, 0); // timeline 500 is source 600: removes source 600..700
        let tl = Timeline::build(&p).unwrap();
        assert_eq!(shape(&tl), vec![(0, 0, 200), (0, 300, 600), (0, 700, 1000)]);
        assert_eq!(tl.total(), 800);
        assert_eq!(tl.joins(), &[200, 500]);

        // A range across both joins comes back as three spans, in order.
        let spans = tl.resolve(150, 550);
        assert_eq!(spans, vec![
            SrcSpan { src: 0, start: 150, end: 200 },
            SrcSpan { src: 0, start: 300, end: 600 },
            SrcSpan { src: 0, start: 700, end: 750 },
        ]);
        assert_eq!(spans.iter().map(SrcSpan::frames).sum::<u64>(), 400);
        // And each span maps back to where it sits.
        assert_eq!(tl.locate(0, 150, 200), vec![(150, 200)]);
        assert_eq!(tl.locate(0, 300, 600), vec![(200, 500)]);
        // A source range with hidden audio in it maps to the visible parts, merged across the join.
        assert_eq!(tl.locate(0, 100, 400), vec![(100, 300)]);
        assert_eq!(tl.locate(0, 210, 290), vec![]);
        assert_eq!(tl.position_from(0, 250), Some(200));
        assert_eq!(tl.position_from(0, 650), Some(500));
        assert_eq!(tl.position_from(0, 1000), None);

        // Ranges that end exactly on a join stay on their side of it.
        assert_eq!(tl.resolve(0, 200), vec![SrcSpan { src: 0, start: 0, end: 200 }]);
        assert_eq!(tl.resolve(200, 201), vec![SrcSpan { src: 0, start: 300, end: 301 }]);
        assert_eq!(tl.resolve(799, 900), vec![SrcSpan { src: 0, start: 999, end: 1000 }]);
        assert!(tl.resolve(800, 900).is_empty());

        assert_eq!(tl.resolve_outside(100, 700).iter().map(SrcSpan::frames).sum::<u64>(), 200);
        assert_eq!(tl.anchor(0), Some((0, 0)));
        assert_eq!(tl.anchor(200), Some((0, 300)));
        assert_eq!(tl.anchor(800), Some((0, 1000)));
    }

    #[test]
    fn a_batch_resolves_every_range_against_the_timeline_before_it() {
        // Three cuts given as positions on the starting timeline...
        let ranges = [(100u64, 200u64), (400, 450), (700, 900)];
        let mut batch = project(&[1000]);
        let before = Timeline::build(&batch).unwrap();
        let ops = ranges.iter().map(|&(a, b)| Op::Cut { spans: before.stored(&before.resolve(a, b)), crossfade: 0 }).collect();
        batch.push_edit(Op::Batch { ops }, None, &who());

        // ...match the same cuts made one at a time with positions recomputed after each.
        let mut seq = project(&[1000]);
        let mut shift = 0;
        for &(a, b) in &ranges {
            cut(&mut seq, a - shift, b - shift, 0);
            shift += b - a;
        }
        let batch_tl = Timeline::build(&batch).unwrap();
        assert_eq!(shape(&batch_tl), shape(&Timeline::build(&seq).unwrap()));
        assert_eq!(shape(&batch_tl), vec![(0, 0, 100), (0, 200, 400), (0, 450, 700), (0, 900, 1000)]);
        // The batch is one edit, so one undo takes all of it back.
        assert_eq!(batch.edits.len(), 2);
        batch.undo(1).unwrap();
        assert_eq!(Timeline::build(&batch).unwrap().total(), 1000);
    }

    #[test]
    fn overlapping_ranges_in_a_batch_remove_their_union() {
        let mut p = project(&[1000]);
        let tl = Timeline::build(&p).unwrap();
        let ops = [(100, 300), (250, 400), (390, 395)].iter().map(|&(a, b)| Op::Cut { spans: tl.stored(&tl.resolve(a, b)), crossfade: 0 }).collect();
        p.push_edit(Op::Batch { ops }, None, &who());
        assert_eq!(shape(&Timeline::build(&p).unwrap()), vec![(0, 0, 100), (0, 400, 1000)]);
    }

    #[test]
    fn reverting_an_edit_in_the_middle_leaves_later_edits_and_labels_in_place() {
        let mut p = project(&[1000]);
        let first = cut(&mut p, 100, 200, 0); // source 100..200
        cut(&mut p, 500, 600, 0); // timeline 500 is source 600: source 600..700
        let tl = Timeline::build(&p).unwrap();
        // A label on timeline 700..720, which is source 900..920.
        let span = tl.stored(&tl.resolve(700, 720)).remove(0);
        assert_eq!(span, Span("src1".into(), 900, 920));
        p.add_label(span.clone(), "check".into(), &who());
        let later_edit = p.edits[2].clone();

        p.check_revertable(std::slice::from_ref(&first)).unwrap();
        p.push_edit(Op::Revert { edits: vec![first.clone()] }, None, &who());

        // The first cut's audio is back, the later cut still removes exactly the audio it named,
        // and the stored edit and label are untouched.
        let tl = Timeline::build(&p).unwrap();
        assert_eq!(shape(&tl), vec![(0, 0, 600), (0, 700, 1000)]);
        assert_eq!(p.edits[2], later_edit);
        assert_eq!(p.labels[0].span, span);
        // The label follows its audio: 100 frames later on the timeline than before.
        assert_eq!(tl.locate(0, 900, 920), vec![(800, 820)]);
        assert_eq!(p.states()[1], crate::project::EditState::Reverted("e4".into()));
        assert!(p.check_revertable(&[first]).is_err());

        // Undoing the revert puts the first cut back.
        p.undo(1).unwrap();
        assert_eq!(Timeline::build(&p).unwrap().total(), 800);
        // And reverting the revert does the same without moving head back.
        p.redo(1).unwrap();
        p.push_edit(Op::Revert { edits: vec!["e4".into()] }, None, &who());
        assert_eq!(Timeline::build(&p).unwrap().total(), 800);
    }

    #[test]
    fn a_new_edit_after_undo_discards_the_undone_edits() {
        let mut p = project(&[1000]);
        cut(&mut p, 0, 100, 0);
        cut(&mut p, 0, 100, 0);
        assert_eq!(p.undo(5).unwrap(), vec!["e3".to_string(), "e2".to_string()]);
        assert!(p.undo(1).is_err());
        assert_eq!(p.redo(1).unwrap(), vec!["e2".to_string()]);
        let (id, _) = p.push_edit(Op::Gain { gain_db: 1.0, spans: None, track: None }, None, &who());
        // The id of the discarded edit is not handed out again.
        assert_eq!(id, "e4");
        assert_eq!(p.edits.iter().map(|e: &Edit| e.id.as_str()).collect::<Vec<_>>(), ["e1", "e2", "e4"]);
        assert!(p.redo(1).is_err());
    }

    #[test]
    fn cuts_mark_their_joins_and_trimmed_ends() {
        let mut p = project(&[1000]);
        cut(&mut p, 400, 500, 240);
        let tl = Timeline::build(&p).unwrap();
        assert_eq!((tl.segs()[0].xf_in, tl.segs()[0].xf_out), (0, 240));
        assert_eq!((tl.segs()[1].xf_in, tl.segs()[1].xf_out), (240, 0));
        // Trimming the head gives the new start a fade; a hard cut elsewhere leaves it alone.
        cut(&mut p, 0, 50, 100);
        cut(&mut p, 600, 700, 0);
        let tl = Timeline::build(&p).unwrap();
        let xf: Vec<(u32, u32)> = tl.segs().iter().map(|s| (s.xf_in, s.xf_out)).collect();
        assert_eq!(xf, vec![(100, 240), (240, 0), (0, 0)]);
    }

    #[test]
    fn an_insert_keeps_its_place_when_earlier_cuts_are_reverted() {
        let mut p = project(&[1000, 50]);
        // Start from the first source only.
        let tl = Timeline::build(&p).unwrap();
        let tail = tl.stored(&tl.resolve(1000, 1050));
        p.push_edit(Op::Cut { spans: tail, crossfade: 0 }, None, &who());
        let first = cut(&mut p, 100, 200, 0);
        // Insert the second source's id again as a fresh source at timeline 300 (source frame 400).
        let id = p.next_source_id();
        let mut extra = p.sources[1].clone();
        extra.id = id.clone();
        p.sources.push(extra);
        let tl = Timeline::build(&p).unwrap();
        let (a_src, frame) = tl.anchor(300).unwrap();
        assert_eq!((a_src, frame), (0, 400));
        p.push_edit(Op::Insert { source: id, at: Some(("src1".into(), frame)), crossfade: 0 }, None, &who());
        assert_eq!(shape(&Timeline::build(&p).unwrap()), vec![(0, 0, 100), (0, 200, 400), (2, 0, 50), (0, 400, 1000)]);
        p.push_edit(Op::Revert { edits: vec![first] }, None, &who());
        assert_eq!(shape(&Timeline::build(&p).unwrap()), vec![(0, 0, 400), (2, 0, 50), (0, 400, 1000)]);
    }

    #[test]
    fn level_edits_follow_their_source_frames() {
        let mut p = project(&[1000]);
        let tl = Timeline::build(&p).unwrap();
        p.push_edit(Op::Gain { gain_db: -6.0, spans: Some(tl.stored(&tl.resolve(100, 200))), track: None }, None, &who());
        p.push_edit(Op::Fade { dir: FadeDir::In, spans: tl.stored(&tl.resolve(0, 10)), track: None }, None, &who());
        p.push_edit(Op::Mute { spans: tl.stored(&tl.resolve(500, 600)), crossfade: 4, track: None }, None, &who());
        p.push_edit(Op::Gain { gain_db: 6.0, spans: None, track: None }, None, &who());
        cut(&mut p, 0, 5, 0);
        let tl = Timeline::build(&p).unwrap();
        let up = 10f32.powf(6.0 / 20.0);
        let down = 10f32.powf(-6.0 / 20.0);
        assert!((tl.gain_at(0, 150) - up * down).abs() < 1e-6);
        assert!((tl.gain_at(0, 300) - up).abs() < 1e-6);
        assert!((tl.gain_at(0, 5) - up * 0.55).abs() < 1e-6);
        assert_eq!(tl.gain_at(0, 550), 0.0);
        assert!((tl.gain_at(0, 500) - up * 0.75).abs() < 1e-6);
        assert!((tl.gain_at(0, 599) - up * 0.75).abs() < 1e-6);
        // A block of gains equals the per-frame answer however it is chunked.
        let mut whole = vec![1.0f32; 1000];
        tl.apply_gain(0, 0, &mut whole, 1);
        let mut parts = vec![1.0f32; 1000];
        for (i, chunk) in parts.chunks_mut(37).enumerate() {
            tl.apply_gain(0, i as u64 * 37, chunk, 1);
        }
        assert_eq!(whole, parts);
        for f in [0u64, 5, 150, 500, 550, 599, 999] {
            assert_eq!(whole[f as usize], tl.gain_at(0, f));
        }
    }
}
