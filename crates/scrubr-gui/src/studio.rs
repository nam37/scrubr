//! The project view: a first look at scrubr's window on rune-kit.
//!
//! `scrubr-gui <project.scrubr>` opens a project and draws its timeline at head: an overview of
//! the whole recording, a ruler, a two-tone waveform (peaks around a loudness body), a mark at
//! every splice in its author's colour with a speech bubble for every label beside them, and
//! level edits along the foot of the lanes. Beside it is
//! the activity log: every edit, by the person or an agent, with undo for one edit or a whole
//! session. Agents work from a shell; the window has no chat. It reloads when the project file
//! changes on disk. Every command is in one list, which the main menu, the tool buttons and the
//! keyboard all read. Playback renders the timeline with its edits applied. Open and Export use
//! the system's file dialogs; an export runs on its own thread and reports how far it has got.
//! With no project named, the window starts empty and offers a new project or an existing one.
//! A new project opens with the Files tab showing, and is made when its first file is added.

use std::f32::consts::FRAC_PI_2;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rune::{Align, App, AppSpec, Chrome, Color, Cx, El, FileDialogOptions, FileDialogResult, Icon, Key, KeyEvent, LineCap, Live, LiveText, Phase, Pixels, ScrollSpec, Shape, TextEdit, TextStyle, Tx, canvas, col, el, icon, memo, oklch, row, text};
use rune_kit::shell::{self, PaneState, SplitView, Toolbar};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Menu, MenuEntry, MenuItem, PaletteSeed, SegmentedControl, TextInput};
use scrubr_core::edit::{self, DEFAULT_CROSSFADE_MS, Request, Revert};
use scrubr_core::project::{self, EditState, FadeDir, Op, Paths, Project, Who, WriteOpts};
use scrubr_core::render::Renderer;
use scrubr_core::timeline::{SrcSpan, TrackState};
use scrubr_core::transcribe::Word;
use scrubr_core::{Kind, Peaks, Timeline, export, peaks, phrase, source, spectrum, words};

use crate::audio::Player;

// ---------------------------------------------------------------- icons

macro_rules! stroke_icon {
    ($name:ident, $id:literal, $body:literal) => {
        static $name: Icon = Icon { name: $id, view: 16.0, body: concat!(r#"<g stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none">"#, $body, "</g>") };
    };
}

static PLAY: Icon = Icon { name: "scrubr-play", view: 16.0, body: r#"<path d="M5.2 3.2L12.6 8L5.2 12.8Z" fill="currentColor"/>"# };
static PAUSE: Icon = Icon { name: "scrubr-pause", view: 16.0, body: r#"<path d="M4.5 3.2H7V12.8H4.5ZM9 3.2H11.5V12.8H9Z" fill="currentColor"/>"# };
static LOGO: Icon = Icon {
    name: "scrubr-logo",
    view: 16.0,
    body: r#"<g stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M2 7V9M5 4.5V11.5M8 2V14M11 5V11M14 7V9"/></g>"#,
};
stroke_icon!(MENU, "scrubr-menu", r#"<path d="M2.5 4.5H13.5M2.5 8H13.5M2.5 11.5H13.5"/>"#);
stroke_icon!(UNDO, "scrubr-undo", r#"<path d="M6 4L3 7L6 10"/><path d="M3.5 7H9.5A3 3 0 0 1 9.5 13H7"/>"#);
stroke_icon!(REDO, "scrubr-redo", r#"<path d="M10 4L13 7L10 10"/><path d="M12.5 7H6.5A3 3 0 0 0 6.5 13H9"/>"#);
stroke_icon!(EXPORT, "scrubr-export", r#"<path d="M8 10V2.5M5 5L8 2.5L11 5"/><path d="M3 9.5V12.5A1 1 0 0 0 4 13.5H12A1 1 0 0 0 13 12.5V9.5"/>"#);
stroke_icon!(PERSON, "scrubr-person", r#"<circle cx="8" cy="5.5" r="2.5"/><path d="M3 13.5C3.6 11 5.6 9.8 8 9.8S12.4 11 13 13.5"/>"#);
stroke_icon!(AGENT, "scrubr-agent", r#"<path d="M8 2.2L9.5 6.5L13.8 8L9.5 9.5L8 13.8L6.5 9.5L2.2 8L6.5 6.5Z"/>"#);
stroke_icon!(SCISSORS, "scrubr-scissors", r#"<circle cx="4" cy="4.5" r="1.8"/><circle cx="4" cy="11.5" r="1.8"/><path d="M5.5 5.6L13.5 11.5M5.5 10.4L13.5 4.5"/>"#);
stroke_icon!(MUTE, "scrubr-mute", r#"<path d="M2.5 6.2H4.8L8 3.5V12.5L4.8 9.8H2.5Z"/><path d="M10.5 6.2L13.5 9.8M13.5 6.2L10.5 9.8"/>"#);
stroke_icon!(FADE, "scrubr-fade", r#"<path d="M2.5 12.5H13.5M2.5 12.5L13.5 4"/>"#);
stroke_icon!(LEVEL, "scrubr-level", r#"<path d="M4 13V8M8 13V3.5M12 13V6"/>"#);
stroke_icon!(KEEP, "scrubr-keep", r#"<path d="M4.5 2V11.5H14M2 4.5H11.5V14"/>"#);
stroke_icon!(IMPORT, "scrubr-import", r#"<path d="M4 2.5H9.5L12 5V13.5H4Z"/><path d="M8 6.5V11M6.2 9.3L8 11L9.8 9.3"/>"#);
stroke_icon!(LAYERS, "scrubr-layers", r#"<path d="M8 2.5L13.5 5.5L8 8.5L2.5 5.5Z"/><path d="M2.5 8.5L8 11.5L13.5 8.5"/>"#);
stroke_icon!(INSERT, "scrubr-insert", r#"<path d="M8 3.5V12.5M3.5 8H12.5"/>"#);
stroke_icon!(PREV, "scrubr-prev", r#"<path d="M4 3.5V12.5"/><path d="M12 3.8L6.5 8L12 12.2Z" fill="currentColor"/>"#);
stroke_icon!(NEXT, "scrubr-next", r#"<path d="M12 3.5V12.5"/><path d="M4 3.8L9.5 8L4 12.2Z" fill="currentColor"/>"#);
stroke_icon!(ZOOM_IN, "scrubr-zoom-in", r#"<circle cx="7" cy="7" r="4"/><path d="M10 10L13.2 13.2M5.3 7H8.7M7 5.3V8.7"/>"#);
stroke_icon!(ZOOM_OUT, "scrubr-zoom-out", r#"<circle cx="7" cy="7" r="4"/><path d="M10 10L13.2 13.2M5.3 7H8.7"/>"#);
stroke_icon!(BUBBLE, "scrubr-bubble", r#"<path d="M3.5 3H12.5A1.5 1.5 0 0 1 14 4.5V9.5A1.5 1.5 0 0 1 12.5 11H8L5 13.5V11H3.5A1.5 1.5 0 0 1 2 9.5V4.5A1.5 1.5 0 0 1 3.5 3Z"/>"#);
static STOP: Icon = Icon { name: "scrubr-stop", view: 16.0, body: r#"<path d="M4.6 4.6H11.4V11.4H4.6Z" fill="currentColor"/>"# };
stroke_icon!(FOLDER, "scrubr-folder", r#"<path d="M2 4.5A1 1 0 0 1 3 3.5H6.2L7.6 5H13A1 1 0 0 1 14 6V11.5A1 1 0 0 1 13 12.5H3A1 1 0 0 1 2 11.5Z"/>"#);
stroke_icon!(UP, "scrubr-up", r#"<path d="M8 12.5V3.5M4.5 7L8 3.5L11.5 7"/>"#);
stroke_icon!(CROSS, "scrubr-cross", r#"<path d="M4.5 4.5L11.5 11.5M11.5 4.5L4.5 11.5"/>"#);
stroke_icon!(FIT, "scrubr-fit", r#"<path d="M2.5 6V3.5H5M11 3.5H13.5V6M13.5 10V12.5H11M5 12.5H2.5V10"/>"#);

const OVERVIEW_H: f32 = 46.0;
const RULER_H: f32 = 26.0;
/// The strip at the foot of the lanes where level edits are marked. It lies over the lanes'
/// own bottom margin, as the splice marks and labels lie over the top one.
const MARKS_H: f32 = 16.0;
const TRANSPORT_H: f32 = 68.0;
/// Every word is written out when no more than this many seconds fill the timeline's width:
/// speech runs at about three words a second, and a word needs some forty points.
const WORDS_SECONDS: f64 = 12.0;
/// Out to this many seconds, the word each stretch of speech starts with is written, to find a
/// place by. Further out thin marks show where there is speech.
const STARTS_SECONDS: f64 = 90.0;
/// The pause that separates one stretch of speech from the next, in seconds.
const PAUSE_SECONDS: f64 = 0.35;
/// The deepest zoom, in frames per point: one sample every 24 points.
const MIN_FPP: f64 = 1.0 / 24.0;
/// At this many frames per point or fewer, the lanes draw a line through the samples in place
/// of a column for each point.
const LINE_FPP: f64 = 3.0;
/// At this many frames per point or fewer, each sample gets a dot on the line.
const DOT_FPP: f64 = 0.2;

// ---------------------------------------------------------------- the look

/// The kit's tokens plus the colours that are scrubr's own.
struct Look {
    k: Kit,
    /// The waveform's loudness body and, fainter, its peaks.
    ink: Color,
    ink_soft: Color,
    /// One colour per kind of author: the person, and agents.
    you: Color,
    agent: Color,
    /// Text and icons on a fill of the person's colour.
    on_you: Color,
    /// The timeline card.
    lane: Color,
    rule: Color,
    /// The window's own background, for covers drawn over it.
    page: Color,
    /// The level meter from quiet to full scale: safe, getting loud, loud, clipping.
    meter: [Color; 4],
}

fn look(dark: bool) -> Look {
    if dark {
        let you = oklch(0.80, 0.14, 75.0);
        let page = oklch(0.195, 0.006, 70.0);
        let k = Kit::from_palette(PaletteSeed { bg: page, fg: oklch(0.94, 0.012, 85.0), accent: you });
        Look {
            page,
            meter: [oklch(0.80, 0.17, 152.0), oklch(0.88, 0.17, 102.0), oklch(0.79, 0.17, 62.0), oklch(0.66, 0.22, 27.0)],
            ink: oklch(0.90, 0.03, 85.0),
            ink_soft: oklch(0.90, 0.03, 85.0).alpha(0.30),
            you,
            agent: oklch(0.80, 0.11, 195.0),
            on_you: oklch(0.20, 0.03, 70.0),
            lane: oklch(0.155, 0.006, 70.0),
            rule: k.c.text_base.alpha(0.08),
            k,
        }
    } else {
        let you = oklch(0.64, 0.155, 60.0);
        let page = oklch(0.962, 0.006, 80.0);
        let k = Kit::from_palette(PaletteSeed { bg: page, fg: oklch(0.24, 0.012, 70.0), accent: you });
        Look {
            page,
            meter: [oklch(0.70, 0.17, 152.0), oklch(0.82, 0.17, 98.0), oklch(0.72, 0.18, 58.0), oklch(0.60, 0.22, 27.0)],
            ink: oklch(0.30, 0.02, 70.0),
            ink_soft: oklch(0.30, 0.02, 70.0).alpha(0.28),
            you,
            agent: oklch(0.56, 0.10, 200.0),
            on_you: Color::WHITE,
            lane: oklch(0.992, 0.003, 85.0),
            rule: k.c.text_base.alpha(0.09),
            k,
        }
    }
}

impl Look {
    fn author(&self, agent: bool) -> Color {
        if agent { self.agent } else { self.you }
    }

    /// A style for times and counts: tabular figures, so digits do not shift as they change.
    fn num(&self, base: TextStyle) -> TextStyle {
        TextStyle { features: "'tnum' 1", ..base }
    }
}

/// Edits made in the window or from an unnamed shell are the person's; any other author is an agent.
fn is_agent(author: &str) -> bool {
    !matches!(author, "you" | "cli")
}

// ---------------------------------------------------------------- the document

/// Where two pieces of audio meet on the timeline.
struct Splice {
    at: u64,
    /// The edits that made the join, latest first: more than one where cuts meet.
    edits: Vec<usize>,
    agent: bool,
    /// The audio taken out here: source index and frame range.
    removed: Option<(usize, u64, u64)>,
}

#[derive(Clone, Copy, PartialEq)]
enum LevelKind {
    Gain,
    FadeIn,
    FadeOut,
    Mute,
}

/// A level edit as it sits on the timeline now.
struct LevelMark {
    start: u64,
    end: u64,
    kind: LevelKind,
    agent: bool,
    edit: usize,
}

struct Note {
    /// The label's id in the project, for removing it.
    id: String,
    start: u64,
    end: u64,
    text: String,
    author: String,
    agent: bool,
    hidden: bool,
}

/// Consecutive edits by one author in one session.
struct Group {
    session: Option<String>,
    agent: bool,
    edits: Vec<usize>,
    /// The session has applied edits that a session undo would take out.
    revertable: bool,
}

/// What was said on the mix or on one track, where it is on the timeline now.
struct Said {
    /// The track's place among the timeline's tracks, or `None` for the mix.
    track: Option<usize>,
    words: Vec<Word>,
}

/// The transcript kept for a project, on its timeline as it stands.
fn said_of(paths: &Paths, tl: &Timeline) -> Vec<Said> {
    let store = words::load(paths);
    store
        .sets
        .iter()
        .filter_map(|set| {
            let track = match &set.track {
                None => None,
                Some(id) => Some(tl.tracks().iter().position(|t| t.id == *id && !t.removed)?),
            };
            Some(Said { track, words: words::on_timeline(set, tl) })
        })
        .collect()
}

fn words_stamp(paths: &Paths) -> Option<SystemTime> {
    std::fs::metadata(words::file(paths)).and_then(|m| m.modified()).ok()
}

struct Doc {
    project: Project,
    tl: Timeline,
    peaks: Vec<Option<Peaks>>,
    states: Vec<EditState>,
    splices: Vec<Splice>,
    levels: Vec<LevelMark>,
    notes: Vec<Note>,
    groups: Vec<Group>,
    /// What was said, if anything has listened: the mix, and any track heard alone.
    said: Vec<Said>,
    /// The loudest sample on the timeline, 0.0 to 1.0, after the level edits.
    peak: f32,
}

fn load(paths: &Paths) -> Result<Doc, String> {
    let project = project::open(paths).map_err(|e| e.to_string())?;
    let tl = Timeline::build(&project).map_err(|e| e.to_string())?;
    let mut peaks: Vec<Option<Peaks>> = project.sources.iter().map(|_| None).collect();
    // The main track's sources, and the files other tracks have laid over them.
    for seg in tl.segs() {
        for src in std::iter::once(seg.src).chain(tl.attached(seg.src).iter().map(|a| a.clip)) {
            if peaks[src].is_none() {
                peaks[src] = Some(source::peaks(paths, &project.sources[src], project.sample_rate).map_err(|e| e.to_string())?);
            }
        }
    }
    let states = project.states();
    let src_of = |id: &str| project.source_index(id);

    // What each applied edit did, in source coordinates.
    let mut cuts: Vec<(usize, u64, u64, usize)> = Vec::new();
    let mut level_spans: Vec<(usize, u64, u64, LevelKind, usize)> = Vec::new();
    let mut inserted: Vec<Option<usize>> = vec![None; project.sources.len()];
    fn walk(
        op: &Op,
        i: usize,
        src_of: &dyn Fn(&str) -> Option<usize>,
        cuts: &mut Vec<(usize, u64, u64, usize)>,
        levels: &mut Vec<(usize, u64, u64, LevelKind, usize)>,
        inserted: &mut [Option<usize>],
    ) {
        let mut spans = |spans: &[project::Span], kind: Option<LevelKind>| {
            for s in spans {
                let Some(src) = src_of(&s.0) else { continue };
                match kind {
                    None => cuts.push((src, s.1, s.2, i)),
                    Some(kind) => levels.push((src, s.1, s.2, kind, i)),
                }
            }
        };
        match op {
            Op::Cut { spans: s, .. } | Op::Keep { spans: s, .. } => spans(s, None),
            Op::Gain { spans: Some(s), .. } => spans(s, Some(LevelKind::Gain)),
            Op::Mute { spans: s, .. } => spans(s, Some(LevelKind::Mute)),
            Op::Fade { dir, spans: s, .. } => spans(s, Some(if *dir == FadeDir::In { LevelKind::FadeIn } else { LevelKind::FadeOut })),
            Op::Insert { source, .. } => {
                if let Some(src) = src_of(source) {
                    inserted[src] = Some(i);
                }
            }
            Op::Batch { ops } => ops.iter().for_each(|op| walk(op, i, src_of, cuts, levels, inserted)),
            _ => {}
        }
    }
    for (i, e) in project.edits.iter().enumerate() {
        if states[i] == EditState::Applied {
            walk(&e.op, i, &src_of, &mut cuts, &mut level_spans, &mut inserted);
        }
    }

    let segs = tl.segs();
    let mut splices = Vec::new();
    for i in 1..segs.len() {
        let (a, b) = (segs[i - 1], segs[i]);
        let (mut edits, removed) = if a.src == b.src && b.start >= a.end {
            // Every cut that took out some of the audio between the two pieces.
            let edits: Vec<usize> = cuts.iter().filter(|c| c.0 == a.src && c.1 < b.start && c.2 > a.end).map(|c| c.3).collect();
            (edits, Some((a.src, a.end, b.start)))
        } else {
            (inserted[b.src].or(inserted[a.src]).into_iter().collect(), None)
        };
        edits.sort_unstable_by(|x, y| y.cmp(x));
        edits.dedup();
        // The mark takes the colour of the latest edit.
        let agent = edits.first().is_some_and(|&i| is_agent(&project.edits[i].author));
        splices.push(Splice { at: tl.seg_start(i), edits, agent, removed });
    }

    let mut levels = Vec::new();
    for (src, start, end, kind, edit) in level_spans {
        for (t0, t1) in tl.locate(src, start, end) {
            levels.push(LevelMark { start: t0, end: t1, kind, agent: is_agent(&project.edits[edit].author), edit });
        }
    }

    let notes = project
        .labels
        .iter()
        .filter_map(|l| {
            let src = project.source_index(&l.span.0)?;
            let seen = tl.locate(src, l.span.1, l.span.2.max(l.span.1 + 1));
            let (start, end, hidden) = match (seen.first(), seen.last()) {
                (Some(first), Some(last)) => (first.0, if l.span.1 == l.span.2 { first.0 } else { last.1 }, false),
                _ => {
                    let at = tl.position_from(src, l.span.1).unwrap_or(tl.total());
                    (at, at, true)
                }
            };
            Some(Note { id: l.id.clone(), start, end, text: l.text.clone(), author: l.author.clone(), agent: is_agent(&l.author), hidden })
        })
        .collect();

    let mut groups: Vec<Group> = Vec::new();
    let mut last: Option<(&str, &Option<String>)> = None;
    for (i, e) in project.edits.iter().enumerate() {
        let revertable = i > 0 && states[i] == EditState::Applied && !matches!(e.op, Op::Revert { .. }) && e.session.is_some();
        match groups.last_mut() {
            Some(g) if last == Some((e.author.as_str(), &e.session)) => {
                g.edits.push(i);
                g.revertable |= revertable;
            }
            _ => groups.push(Group { session: e.session.clone(), agent: is_agent(&e.author), edits: vec![i], revertable }),
        }
        last = Some((e.author.as_str(), &e.session));
    }
    let said = said_of(paths, &tl);
    let mut doc = Doc { project, tl, peaks, states, splices, levels, notes, groups, said, peak: 1.0 };
    let total = doc.tl.total().max(1) as f64;
    doc.peak = (0..doc.project.channels.min(2) as usize)
        .flat_map(|c| doc.columns(c, 0.0, total / 2048.0, 2048))
        .fold(0f32, |m, c| if c.any { m.max(c.max).max(-c.min) } else { m });
    Ok(doc)
}

/// One column of a waveform: its peaks and its loudness, after the level edits.
#[derive(Clone, Copy, Default)]
struct Column {
    min: f32,
    max: f32,
    rms: f32,
    any: bool,
}

/// What one lane of the timeline shows: a channel of the audio when the project has one track,
/// or a whole track when it has several.
#[derive(Clone, Copy, PartialEq)]
enum Lane {
    Channel(usize),
    Track(usize),
}

impl Doc {
    fn rate(&self) -> u32 {
        self.project.sample_rate
    }

    /// The words to draw in a lane. A track shows what was heard on it alone; the main track
    /// shows the mix when nothing listened to it by itself. A project with one track has one
    /// strip of words, in its last lane.
    fn said_for(&self, lane: Lane, last: bool) -> Option<&Said> {
        let of = |track: Option<usize>| self.said.iter().find(|s| s.track == track && !s.words.is_empty());
        match lane {
            Lane::Track(0) => of(Some(0)).or_else(|| of(None)),
            Lane::Track(t) => of(Some(t)),
            Lane::Channel(_) if last => of(None).or_else(|| of(Some(0))),
            Lane::Channel(_) => None,
        }
    }

    /// The lanes, top to bottom.
    fn lanes(&self) -> Vec<Lane> {
        let live: Vec<usize> = self.tl.tracks().iter().enumerate().filter(|(_, t)| !t.removed).map(|(i, _)| i).collect();
        if live.len() > 1 { live.into_iter().map(Lane::Track).collect() } else { (0..self.project.channels.clamp(1, 2) as usize).map(Lane::Channel).collect() }
    }

    /// `cols` columns of `fpc` timeline frames each, from `start`, for one track with its
    /// channels folded together. A column the track has no audio in is left empty, so a lane
    /// shows where its clips are.
    fn track_columns(&self, track: usize, start: f64, fpc: f64, cols: usize) -> Vec<Column> {
        let segs = self.tl.segs();
        let total = self.tl.total();
        let mut out = vec![Column::default(); cols];
        let mut si = 0;
        for (i, column) in out.iter_mut().enumerate() {
            let t0 = (start + i as f64 * fpc).floor().max(0.0) as u64;
            let t1 = ((start + (i + 1) as f64 * fpc).ceil().max(0.0) as u64).max(t0 + 1).min(total);
            if t0 >= total {
                break;
            }
            while si < segs.len() && self.tl.seg_start(si + 1) <= t0 {
                si += 1;
            }
            let (mut min, mut max, mut squares, mut frames) = (f32::INFINITY, f32::NEG_INFINITY, 0f64, 0u64);
            let mut j = si;
            while j < segs.len() && self.tl.seg_start(j) < t1 {
                let seg = segs[j];
                let seg_t = self.tl.seg_start(j);
                let from = seg.start + t0.saturating_sub(seg_t);
                let to = seg.start + (t1 - seg_t).min(seg.frames());
                j += 1;
                if from >= to {
                    continue;
                }
                // The audio this track has at main frames `from..to`: the file it is in, where
                // it starts there, how long it is, and the main frame the level is read at.
                let mut pieces: Vec<(usize, u64, u64, u64)> = Vec::new();
                if track == 0 {
                    pieces.push((seg.src, from, to - from, from + (to - from) / 2));
                } else {
                    for a in self.tl.attached(seg.src).iter().filter(|a| a.track == track && a.start < to && a.end > from) {
                        let (lo, hi) = (a.start.max(from), a.end.min(to));
                        pieces.push((a.clip, a.clip_start + (lo - a.start), hi - lo, lo + (hi - lo) / 2));
                    }
                }
                for (audio, first, len, at) in pieces {
                    let Some(p) = &self.peaks[audio] else { continue };
                    let gain = self.tl.gain_for(track, seg.src, at);
                    for ch in 0..p.channels() {
                        let m = p.range(ch, first, first + len);
                        let rms = p.rms(ch, first, first + len) * gain.abs();
                        min = min.min(m.min * gain);
                        max = max.max(m.max * gain);
                        squares += (rms as f64).powi(2) * len as f64 / p.channels() as f64;
                    }
                    frames += len;
                }
            }
            if frames > 0 {
                *column = Column { min, max, rms: (squares / frames as f64).sqrt() as f32, any: true };
            }
        }
        out
    }

    /// How much taller than true scale the lanes draw, so a quiet recording fills them. The
    /// peaks land at nine tenths of a lane; a recording that already does is left alone.
    fn magnify(&self) -> f32 {
        if self.peak <= 0.0 { 1.0 } else { (0.9 / self.peak).clamp(1.0, 8.0) }
    }

    /// `cols` columns of `fpc` timeline frames each, from `start`, for one channel.
    fn columns(&self, channel: usize, start: f64, fpc: f64, cols: usize) -> Vec<Column> {
        let segs = self.tl.segs();
        let total = self.tl.total();
        let mut out = vec![Column::default(); cols];
        let mut si = 0;
        for (i, column) in out.iter_mut().enumerate() {
            let t0 = (start + i as f64 * fpc).floor().max(0.0) as u64;
            let t1 = ((start + (i + 1) as f64 * fpc).ceil().max(0.0) as u64).max(t0 + 1).min(total);
            if t0 >= total {
                break;
            }
            while si < segs.len() && self.tl.seg_start(si + 1) <= t0 {
                si += 1;
            }
            let (mut min, mut max, mut squares, mut frames) = (f32::INFINITY, f32::NEG_INFINITY, 0f64, 0u64);
            let mut j = si;
            while j < segs.len() && self.tl.seg_start(j) < t1 {
                let seg = segs[j];
                let seg_t = self.tl.seg_start(j);
                let from = seg.start + t0.saturating_sub(seg_t);
                let to = seg.start + (t1 - seg_t).min(seg.frames());
                if let (true, Some(p)) = (from < to, &self.peaks[seg.src]) {
                    let ch = channel.min(p.channels() - 1);
                    let gain = self.tl.gain_at(seg.src, from + (to - from) / 2);
                    let m = p.range(ch, from, to);
                    let rms = p.rms(ch, from, to) * gain.abs();
                    min = min.min(m.min * gain);
                    max = max.max(m.max * gain);
                    squares += (rms as f64).powi(2) * (to - from) as f64;
                    frames += to - from;
                }
                j += 1;
            }
            if frames > 0 {
                *column = Column { min, max, rms: (squares / frames as f64).sqrt() as f32, any: true };
            }
        }
        out
    }
}

// ---------------------------------------------------------------- state

/// Something a command wants that only a handler holding the window's `Tx` can start.
#[derive(Clone, Copy)]
enum Ask {
    Open,
    Export,
}

/// Work the command line program is doing for the window: transcribing, cutting by words,
/// tightening pauses, measuring. The window carries no speech model; `scrubr`, beside it, does,
/// and what it measures is what an agent measures.
struct Job {
    /// What is being done, for the notice: "Transcribing", "Cutting".
    what: String,
    /// The last line of progress the program printed.
    said: Arc<Mutex<String>>,
    /// What to tell the person when it is done, with the program's reply, or why it failed.
    result: mpsc::Receiver<Result<(String, serde_json::Value), String>>,
    shown: String,
    /// Something for the window to do with the reply, such as going to what was found.
    after: Option<fn(&mut Studio, &serde_json::Value)>,
}

/// What the lanes show when they show a spectrogram: one picture of the mix, measured on a
/// thread of its own each time the view comes to rest somewhere new.
#[derive(Clone, Debug, PartialEq)]
struct SpecView {
    generation: u64,
    /// Timeline frames.
    start: u64,
    end: u64,
    columns: u32,
    rows: u32,
}

type SpecAnswer = Option<(f64, u32, u32, Vec<u8>)>;

#[derive(Default)]
struct Spectro {
    on: bool,
    /// `--spectrogram`: measure before the first frame, for stills.
    at_once: bool,
    pixels: Pixels,
    /// What the picture shows, and the frequency at its top edge.
    shown: Option<SpecView>,
    top_hz: f64,
    /// What is being measured.
    asked: Option<(SpecView, mpsc::Receiver<SpecAnswer>)>,
}

/// The lowest frequency the window's spectrogram shows, and the levels drawn black and brightest.
const SPEC_MIN_HZ: f64 = 40.0;
const SPEC_DB: (f64, f64) = (-100.0, -10.0);
/// The strip at the top of the lanes that the spectrogram leaves clear, where splices are marked.
const SPEC_TOP: f32 = 24.0;

/// The stretch of the timeline in view, as a spectrogram to measure.
fn spec_wanted(s: &Studio) -> Option<SpecView> {
    let doc = s.doc.as_ref()?;
    let [w, h] = s.wave;
    let h = h - SPEC_TOP;
    if !s.spectro.on || w < 8.0 || h < 8.0 {
        return None;
    }
    let total = doc.tl.total();
    let start = (s.start.max(0.0) as u64).min(total);
    let end = ((s.start + w as f64 * s.fpp).ceil().max(0.0) as u64).min(total);
    (end > start).then(|| SpecView { generation: s.generation, start, end, columns: (((end - start) as f64 / s.fpp).round() as u32).clamp(1, 2400), rows: (h as u32).clamp(16, 900) })
}

fn measure_spec(project: &Project, paths: &Paths, view: &SpecView) -> SpecAnswer {
    let grid = spectrum::measure(project, paths, None, Some((view.start, view.end)), view.columns as usize, view.rows as usize, SPEC_MIN_HZ, None).ok()?;
    let (w, h, bytes) = spectrum::rgba(&grid, SPEC_DB.0, SPEC_DB.1);
    Some((grid.max_hz, w, h, bytes))
}

/// Keeps the spectrogram in step with the view. True when the picture changed.
fn watch_spectrogram(s: &mut Studio) -> bool {
    let mut changed = false;
    if let Some((view, answer)) = &s.spectro.asked {
        match answer.try_recv() {
            Ok(Some((top_hz, w, h, bytes))) => {
                s.spectro.pixels.set(w, h, bytes);
                s.spectro.top_hz = top_hz;
                s.spectro.shown = Some(view.clone());
                s.spectro.asked = None;
                changed = true;
            }
            Ok(None) | Err(mpsc::TryRecvError::Disconnected) => s.spectro.asked = None,
            Err(mpsc::TryRecvError::Empty) => return false,
        }
    }
    let Some(wanted) = spec_wanted(s) else { return changed };
    if s.spectro.shown.as_ref() == Some(&wanted) {
        return changed;
    }
    let Some(doc) = &s.doc else { return changed };
    let (project, paths, view) = (doc.project.clone(), s.paths.clone(), wanted.clone());
    if s.spectro.at_once {
        s.spectro.at_once = false;
        if let Some((top_hz, w, h, bytes)) = measure_spec(&project, &paths, &view) {
            s.spectro.pixels.set(w, h, bytes);
            s.spectro.top_hz = top_hz;
            s.spectro.shown = Some(view);
        }
        return true;
    }
    let (send, answer) = mpsc::channel();
    if std::thread::Builder::new().name("scrubr-spectrogram".into()).spawn(move || drop(send.send(measure_spec(&project, &paths, &view)))).is_ok() {
        s.spectro.asked = Some((wanted, answer));
    }
    changed
}

/// The spectrogram over the lanes: the picture last measured, put where its audio is now, so
/// it moves with the view while a fresh one is on its way.
fn spectrogram_sheet(s: &Studio, k: &Kit, w: f32, h: f32) -> El {
    let night = oklch(0.06, 0.0, 0.0);
    let h = h - SPEC_TOP;
    let mut sheet = el().key("spectrogram").absolute(0.0, SPEC_TOP).size(w, h).clip_rounded(0.0).fill(night);
    let Some(shown) = s.spectro.shown.as_ref().filter(|shown| shown.generation == s.generation) else {
        return sheet.child(row().key("spectrogram-wait").absolute(0.0, 0.0).size(w, h).center().child(text("Measuring\u{2026}").font(k.ty.small).color(oklch(0.75, 0.0, 0.0))));
    };
    let x = ((shown.start as f64 - s.start) / s.fpp) as f32;
    let wide = ((shown.end - shown.start) as f64 / s.fpp) as f32;
    sheet = sheet.child(el().key("spectrogram-picture").absolute(x, 0.0).size(wide, h).pixels(&s.spectro.pixels));
    // A few frequencies named at the left, where they fall on the logarithmic scale.
    for (hz, name) in [(100.0, "100 Hz"), (1000.0, "1 kHz"), (5000.0, "5 kHz")] {
        let y = ((hz / s.spectro.top_hz).ln() / (SPEC_MIN_HZ / s.spectro.top_hz).ln()) as f32 * (h - 1.0);
        if hz < s.spectro.top_hz && y > 44.0 && y < h - 44.0 {
            sheet = sheet
                .child(el().key(format!("hz-line-{hz}")).absolute(0.0, y).size(6.0, 1.0).fill(oklch(0.85, 0.0, 0.0)))
                .child(row().key(format!("hz-{hz}")).absolute(9.0, y - 8.0).h(16.0).px(4.0).center_y().radius(4.0).fill(night.alpha(0.65)).child(text(name).font(k.ty.small).color(oklch(0.88, 0.0, 0.0))));
        }
    }
    sheet
}

/// A project being made from an audio file, on its own thread: the file is hashed, converted if
/// it has to be, and its waveform is drawn up.
struct Creating {
    /// The audio file's name, for telling the person what is going on.
    name: String,
    /// The new project's path, or why it could not be made.
    result: mpsc::Receiver<Result<PathBuf, String>>,
}

/// What a file being added does to the project, at a place on the timeline.
type AddChange = Arc<dyn Fn(&Paths, &WriteOpts, (u64, u64)) -> scrubr_core::Result<()> + Send + Sync>;

/// A file being added to the project on its own thread. A long or compressed file is hashed and
/// converted first, and that would stall the window.
struct Adding {
    /// The file's name, for telling the person what is going on.
    name: String,
    /// What the change is called when the person is told how it went: "insert", "new track".
    what: &'static str,
    /// The place it was asked for, in terms of the audio, to find again if the project changes.
    kept: Place,
    range: (u64, u64),
    change: AddChange,
    /// On a second try: who wrote the project first.
    after: Option<String>,
    /// How it went. An error says whether the project had been written in the meantime.
    result: mpsc::Receiver<Result<(), (bool, String)>>,
}

/// One row of the Files tab: a folder, or an audio file scrubr can read.
struct Entry {
    name: String,
    path: PathBuf,
    folder: bool,
    bytes: u64,
}

/// The Files tab: a folder on disk, what is in it, and the file being listened to.
struct Browser {
    dir: PathBuf,
    entries: Vec<Entry>,
    /// Why the folder could not be read.
    error: Option<String>,
    /// The file being played before it is added, and its player.
    playing: Option<(PathBuf, Player)>,
}

/// The endings of the files the Files tab lists: what `scrubr new` and `insert` read.
const AUDIO_ENDINGS: [&str; 8] = ["wav", "wave", "flac", "mp3", "ogg", "m4a", "mp4", "aac"];

/// The folders and audio files in `dir`: folders first, each in name order. Names that start
/// with a dot and projects' cache folders are left out.
fn list_folder(dir: &std::path::Path) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = std::fs::read_dir(dir)
        .map_err(|e| format!("This folder could not be read: {e}"))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().ok()?;
            let audio = path.extension().and_then(|e| e.to_str()).is_some_and(|e| AUDIO_ENDINGS.contains(&e.to_ascii_lowercase().as_str()));
            // A project's own cache folder is scrubr's business, not something to browse.
            let hidden = name.starts_with('.') || name.ends_with(".scrubr.cache");
            (!hidden && (meta.is_dir() || (meta.is_file() && audio))).then(|| Entry { name, path, folder: meta.is_dir(), bytes: meta.len() })
        })
        .collect();
    entries.sort_by(|a, b| b.folder.cmp(&a.folder).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries)
}

/// Shows a folder in the Files tab. Whatever was being listened to stops.
fn browse(s: &mut Studio, dir: PathBuf) {
    s.files.playing = None;
    match list_folder(&dir) {
        Ok(entries) => (s.files.entries, s.files.error) = (entries, None),
        Err(e) => (s.files.entries, s.files.error) = (Vec::new(), Some(e)),
    }
    s.files.dir = dir;
}

/// An export running on its own thread.
struct Exporting {
    name: String,
    frames: u64,
    /// Frames written so far, set by the export.
    done: Arc<AtomicU64>,
    result: mpsc::Receiver<Result<export::Report, String>>,
    /// The percentage last shown, so the view is rebuilt only when it changes.
    shown: u32,
}

pub struct Studio {
    paths: Paths,
    /// True while no project is open: the window started without one.
    blank: bool,
    /// True for a new project that has no audio yet. Nothing is on disk until the first file is
    /// added from the Files tab, because a project takes its rate and channels from that file.
    fresh: bool,
    ask: Option<Ask>,
    exporting: Option<Exporting>,
    creating: Option<Creating>,
    adding: Option<Adding>,
    job: Option<Job>,
    spectro: Spectro,
    /// The transcript file's modified time when it was last read.
    words_stamp: Option<SystemTime>,
    /// The card for finding words is open.
    finding: bool,
    find_text: TextEdit,
    /// Where the words looked for were found, and which of those is selected.
    found: Vec<(u64, u64)>,
    found_at: usize,
    doc: Option<Doc>,
    error: Option<String>,
    looks: [Look; 2],
    inspector: PaneState,
    /// The log's tab: activity or labels.
    tab: usize,
    /// The main menu, and which of its menus is unfolded.
    menu: bool,
    submenu: Option<usize>,
    /// First visible frame, and frames per point.
    start: f64,
    fpp: f64,
    /// The timeline's size in points, from layout.
    wave: [f32; 2],
    fit_pending: bool,
    /// A range to show instead of the whole timeline once the layout is known.
    view_request: Option<(f64, f64)>,
    selection: Option<(f64, f64)>,
    /// The track the last press on the timeline was in, by id. "Mute on track" acts on it.
    picked: Option<String>,
    cursor: f64,
    selected: Option<usize>,
    /// The label the pointer is over, by its place in the document's notes: its card is shown.
    hover_note: Option<usize>,
    /// The range a label is being written for, in frames: a card with a text field is shown.
    labeling: Option<(f64, f64)>,
    label_text: TextEdit,
    files: Browser,
    /// `--outside-cut A..B`: no reload until the window next writes, so that write meets a
    /// project another program has changed. For checks.
    hold_reloads: bool,
    /// Something to tell the person, at the right of the transport: why an edit failed, how an
    /// export went, who just edited the project. There is nothing there otherwise.
    notice: Option<String>,
    /// A notice that goes away by itself, and when: the word of an outside edit.
    fleeting: Option<(String, f64)>,
    /// `--outside-edit SECONDS`: at that time, act as if the project file had just been written
    /// by someone else. For stills and checks of what the window does then.
    pretend_edit_at: Option<f64>,
    /// The project file's modified time when it was last read.
    stamp: Option<SystemTime>,
    checked_at: f64,
    /// Bumped on every reload, so drawings cached for the old document are rebuilt.
    generation: u64,
    /// Plays the timeline as it stands. Reopened on every reload, since an edit changes the audio.
    player: Option<Player>,
    /// Why there is no player, when there is none.
    audio_note: String,
    no_audio: bool,
    mute: bool,
    /// The playhead's place and the counter's text. Live values: moving them does not re-run
    /// the view.
    playhead: Live<[f32; 2]>,
    clock: LiveText,
    meters: Meters,
    /// `--play-check SECONDS`: play muted for that long, print what happened, and exit.
    play_check: Option<f64>,
    play_check_began: Option<f64>,
}

/// The bottom of the level meters, in dBFS. The top is full scale.
const METER_FLOOR: f32 = -60.0;
/// How fast a bar falls once the audio is quieter than it shows, in dB a second.
const METER_FALL: f32 = 24.0;
/// How long the tick stays at the highest level before it falls too, in seconds.
const METER_HOLD: f64 = 1.2;
/// How long the light at the end of a bar stays on after a sample at full scale, in seconds.
const METER_CLIP: f64 = 2.0;

/// The level meters beside the counter: what is playing, left and right. The bars rise at once
/// and fall slowly, as a peak meter does. Everything drawn moves through live values, so a
/// frame of playback does not re-run the view.
struct Meters {
    /// What each bar shows, in dBFS.
    level: [f32; 2],
    /// The highest level each bar reached lately, and when.
    hold: [(f32, f64); 2],
    clip_until: [f64; 2],
    /// When the meters last moved.
    at: f64,
    /// The highest level since the window opened, for `--play-check`.
    seen: [f32; 2],
    /// `--meter L,R` in dBFS: levels to hold, for stills.
    fixed: Option<[f32; 2]>,
    /// Where the cover over the unlit part of each bar starts, the tick, and the light.
    cover: [Live<[f32; 2]>; 2],
    tick: [Live<[f32; 2]>; 2],
    clip: [Live<f32>; 2],
}

impl Meters {
    fn new(fixed: Option<[f32; 2]>) -> Meters {
        Meters {
            level: [METER_FLOOR; 2],
            hold: [(METER_FLOOR, 0.0); 2],
            clip_until: [0.0; 2],
            at: 0.0,
            seen: [f32::NEG_INFINITY; 2],
            fixed,
            cover: [Live::new([0.0, 0.0]), Live::new([0.0, 0.0])],
            tick: [Live::new([-100.0, 0.0]), Live::new([-100.0, 0.0])],
            clip: [Live::new(0.0), Live::new(0.0)],
        }
    }
}

/// How wide the meter bars are, and whether the timeline's length still fits beside the
/// counter: a narrow window gives the length up before the meters.
fn meter_room(s: &Studio) -> (f32, bool) {
    // What the transport has left once its buttons and the counter are placed, less what the
    // meters need around their bars.
    let room = s.wave[0] - 467.0 - 56.0;
    // The length takes 86 points. While both fit, the last edit's line keeps 180 too.
    if room - 86.0 >= 110.0 { ((room - 86.0 - 180.0).clamp(110.0, 200.0), true) } else { (room.clamp(72.0, 200.0), false) }
}

/// Moves the meters to the level of what just played. True while anything is still lit.
fn step_meters(s: &mut Studio, t: f64) -> bool {
    let (w, _) = meter_room(s);
    let played = match (s.meters.fixed, &s.player) {
        (Some(db), _) => db.map(|db| 10f32.powf(db / 20.0)),
        (None, Some(p)) => p.take_levels(),
        (None, None) => [0.0; 2],
    };
    let m = &mut s.meters;
    let fall = METER_FALL * (t - m.at).clamp(0.0, 0.25) as f32;
    m.at = t;
    let x_of = |db: f32| w * (db.clamp(METER_FLOOR, 0.0) - METER_FLOOR) / -METER_FLOOR;
    let mut lit = false;
    for (c, &played) in played.iter().enumerate() {
        let db = if played > 0.0 { 20.0 * played.log10() } else { f32::NEG_INFINITY };
        m.seen[c] = m.seen[c].max(db);
        if played >= scrubr_core::analysis::CLIP_LEVEL {
            m.clip_until[c] = t + METER_CLIP;
        }
        m.level[c] = db.max(m.level[c] - fall).max(METER_FLOOR);
        if m.level[c] >= m.hold[c].0 {
            m.hold[c] = (m.level[c], t);
        } else if t - m.hold[c].1 > METER_HOLD {
            m.hold[c].0 = (m.hold[c].0 - fall).max(m.level[c]);
        }
        if m.fixed.is_some() {
            // A still shows the tick a little above the bar, as it sits just after a peak.
            m.hold[c].0 = (m.level[c] + 5.0).min(0.0);
        }
        m.cover[c].set([x_of(m.level[c]), 0.0]);
        m.tick[c].set([if m.hold[c].0 > METER_FLOOR + 0.5 { x_of(m.hold[c].0) } else { -100.0 }, 0.0]);
        m.clip[c].set(if t < m.clip_until[c] { 1.0 } else { 0.0 });
        lit |= m.level[c] > METER_FLOOR || m.hold[c].0 > METER_FLOOR || t < m.clip_until[c];
    }
    lit
}

fn stamp(paths: &Paths) -> Option<SystemTime> {
    std::fs::metadata(&paths.project).and_then(|m| m.modified()).ok()
}

/// The playhead in timeline frames: the player's position while it plays, the cursor otherwise.
fn head(s: &Studio) -> f64 {
    match &s.player {
        Some(p) if p.is_playing() => p.position() as f64,
        _ => s.cursor,
    }
}

/// Moves the cursor, and the player with it.
fn set_cursor(s: &mut Studio, frame: f64) {
    s.cursor = frame.clamp(0.0, total(s));
    if let Some(p) = &s.player {
        p.seek(s.cursor as u64);
    }
}

/// Opens a player over the timeline as it stands now, paused at the cursor.
fn open_player(s: &mut Studio) {
    s.player = None;
    let Some(doc) = &s.doc else { return };
    if s.no_audio {
        s.audio_note = "audio off".into();
        return;
    }
    match Player::timeline(&doc.project, &s.paths, if s.mute { 0.0 } else { 1.0 }) {
        Ok(p) => {
            p.seek(s.cursor as u64);
            s.audio_note = String::new();
            s.player = Some(p);
        }
        Err(e) => s.audio_note = format!("no audio: {e}"),
    }
}

/// Plays a file from the Files tab, or stops it if it is the one playing. The timeline pauses.
fn toggle_listen(s: &mut Studio, file: PathBuf) {
    if s.files.playing.take().is_some_and(|(playing, _)| playing == file) {
        return;
    }
    if let Some(p) = s.player.as_ref().filter(|p| p.is_playing()) {
        p.pause();
        s.cursor = p.position() as f64;
    }
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if s.no_audio {
        s.notice = Some("Audio is switched off, so there is nothing to hear.".into());
        return;
    }
    match Player::file(&file, if s.mute { 0.0 } else { 1.0 }) {
        Ok(p) => {
            p.play();
            s.notice = None;
            s.files.playing = Some((file, p));
        }
        Err(e) => s.notice = Some(format!("{name} could not be played: {e}")),
    }
}

/// True, with a word to the person, while a file is still being added: the project is being
/// written, so nothing else can change it yet.
fn busy_adding(s: &mut Studio) -> bool {
    let Some(adding) = &s.adding else { return false };
    s.notice = Some(format!("Still adding {}. Wait for it, then try again.", adding.name));
    true
}

/// Starts a change that adds a file, on its own thread. `watch_adding` finishes it.
fn start_adding(s: &mut Studio, name: String, what: &'static str, range: (u64, u64), change: AddChange, after: Option<String>) {
    let kept = place(s, range);
    let paths = s.paths.clone();
    let opts = WriteOpts { if_version: s.doc.as_ref().map(|d| d.project.version), ..Default::default() };
    let (answer, result) = mpsc::channel();
    let run = change.clone();
    let started = std::thread::Builder::new().name("scrubr-add".into()).spawn(move || {
        let made = run(&paths, &opts, range).map_err(|e| (e.kind() == Kind::Conflict, e.to_string()));
        let _ = answer.send(made);
    });
    match started {
        Ok(_) => {
            s.hold_reloads = false;
            s.notice = Some(format!("Adding {name}\u{2026}"));
            s.adding = Some(Adding { name, what, kept, range, change, after, result });
        }
        Err(e) => s.notice = Some(format!("Could not start: {e}")),
    }
}

/// Shows the project once a file has been added. If another program wrote the project first,
/// the place is looked for again and the file is added there, once, when the same audio is
/// still in one piece. True when the window has changed.
fn watch_adding(s: &mut Studio) -> bool {
    let Some(adding) = &s.adding else { return false };
    let made = match adding.result.try_recv() {
        Ok(made) => made,
        Err(mpsc::TryRecvError::Empty) => return false,
        Err(mpsc::TryRecvError::Disconnected) => Err((false, "it stopped without a result.".to_string())),
    };
    let Some(Adding { name, what, kept, range, change, after, .. }) = s.adding.take() else { return false };
    match made {
        Ok(()) => {
            reload(s);
            let rate = s.doc.as_ref().map_or(48_000, |d| d.rate());
            s.notice = after.map(|who| format!("{who} edited the project first. Your {what} was made on the same audio, now at {}.", timecode(range.0 as f64, rate)));
            if what == "insert" {
                set_cursor(s, range.0 as f64);
                reveal(s, range.0 as f64);
            }
            let dir = s.files.dir.clone();
            browse(s, dir);
        }
        Err((true, _)) => {
            reload(s);
            let who = last_author(s);
            match find(s, &kept).filter(|_| after.is_none()) {
                Some(moved) => start_adding(s, name, what, moved, change, Some(who)),
                None => s.notice = Some(format!("{who} edited the project first, and the audio your {what} was for has changed. {name} was not added: look again and repeat it.")),
            }
        }
        Err((false, e)) => s.notice = Some(format!("{name} could not be added: {e}")),
    }
    true
}

/// Inserts a file from the Files tab into the timeline at the playhead.
fn add_file(s: &mut Studio, file: PathBuf) {
    if s.fresh {
        return first_file(s, file);
    }
    if busy_adding(s) {
        return;
    }
    let Some(doc) = &s.doc else { return };
    let at = (head(s).round().max(0.0) as u64).min(doc.tl.total());
    let (rate, crossfade) = (doc.rate(), (DEFAULT_CROSSFADE_MS / 1000.0 * doc.rate() as f64).round() as u32);
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    s.files.playing = None;
    let shown = name.clone();
    let change: AddChange = Arc::new(move |paths: &Paths, opts: &WriteOpts, range: (u64, u64)| {
        let desc = format!("insert {name} at {}", timecode(range.0 as f64, rate));
        edit::apply(paths, opts, &you(), false, |_, _| Ok((vec![Request::Insert { file: file.clone(), at: range.0, crossfade }], Some(desc)))).map(|_| ())
    });
    start_adding(s, shown, "insert", (at, at), change, None);
}

/// Lays a file from the Files tab on a new track, starting at the playhead. Nothing moves.
fn add_file_as_track(s: &mut Studio, file: PathBuf) {
    if busy_adding(s) {
        return;
    }
    let Some(doc) = &s.doc else { return };
    let at = (head(s).round().max(0.0) as u64).min(doc.tl.total());
    // The track is named after the file; a second one of the same name gets a number.
    let stem = file.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "track".into());
    let taken = |name: &str| doc.tl.tracks().iter().any(|t| !t.removed && (t.name == name || t.id == name));
    let name = std::iter::once(stem.clone()).chain((2..).map(|n| format!("{stem} {n}"))).find(|name| !taken(name)).unwrap_or(stem);
    let shown = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    s.files.playing = None;
    let change: AddChange = Arc::new(move |paths: &Paths, opts: &WriteOpts, range: (u64, u64)| edit::track_add(paths, opts, &you(), &file, Some(&name), None, range.0).map(|_| ()));
    start_adding(s, shown, "new track", (at, at), change, None);
}

fn toggle_play(s: &mut Studio) {
    s.files.playing = None;
    let Some(p) = &s.player else {
        s.notice = Some(if s.audio_note.is_empty() { "No audio output.".into() } else { s.audio_note.clone() });
        return;
    };
    if p.is_playing() {
        p.pause();
        s.cursor = p.position() as f64;
    } else {
        p.play();
    }
}

/// Puts the playhead and the counter where the audio is.
fn sync(s: &Studio) {
    let Some(doc) = &s.doc else { return };
    let at = head(s);
    let x = ((at - s.start) / s.fpp) as f32;
    s.playhead.set([if x >= 0.0 && x <= s.wave[0] { x } else { -100.0 }, 0.0]);
    s.clock.set(timecode(at, doc.rate()));
}

fn reload(s: &mut Studio) {
    if s.blank {
        s.doc = None;
        s.error = None;
        s.player = None;
        return;
    }
    // Keep the place the audio had reached: the player is about to be replaced.
    s.cursor = head(s);
    s.stamp = stamp(&s.paths);
    s.words_stamp = words_stamp(&s.paths);
    s.generation += 1;
    match load(&s.paths) {
        Ok(doc) => {
            s.hover_note = s.hover_note.filter(|&i| i < doc.notes.len());
            s.doc = Some(doc);
            s.error = None;
            if s.fit_pending { fit(s) } else { clamp(s) }
            s.cursor = s.cursor.min(total(s));
            s.selection = s.selection.filter(|&(_, b)| b <= total(s));
            s.selected = s.selected.filter(|&i| s.doc.as_ref().is_some_and(|d| i < d.project.edits.len()));
        }
        Err(e) => {
            s.doc = None;
            s.error = Some(e);
        }
    }
    open_player(s);
}

fn init_with(path: Option<PathBuf>) -> Studio {
    let args = rune::extra_args();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let has = |name: &str| args.iter().any(|a| a == name);
    let play_check = flag("--play-check").and_then(|t| t.parse::<f64>().ok());
    let mut s = Studio {
        blank: path.is_none(),
        fresh: false,
        paths: Paths::new(path.unwrap_or_default()),
        ask: None,
        exporting: None,
        creating: None,
        adding: None,
        job: None,
        spectro: Spectro { on: has("--spectrogram"), at_once: has("--spectrogram"), ..Default::default() },
        words_stamp: None,
        finding: false,
        find_text: TextEdit::default(),
        found: Vec::new(),
        found_at: 0,
        picked: None,
        doc: None,
        error: None,
        looks: [look(false), look(true)],
        inspector: PaneState::new(336.0).range(280.0, 480.0),
        tab: 0,
        menu: false,
        submenu: None,
        start: 0.0,
        fpp: 1.0,
        wave: [0.0; 2],
        fit_pending: true,
        view_request: None,
        selection: None,
        cursor: 0.0,
        selected: None,
        hover_note: flag("--note").and_then(|t| t.parse().ok()),
        labeling: None,
        label_text: TextEdit::default(),
        files: Browser { dir: PathBuf::new(), entries: Vec::new(), error: None, playing: None },
        hold_reloads: false,
        notice: None,
        fleeting: None,
        pretend_edit_at: flag("--outside-edit").and_then(|t| t.parse().ok()),
        stamp: None,
        checked_at: 0.0,
        generation: 0,
        player: None,
        audio_note: String::new(),
        no_audio: has("--no-audio"),
        mute: has("--mute") || play_check.is_some(),
        playhead: Live::new([-100.0, 0.0]),
        clock: LiveText::new("0:00.000"),
        meters: Meters::new(flag("--meter").and_then(|text| {
            let (l, r) = text.split_once(',')?;
            Some([l.parse().ok()?, r.parse().ok()?])
        })),
        play_check,
        play_check_began: None,
    };
    reload(&mut s);
    // For stills: `--view START..END` in seconds, `--select START..END`, `--edit ID`,
    // `--cursor SECONDS`, `--tab labels`, `--menu edit`, `--no-audio`, `--meter L,R` in dBFS,
    // `--tab files` with `--files FOLDER` for the Files tab, `--outside-cut A..B` to have another
    // author cut that range behind the window's back,
    // `--note N` to show the Nth label's card, `--label-card` to open the card for writing one, `--outside-edit SECONDS` to reload then as if
    // another program had written the project.
    // `--mute` plays silently.
    let rate = s.doc.as_ref().map_or(48_000.0, |d| d.rate() as f64);
    let range = |text: String| {
        let (a, b) = text.split_once("..")?;
        Some((a.parse::<f64>().ok()? * rate, b.parse::<f64>().ok()? * rate))
    };
    s.view_request = flag("--view").and_then(range);
    s.selection = flag("--select").and_then(range);
    if let Some(at) = flag("--cursor").and_then(|t| t.parse::<f64>().ok()) {
        set_cursor(&mut s, at * rate);
    }
    if let (Some(id), Some(doc)) = (flag("--edit"), &s.doc) {
        s.selected = doc.project.edits.iter().position(|e| e.id == id);
    }
    if has("--label-card") {
        begin_label(&mut s);
    }
    // The Files tab opens on the project's own folder, or the one `--files` names.
    let folder = flag("--files").map(PathBuf::from).unwrap_or_else(|| s.paths.dir.clone());
    if !s.blank {
        browse(&mut s, folder);
    }
    // `--new`: start as New does, with the Files tab on the folder `--files` names.
    if has("--new") && s.blank {
        start_fresh(&mut s);
        if let Some(dir) = flag("--files") {
            browse(&mut s, PathBuf::from(dir));
        }
    }
    // `--outside-cut A..B` in seconds: another author cuts that range once the window has read
    // the project, and the window is not told.
    if let (Some((a, b)), Some(version)) = (flag("--outside-cut").and_then(range), s.doc.as_ref().map(|d| d.project.version)) {
        let range = (a as u64, b as u64);
        let outside = Who { author: "codex".into(), session: Some("s9".into()) };
        let _ = edit::apply(&s.paths, &WriteOpts { if_version: Some(version), ..Default::default() }, &outside, false, |_, _| Ok((vec![Request::Cut { range, crossfade: 240 }], None)));
        s.hold_reloads = true;
    }
    match flag("--tab").as_deref() {
        Some("labels") => s.tab = 1,
        Some("files") => s.tab = 2,
        _ => {}
    }
    if let Some(name) = flag("--menu") {
        s.menu = true;
        s.submenu = MENUS.iter().position(|m| m.eq_ignore_ascii_case(&name));
    }
    s
}

// ---------------------------------------------------------------- the view's camera

fn total(s: &Studio) -> f64 {
    s.doc.as_ref().map_or(0.0, |d| d.tl.total() as f64)
}

fn clamp(s: &mut Studio) {
    let w = s.wave[0].max(1.0) as f64;
    let total = total(s).max(1.0);
    s.fpp = s.fpp.clamp(MIN_FPP.min(total / w), (total / w).max(MIN_FPP.min(total / w)));
    s.start = s.start.clamp(0.0, (total - w * s.fpp).max(0.0));
}

fn fit(s: &mut Studio) {
    if s.doc.is_none() || s.wave[0] <= 0.0 {
        return;
    }
    s.fit_pending = false;
    match s.view_request.take() {
        Some((a, b)) if b > a => {
            s.fpp = (b - a) / s.wave[0] as f64;
            s.start = a;
        }
        _ => {
            s.fpp = total(s) / s.wave[0] as f64;
            s.start = 0.0;
        }
    }
    clamp(s);
}

fn zoom_about(s: &mut Studio, x: f64, factor: f64) {
    let anchor = s.start + x * s.fpp;
    s.fpp /= factor.max(0.01);
    clamp(s);
    s.start = anchor - x * s.fpp;
    clamp(s);
}

fn zoom_center(s: &mut Studio, factor: f64) {
    zoom_about(s, s.wave[0] as f64 * 0.5, factor);
}

fn pan(s: &mut Studio, points: f64) {
    s.start += points * s.fpp;
    clamp(s);
}

/// Brings timeline frame `t` into view, a third of the way across, if it is not already.
fn reveal(s: &mut Studio, t: f64) {
    let w = s.wave[0] as f64;
    if t < s.start || t > s.start + w * s.fpp {
        s.start = t - w * s.fpp / 3.0;
        clamp(s);
    }
}

// ---------------------------------------------------------------- text

/// `12:03.250`, or `1:02:03.250` past an hour.
fn timecode(frame: f64, rate: u32) -> String {
    let ms = (frame.max(0.0) / rate as f64 * 1000.0).round() as u64;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{ms:03}") } else { format!("{m}:{s:02}.{ms:03}") }
}

fn duration(frames: f64, rate: u32) -> String {
    let secs = frames / rate as f64;
    if secs >= 60.0 { format!("{}:{:06.3}", (secs / 60.0) as u64, secs % 60.0) } else { format!("{secs:.3} s") }
}

fn ruler_label(seconds: f64, decimals: usize) -> String {
    let whole = seconds.floor() as u64;
    let (h, m, s) = (whole / 3600, whole / 60 % 60, whole % 60);
    let mut out = if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") };
    if decimals > 0 {
        out.push_str(&format!("{:.*}", decimals, seconds - whole as f64)[1..]);
    }
    out
}

/// Seconds between ruler labels so they sit at least `min_points` apart, and minor ticks per label.
fn ruler_step(seconds_per_point: f64, min_points: f64) -> (f64, usize, u32) {
    const STEPS: [(f64, usize, u32); 23] = [
        (0.00001, 5, 5), (0.00005, 5, 5), (0.0001, 4, 5), (0.0005, 4, 5),
        (0.001, 3, 5), (0.005, 3, 5), (0.01, 2, 5), (0.05, 2, 5), (0.1, 1, 5), (0.5, 1, 5), (1.0, 0, 5), (2.0, 0, 4), (5.0, 0, 5), (10.0, 0, 5),
        (15.0, 0, 3), (30.0, 0, 6), (60.0, 0, 6), (120.0, 0, 4), (300.0, 0, 5), (600.0, 0, 5), (900.0, 0, 3), (1800.0, 0, 6), (3600.0, 0, 6),
    ];
    let want = seconds_per_point * min_points;
    STEPS.iter().copied().find(|(step, ..)| *step >= want).unwrap_or(((want / 3600.0).ceil() * 3600.0, 0, 6))
}

/// Seconds since 1970 for `2026-11-12T13:55:45Z`.
fn epoch(stamp: &str) -> Option<i64> {
    let num = |range: std::ops::Range<usize>| stamp.get(range)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, min, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400 + h * 3600 + min * 60 + s)
}

fn ago(stamp: Option<&str>) -> String {
    let Some(then) = stamp.and_then(epoch) else { return String::new() };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    match now - then {
        i64::MIN..60 => "just now".into(),
        60..3600 => format!("{} min ago", (now - then) / 60),
        3600..86_400 => format!("{} h ago", (now - then) / 3600),
        _ => stamp.unwrap_or("")[..10].to_string(),
    }
}

/// What an edit is, for its chip and icon: the operation, or the one operation a batch repeats.
fn kind(op: &Op) -> (&'static str, &'static Icon) {
    match op {
        Op::Import { .. } => ("Import", &IMPORT),
        Op::Cut { .. } => ("Cut", &SCISSORS),
        Op::Keep { .. } => ("Keep", &KEEP),
        Op::Gain { .. } => ("Gain", &LEVEL),
        Op::Fade { .. } => ("Fade", &FADE),
        Op::Normalize { .. } => ("Normalize", &LEVEL),
        Op::Mute { .. } => ("Mute", &MUTE),
        Op::Insert { .. } => ("Insert", &INSERT),
        Op::Track { .. } | Op::Clip { .. } => ("Track", &LAYERS),
        Op::TrackSet { .. } => ("Mix", &LEVEL),
        Op::TrackRemove { .. } => ("Track", &CROSS),
        Op::Revert { .. } => ("Undo", &UNDO),
        // Audio put on a track is a batch: the time it needs, the track and the clip.
        Op::Batch { ops } if ops.iter().any(|op| matches!(op, Op::Track { .. } | Op::Clip { .. })) => ("Track", &LAYERS),
        Op::Batch { ops } => match ops.first() {
            Some(first) if ops.iter().all(|op| op.name() == first.name()) => kind(first),
            _ => ("Batch", &LAYERS),
        },
    }
}

/// What an edit did, in the log's words: its own description, a batch's count set as a
/// multiplication sign, and an undo named after what it undid.
fn describe(e: &project::Edit, project: &Project) -> String {
    let text = e.desc.clone().unwrap_or_else(|| e.op.name().to_string());
    match &e.op {
        Op::Batch { .. } => text.replace(" x", " \u{00d7}"),
        Op::Revert { edits } => match (edits.as_slice(), text.strip_prefix("revert session ")) {
            (_, Some(session)) => format!("undid session {session}"),
            ([one], _) => match project.edits.iter().find(|x| &x.id == one) {
                Some(target) => format!("undid {}", describe(target, project)),
                None => format!("undid {one}"),
            },
            (many, _) => format!("undid {} edits", many.len()),
        },
        _ => text,
    }
}

fn shown_name(author: &str) -> String {
    if is_agent(author) { author.to_string() } else { "You".to_string() }
}

// ---------------------------------------------------------------- drawing

/// The two-tone waveform of one lane: every column's peaks, fainter, around its loudness body.
#[allow(clippy::too_many_arguments)]
fn lane(mut shape: Shape, cols: &[Column], col_w: f32, mid: f32, amp: f32, body: Color, soft: Color, tint: impl Fn(usize) -> Option<(Color, Color)>) -> Shape {
    for (i, c) in cols.iter().enumerate() {
        if !c.any {
            continue;
        }
        let (body, soft) = tint(i).unwrap_or((body, soft));
        let x = (i as f32 + 0.5) * col_w;
        let (top, bottom) = (mid - c.max.clamp(-1.0, 1.0) * amp, mid - c.min.clamp(-1.0, 1.0) * amp);
        if bottom - top > col_w {
            shape = shape.line([x, top], [x, bottom], col_w, LineCap::Butt, soft);
        }
        let half = (c.rms.min(1.0) * amp).max(col_w * 0.5);
        shape = shape.line([x, mid - half], [x, mid + half], col_w, LineCap::Butt, body);
    }
    shape
}

/// The diagonal mark of a splice, as a razor leaves it on tape.
fn slash(shape: Shape, x: f32, y: f32, h: f32, width: f32, color: Color) -> Shape {
    shape.line([x - h * 0.3, y + h], [x + h * 0.3, y], width, LineCap::Round, color)
}

fn overview(s: &Studio, doc: &Doc, lk: &Look, w: f32, scale: f32) -> El {
    let total = doc.tl.total().max(1) as f64;
    let h = OVERVIEW_H;
    let version = (s.generation, w.to_bits(), scale.to_bits(), lk.k.dark);
    let strip = memo("overview-shape", version, || {
        let cols = (w * scale).round().max(1.0) as usize;
        let col_w = w / cols as f32;
        let mut shape = Shape::new();
        let channels = doc.project.channels.min(2) as usize;
        // Both channels folded into one band: the wider of the two.
        let mut folded = doc.columns(0, 0.0, total / cols as f64, cols);
        for c in 1..channels {
            for (a, b) in folded.iter_mut().zip(doc.columns(c, 0.0, total / cols as f64, cols)) {
                *a = Column { min: a.min.min(b.min), max: a.max.max(b.max), rms: a.rms.max(b.rms), any: a.any || b.any };
            }
        }
        // The overview is scaled to its own loudest column, so a quiet recording still shows its shape.
        let peak = folded.iter().fold(0.05f32, |m, c| m.max(c.max).max(-c.min));
        shape = lane(shape, &folded, col_w, h * 0.5 - 3.0, (h * 0.5 - 8.0) / peak, lk.ink.alpha(0.62), lk.ink.alpha(0.22), |_| None);
        for splice in &doc.splices {
            let x = (splice.at as f64 / total) as f32 * w;
            shape = shape.line([x, h - 7.0], [x, h - 2.0], 1.5, LineCap::Butt, lk.author(splice.agent));
        }
        canvas(shape).absolute(0.0, 0.0).size(w, h)
    });
    let x0 = (s.start / total) as f32 * w;
    let x1 = (((s.start + s.wave[0] as f64 * s.fpp) / total) as f32 * w).min(w);
    let mut strip_el = el()
        .key("overview")
        .w_full()
        .h(h)
        .fill(lk.k.c.text_base.alpha(0.03))
        .on_drag(|s: &mut Studio, d, _| {
            let (w, total) = (s.wave[0].max(1.0) as f64, self::total(s));
            s.start = d.x as f64 / w * total - w * s.fpp * 0.5;
            clamp(s);
        })
        .child(strip);
    if w > 0.0 && x1 - x0 < w - 0.5 {
        strip_el = strip_el.child(
            el().key("viewport")
                .absolute(x0, 2.0)
                .size((x1 - x0).max(3.0), h - 4.0)
                .radius(4.0)
                .fill(lk.you.alpha(0.13))
                .border(1.0, lk.you.alpha(0.9)),
        );
    }
    strip_el
}

fn ruler(s: &Studio, doc: &Doc, lk: &Look, w: f32) -> El {
    let k = &lk.k;
    let rate = doc.rate() as f64;
    let (step, decimals, minor) = ruler_step(s.fpp / rate, 96.0);
    let first = (s.start / rate / step).floor() as i64;
    let last = ((s.start + w as f64 * s.fpp) / rate / step).ceil() as i64;
    let x_of = |seconds: f64| ((seconds * rate - s.start) / s.fpp) as f32;
    let mut shape = Shape::new();
    let mut labels = Vec::new();
    for n in first..=last {
        let seconds = n as f64 * step;
        for m in 0..minor {
            let x = x_of(seconds + step * m as f64 / minor as f64);
            if (0.0..=w).contains(&x) {
                let tall = m == 0;
                shape = shape.line([x, if tall { RULER_H - 9.0 } else { RULER_H - 4.0 }], [x, RULER_H], 1.0, LineCap::Butt, k.c.text_base.alpha(if tall { 0.35 } else { 0.16 }));
            }
        }
        let x = x_of(seconds);
        if seconds >= 0.0 && x >= -2.0 && x < w - 44.0 {
            labels.push(text(ruler_label(seconds, decimals)).key(format!("tick-{n}")).absolute(x + 5.0, 4.0).font(lk.num(k.ty.small)).color(k.c.text_muted));
        }
    }
    let mut ruler = el()
        .key("ruler")
        .w_full()
        .h(RULER_H)
        .border_sides([1.0, 0.0, 1.0, 0.0], lk.rule)
        .child(canvas(shape).key("ruler-canvas").absolute(0.0, 0.0).size(w, RULER_H));
    for label in labels {
        ruler = ruler.child(label);
    }
    ruler
}

/// The audio in view, rendered with its edits: the frame it starts at, its channel count and
/// its interleaved samples. For views closer than the peak cache can show.
fn rendered(s: &Studio, doc: &Doc, w: f32, track: Option<usize>) -> Option<(u64, usize, Vec<f32>)> {
    let mut renderer = Renderer::of(&doc.project, &s.paths, track).ok()?;
    let total = renderer.total();
    let first = (s.start.floor().max(0.0) as u64).min(total);
    let last = ((s.start + w as f64 * s.fpp).ceil().max(0.0) as u64 + 2).min(total);
    let mut samples = Vec::new();
    renderer.read(first, (last - first) as usize, &mut samples).ok()?;
    Some((first, renderer.channels().max(1), samples))
}

/// `cols` columns of `fpc` frames each from `start`, for one channel of rendered audio.
fn sample_columns(first: u64, channels: usize, samples: &[f32], channel: usize, start: f64, fpc: f64, cols: usize) -> Vec<Column> {
    let frames = samples.len() / channels;
    let ch = channel.min(channels - 1);
    (0..cols)
        .map(|i| {
            let t0 = (start + i as f64 * fpc).floor().max(0.0) as u64;
            let t1 = ((start + (i + 1) as f64 * fpc).ceil().max(0.0) as u64).max(t0 + 1);
            let (a, b) = (t0.saturating_sub(first) as usize, (t1.saturating_sub(first) as usize).min(frames));
            if t0 < first || a >= b {
                return Column::default();
            }
            let (mut min, mut max, mut squares) = (f32::INFINITY, f32::NEG_INFINITY, 0f64);
            // Start from where the column before left off, so neighbouring columns always meet.
            if a > 0 {
                let before = samples[(a - 1) * channels + ch];
                (min, max) = (before, before);
            }
            for f in a..b {
                let v = samples[f * channels + ch];
                min = min.min(v);
                max = max.max(v);
                squares += (v as f64) * (v as f64);
            }
            Column { min, max, rms: (squares / (b - a) as f64).sqrt() as f32, any: true }
        })
        .collect()
}

fn wave_shape(s: &Studio, doc: &Doc, lk: &Look, w: f32, h: f32, scale: f32) -> Shape {
    let lanes = doc.lanes();
    let cols = (w * scale).round().max(1.0) as usize;
    let col_w = w / cols as f32;
    let fpc = s.fpp * col_w as f64;
    let lane_h = h / lanes.len() as f32;
    let heard = doc.tl.mix_gains(2, None);
    let selected = s.selection.map(|(a, b)| (((a - s.start) / fpc) as isize, ((b - s.start) / fpc) as isize));
    // A column narrower than the peak cache's smallest bucket has nothing of its own to show
    // there, so from that zoom inwards the lanes are drawn from the audio itself.
    let close = fpc < peaks::BASE as f64;
    let mut shape = Shape::new();
    for (c, &kind) in lanes.iter().enumerate() {
        // What the lane is drawn from at a close zoom, and which channel of it: the mix's own
        // channel, or one track heard alone with its channels averaged.
        let (fine, pick) = match (kind, close) {
            (_, false) => (None, 0),
            (Lane::Channel(channel), true) => (rendered(s, doc, w, None), channel),
            (Lane::Track(track), true) => (
                rendered(s, doc, w, Some(track)).map(|(first, channels, samples)| (first, 1, samples.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect())),
                0,
            ),
        };
        // A track that is muted, or left out by another's solo, is drawn faint.
        let quiet = matches!(kind, Lane::Track(t) if heard[t] == [0.0, 0.0]);
        let (ink, ink_soft) = if quiet { (lk.ink.alpha(0.32), lk.ink_soft.alpha(0.4)) } else { (lk.ink, lk.ink_soft) };
        let mid = lane_h * (c as f32 + 0.5);
        shape = shape.line([0.0, mid], [w, mid], 1.0, LineCap::Butt, lk.rule);
        if c > 0 {
            shape = shape.line([0.0, lane_h * c as f32], [w, lane_h * c as f32], 1.0, LineCap::Butt, lk.rule);
        }
        let amp = (lane_h * 0.5 - 10.0) * doc.magnify();
        if let (Some((first, channels, samples)), true) = (&fine, s.fpp <= LINE_FPP) {
            // Closest of all: a line through the samples, and a dot on each once they are apart.
            let ch = pick.min(channels - 1);
            let points: Vec<[f32; 2]> = samples
                .chunks_exact(*channels)
                .enumerate()
                .map(|(i, frame)| [(((first + i as u64) as f64 - s.start) / s.fpp) as f32, mid - frame[ch].clamp(-1.0, 1.0) * amp])
                .collect();
            if points.len() > 1 {
                shape = shape.stroke(points.clone(), 1.25, ink);
            }
            if s.fpp <= DOT_FPP {
                for [x, y] in points.into_iter().filter(|p| p[0] >= -4.0 && p[0] <= w + 4.0) {
                    shape = shape.line([x - 0.01, y], [x + 0.01, y], 4.5, LineCap::Round, ink);
                }
            }
            continue;
        }
        if let Some((first, channels, samples)) = &fine {
            // Closer than a cycle of most sounds, a loudness body means nothing: draw the span
            // the wave covers in each column, which is its true shape.
            for (i, column) in sample_columns(*first, *channels, samples, pick, s.start, fpc, cols).iter().enumerate().filter(|(_, column)| column.any) {
                let chosen = selected.is_some_and(|(a, b)| (i as isize) >= a && (i as isize) < b);
                let x = (i as f32 + 0.5) * col_w;
                let (top, bottom) = (mid - column.max.clamp(-1.0, 1.0) * amp, mid - column.min.clamp(-1.0, 1.0) * amp);
                let thin = (1.0 - (bottom - top)).max(0.0) * 0.5;
                shape = shape.line([x, top - thin], [x, bottom + thin], col_w, LineCap::Butt, if chosen { lk.you } else { ink });
            }
            continue;
        }
        let columns = match kind {
            Lane::Channel(channel) => doc.columns(channel, s.start, fpc, cols),
            Lane::Track(track) => doc.track_columns(track, s.start, fpc, cols),
        };
        shape = lane(shape, &columns, col_w, mid, amp, ink, ink_soft, |i| match selected {
            Some((a, b)) if (i as isize) >= a && (i as isize) < b => Some((lk.you, lk.you.alpha(0.38))),
            _ => None,
        });
    }
    // Splices: a slash at the top of the lanes, and a seam down them when there is room to read it.
    let visible: Vec<&Splice> = doc.splices.iter().filter(|j| (j.at as f64) >= s.start && (j.at as f64) <= s.start + w as f64 * s.fpp).collect();
    let roomy = visible.len() as f32 * 90.0 < w;
    for splice in visible {
        let x = ((splice.at as f64 - s.start) / s.fpp) as f32;
        let color = lk.author(splice.agent);
        let chosen = s.selected.is_some_and(|i| splice.edits.contains(&i));
        if roomy || chosen {
            shape = shape.line([x, 14.0], [x, h], if chosen { 1.5 } else { 1.0 }, LineCap::Butt, color.alpha(if chosen { 0.75 } else { 0.18 }));
        }
        shape = slash(shape, x, 3.0, 10.0, if chosen { 2.5 } else { 1.75 }, color);
    }
    shape
}

/// The audio a cut took out, drawn small and hatched: nothing is thrown away.
fn ghost(doc: &Doc, lk: &Look, removed: (usize, u64, u64), w: f32, h: f32) -> Shape {
    let (src, start, end) = removed;
    let mut shape = Shape::new();
    // Hatching, each stroke cut off at the box's edges.
    let mut x = -h;
    while x < w {
        let (x0, x1) = (x.max(0.0), (x + h).min(w));
        shape = shape.line([x0, h - (x0 - x)], [x1, h - (x1 - x)], 1.0, LineCap::Butt, lk.k.c.text_base.alpha(0.07));
        x += 7.0;
    }
    let Some(p) = &doc.peaks[src] else { return shape };
    let cols = w as usize;
    let fpc = (end - start) as f64 / cols as f64;
    let columns: Vec<Column> = (0..cols)
        .map(|i| {
            let from = start + (i as f64 * fpc) as u64;
            let to = (start + ((i + 1) as f64 * fpc).ceil() as u64).max(from + 1).min(end.max(from + 1));
            let m = p.range(0, from, to);
            Column { min: m.min, max: m.max, rms: p.rms(0, from, to), any: true }
        })
        .collect();
    lane(shape, &columns, 1.0, h * 0.5, h * 0.5 - 3.0, lk.ink.alpha(0.55), lk.ink.alpha(0.22), |_| None)
}

fn callout(s: &Studio, doc: &Doc, lk: &Look, w: f32) -> Option<El> {
    let k = &lk.k;
    let edit = s.selected?;
    let splice = doc.splices.iter().find(|j| j.edits.contains(&edit) && (j.at as f64) >= s.start && (j.at as f64) <= s.start + w as f64 * s.fpp)?;
    let removed = splice.removed?;
    let x = ((splice.at as f64 - s.start) / s.fpp) as f32;
    let (cw, gh) = (236.0, 40.0);
    // Oldest first: the ids of the cuts that meet here, and who made them.
    let ids: Vec<&str> = splice.edits.iter().rev().map(|&i| doc.project.edits[i].id.as_str()).collect();
    let mut by: Vec<String> = Vec::new();
    for &i in splice.edits.iter().rev() {
        let name = shown_name(&doc.project.edits[i].author);
        if !by.contains(&name) {
            by.push(name);
        }
    }
    let credit = format!("{} · {} · still in the source", ids.join(" + "), by.join(" and "));
    let mut card = col()
        .key("callout")
        .absolute((x + 10.0).min(w - cw - 8.0).max(8.0), 22.0)
        .w(cw)
        .padding(10.0)
        .gap(6.0)
        .radius(k.radius.md)
        .fill(k.c.layer_03)
        .border(1.0, k.c.border_base)
        .child(
            row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(el().size(7.0, 7.0).radius(4.0).fill(lk.author(splice.agent)))
                .child(text(format!("{} removed here", duration((removed.2 - removed.1) as f64, doc.rate()))).font(lk.num(k.ty.ui_medium)).color(k.c.text_base)),
        )
        .child(canvas(ghost(doc, lk, removed, cw - 20.0, gh)).key("ghost").size(cw - 20.0, gh).radius(k.radius.sm).fill(lk.lane))
        .child(text(credit).font(k.ty.small).color(k.c.text_muted));
    for shadow in &k.elevation.floating {
        card = card.box_shadow(*shadow);
    }
    Some(card)
}

/// The track the last press on the timeline was in, when the project shows a lane for each track.
fn picked_track(s: &Studio) -> Option<(usize, &TrackState)> {
    let doc = s.doc.as_ref()?;
    let id = s.picked.as_ref()?;
    doc.lanes().into_iter().find_map(|lane| match lane {
        Lane::Track(t) if doc.tl.tracks()[t].id == *id => Some((t, &doc.tl.tracks()[t])),
        _ => None,
    })
}

/// Takes a track out of the project. Undo brings it back.
fn remove_track(s: &mut Studio, id: String) {
    act(s, move |paths| edit::track_remove(paths, &WriteOpts::default(), &you(), &id).map(|_| ()));
}

/// Changes a track's place in the mix. It names the track by id, so it means the same whatever
/// else has happened to the project.
fn set_track(s: &mut Studio, id: String, change: edit::TrackChange) {
    act(s, move |paths| edit::track_set(paths, &WriteOpts::default(), &you(), &id, &change).map(|_| ()));
}

/// A track's name and its place in the mix, at the head of its lane: mute, solo, level in 1 dB
/// steps and pan in tenths. A press on the name picks the track, and every track but the main
/// one can be removed.
fn track_head(lk: &Look, t: &TrackState, y: f32, main: bool, picked: bool) -> El {
    let k = &lk.k;
    let level = (t.gain_db * 10.0).round() / 10.0;
    let pan = (t.pan * 10.0).round() / 10.0;
    let button = |key: String, label: &'static str, on: bool, help: &'static str, change: edit::TrackChange| {
        let id = t.id.clone();
        row()
            .key(key)
            .size(18.0, 18.0)
            .radius(5.0)
            .center()
            .fill(if on { lk.you } else { k.c.text_base.alpha(0.09) })
            .help(help)
            .on_press(move |s: &mut Studio| set_track(s, id.clone(), change.clone()))
            .child(text(label).font(k.ty.small_medium).color(if on { lk.on_you } else { k.c.text_muted }))
    };
    let change = edit::TrackChange::default;
    let pan_text = if pan == 0.0 { "C".to_string() } else { format!("{}{}", if pan < 0.0 { "L" } else { "R" }, (pan.abs() * 100.0).round() as i32) };
    let figure = |value: String, width: f32| row().w(width).h(18.0).center().child(text(value).font(lk.num(k.ty.small)).color(k.c.text_muted));
    let pick = t.id.clone();
    let name = row()
        .key(format!("track-name-{}", t.id))
        .h(18.0)
        .px(6.0)
        .center_y()
        .radius(5.0)
        .fill(if picked { lk.you } else { k.c.text_base.alpha(0.0) })
        .help(if picked { "Picked: Shift+M mutes the selection on this track alone" } else { "Pick this track, to mute a selection on it alone" })
        .on_press(move |s: &mut Studio| s.picked = Some(pick.clone()))
        .child(text(t.name.clone()).font(k.ty.small_medium).color(if picked { lk.on_you } else { k.c.text_base }));
    let mut head = row()
        .key(format!("track-{}", t.id))
        .absolute(8.0, y)
        .h(24.0)
        .px(3.0)
        .gap(3.0)
        .center_y()
        .radius(7.0)
        // Over the waveform, so it gets a plate of its own to stay readable.
        .fill(lk.lane.alpha(0.9))
        .child(name)
        .child(button(format!("track-mute-{}", t.id), "M", t.mute, "Mute this track", edit::TrackChange { mute: Some(!t.mute), ..change() }))
        .child(button(format!("track-solo-{}", t.id), "S", t.solo, "Solo: only soloed tracks are heard", edit::TrackChange { solo: Some(!t.solo), ..change() }))
        .child(el().w(5.0))
        .child(button(format!("track-down-{}", t.id), "\u{2212}", false, "1 dB quieter", edit::TrackChange { gain_db: Some(level - 1.0), ..change() }))
        .child(figure(format!("{level:+} dB"), 46.0))
        .child(button(format!("track-up-{}", t.id), "+", false, "1 dB louder", edit::TrackChange { gain_db: Some(level + 1.0), ..change() }))
        .child(el().w(5.0))
        .child(button(format!("track-left-{}", t.id), "L", false, "Pan a step to the left", edit::TrackChange { pan: Some(((pan - 0.1) * 10.0).round().max(-10.0) / 10.0), ..change() }))
        .child(figure(pan_text, 26.0))
        .child(button(format!("track-right-{}", t.id), "R", false, "Pan a step to the right", edit::TrackChange { pan: Some(((pan + 0.1) * 10.0).round().min(10.0) / 10.0), ..change() }));
    if !main {
        let id = t.id.clone();
        head = head.child(el().w(5.0)).child(
            row()
                .key(format!("track-rm-{}", t.id))
                .size(18.0, 18.0)
                .radius(5.0)
                .center()
                .fill(k.c.text_base.alpha(0.09))
                .help("Remove this track. Undo brings it back.")
                .on_press(move |s: &mut Studio| remove_track(s, id.clone()))
                .child(icon(&CROSS, 10.0).color(k.c.text_muted)),
        );
    }
    head
}

/// The card for the label under the pointer: who left it, what it marks and what it says.
fn note_card(s: &Studio, doc: &Doc, lk: &Look, w: f32) -> Option<El> {
    let k = &lk.k;
    let note = doc.notes.get(s.hover_note?).filter(|n| !n.hidden)?;
    let color = lk.author(note.agent);
    let x = ((note.start as f64 - s.start) / s.fpp) as f32;
    let cw = 268.0;
    let rate = doc.rate();
    let marks = if note.end > note.start { format!("{}  \u{2192}  {}", timecode(note.start as f64, rate), timecode(note.end as f64, rate)) } else { timecode(note.start as f64, rate) };
    let mut card = col()
        .key("note-card")
        .absolute((x - 2.0).min(w - cw - 8.0).max(8.0), 23.0)
        .w(cw)
        .radius(k.radius.md)
        .clip_rounded(k.radius.md)
        .fill(k.c.layer_03)
        .border(1.0, k.c.border_base)
        // A band in the author's colour, then the note.
        .child(
            row()
                .w_full()
                .h(28.0)
                .px(10.0)
                .gap(6.0)
                .center_y()
                .fill(color.alpha(0.14))
                .child(icon(&BUBBLE, 13.0).color(color))
                .child(text(shown_name(&note.author)).font(k.ty.small_medium).color(color))
                .child(el().grow(1.0))
                .child(text(marks).font(lk.num(k.ty.small)).color(k.c.text_muted)),
        )
        .child(col().w_full().px(10.0).pt(8.0).pb(10.0).child(text(note.text.clone()).font(k.ty.ui).color(k.c.text_base).wrap()));
    for shadow in &k.elevation.floating {
        card = card.box_shadow(*shadow);
    }
    Some(card)
}

/// Opens the card for writing a label: over the selection, or at the playhead when there is none.
fn begin_label(s: &mut Studio) {
    let at = head(s);
    s.labeling = Some(s.selection.filter(|(a, b)| b > a).unwrap_or((at, at)));
    s.label_text = TextEdit::default();
    s.label_text.focused = true;
    s.hover_note = None;
    reveal(s, s.labeling.map_or(at, |(a, _)| a));
}

fn cancel_label(s: &mut Studio) {
    s.labeling = None;
    s.label_text.focused = false;
}

/// Adds the label being written. An empty one is dropped.
fn commit_label(s: &mut Studio) {
    let Some((a, b)) = s.labeling else { return };
    let text = s.label_text.text.trim().to_string();
    if text.is_empty() {
        return cancel_label(s);
    }
    let range = (a.round() as u64, b.round() as u64);
    let made = act_at(s, range, "label", move |paths, opts, range| edit::add_label(paths, opts, &you(), |_, _| Ok((range, text.clone()))).map(|_| ()));
    // If it could not be added the card stays, with the reason in the transport.
    if made.is_some() || s.selection.is_none() && s.notice.as_ref().is_some_and(|n| n.contains("edited the project first")) {
        cancel_label(s);
    }
}

fn remove_label(s: &mut Studio, id: String) {
    s.hover_note = None;
    act(s, move |paths| edit::remove_label(paths, &WriteOpts::default(), &id).map(|_| ()));
}

/// The card a label is written in: what it will mark, and a field for the note.
fn label_composer(s: &Studio, doc: &Doc, lk: &Look, w: f32) -> Option<El> {
    let k = &lk.k;
    let (a, b) = s.labeling?;
    let x = ((a - s.start) / s.fpp) as f32;
    let cw = 300.0;
    let rate = doc.rate();
    let marks = if b > a { format!("{}  \u{2192}  {}", timecode(a, rate), timecode(b, rate)) } else { timecode(a, rate) };
    let field = TextInput::new(&s.label_text)
        .placeholder("Note for this spot")
        .on_edit(|s: &mut Studio| &mut s.label_text)
        .on_submit(|s: &mut Studio, _| commit_label(s))
        .on_key(|s: &mut Studio, e, _| {
            let escape = matches!(e.key, Key::Escape);
            if escape {
                cancel_label(s);
            }
            escape
        })
        .build(k);
    let mut card = col()
        .key("label-composer")
        .absolute((x - 2.0).min(w - cw - 8.0).max(8.0), 23.0)
        .w(cw)
        .radius(k.radius.md)
        .clip_rounded(k.radius.md)
        .fill(k.c.layer_03)
        .border(1.0, k.c.border_base)
        .child(
            row()
                .w_full()
                .h(28.0)
                .px(10.0)
                .gap(6.0)
                .center_y()
                .fill(lk.you.alpha(0.14))
                .child(icon(&BUBBLE, 13.0).color(lk.you))
                .child(text("New label").font(k.ty.small_medium).color(lk.you))
                .child(el().grow(1.0))
                .child(text(marks).font(lk.num(k.ty.small)).color(k.c.text_muted)),
        )
        .child(
            col()
                .w_full()
                .px(10.0)
                .pt(9.0)
                .pb(9.0)
                .gap(7.0)
                .child(field)
                .child(text("Enter adds it \u{b7} Esc cancels").font(k.ty.small).color(k.c.text_faint)),
        );
    for shadow in &k.elevation.floating {
        card = card.box_shadow(*shadow);
    }
    Some(card)
}

/// Reads the kept transcript again: something has listened since it was last read.
fn read_said(s: &mut Studio) {
    if let Some(doc) = &mut s.doc {
        doc.said = said_of(&s.paths, &doc.tl);
    }
    s.words_stamp = words_stamp(&s.paths);
    s.generation += 1;
}

/// A press on a word selects the stretch it was heard over and puts the playhead at its start.
fn pick_word(s: &mut Studio, start: u64, end: u64) {
    s.selection = Some((start as f64, end as f64));
    set_cursor(s, start as f64);
}

fn has_words(s: &Studio) -> bool {
    s.doc.as_ref().is_some_and(|d| d.said.iter().any(|said| !said.words.is_empty()))
}

fn begin_find(s: &mut Studio) {
    cancel_label(s);
    s.finding = true;
    s.find_text.focused = true;
    s.hover_note = None;
}

fn cancel_find(s: &mut Studio) {
    s.finding = false;
    s.find_text.focused = false;
}

/// Goes to the next place the words in the find card were heard, and selects it.
fn find_next(s: &mut Studio) {
    let Some(doc) = &s.doc else { return };
    let wanted: Vec<String> = s.find_text.text.split_whitespace().map(phrase::plain).filter(|w| !w.is_empty()).collect();
    let mut found: Vec<(u64, u64)> = Vec::new();
    for said in doc.said.iter().filter(|said| !wanted.is_empty() && said.words.len() >= wanted.len()) {
        let heard: Vec<String> = said.words.iter().map(|w| phrase::plain(&w.text)).collect();
        for i in 0..=heard.len() - wanted.len() {
            if heard[i..i + wanted.len()] == wanted[..] {
                found.push((said.words[i].start, said.words[i + wanted.len() - 1].end));
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    if found.is_empty() {
        s.found = found;
        if !wanted.is_empty() {
            s.notice = Some(format!("\u{201c}{}\u{201d} is not among the words that were heard.", s.find_text.text.trim()));
        }
        return;
    }
    // The first one from the playhead on, then each one after it, round to the start again.
    let from = s.selection.map_or(s.cursor, |(a, _)| a + 1.0);
    let at = found.iter().position(|f| f.0 as f64 >= from).unwrap_or(0);
    let (a, b) = found[at];
    s.found = found;
    s.found_at = at;
    s.notice = None;
    s.selection = Some((a as f64, b as f64));
    set_cursor(s, a as f64);
    reveal(s, a as f64);
}

/// The card for finding words: a field, and which match is selected.
fn find_card(s: &Studio, lk: &Look, w: f32) -> Option<El> {
    if !s.finding {
        return None;
    }
    let k = &lk.k;
    let cw = 300.0;
    let count = if s.found.is_empty() { String::new() } else { format!("{} of {}", s.found_at + 1, s.found.len()) };
    let field = TextInput::new(&s.find_text)
        .placeholder("Words that were said")
        .on_edit(|s: &mut Studio| &mut s.find_text)
        .on_submit(|s: &mut Studio, _| find_next(s))
        .on_key(|s: &mut Studio, e, _| {
            let escape = matches!(e.key, Key::Escape);
            if escape {
                cancel_find(s);
            }
            escape
        })
        .build(k);
    let mut card = col()
        .key("find-words")
        .absolute(((w - cw) / 2.0).max(8.0), 23.0)
        .w(cw)
        .radius(k.radius.md)
        .clip_rounded(k.radius.md)
        .fill(k.c.layer_03)
        .border(1.0, k.c.border_base)
        .child(
            row()
                .w_full()
                .h(28.0)
                .px(10.0)
                .gap(6.0)
                .center_y()
                .fill(lk.you.alpha(0.14))
                .child(text("Find words").font(k.ty.small_medium).color(lk.you))
                .child(el().grow(1.0))
                .child(text(count).key("find-count").font(lk.num(k.ty.small)).color(k.c.text_muted)),
        )
        .child(col().w_full().px(10.0).pt(9.0).pb(9.0).gap(7.0).child(field).child(text("Enter goes to the next \u{b7} Esc closes").font(k.ty.small).color(k.c.text_faint)));
    for shadow in &k.elevation.floating {
        card = card.box_shadow(*shadow);
    }
    Some(card)
}

/// The command line program, which is beside the window's own. `SCRUBR_CLI` names another.
fn cli() -> PathBuf {
    if let Some(named) = std::env::var_os("SCRUBR_CLI") {
        return PathBuf::from(named);
    }
    let name = if cfg!(windows) { "scrubr.exe" } else { "scrubr" };
    std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join(name))).unwrap_or_else(|| PathBuf::from(name))
}

/// Has the command line program do something that needs the speech model, on its own thread.
/// Its progress lines and its reply come back through the job.
fn start_job(s: &mut Studio, what: String, args: Vec<String>, done: fn(&serde_json::Value) -> String, after: Option<fn(&mut Studio, &serde_json::Value)>) {
    use std::io::{BufRead, Read};
    use std::process::{Command, Stdio};
    let said = Arc::new(Mutex::new(String::new()));
    let (answer, result) = mpsc::channel();
    let lines = said.clone();
    let started = std::thread::Builder::new().name("scrubr-job".into()).spawn(move || {
        let program = cli();
        let mut command = Command::new(&program);
        command.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            // No console window for a console program started by a window program.
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let outcome = (|| -> Result<(String, serde_json::Value), String> {
            let mut child = command.spawn().map_err(|e| format!("{} could not be started ({e}). The command line program has to be beside the window's.", program.display()))?;
            // The reply is read on a thread of its own, so a long one cannot hold the program up
            // while its progress is being read here.
            let mut out = child.stdout.take();
            let reply = std::thread::spawn(move || {
                let mut text = String::new();
                if let Some(out) = &mut out {
                    let _ = out.read_to_string(&mut text);
                }
                text
            });
            if let Some(err) = child.stderr.take() {
                for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                    if let Ok(mut last) = lines.lock() {
                        *last = line;
                    }
                }
            }
            let text = reply.join().unwrap_or_default();
            let _ = child.wait();
            let reply: serde_json::Value = serde_json::from_str(&text).map_err(|_| "The command line program gave no reply.".to_string())?;
            if reply["ok"] == true {
                Ok((done(&reply), reply))
            } else {
                let part = |name: &str| reply["error"][name].as_str().unwrap_or_default().to_string();
                Err(format!("{} {}", part("message"), part("hint")).trim().to_string())
            }
        })();
        let _ = answer.send(outcome);
    });
    match started {
        Ok(_) => {
            s.notice = Some(format!("{what}\u{2026}"));
            s.job = Some(Job { what, said, result, shown: String::new(), after });
        }
        Err(e) => s.notice = Some(format!("Could not start: {e}")),
    }
}

/// Shows a job's progress, and what it came to when it ends. True when the window has changed.
fn watch_job(s: &mut Studio) -> bool {
    let Some(job) = &mut s.job else { return false };
    match job.result.try_recv() {
        Ok(outcome) => {
            let after = job.after;
            s.job = None;
            match outcome {
                Ok((said, reply)) => {
                    s.notice = Some(said);
                    if let Some(after) = after {
                        after(s, &reply);
                    }
                }
                Err(why) => s.notice = Some(why),
            }
            read_said(s);
            true
        }
        Err(mpsc::TryRecvError::Empty) => {
            let line = job.said.lock().map(|line| line.clone()).unwrap_or_default();
            if line == job.shown {
                return false;
            }
            // "transcribe: 0:10.000 of 0:46.010" reads as "Transcribing… 0:10.000 of 0:46.010".
            let rest = line.split_once(": ").map_or(line.as_str(), |(_, rest)| rest).to_string();
            s.notice = Some(format!("{}\u{2026} {rest}", job.what));
            job.shown = line;
            true
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            s.job = None;
            s.notice = Some("It stopped without a result.".into());
            true
        }
    }
}

/// Listens to the selection, or to everything when there is none, and keeps the words. The
/// picked track is heard alone; otherwise the mix.
fn transcribe(s: &mut Studio) {
    if s.doc.is_none() || s.job.is_some() {
        return;
    }
    let mut args = vec!["transcribe".to_string(), s.paths.project.to_string_lossy().into_owned(), "--json".to_string()];
    if let Some((a, b)) = s.selection.filter(|(a, b)| b > a) {
        args.extend(["--range".to_string(), format!("{}f..{}f", a.round() as u64, b.round() as u64)]);
    }
    let mut what = "Transcribing".to_string();
    if let Some((_, track)) = picked_track(s) {
        args.extend(["--track".to_string(), track.id.clone()]);
        what = format!("Transcribing {}", track.name);
    }
    start_job(s, what, args, |reply| format!("Heard {} words.", reply["word_count"]), None);
}

/// The project and, when part of the timeline is selected, that part as a `--range`.
fn tool_args(s: &Studio, command: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = command.iter().map(|part| part.to_string()).collect();
    args.insert(1, s.paths.project.to_string_lossy().into_owned());
    if let Some((a, b)) = s.selection.filter(|(a, b)| b > a) {
        args.extend(["--range".to_string(), format!("{}f..{}f", a.round() as u64, b.round() as u64)]);
    }
    args.push("--json".to_string());
    args
}

/// Shortens every long pause in the selection, or in everything, as `scrubr tighten` does:
/// one edit, undone in one step.
fn tighten_pauses(s: &mut Studio) {
    if s.job.is_some() || busy_adding(s) {
        return;
    }
    let Some(version) = s.doc.as_ref().map(|doc| doc.project.version) else { return };
    let mut args = tool_args(s, &["tighten"]);
    args.extend(["--if-version".to_string(), version.to_string(), "--author".to_string(), "you".to_string()]);
    s.hold_reloads = false;
    start_job(
        s,
        "Tightening pauses".to_string(),
        args,
        |reply| format!("Shortened {} pauses to {:.1} s each: {:.1} s removed.", reply["pause_count"], reply["to_seconds"].as_f64().unwrap_or(0.0), reply["removed_seconds"].as_f64().unwrap_or(0.0)),
        None,
    );
}

/// Reads back every join the edits have left, as `scrubr analyze --joins` does, and goes to
/// the first that does not sit in quiet.
fn check_joins(s: &mut Studio) {
    if s.doc.is_none() || s.job.is_some() {
        return;
    }
    let args = tool_args(s, &["analyze", "--joins"]);
    start_job(
        s,
        "Checking joins".to_string(),
        args,
        |reply| match (reply["join_count"].as_u64().unwrap_or(0), reply["joins_to_check"].as_u64().unwrap_or(0)) {
            (0, _) => "There are no joins here: nothing has been cut or put in.".to_string(),
            (1, 0) => "The one join sits in quiet.".to_string(),
            (joins, 0) => format!("All {joins} joins sit in quiet."),
            (joins, check) => format!("{check} of {joins} joins to check. The playhead is at the first."),
        },
        Some(|s, reply| {
            let (Some(at), Some(rate)) = (first_to_check(reply).and_then(|j| j["at"].as_f64()), s.doc.as_ref().map(|d| d.rate() as f64)) else { return };
            set_cursor(s, at * rate);
            // A little before it in view, so what leads up to the join is seen and heard.
            let w = s.wave[0] as f64;
            if at * rate < s.start || at * rate > s.start + w * s.fpp {
                s.start = at * rate - 0.3 * w * s.fpp;
                clamp(s);
            }
        }),
    );
}

fn first_to_check(reply: &serde_json::Value) -> Option<&serde_json::Value> {
    reply["joins"].as_array()?.iter().find(|j| j["check"] == true)
}

/// Measures the loudness of the selection, or of everything.
fn measure_loudness(s: &mut Studio) {
    if s.doc.is_none() || s.job.is_some() {
        return;
    }
    let args = tool_args(s, &["analyze", "--loudness"]);
    start_job(
        s,
        "Measuring".to_string(),
        args,
        |reply| {
            let l = &reply["loudness"];
            let of = if reply["range"].is_object() { "The selection" } else { "The timeline" };
            match l["integrated_lufs"].as_f64() {
                Some(lufs) => format!("{of} is at {lufs:.1} LUFS, with a true peak of {:.1} dBTP and a range of {:.1} LU.", l["true_peak_dbtp"].as_f64().unwrap_or(0.0), l["range_lu"].as_f64().unwrap_or(0.0)),
                None => format!("{of} is too quiet to measure."),
            }
        },
        None,
    );
}

/// The words the selection covers, as a phrase, with the track they were heard on if that was
/// one track alone.
fn chosen_words(s: &Studio) -> Option<(String, Option<String>)> {
    let doc = s.doc.as_ref()?;
    let (a, b) = s.selection.filter(|(a, b)| b > a)?;
    let said = match picked_track(s) {
        Some((t, _)) => doc.said_for(Lane::Track(t), false),
        None => doc.said_for(Lane::Track(0), false),
    }?;
    let inside: Vec<&str> = said.words.iter().filter(|w| w.start as f64 >= a - 1.0 && w.end as f64 <= b + 1.0).map(|w| w.text.as_str()).collect();
    (!inside.is_empty()).then(|| (inside.join(" "), said.track.map(|t| doc.tl.tracks()[t].id.clone())))
}

/// Cuts the selected words out, the way `scrubr cut --phrase` does: at the quietest moment
/// either side, and only if listening across the join finds it clean.
fn cut_words(s: &mut Studio) {
    let Some((phrase, heard_on)) = chosen_words(s) else { return };
    if s.job.is_some() || busy_adding(s) {
        return;
    }
    let (Some(doc), Some((a, b))) = (&s.doc, s.selection) else { return };
    // A little either side of the selection is listened to, so the pauses are in reach.
    let margin = 1.5 * doc.rate() as f64;
    let within = format!("{}f..{}f", (a - margin).max(0.0).round() as u64, ((b + margin).round() as u64).min(doc.tl.total()));
    let mut args = vec![
        "cut".to_string(),
        s.paths.project.to_string_lossy().into_owned(),
        "--phrase".to_string(),
        phrase,
        "--within".to_string(),
        within,
        "--if-version".to_string(),
        doc.project.version.to_string(),
        "--author".to_string(),
        "you".to_string(),
        "--json".to_string(),
    ];
    if let Some(track) = heard_on {
        args.extend(["--heard-on".to_string(), track]);
    }
    s.hold_reloads = false;
    start_job(s, "Cutting".to_string(), args, |reply| format!("Cut \u{201c}{}\u{201d}. The join was listened to and is clean.", reply["phrase"].as_str().unwrap_or_default()), None);
}

/// Selects what a label marks, or goes to it when it marks a point.
fn pick_note(s: &mut Studio, i: usize) {
    let Some((a, b)) = s.doc.as_ref().and_then(|d| d.notes.get(i)).map(|n| (n.start as f64, n.end as f64)) else { return };
    set_cursor(s, a);
    s.selection = (b > a).then_some((a, b));
}

fn wave_area(s: &Studio, doc: &Doc, lk: &Look, scale: f32) -> El {
    let k = &lk.k;
    let [w, h] = s.wave;
    let mut area = el()
        .key("wave")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .on_layout(|s: &mut Studio, r| {
            let size = [r[2], r[3]];
            if s.wave != size {
                s.wave = size;
                if s.fit_pending { fit(s) } else { clamp(s) }
            }
        })
        .on_wheel(|s: &mut Studio, e, _| {
            // With Ctrl or Cmd held the wheel zooms about the pointer: one notch is about a third.
            if e.mods.ctrl || e.mods.cmd {
                zoom_about(s, e.x as f64, 2f64.powf((e.dy as f64 / 240.0).clamp(-2.0, 2.0)));
            } else {
                pan(s, -(e.dx + e.dy) as f64);
            }
            true
        })
        .on_magnify(|s: &mut Studio, e, _| {
            // A pinch on macOS, and Ctrl + wheel everywhere else, which Rune hands over as a
            // pinch. There a wheel turned away from the hand arrives as a pinch inwards, the
            // reverse of what a wheel does in every other program, so it is turned round here.
            let delta = if cfg!(target_os = "macos") { e.delta as f64 } else { -(e.delta as f64) };
            // The same step in and out, so a notch one way undoes a notch the other.
            let factor = if delta >= 0.0 { 1.0 + delta } else { 1.0 / (1.0 - delta) };
            zoom_about(s, e.x as f64, factor);
            true
        })
        .on_drag(|s: &mut Studio, d, _| {
            // The press picks the track whose lane it is in.
            let lanes = s.doc.as_ref().map(|doc| doc.lanes()).unwrap_or_default();
            let lane = ((d.start_y / s.wave[1].max(1.0) * lanes.len() as f32).floor().max(0.0) as usize).min(lanes.len().saturating_sub(1));
            s.picked = match lanes.get(lane) {
                Some(Lane::Track(t)) => s.doc.as_ref().map(|doc| doc.tl.tracks()[*t].id.clone()),
                _ => None,
            };
            // A drag selects a range; a press without one moves the cursor.
            let frame = |x: f32| (s.start + x as f64 * s.fpp).clamp(0.0, total(s));
            if (d.x - d.start_x).abs() > 2.0 {
                let (a, b) = (frame(d.start_x), frame(d.x));
                s.selection = Some((a.min(b), a.max(b)));
            } else if d.phase == Phase::Up {
                let at = frame(d.x);
                set_cursor(s, at);
                s.selection = None;
            }
        });
    if w <= 0.0 || h <= 0.0 {
        return area;
    }
    let version = (s.generation, s.start.to_bits(), s.fpp.to_bits(), w.to_bits(), h.to_bits(), scale.to_bits(), s.selection.map(|(a, b)| (a.to_bits(), b.to_bits())), s.selected, k.dark);
    area = area.child(memo("wave-shape", version, || canvas(wave_shape(s, doc, lk, w, h, scale)).absolute(0.0, 0.0).size(w, h)));
    if s.spectro.on {
        area = area.child(spectrogram_sheet(s, k, w, h));
    }
    if let Some((a, b)) = s.selection {
        let (x0, x1) = ((((a - s.start) / s.fpp) as f32).max(0.0), (((b - s.start) / s.fpp) as f32).min(w));
        if x1 > x0 {
            area = area.child(el().key("selection").absolute(x0, 0.0).size(x1 - x0, h).fill(lk.you.alpha(0.10)).border_sides([0.0, 1.0, 0.0, 1.0], lk.you.alpha(0.75)));
        }
    }
    let lanes = doc.lanes();
    let picked = picked_track(s).map(|(t, _)| t);
    // The selection is stronger in the picked track's lane: a mute can be made there alone.
    if let (Some((a, b)), Some(t)) = (s.selection, picked)
        && let Some(i) = lanes.iter().position(|lane| *lane == Lane::Track(t))
    {
        let (x0, x1) = ((((a - s.start) / s.fpp) as f32).max(0.0), (((b - s.start) / s.fpp) as f32).min(w));
        let lane_h = h / lanes.len() as f32;
        if x1 > x0 {
            area = area.child(el().key("selection-track").absolute(x0, lane_h * i as f32).size(x1 - x0, lane_h).fill(lk.you.alpha(0.14)));
        }
    }
    for (i, lane) in lanes.iter().enumerate() {
        let y = h / lanes.len() as f32 * i as f32 + 18.0;
        match *lane {
            Lane::Channel(c) if lanes.len() == 2 => {
                let name = ["L", "R"][c];
                area = area.child(
                    row()
                        .key(format!("lane-{name}"))
                        .absolute(8.0, y)
                        .size(18.0, 18.0)
                        .radius(5.0)
                        .center()
                        .fill(k.c.text_base.alpha(0.07))
                        .child(text(name).font(k.ty.small_medium).color(k.c.text_muted)),
                );
            }
            Lane::Channel(_) => {}
            Lane::Track(t) => area = area.child(track_head(lk, &doc.tl.tracks()[t], y, t == 0, picked == Some(t))),
        }
    }
    // What was said, along the audio it was said in: a strip at the foot of each lane that has
    // words. Close in, the words themselves, each on a pill as long as the word was heard for and
    // a press away from being selected. Further out, thin marks where there is speech.
    let across = w as f64 * s.fpp / doc.rate() as f64;
    let right = s.start + w as f64 * s.fpp;
    let x_of = |t: u64| ((t as f64 - s.start) / s.fpp) as f32;
    for (i, lane) in lanes.iter().enumerate() {
        let last = i + 1 == lanes.len();
        let Some(said) = doc.said_for(*lane, last) else { continue };
        let y = h / lanes.len() as f32 * (i + 1) as f32 - if last { MARKS_H + 20.0 } else { 20.0 };
        let first = said.words.partition_point(|word| (word.end as f64) < s.start);
        let shown = said.words[first..].iter().take_while(|word| (word.start as f64) <= right);
        if across > STARTS_SECONDS {
            let mut marks = Shape::new();
            let mut run: Option<(f32, f32)> = None;
            for word in shown {
                let x0 = x_of(word.start);
                let x1 = x_of(word.end).max(x0 + 1.0);
                run = match run {
                    Some((a, b)) if x0 <= b + 1.5 => Some((a, b.max(x1))),
                    Some((a, b)) => {
                        marks = marks.line([a, 2.0], [b, 2.0], 2.0, LineCap::Butt, k.c.text_faint);
                        Some((x0, x1))
                    }
                    None => Some((x0, x1)),
                };
            }
            if let Some((a, b)) = run {
                marks = marks.line([a, 2.0], [b, 2.0], 2.0, LineCap::Butt, k.c.text_faint);
            }
            area = area.child(canvas(marks).key(format!("said-{i}")).absolute(0.0, y + 12.0).size(w, 4.0));
            continue;
        }
        let every = across <= WORDS_SECONDS;
        let pause = (PAUSE_SECONDS * doc.rate() as f64) as u64;
        let mut free = f32::NEG_INFINITY;
        // Three layers, so a word that is longer written than said can lie over its neighbours'
        // pills and still be read: plates under such words, then the pills, then the words.
        let (mut plates, mut pills, mut labels) = (Vec::new(), Vec::new(), Vec::new());
        for (n, word) in shown.enumerate() {
            let at = first + n;
            let heard = x_of(word.start);
            if every {
                let chosen = s.selection.is_some_and(|(a, b)| b > a && word.start as f64 >= a - 1.0 && word.end as f64 <= b + 1.0);
                let (start, end) = (word.start, word.end);
                // The pill is the word's own stretch of the waveform, start to end. One point is
                // left between it and the next, since words are heard back to back.
                let x0 = heard.max(-8.0);
                let x1 = x_of(word.end).min(w + 8.0);
                let wide = (x1 - x0 - 1.0).max(3.0);
                let tint = if chosen { lk.you } else { k.c.text_base.alpha(if at % 2 == 0 { 0.22 } else { 0.12 }) };
                pills.push(
                    el().key(format!("span-{i}-{at}"))
                        .absolute(x0, y)
                        .size(wide, 16.0)
                        .radius(4.0)
                        .fill(lk.lane.alpha(0.92))
                        .on_press(move |s: &mut Studio| pick_word(s, start, end))
                        .child(el().size(wide, 16.0).radius(4.0).fill(tint)),
                );
                // The model was not sure of a faint word.
                let ink = if chosen { lk.on_you } else if word.probability < 0.5 { k.c.text_faint } else { k.c.text_muted };
                let needs = word.text.chars().count() as f32 * 5.9 + 6.0;
                let centred = x0 + (wide - needs) / 2.0;
                if needs <= wide && centred >= free {
                    // Said slowly enough to be written inside its pill.
                    labels.push(
                        row()
                            .key(format!("word-{i}-{at}"))
                            .absolute(x0, y)
                            .size(wide, 16.0)
                            .center()
                            .on_press(move |s: &mut Studio| pick_word(s, start, end))
                            .child(text(word.text.clone()).font(k.ty.small).color(ink)),
                    );
                    free = x0 + wide;
                    continue;
                }
                // Longer written than said: centred on its pill, or just after the word before it
                // when speech is too quick for both to fit. A sentence reads as a sentence, a
                // little late. A word that would end up far from its audio is left out, and its
                // pill still shows where it was said.
                let x = centred.max(free).max(if x0 >= 0.0 { 0.0 } else { f32::NEG_INFINITY });
                if x - centred > 220.0 {
                    continue;
                }
                if !chosen {
                    plates.push(el().key(format!("plate-{i}-{at}")).absolute(x, y).size(needs, 16.0).radius(4.0).fill(lk.lane.alpha(0.9)));
                }
                labels.push(
                    row()
                        .key(format!("word-{i}-{at}"))
                        .absolute(x, y)
                        .h(16.0)
                        .px(3.0)
                        .center_y()
                        .on_press(move |s: &mut Studio| pick_word(s, start, end))
                        .child(text(word.text.clone()).font(k.ty.small).color(ink)),
                );
                free = x + needs + 3.0;
                continue;
            }
            let (x, label) = {
                // Further out only the word after each pause is written, where it was heard.
                let starts = at == 0 || word.start >= said.words[at - 1].end + pause;
                if !starts || heard < free {
                    continue;
                }
                (heard, format!("{}\u{2026}", word.text.trim_end_matches(|c: char| c.is_ascii_punctuation())))
            };
            free = x + label.chars().count() as f32 * 5.9 + 9.0;
            let chosen = s.selection.is_some_and(|(a, b)| b > a && word.start as f64 >= a - 1.0 && word.end as f64 <= b + 1.0);
            let (start, end) = (word.start, word.end);
            area = area.child(
                row()
                    .key(format!("word-{i}-{at}"))
                    .absolute(x, y)
                    .h(16.0)
                    .px(3.0)
                    .center_y()
                    .radius(4.0)
                    .fill(if chosen { lk.you } else { lk.lane.alpha(0.9) })
                    .on_press(move |s: &mut Studio| pick_word(s, start, end))
                    // The model was not sure of a faint word.
                    .child(text(label).font(k.ty.small).color(if chosen { lk.on_you } else if word.probability < 0.5 { k.c.text_faint } else { k.c.text_muted })),
            );
        }
        for layer in plates.into_iter().chain(pills).chain(labels) {
            area = area.child(layer);
        }
    }
    if doc.magnify() > 1.05 {
        // Say so when the lanes are not at true scale.
        let note = format!("peak {:.1} dB · shown \u{00d7}{:.1}", 20.0 * doc.peak.log10(), doc.magnify());
        area = area.child(
            row()
                .key("magnify")
                .pin(None, Some(18.0), Some(8.0), None)
                .h(18.0)
                .px(7.0)
                .center_y()
                .radius(5.0)
                .fill(k.c.text_base.alpha(0.07))
                .child(text(note).font(lk.num(k.ty.small)).color(k.c.text_muted)),
        );
    }
    area = area.child(marks(s, doc, lk, w, h));
    // Labels: a speech bubble in the strip the splice marks share, where what it marks begins,
    // and a line over the range it marks. The pointer over a bubble shows the note in a card.
    let view_end = s.start + w as f64 * s.fpp;
    let mut last_x = f32::NEG_INFINITY;
    let mut shown: Vec<(usize, &Note)> = doc.notes.iter().enumerate().filter(|(_, n)| !n.hidden && (n.end as f64) >= s.start && (n.start as f64) <= view_end).collect();
    shown.sort_by_key(|(_, n)| n.start);
    for (i, note) in shown {
        let color = lk.author(note.agent);
        let x_of = |t: u64| ((t as f64 - s.start) / s.fpp) as f32;
        let (x0, x1) = (x_of(note.start).max(0.0), x_of(note.end).min(w));
        let open = s.hover_note == Some(i);
        if x1 - x0 > 2.0 {
            // While its card is open the whole range is tinted, so it is plain what the note is about.
            area = area.child(el().key(format!("note-span-{i}")).absolute(x0, 0.0).size(x1 - x0, if open { h } else { 2.0 }).fill(color.alpha(if open { 0.10 } else { 0.8 })));
        }
        // Bubbles that would sit on top of each other step to the right.
        let x = (x0 - 2.0).max(last_x + 23.0).min(w - 22.0);
        last_x = x;
        area = area.child(
            row()
                .key(format!("note-{i}"))
                .absolute(x, 2.0)
                .size(21.0, 17.0)
                .radius(6.0)
                .center()
                .fill(if open { color } else { lk.lane })
                .border(1.0, color.alpha(0.6))
                .on_hover_change(move |s: &mut Studio, inside, _| {
                    if inside {
                        s.hover_note = Some(i);
                    } else if s.hover_note == Some(i) {
                        s.hover_note = None;
                    }
                })
                .on_press(move |s: &mut Studio| pick_note(s, i))
                .child(icon(&BUBBLE, 12.0).color(if open { lk.lane } else { color })),
        );
    }
    // The playhead: a line with a handle at its top, moved by a live value as the audio plays.
    let handle = Shape::new().fill(vec![[0.0, 0.0], [11.0, 0.0], [5.5, 8.0]], k.c.text_base);
    area = area.child(
        el().key("playhead")
            .absolute(-0.75, 0.0)
            .size(1.5, h)
            .fill(k.c.text_base)
            .live_offset(&s.playhead)
            .child(canvas(handle).key("playhead-handle").absolute(-4.75, 0.0).size(11.0, 8.0)),
    );
    if let Some(card) = callout(s, doc, lk, w) {
        area = area.child(card);
    }
    if let Some(card) = note_card(s, doc, lk, w) {
        area = area.child(card);
    }
    if let Some((a, b)) = s.labeling {
        // What the new label will mark: its range, or a line where it marks a point.
        let (x0, x1) = ((((a - s.start) / s.fpp) as f32).max(0.0), (((b - s.start) / s.fpp) as f32).min(w));
        area = area.child(el().key("label-range").absolute(x0, 0.0).size((x1 - x0).max(1.5), h).fill(lk.you.alpha(if x1 - x0 > 1.5 { 0.10 } else { 0.8 })));
    }
    if let Some(card) = label_composer(s, doc, lk, w) {
        area = area.child(card);
    }
    if let Some(card) = find_card(s, lk, w) {
        area = area.child(card);
    }
    area
}

/// Level edits as bars in their author's colour, along the foot of the lanes.
fn marks(s: &Studio, doc: &Doc, lk: &Look, w: f32, h: f32) -> El {
    let end = s.start + w as f64 * s.fpp;
    let x_of = |t: u64| ((t as f64 - s.start) / s.fpp) as f32;
    let mut shape = Shape::new();
    for m in doc.levels.iter().filter(|m| (m.end as f64) > s.start && (m.start as f64) < end) {
        let (x0, x1) = (x_of(m.start).max(0.0), x_of(m.end).min(w));
        let color = lk.author(m.agent).alpha(if s.selected == Some(m.edit) { 1.0 } else { 0.75 });
        let (top, bottom) = (4.0, 11.0);
        match m.kind {
            LevelKind::Gain => shape = shape.fill(vec![[x0, top + 2.0], [x1, top + 2.0], [x1, bottom - 2.0], [x0, bottom - 2.0]], color),
            LevelKind::FadeIn => shape = shape.fill(vec![[x0, bottom], [x1, top], [x1, bottom]], color),
            LevelKind::FadeOut => shape = shape.fill(vec![[x0, top], [x1, bottom], [x0, bottom]], color),
            LevelKind::Mute => {
                shape = shape.line([x0, bottom], [x1, bottom], 1.0, LineCap::Butt, color);
                let mut x = x0;
                while x + 3.0 <= x1 {
                    shape = shape.line([x, bottom], [x + 3.0, top], 1.0, LineCap::Butt, color);
                    x += 5.0;
                }
            }
        }
    }
    canvas(shape).key("marks").absolute(0.0, h - MARKS_H).size(w, MARKS_H)
}

/// A group of small tools on a shared plate.
fn plate(lk: &Look, tools: Vec<El>) -> El {
    tools.into_iter().fold(row().padding(3.0).gap(2.0).center_y().radius(lk.k.radius.md + 2.0).fill(lk.k.c.text_base.alpha(0.05)), |plate, tool| plate.child(tool))
}

/// A button for one command: its name is the tooltip, with the command's own shortcut and state.
fn tool(s: &Studio, k: &Kit, i: &'static Icon, label: &str) -> El {
    let c = command(label);
    IconButton::new(i)
        .variant(IconButtonVariant::GhostMuted)
        .disabled(!c.available(s))
        .help(c.label.to_string())
        .shortcut(c.keys.label())
        .on_press(c.run)
        .build(k)
        .key(format!("tool-{}", c.label))
}

/// Level meters for what is playing: a bar for each channel on a dB scale, a tick at the highest
/// recent level, and a light at the end that stays on for a moment after a sample at full scale.
fn meters(s: &Studio, doc: &Doc, lk: &Look, w: f32) -> El {
    let k = &lk.k;
    let m = &s.meters;
    let stereo = doc.project.channels > 1;
    // Two bars have the scale between them, so the block is the same above and below its middle
    // and lines up with the buttons beside it. One bar has the scale under it.
    let (bar_h, pitch, scale_y, height) = if stereo { (8.0, 22.0, 10.0, 30.0) } else { (10.0, 0.0, 12.0, 22.0) };
    // Room at the left for the channel's letter, or for the first figure of the scale, which
    // is centred on the bar's end.
    let x0 = if stereo { 14.0 } else { 10.0 };
    let part = |db: f32| (db - METER_FLOOR) / -METER_FLOOR;
    let small = TextStyle { size: 9.0, weight: 500.0, line_height: 10.0, ..lk.num(k.ty.small) };
    let stops = [(0.0, lk.meter[0]), (part(-20.0), lk.meter[0]), (part(-12.0), lk.meter[1]), (part(-6.0), lk.meter[2]), (1.0, lk.meter[3])];
    let mut plate = el().key("meters").size(x0 + w + 28.0, height);
    for c in 0..if stereo { 2 } else { 1 } {
        let y = c as f32 * pitch;
        if stereo {
            plate = plate.child(text(["L", "R"][c]).absolute(1.0, y + bar_h / 2.0 - 5.0).font(TextStyle { weight: 650.0, ..small }).color(k.c.text_muted));
        }
        plate = plate
            .child(
                el().key(format!("meter-{c}"))
                    .absolute(x0, y)
                    .size(w, bar_h)
                    .radius(bar_h / 2.0)
                    .clip_rounded(bar_h / 2.0)
                    .child(el().absolute(0.0, 0.0).size(w, bar_h).linear_gradient(FRAC_PI_2, stops))
                    // The unlit part: the same colours, mostly covered. The cover slides right as the level rises.
                    .child(el().key(format!("meter-cover-{c}")).absolute(0.0, 0.0).size(w, bar_h).fill(lk.page.alpha(0.84)).live_offset(&m.cover[c]))
                    .child(el().key(format!("meter-hold-{c}")).absolute(-1.5, 0.0).size(1.5, bar_h).fill(k.c.text_base).live_offset(&m.tick[c])),
            )
            .child(
                el().absolute(x0 + w + 3.0, y)
                    .size(5.0, bar_h)
                    .radius(1.5)
                    .fill(k.c.text_base.alpha(0.10))
                    .child(el().key(format!("meter-clip-{c}")).absolute(0.0, 0.0).size(5.0, bar_h).radius(1.5).fill(lk.meter[3]).live_opacity(&m.clip[c])),
            );
    }
    // A narrow meter names fewer marks.
    let marks: &[f32] = if w >= 140.0 { &[-60.0, -48.0, -36.0, -24.0, -12.0, -6.0, 0.0] } else { &[-48.0, -24.0, -12.0, -6.0, 0.0] };
    for &db in marks {
        let x = part(db) * w;
        // Name a mark only where its neighbours leave room for the figures.
        let named = if db == -6.0 { w >= 190.0 } else if db == -12.0 { w >= 110.0 } else { true };
        if !named {
            continue;
        }
        let label = if db == 0.0 { "0".to_string() } else { format!("\u{2212}{}", -db as i32) };
        plate = plate.child(row().absolute(x0 + x - 14.0, scale_y).size(28.0, 10.0).center().child(text(label).font(small).color(k.c.text_faint)));
    }
    plate.child(text("dB").absolute(x0 + w + 8.0, scale_y).font(small).color(k.c.text_faint))
}

fn transport(s: &Studio, doc: &Doc, lk: &Look) -> El {
    let k = &lk.k;
    let rate = doc.rate();
    let (meter_w, total_fits) = meter_room(s);
    let counter = TextStyle { size: 27.0, weight: 600.0, line_height: 30.0, tracking: -0.4, ..lk.num(k.ty.ui) };
    // The right of the row speaks only when there is something to say. The last edit is not
    // repeated here: it heads the activity log. A sound device that failed is worth a word;
    // audio switched off on purpose is not.
    let status = s.notice.clone().or_else(|| (s.player.is_none() && !s.no_audio && !s.audio_note.is_empty()).then(|| s.audio_note.clone()));
    // Word of an outside edit carries its author's colour.
    let dot = match (&s.fleeting, doc.project.head_index().map(|i| &doc.project.edits[i])) {
        (Some(_), Some(e)) => lk.author(is_agent(&e.author)),
        _ => k.c.text_faint,
    };
    let playing = s.player.as_ref().is_some_and(Player::is_playing);
    row()
        .key("transport")
        .w_full()
        .h(TRANSPORT_H)
        .px(6.0)
        .gap(14.0)
        .center_y()
        .child(
            row()
                .key("play")
                .size(42.0, 42.0)
                .radius(21.0)
                .center()
                .fill(lk.you)
                .help(if playing { "Pause" } else { "Play" })
                .on_press(toggle_play)
                .child(icon(if playing { &PAUSE } else { &PLAY }, 18.0).color(lk.on_you)),
        )
        .child(plate(lk, vec![tool(s, k, &PREV, "Previous splice"), tool(s, k, &NEXT, "Next splice")]))
        .child(
            row()
                .gap(10.0)
                .items_end()
                // The placeholder sets the box a live text draws in: as wide as the longest time.
                .child(text(timecode(doc.tl.total() as f64, rate)).key("counter").live_text(&s.clock).font(counter).color(k.c.text_base))
                .child(if total_fits { text(format!("/ {}", timecode(doc.tl.total() as f64, rate))).font(lk.num(k.ty.ui)).color(k.c.text_faint).pb(3.0) } else { el() }),
        )
        .child(meters(s, doc, lk, meter_w))
        .child(el().w(4.0))
        .child(plate(lk, vec![tool(s, k, &SCISSORS, "Cut selection"), tool(s, k, &MUTE, "Mute selection")]))
        .child(plate(lk, vec![tool(s, k, &ZOOM_OUT, "Zoom out"), tool(s, k, &ZOOM_IN, "Zoom in"), tool(s, k, &FIT, "Fit timeline")]))
        .child(el().grow(1.0))
        .child(match status {
            Some(status) => row()
                .key("notice")
                .gap(7.0)
                .center_y()
                .min_w(0.0)
                .shrink()
                .child(el().size(7.0, 7.0).radius(4.0).fill(dot))
                // The line is cut short when the window is narrow; the whole of it is in the tooltip.
                .help(status.clone())
                .child(text(status).font(lk.num(k.ty.small)).color(k.c.text_muted).truncate()),
            None => el(),
        })
}

// ---------------------------------------------------------------- the activity log

fn avatar(lk: &Look, agent: bool) -> El {
    let color = lk.author(agent);
    row().size(24.0, 24.0).radius(12.0).center().fill(color.alpha(0.16)).child(icon(if agent { &AGENT } else { &PERSON }, 14.0).color(color))
}

fn chip(lk: &Look, label: &str, color: Color) -> El {
    row().h(18.0).px(7.0).center_y().radius(5.0).fill(color.alpha(0.13)).child(text(label.to_string()).font(lk.k.ty.small_medium).color(color))
}

fn small_button(k: &Kit, label: &str, key: String, on_press: impl Fn(&mut Studio) + 'static) -> El {
    Button::new(label.to_string()).variant(ButtonVariant::Neutral).control_size(ControlSize::Small).on_press(on_press).build(k).key(key)
}

fn activity(s: &Studio, doc: &Doc, lk: &Look) -> El {
    let k = &lk.k;
    let head = doc.project.head_index();
    let mut list = col().key("activity-list").w_full().gap(14.0);
    for (g, group) in doc.groups.iter().enumerate().rev() {
        let color = lk.author(group.agent);
        let mut rows = col().w_full().gap(2.0);
        // An agent's run gets a header: the session, and one button to take all of it back.
        if let Some(session) = group.session.clone().filter(|_| group.agent) {
            let mut title = row()
                .w_full()
                .h(24.0)
                .px(8.0)
                .gap(6.0)
                .center_y()
                .child(text(format!("Session {session}")).font(k.ty.small_medium).color(color))
                .child(text(format!("{} edit{}", group.edits.len(), if group.edits.len() == 1 { "" } else { "s" })).font(lk.num(k.ty.small)).color(k.c.text_faint))
                .child(el().grow(1.0));
            if group.revertable {
                title = title.child(small_button(k, "Undo session", format!("undo-session-{g}"), move |s| {
                    let session = session.clone();
                    act(s, move |paths| edit::revert(paths, &WriteOpts::default(), &you(), &Revert::Session(session)).map(|_| ()));
                }));
            }
            rows = rows.child(title);
        }
        for &i in group.edits.iter().rev() {
            let e = &doc.project.edits[i];
            let state = &doc.states[i];
            let live = *state == EditState::Applied;
            let chosen = s.selected == Some(i);
            let (label, glyph) = kind(&e.op);
            let (chip_label, chip_color) = if live { (label, color) } else { ("Undone", k.c.text_faint) };
            let mut top = row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(text(shown_name(&e.author)).font(k.ty.small_medium).color(if live { color } else { k.c.text_faint }))
                .child(text(e.id.clone()).font(lk.num(k.ty.small)).color(k.c.text_faint));
            if head == Some(i) {
                top = top.child(chip(lk, "head", k.c.text_muted));
            }
            top = top.child(el().grow(1.0)).child(text(ago(e.time.as_deref())).font(lk.num(k.ty.small)).color(k.c.text_faint));
            let bottom = row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(icon(glyph, 14.0).color(if live { k.c.text_muted } else { k.c.text_faint }))
                .child(text(describe(e, &doc.project)).font(lk.num(k.ty.ui)).color(if live { k.c.text_base } else { k.c.text_faint }).truncate().grow(1.0).min_w(0.0))
                .child(chip(lk, chip_label, chip_color));
            let mut body = col().grow(1.0).min_w(0.0).gap(3.0).child(top).child(bottom);
            if chosen {
                // The row's own undo: take this one edit out, or put it back.
                let action = match state {
                    EditState::Applied if i > 0 => {
                        let id = e.id.clone();
                        Some(small_button(k, "Undo this edit", format!("undo-{}", e.id), move |s| {
                            let id = id.clone();
                            act(s, move |paths| edit::revert(paths, &WriteOpts::default(), &you(), &Revert::Edits(vec![id])).map(|_| ()));
                        }))
                    }
                    EditState::Reverted(by) => {
                        let by = by.clone();
                        Some(small_button(k, "Put it back", format!("restore-{}", e.id), move |s| {
                            let by = by.clone();
                            act(s, move |paths| edit::revert(paths, &WriteOpts::default(), &you(), &Revert::Edits(vec![by])).map(|_| ()));
                        }))
                    }
                    _ => None,
                };
                if let Some(action) = action {
                    body = body.child(row().w_full().pt(4.0).child(action));
                }
            }
            let mut item = row()
                .key(format!("edit-{}", e.id))
                .w_full()
                .px(8.0)
                .py(8.0)
                .gap(10.0)
                .radius(k.radius.md)
                .child(avatar(lk, group.agent).opacity(if live { 1.0 } else { 0.45 }))
                .child(body)
                .on_press(move |s: &mut Studio| select(s, i));
            if chosen {
                item = item.fill(color.alpha(0.10));
            }
            rows = rows.child(k.interactive(item));
        }
        // The bracket down the side is one author's run, in their colour.
        list = list.child(row().key(format!("group-{g}")).w_full().gap(6.0).child(el().w(2.0).self_stretch().radius(1.0).fill(color.alpha(0.8))).child(rows.grow(1.0).min_w(0.0)));
    }
    list
}

fn labels(doc: &Doc, lk: &Look) -> El {
    let k = &lk.k;
    let add = command("Add label\u{2026}");
    let mut list = col().key("label-list").w_full().gap(2.0).child(
        row()
            .w_full()
            .pb(6.0)
            .gap(8.0)
            .center_y()
            .child(small_button(k, "Add label", "label-add".into(), add.run))
            .child(text(format!("{} \u{b7} at the playhead, or over the selection", add.keys.label())).font(k.ty.small).color(k.c.text_faint).truncate()),
    );
    if doc.notes.is_empty() {
        return list.child(text("No labels yet. Agents leave them with `scrubr label add`.").font(k.ty.small).color(k.c.text_faint).wrap());
    }
    for (i, note) in doc.notes.iter().enumerate() {
        let color = lk.author(note.agent);
        let at = note.start as f64;
        let place = match (note.hidden, note.end > note.start) {
            (true, _) => "audio removed".to_string(),
            (false, true) => format!("{} · {}", timecode(at, doc.rate()), duration((note.end - note.start) as f64, doc.rate())),
            (false, false) => timecode(at, doc.rate()),
        };
        let item = row()
            .key(format!("label-{i}"))
            .w_full()
            .px(8.0)
            .py(8.0)
            .gap(10.0)
            .radius(k.radius.md)
            .child(avatar(lk, note.agent))
            .child(
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .gap(3.0)
                    .child(
                        row()
                            .w_full()
                            .gap(6.0)
                            .child(text(shown_name(&note.author)).font(k.ty.small_medium).color(color))
                            .child(el().grow(1.0))
                            .child(text(place).font(lk.num(k.ty.small)).color(k.c.text_faint))
                            .child({
                                let id = note.id.clone();
                                row()
                                    .key(format!("label-remove-{i}"))
                                    .size(18.0, 18.0)
                                    .radius(5.0)
                                    .center()
                                    .hover(k.c.text_base.alpha(0.08))
                                    .help("Remove this label")
                                    .on_press(move |s: &mut Studio| remove_label(s, id.clone()))
                                    .child(icon(&CROSS, 11.0).color(k.c.text_faint))
                            }),
                    )
                    .child(text(note.text.clone()).font(k.ty.ui).color(k.c.text_base).wrap()),
            )
            .on_press(move |s: &mut Studio| {
                set_cursor(s, at);
                reveal(s, at);
            });
        list = list.child(k.interactive(item));
    }
    list
}

/// A size as file lists show it.
fn file_size(bytes: u64) -> String {
    size_text(bytes)
}

/// The Files tab: a folder to walk through, with a Play button and an Add button on each audio
/// file, and under it the files the project already uses.
/// The Files tab on its own, for a new project that has no audio yet and so no log.
fn fresh_panel(s: &Studio, lk: &Look) -> El {
    let k = &lk.k;
    col()
        .key("log")
        .w_full()
        .h_full()
        .child(row().w_full().px(18.0).pt(16.0).pb(10.0).center_y().child(text("Files").font(k.ty.ui_medium).color(k.c.text_base)))
        .child(col().w_full().grow(1.0).min_h(0.0).px(10.0).pb(16.0).scroll(ScrollSpec { stick_bottom: false }).child(files(s, None, lk)))
}

fn files(s: &Studio, doc: Option<&Doc>, lk: &Look) -> El {
    let k = &lk.k;
    let b = &s.files;
    let folder = b.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| b.dir.to_string_lossy().into_owned());
    let parent = b.dir.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| p.to_path_buf());
    let mut up = IconButton::new(&UP).variant(IconButtonVariant::GhostMuted).disabled(parent.is_none()).help("Up a folder");
    if let Some(parent) = parent {
        up = up.on_press(move |s: &mut Studio| browse(s, parent.clone()));
    }
    let mut head = row()
        .w_full()
        .pb(6.0)
        .gap(6.0)
        .center_y()
        .child(up.build(k).key("files-up"))
        .child(icon(&FOLDER, 14.0).color(k.c.text_muted))
        .child(text(folder).font(k.ty.ui_medium).color(k.c.text_base).truncate().help(b.dir.to_string_lossy().into_owned()))
        .child(el().grow(1.0));
    if !s.blank && b.dir != s.paths.dir {
        head = head.child(small_button(k, "Project folder", "files-home".into(), |s: &mut Studio| {
            let dir = s.paths.dir.clone();
            browse(s, dir);
        }));
    }
    let mut list = col().key("file-list").w_full().gap(1.0).child(head);
    if let Some(error) = &b.error {
        list = list.child(text(error.clone()).font(k.ty.small).color(k.c.text_muted).wrap());
    } else if b.entries.is_empty() {
        list = list.child(text("No folders or audio files here.").font(k.ty.small).color(k.c.text_faint).wrap());
    }
    let at = doc.map(|doc| timecode(self::head(s), doc.rate()));
    for (i, entry) in b.entries.iter().enumerate() {
        let path = entry.path.clone();
        if entry.folder {
            let item = row()
                .key(format!("folder-{i}"))
                .w_full()
                .h(30.0)
                .px(8.0)
                .gap(9.0)
                .center_y()
                .radius(k.radius.md)
                .child(icon(&FOLDER, 14.0).color(k.c.text_muted))
                .child(text(entry.name.clone()).font(k.ty.ui).color(k.c.text_base).truncate())
                .on_press(move |s: &mut Studio| browse(s, path.clone()));
            list = list.child(k.interactive(item));
            continue;
        }
        let playing = b.playing.as_ref().is_some_and(|(p, _)| *p == entry.path);
        let ending = entry.path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
        let (listen, add, lay) = (path.clone(), path.clone(), path);
        list = list.child(
            row()
                .key(format!("file-{i}"))
                .w_full()
                .px(8.0)
                .py(5.0)
                .gap(9.0)
                .center_y()
                .child(
                    row()
                        .key(format!("file-play-{i}"))
                        .size(24.0, 24.0)
                        .radius(12.0)
                        .center()
                        .fill(if playing { lk.you } else { k.c.text_base.alpha(0.07) })
                        .help(if playing { "Stop" } else { "Play this file" })
                        .on_press(move |s: &mut Studio| toggle_listen(s, listen.clone()))
                        .child(icon(if playing { &STOP } else { &PLAY }, 11.0).color(if playing { lk.on_you } else { k.c.text_base })),
                )
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(1.0)
                        .child(text(entry.name.clone()).font(k.ty.ui).color(k.c.text_base).truncate())
                        .child(text(format!("{ending} \u{b7} {}", file_size(entry.bytes))).font(lk.num(k.ty.small)).color(k.c.text_faint)),
                )
                .child(match &at {
                    Some(at) => Button::new("Insert".to_string())
                        .variant(ButtonVariant::Neutral)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut Studio| add_file(s, add.clone()))
                        .build(k)
                        .key(format!("file-add-{i}"))
                        .help(format!("Insert into the timeline at the playhead, {at}. What follows moves later.")),
                    // A new project: its first file.
                    None => Button::new("Add".to_string())
                        .variant(ButtonVariant::Neutral)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut Studio| add_file(s, add.clone()))
                        .build(k)
                        .key(format!("file-add-{i}"))
                        .help("Start the project with this file. The project is made beside it, with the same name."),
                })
                .child(match &at {
                    Some(at) => Button::new("Track".to_string())
                        .variant(ButtonVariant::Neutral)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut Studio| add_file_as_track(s, lay.clone()))
                        .build(k)
                        .key(format!("file-track-{i}"))
                        .help(format!("Lay on a new track from the playhead, {at}. Nothing moves.")),
                    None => el(),
                }),
        );
    }
    let Some(doc) = doc else { return list };
    // What the project is already made of.
    list = list.child(el().h(10.0)).child(el().w_full().h(1.0).fill(lk.rule)).child(el().h(6.0)).child(text("In this project").font(k.ty.small_medium).color(k.c.text_muted).px(8.0));
    // Silence the project made for itself, where a track runs past the others, is not a file.
    for source in doc.project.sources.iter().filter(|source| !source.silence) {
        let missing = !s.paths.source_file(source).is_file();
        let name = std::path::Path::new(&source.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| source.path.clone());
        let length = duration(source.frames as f64, doc.rate());
        list = list.child(
            row()
                .w_full()
                .px(8.0)
                .py(4.0)
                .gap(8.0)
                .center_y()
                .child(text(name).font(k.ty.ui).color(if missing { k.c.text_muted } else { k.c.text_base }).truncate().help(source.path.clone()))
                .child(el().grow(1.0))
                .child(if missing { chip(lk, "missing", lk.meter[3]) } else { text(length).font(lk.num(k.ty.small)).color(k.c.text_faint) }),
        );
    }
    list
}

fn log(s: &Studio, doc: &Doc, lk: &Look) -> El {
    let k = &lk.k;
    let applied = doc.states.iter().filter(|s| **s == EditState::Applied).count();
    let tabs = [format!("Activity  {applied}"), format!("Labels  {}", doc.notes.len()), "Files".to_string()];
    col()
        .key("log")
        .w_full()
        .h_full()
        .child(
            row()
                .w_full()
                .px(14.0)
                .pt(12.0)
                .pb(10.0)
                .gap(10.0)
                .center_y()
                .child(SegmentedControl::new(&tabs, Some(s.tab)).fit().on_pick(|s: &mut Studio, i, _| {
                    s.tab = i;
                    // The folder may have changed since it was last read.
                    if i == 2 {
                        let dir = s.files.dir.clone();
                        browse(s, dir);
                    }
                }).build(k).key("log-tabs"))
                .child(el().grow(1.0))
                .child(text(format!("v{}", doc.project.version)).font(lk.num(k.ty.small)).color(k.c.text_faint)),
        )
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .px(10.0)
                .pb(16.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(match s.tab {
                    0 => activity(s, doc, lk),
                    1 => labels(doc, lk),
                    _ => files(s, Some(doc), lk),
                }),
        )
}

// ---------------------------------------------------------------- actions

fn you() -> Who {
    Who { author: "you".into(), session: None }
}

/// Runs a change to the project and reloads, or says why it could not be made. For changes that
/// name what they act on by id, which mean the same whatever else has happened to the project.
fn act(s: &mut Studio, change: impl FnOnce(&Paths) -> scrubr_core::Result<()>) {
    if busy_adding(s) {
        return;
    }
    s.hold_reloads = false;
    match change(&s.paths) {
        Ok(()) => {
            s.notice = None;
            reload(s);
        }
        Err(e) => s.notice = Some(e.to_string()),
    }
}

/// How a write that checked the project's version came out.
#[derive(PartialEq)]
enum Wrote {
    Done,
    /// Another program had written the project since this window read it. Nothing was changed,
    /// and the window now shows the project as it is.
    Changed,
    Failed,
}

/// Runs a change that depends on what this window last read: a position on the timeline, or
/// which edit is at head. It is refused if the project has been written since, and the window
/// reloads; the caller decides whether the change still makes sense.
fn act_checked(s: &mut Studio, change: impl FnOnce(&Paths, &WriteOpts) -> scrubr_core::Result<()>) -> Wrote {
    if busy_adding(s) {
        return Wrote::Failed;
    }
    s.hold_reloads = false;
    let opts = WriteOpts { if_version: s.doc.as_ref().map(|d| d.project.version), ..Default::default() };
    match change(&s.paths, &opts) {
        Ok(()) => {
            s.notice = None;
            reload(s);
            Wrote::Done
        }
        Err(e) if e.kind() == Kind::Conflict => {
            reload(s);
            Wrote::Changed
        }
        Err(e) => {
            s.notice = Some(e.to_string());
            Wrote::Failed
        }
    }
}

/// Who made the edit now at head, for telling the person whose write came first.
fn last_author(s: &Studio) -> String {
    s.doc.as_ref().and_then(|d| d.project.head_index().map(|i| shown_name(&d.project.edits[i].author))).unwrap_or_else(|| "Someone".into())
}

/// A place on the timeline in terms of the source audio, which still means something after the
/// project has changed: the audio a range covered, or the frame a point sat on.
enum Place {
    Range(Vec<SrcSpan>),
    Point(Option<(usize, u64)>),
}

fn place(s: &Studio, range: (u64, u64)) -> Place {
    match &s.doc {
        Some(doc) if range.1 > range.0 => Place::Range(doc.tl.resolve(range.0, range.1)),
        Some(doc) => Place::Point(doc.tl.anchor(range.0)),
        None => Place::Point(None),
    }
}

/// Where a place is on the timeline now, if the same audio is still there in one piece.
fn find(s: &Studio, place: &Place) -> Option<(u64, u64)> {
    let tl = &s.doc.as_ref()?.tl;
    match place {
        Place::Range(spans) => {
            let first = spans.first()?;
            let a = tl.position_from(first.src, first.start)?;
            let b = a + spans.iter().map(|x| x.frames()).sum::<u64>();
            (b <= tl.total() && tl.resolve(a, b) == *spans).then_some((a, b))
        }
        Place::Point(at) => {
            let (src, frame) = (*at)?;
            let t = tl.position_from(src, frame)?;
            (tl.anchor(t) == Some((src, frame))).then_some((t, t))
        }
    }
}

/// Runs a change at a place on the timeline. If another program wrote the project first, the
/// place is looked for again in the project as it now is, and the change is made there when the
/// same audio is still in one piece. Returns where it was made.
fn act_at(s: &mut Studio, range: (u64, u64), what: &str, change: impl Fn(&Paths, &WriteOpts, (u64, u64)) -> scrubr_core::Result<()>) -> Option<(u64, u64)> {
    let kept = place(s, range);
    match act_checked(s, |paths, opts| change(paths, opts, range)) {
        Wrote::Done => Some(range),
        Wrote::Failed => None,
        Wrote::Changed => {
            let who = last_author(s);
            match find(s, &kept) {
                Some(moved) if act_checked(s, |paths, opts| change(paths, opts, moved)) == Wrote::Done => {
                    s.notice = Some(format!("{who} edited the project first. Your {what} was made on the same audio, now at {}.", timecode(moved.0 as f64, s.doc.as_ref().map_or(48_000, |d| d.rate()))));
                    Some(moved)
                }
                _ => {
                    s.selection = None;
                    s.notice = Some(format!("{who} edited the project first, and the audio your {what} was for has changed. Nothing was done: look again and repeat it."));
                    None
                }
            }
        }
    }
}

fn select(s: &mut Studio, i: usize) {
    s.selected = if s.selected == Some(i) { None } else { Some(i) };
    let at = s.doc.as_ref().and_then(|d| d.splices.iter().find(|j| j.edits.contains(&i)).map(|j| j.at as f64));
    if let (Some(_), Some(at)) = (s.selected, at) {
        reveal(s, at);
    }
}

/// Undo and redo move head, so they mean something only for the project this window last read.
fn step_head(s: &mut Studio, back: bool) {
    let wrote = act_checked(s, |paths, opts| if back { edit::undo(paths, opts, 1).map(|_| ()) } else { edit::redo(paths, opts, 1).map(|_| ()) });
    if wrote == Wrote::Changed {
        s.notice = Some(format!("{} edited the project first, so nothing was {}. Look at the log and repeat it if you still mean to.", last_author(s), if back { "undone" } else { "redone" }));
    }
}

fn undo(s: &mut Studio) {
    step_head(s, true);
}

fn redo(s: &mut Studio) {
    step_head(s, false);
}

/// The selection as whole frames, with what a cut or mute of it is called in the log.
fn selected_range(s: &Studio, verb: &str) -> Option<((u64, u64), u32, String)> {
    let doc = s.doc.as_ref()?;
    let (a, b) = s.selection?;
    let (a, b) = (a.round() as u64, (b.round() as u64).min(doc.tl.total()));
    let crossfade = (DEFAULT_CROSSFADE_MS / 1000.0 * doc.rate() as f64).round() as u32;
    (b > a).then(|| ((a, b), crossfade, format!("{verb} {}..{}", timecode(a as f64, doc.rate()), timecode(b as f64, doc.rate()))))
}

fn cut_selection(s: &mut Studio) {
    let Some((range, crossfade, _)) = selected_range(s, "cut") else { return };
    let rate = s.doc.as_ref().map_or(48_000, |d| d.rate());
    let name = move |range: (u64, u64)| format!("cut {}..{}", timecode(range.0 as f64, rate), timecode(range.1 as f64, rate));
    let made = act_at(s, range, "cut", move |paths, opts, range| edit::apply(paths, opts, &you(), false, |_, _| Ok((vec![Request::Cut { range, crossfade }], Some(name(range))))).map(|_| ()));
    if let Some(range) = made {
        s.selection = None;
        set_cursor(s, range.0 as f64);
    }
}

fn mute_selection(s: &mut Studio) {
    let Some((range, crossfade, _)) = selected_range(s, "mute") else { return };
    let rate = s.doc.as_ref().map_or(48_000, |d| d.rate());
    let name = move |range: (u64, u64)| format!("mute {}..{}", timecode(range.0 as f64, rate), timecode(range.1 as f64, rate));
    let made = act_at(s, range, "mute", move |paths, opts, range| edit::apply(paths, opts, &you(), false, |_, _| Ok((vec![Request::Mute { range, crossfade }], Some(name(range))))).map(|_| ()));
    if let Some(range) = made {
        s.selection = Some((range.0 as f64, range.1 as f64));
    }
}

/// Silences the selection on the picked track alone. The other tracks play on, and nothing moves.
fn mute_on_track(s: &mut Studio) {
    let Some((range, crossfade, _)) = selected_range(s, "mute") else { return };
    let Some((id, track)) = picked_track(s).map(|(_, t)| (t.id.clone(), t.name.clone())) else { return };
    let rate = s.doc.as_ref().map_or(48_000, |d| d.rate());
    let made = act_at(s, range, "mute", move |paths, opts, range| {
        let desc = format!("mute {track} {}..{}", timecode(range.0 as f64, rate), timecode(range.1 as f64, rate));
        let request = Request::OnTrack { track: id.clone(), request: Box::new(Request::Mute { range, crossfade }) };
        edit::apply(paths, opts, &you(), false, |_, _| Ok((vec![request], Some(desc)))).map(|_| ())
    });
    if let Some(range) = made {
        s.selection = Some((range.0 as f64, range.1 as f64));
    }
}

/// Puts the start or the end of the selection at the playhead. The other end stays where it is,
/// or joins it when it would fall on the wrong side.
fn mark_selection(s: &mut Studio, start: bool) {
    let at = head(s);
    s.selection = Some(match (s.selection, start) {
        (Some((_, b)), true) => (at, b.max(at)),
        (Some((a, _)), false) => (a.min(at), at),
        (None, _) => (at, at),
    });
}

/// True for a file the window can start a project from.
fn is_audio(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| AUDIO_ENDINGS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Starts a new project with nothing in it, and shows the Files tab to add audio from. Nothing
/// is written yet: the project is made when its first file is added.
fn start_fresh(s: &mut Studio) {
    if let Some(p) = &s.player {
        p.pause();
    }
    // The Files tab starts where the person last was: the folder it showed, the open project's,
    // or their home folder.
    let dir = if !s.files.dir.as_os_str().is_empty() {
        s.files.dir.clone()
    } else if !s.blank {
        s.paths.dir.clone()
    } else {
        std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
    };
    s.paths = Paths::new(PathBuf::new());
    s.blank = true;
    s.fresh = true;
    s.doc = None;
    s.error = None;
    s.player = None;
    s.selection = None;
    s.picked = None;
    s.selected = None;
    s.hover_note = None;
    s.labeling = None;
    s.notice = None;
    s.view_request = None;
    s.cursor = 0.0;
    s.start = 0.0;
    s.tab = 2;
    if !s.inspector.is_open() {
        s.inspector.toggle();
    }
    browse(s, dir);
}

/// Makes a project from an audio file on its own thread: the file is hashed, converted if it
/// has to be, and its waveform is drawn up. `watch_creating` opens it.
fn make_project(s: &mut Studio, file: PathBuf, project: PathBuf) {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (answer, result) = mpsc::channel();
    let started = std::thread::Builder::new().name("scrubr-new".into()).spawn(move || {
        let made = edit::new_project(&Paths::new(&project), &[file], &you(), false).map(|_| project).map_err(|e| e.to_string());
        let _ = answer.send(made);
    });
    match started {
        Ok(_) => {
            s.notice = None;
            s.creating = Some(Creating { name, result });
        }
        Err(e) => s.notice = Some(format!("Could not start: {e}")),
    }
}

/// Gives a new project its first file. `ep42.wav` becomes `ep42.scrubr` beside it, or
/// `ep42 2.scrubr` if that name is taken: New never opens a project that is already there.
fn first_file(s: &mut Studio, file: PathBuf) {
    if s.creating.is_some() {
        return;
    }
    s.files.playing = None;
    let stem = file.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into());
    let dir = file.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let project = std::iter::once(format!("{stem}.scrubr")).chain((2..).map(|n| format!("{stem} {n}.scrubr"))).map(|name| dir.join(name)).find(|p| !p.exists()).expect("the numbers do not run out");
    make_project(s, file, project);
}

/// Starts a project from an audio file picked in the Open dialog: `ep42.wav` becomes
/// `ep42.scrubr` beside it. If that project is already there it is opened as it is.
fn new_project(s: &mut Studio, file: PathBuf) {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !is_audio(&file) {
        s.notice = Some(format!("{name} is not an audio file scrubr reads. It reads WAV, FLAC, MP3, OGG and AAC (.m4a)."));
        return;
    }
    let project = file.with_extension("scrubr");
    if project.exists() {
        open_project(s, project.clone());
        let made = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        s.notice = Some(format!("{made} was already beside {name}, so it was opened as it is."));
        return;
    }
    make_project(s, file, project);
}

/// Opens the project a `new_project` has finished making. True when the window has changed.
fn watch_creating(s: &mut Studio) -> bool {
    let Some(creating) = &s.creating else { return false };
    let made = match creating.result.try_recv() {
        Ok(made) => made,
        Err(mpsc::TryRecvError::Empty) => return false,
        Err(mpsc::TryRecvError::Disconnected) => Err("it stopped without a result.".to_string()),
    };
    let name = s.creating.take().map(|c| c.name).unwrap_or_default();
    let fresh = s.fresh;
    match made {
        // A project started with New stays on the Files tab, where the next file comes from.
        Ok(project) if fresh => {
            let made = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            open_project(s, project);
            s.tab = 2;
            s.notice = Some(format!("Made {made} beside {name}."));
        }
        Ok(project) => open_project(s, project),
        Err(e) => s.notice = Some(format!("A project could not be made from {name}: {e}")),
    }
    true
}

/// Shows another project in this window. An audio file is taken as a request for a new project.
fn open_project(s: &mut Studio, path: PathBuf) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if is_audio(&path) {
        return new_project(s, path);
    }
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("scrubr")) {
        s.notice = Some(format!("{name} is neither a scrubr project nor an audio file scrubr reads."));
        return;
    }
    if let Some(p) = &s.player {
        p.pause();
    }
    s.paths = Paths::new(path);
    s.blank = false;
    s.fresh = false;
    cancel_find(s);
    s.found.clear();
    s.selection = None;
    s.picked = None;
    s.selected = None;
    s.notice = None;
    s.view_request = None;
    s.cursor = 0.0;
    s.start = 0.0;
    s.tab = 0;
    s.fit_pending = true;
    reload(s);
    s.cursor = 0.0;
    let folder = s.paths.dir.clone();
    browse(s, folder);
}

/// A size as people read it: 1.04 GB, 412 MB, 8.4 MB, 940 KB.
fn size_text(bytes: u64) -> String {
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 1000.0 {
        format!("{:.2} GB", mb / 1000.0)
    } else if mb >= 100.0 { format!("{mb:.0} MB") } else if mb >= 1.0 { format!("{mb:.1} MB") } else { format!("{:.0} KB", bytes as f64 / 1000.0) }
}

/// Renders the timeline at head to `out` on a thread of its own. `poll` watches it.
fn start_export(s: &mut Studio, mut out: PathBuf) {
    let Some(doc) = &s.doc else { return };
    if out.extension().is_none() {
        out.set_extension("flac");
    }
    let name = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (project, paths) = (doc.project.clone(), s.paths.clone());
    let done = Arc::new(AtomicU64::new(0));
    let options = export::Options { progress: Some(done.clone()), ..Default::default() };
    let (answer, result) = mpsc::channel();
    let started = std::thread::Builder::new().name("scrubr-export".into()).spawn(move || {
        let _ = answer.send(export::export(&project, &paths, &out, &options).map_err(|e| e.to_string()));
    });
    match started {
        Ok(_) => {
            s.notice = Some(format!("Exporting {name}\u{2026}"));
            s.exporting = Some(Exporting { name, frames: doc.tl.total(), done, result, shown: 0 });
        }
        Err(e) => s.notice = Some(format!("Could not start the export: {e}")),
    }
}

/// Follows a running export. True when what the window shows has changed.
fn watch_export(s: &mut Studio) -> bool {
    let Some(x) = &mut s.exporting else { return false };
    let finished = match x.result.try_recv() {
        Ok(Ok(report)) => {
            let rate = s.doc.as_ref().map_or(48_000, |d| d.rate());
            let format = match (report.kbps, report.depth) {
                (Some(kbps), _) => format!("MP3 {kbps} kbps"),
                (None, export::Depth::Int(bits)) => format!("{} {bits}-bit", report.format.name().to_uppercase()),
                (None, export::Depth::Float) => format!("{} 32-bit float", report.format.name().to_uppercase()),
            };
            let clipped = if report.clipped_samples > 0 { format!(" \u{b7} {} samples clipped", report.clipped_samples) } else { String::new() };
            format!("Exported {} to {} \u{b7} {format} \u{b7} {}{clipped}", timecode(report.frames as f64, rate), x.name, size_text(report.bytes))
        }
        Ok(Err(e)) => format!("Export failed: {e}"),
        Err(mpsc::TryRecvError::Disconnected) => "Export failed: it stopped without a result.".to_string(),
        Err(mpsc::TryRecvError::Empty) => {
            let percent = (x.done.load(Relaxed) * 100 / x.frames.max(1)).min(99) as u32;
            let moved = percent != x.shown;
            x.shown = percent;
            return moved;
        }
    };
    s.exporting = None;
    s.notice = Some(finished);
    true
}

/// Starts what a command asked for, now that a `Tx` is at hand. Every place that runs a command
/// calls this after it.
fn flush_asks(s: &mut Studio, tx: &mut Tx) {
    let folder = (!s.blank).then(|| s.paths.dir.clone());
    match s.ask.take() {
        Some(Ask::Open) => {
            let options = FileDialogOptions { title: "Open project".into(), directory: folder, ..Default::default() }.filter("scrubr project", &["scrubr"]).filter("Audio, as a new project", &AUDIO_ENDINGS);
            tx.open_file(options, |s: &mut Studio, result: FileDialogResult, _| match result {
                Ok(Some(path)) => open_project(s, path),
                Ok(None) => {}
                Err(e) => s.notice = Some(format!("Could not show the file dialog: {e}")),
            });
        }
        Some(Ask::Export) => {
            let stem = s.paths.project.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "export".into());
            let options = FileDialogOptions { title: "Export".into(), directory: folder, file_name: Some(format!("{stem}.flac")), ..Default::default() }
                .filter("FLAC", &["flac"])
                .filter("WAV", &["wav"])
                .filter("MP3", &["mp3"]);
            tx.save_file(options, |s: &mut Studio, result: FileDialogResult, _| match result {
                Ok(Some(path)) => start_export(s, path),
                Ok(None) => {}
                Err(e) => s.notice = Some(format!("Could not show the file dialog: {e}")),
            });
        }
        None => {}
    }
}

/// Moves the cursor to the next or previous splice.
fn step_splice(s: &mut Studio, forward: bool) {
    let Some(doc) = &s.doc else { return };
    let at = if forward {
        doc.splices.iter().map(|j| j.at as f64).find(|&t| t > s.cursor + 1.0)
    } else {
        doc.splices.iter().rev().map(|j| j.at as f64).find(|&t| t < s.cursor - 1.0)
    };
    if let Some(at) = at {
        set_cursor(s, at);
        reveal(s, at);
    }
}

// ---------------------------------------------------------------- commands

/// The key that runs a command. `Cmd` is Command on macOS and Ctrl elsewhere.
#[derive(Clone, Copy, PartialEq)]
enum Keys {
    Char(char),
    Shift(char),
    Cmd(char),
    CmdShift(char),
    Delete,
    Escape,
    Home,
    Up,
    Down,
    Left,
    Right,
}

impl Keys {
    fn pressed(self, e: &KeyEvent) -> bool {
        match self {
            Keys::Cmd(c) => e.is_command(c) && !e.mods.shift,
            Keys::CmdShift(c) => e.is_command(c) && e.mods.shift,
            // A letter with Shift held is another command's key.
            Keys::Char(c) => matches!(&e.key, Key::Text(t) if !e.mods.cmd && !(c.is_ascii_alphabetic() && e.mods.shift) && t.chars().map(|x| x.to_ascii_lowercase()).eq([c])),
            Keys::Shift(c) => matches!(&e.key, Key::Text(t) if !e.mods.cmd && e.mods.shift && t.chars().map(|x| x.to_ascii_lowercase()).eq([c])),
            Keys::Delete => matches!(e.key, Key::Delete | Key::Backspace),
            Keys::Escape => matches!(e.key, Key::Escape),
            Keys::Home => matches!(e.key, Key::Home),
            Keys::Up => matches!(e.key, Key::Up),
            Keys::Down => matches!(e.key, Key::Down),
            Keys::Left => matches!(e.key, Key::Left),
            Keys::Right => matches!(e.key, Key::Right),
        }
    }

    /// The key as menus and tooltips show it.
    fn label(self) -> String {
        let mac = cfg!(target_os = "macos");
        match self {
            Keys::Cmd(c) if mac => format!("\u{2318}{}", c.to_ascii_uppercase()),
            Keys::Cmd(c) => format!("Ctrl+{}", c.to_ascii_uppercase()),
            Keys::CmdShift(c) if mac => format!("\u{21e7}\u{2318}{}", c.to_ascii_uppercase()),
            Keys::CmdShift(c) => format!("Ctrl+Shift+{}", c.to_ascii_uppercase()),
            Keys::Shift(c) if mac => format!("\u{21e7}{}", c.to_ascii_uppercase()),
            Keys::Shift(c) => format!("Shift+{}", c.to_ascii_uppercase()),
            Keys::Char(' ') => "Space".into(),
            Keys::Char(c) => c.to_ascii_uppercase().to_string(),
            Keys::Delete => "Del".into(),
            Keys::Escape => "Esc".into(),
            Keys::Home => "Home".into(),
            Keys::Up => "\u{2191}".into(),
            Keys::Down => "\u{2193}".into(),
            Keys::Left => "\u{2190}".into(),
            Keys::Right => "\u{2192}".into(),
        }
    }
}

/// One thing the window can do. The main menu, the tool buttons and the keyboard all read this
/// list, so a command has one name, one shortcut and one rule for when it is available.
struct Command {
    menu: usize,
    label: &'static str,
    keys: Keys,
    /// Starts a new group in its menu.
    divide: bool,
    /// False for commands the window does not have yet: listed, never available.
    built: bool,
    enabled: fn(&Studio) -> bool,
    run: fn(&mut Studio),
}

const OPEN: &str = "Open project\u{2026}";
const NEW: &str = "New project";

impl Command {
    /// Every command but New and Open needs a project to act on.
    fn available(&self, s: &Studio) -> bool {
        self.built && (s.doc.is_some() || self.label == OPEN || self.label == NEW) && (self.enabled)(s)
    }
}

const MENUS: [&str; 5] = ["File", "Edit", "View", "Transport", "Tools"];

fn always(_: &Studio) -> bool {
    true
}

/// A selection with some length: I or O alone leaves one with none, which is nothing to cut.
fn has_selection(s: &Studio) -> bool {
    s.selection.is_some_and(|(a, b)| b > a)
}

fn has_splices(s: &Studio) -> bool {
    s.doc.as_ref().is_some_and(|d| !d.splices.is_empty())
}

static COMMANDS: [Command; 30] = [
    Command { menu: 0, label: NEW, keys: Keys::Cmd('n'), divide: false, built: true, enabled: |s| s.creating.is_none(), run: start_fresh },
    Command { menu: 0, label: OPEN, keys: Keys::Cmd('o'), divide: false, built: true, enabled: always, run: |s| s.ask = Some(Ask::Open) },
    Command { menu: 0, label: "Export\u{2026}", keys: Keys::Cmd('e'), divide: false, built: true, enabled: |s| s.exporting.is_none(), run: |s| s.ask = Some(Ask::Export) },
    Command {
        menu: 1,
        label: "Undo",
        keys: Keys::Cmd('z'),
        divide: false,
        built: true,
        enabled: |s| s.doc.as_ref().and_then(|d| d.project.head_index()).is_some_and(|i| i > 0),
        run: undo,
    },
    Command {
        menu: 1,
        label: "Redo",
        keys: Keys::CmdShift('z'),
        divide: false,
        built: true,
        enabled: |s| s.doc.as_ref().is_some_and(|d| d.project.head_index().is_some_and(|i| i + 1 < d.project.edits.len())),
        run: redo,
    },
    Command { menu: 1, label: "Cut selection", keys: Keys::Delete, divide: true, built: true, enabled: has_selection, run: cut_selection },
    Command { menu: 1, label: "Mute selection", keys: Keys::Char('m'), divide: false, built: true, enabled: has_selection, run: mute_selection },
    Command {
        menu: 1,
        label: "Mute on track",
        keys: Keys::Shift('m'),
        divide: false,
        built: true,
        enabled: |s| has_selection(s) && picked_track(s).is_some(),
        run: mute_on_track,
    },
    Command { menu: 1, label: "Cut words", keys: Keys::Shift('x'), divide: false, built: true, enabled: |s| s.job.is_none() && chosen_words(s).is_some(), run: cut_words },
    Command { menu: 1, label: "Add label\u{2026}", keys: Keys::Char('l'), divide: true, built: true, enabled: always, run: begin_label },
    Command { menu: 1, label: "Find words\u{2026}", keys: Keys::Cmd('f'), divide: true, built: true, enabled: has_words, run: begin_find },
    Command { menu: 1, label: "Set selection start", keys: Keys::Char('i'), divide: true, built: true, enabled: always, run: |s| mark_selection(s, true) },
    Command { menu: 1, label: "Set selection end", keys: Keys::Char('o'), divide: false, built: true, enabled: always, run: |s| mark_selection(s, false) },
    Command {
        menu: 1,
        label: "Deselect",
        keys: Keys::Escape,
        divide: true,
        built: true,
        enabled: |s| s.selection.is_some() || s.selected.is_some(),
        run: |s| {
            s.selection = None;
            s.selected = None;
        },
    },
    Command { menu: 2, label: "Zoom in", keys: Keys::Up, divide: false, built: true, enabled: always, run: |s| zoom_center(s, 1.5) },
    Command { menu: 2, label: "Zoom out", keys: Keys::Down, divide: false, built: true, enabled: always, run: |s| zoom_center(s, 1.0 / 1.5) },
    Command {
        menu: 2,
        label: "Zoom to selection",
        keys: Keys::Char('z'),
        divide: false,
        built: true,
        enabled: has_selection,
        run: |s| {
            if let Some((a, b)) = s.selection {
                s.fpp = (b - a) / (s.wave[0] as f64).max(1.0) * 1.1;
                s.start = a - (b - a) * 0.05;
                clamp(s);
            }
        },
    },
    Command {
        menu: 2,
        label: "Fit timeline",
        keys: Keys::Char('f'),
        divide: false,
        built: true,
        enabled: always,
        run: |s| {
            s.fit_pending = true;
            fit(s);
        },
    },
    Command { menu: 2, label: "Scroll back", keys: Keys::Left, divide: true, built: true, enabled: always, run: |s| pan(s, -0.2 * s.wave[0] as f64) },
    Command { menu: 2, label: "Scroll forward", keys: Keys::Right, divide: false, built: true, enabled: always, run: |s| pan(s, 0.2 * s.wave[0] as f64) },
    Command { menu: 2, label: "Toggle the log", keys: Keys::Char('h'), divide: true, built: true, enabled: always, run: |s| s.inspector.toggle() },
    Command { menu: 3, label: "Play or pause", keys: Keys::Char(' '), divide: false, built: true, enabled: |s| s.player.is_some(), run: toggle_play },
    Command {
        menu: 3,
        label: "Go to start",
        keys: Keys::Home,
        divide: true,
        built: true,
        enabled: always,
        run: |s| {
            set_cursor(s, 0.0);
            s.start = 0.0;
        },
    },
    Command { menu: 3, label: "Previous splice", keys: Keys::Char('['), divide: false, built: true, enabled: has_splices, run: |s| step_splice(s, false) },
    Command { menu: 3, label: "Next splice", keys: Keys::Char(']'), divide: false, built: true, enabled: has_splices, run: |s| step_splice(s, true) },
    // Tools look, listen and measure. Those that act on part of the timeline take the selection,
    // or everything when there is none.
    Command { menu: 4, label: "Spectrogram", keys: Keys::Char('s'), divide: false, built: true, enabled: always, run: |s| s.spectro.on = !s.spectro.on },
    Command { menu: 4, label: "Transcribe", keys: Keys::Cmd('t'), divide: true, built: true, enabled: |s| s.job.is_none(), run: transcribe },
    Command { menu: 4, label: "Tighten pauses", keys: Keys::Shift('p'), divide: false, built: true, enabled: |s| s.job.is_none(), run: tighten_pauses },
    Command { menu: 4, label: "Check joins", keys: Keys::Shift('j'), divide: false, built: true, enabled: |s| s.job.is_none() && has_splices(s), run: check_joins },
    Command { menu: 4, label: "Measure loudness", keys: Keys::Shift('l'), divide: false, built: true, enabled: |s| s.job.is_none(), run: measure_loudness },
];

fn command(label: &str) -> &'static Command {
    COMMANDS.iter().find(|c| c.label == label).expect("the command is in the list")
}

fn close_menu(s: &mut Studio) {
    s.menu = false;
    s.submenu = None;
}

/// The main menu: one button in the header that unfolds every command, by menu.
fn main_menu(s: &Studio, k: &Kit, layer: &mut Vec<El>) -> El {
    let trigger = IconButton::new(&MENU)
        .variant(IconButtonVariant::GhostMuted)
        .expanded(s.menu)
        .label("Menu")
        .on_press(|s: &mut Studio| if s.menu { close_menu(s) } else { s.menu = true })
        .build(k)
        .key("menu-button");
    let mut menu = Menu::new("main-menu").trigger(trigger).min_width(180.0);
    for (m, title) in MENUS.iter().enumerate() {
        let mut entries = Vec::new();
        for c in COMMANDS.iter().filter(|c| c.menu == m) {
            if c.divide {
                entries.push(MenuEntry::Separator);
            }
            if c.label == "Transcribe" {
                // What the tools under it act on. It also gives the menu room for their names.
                entries.push(MenuEntry::Label("On the selection, or on everything".into()));
            }
            let mut item = MenuItem::new(c.label).shortcut(c.keys.label()).disabled(!c.available(s)).on_select_tx(move |s: &mut Studio, tx: &mut Tx| {
                close_menu(s);
                (c.run)(s);
                flush_asks(s, tx);
            });
            if c.label == "Spectrogram" {
                item = item.checked(s.spectro.on);
            }
            if !c.built {
                item = item.badge("soon");
            }
            entries.push(MenuEntry::item(item));
        }
        // The kit's rows only answer a press, so a clear sheet across the row watches for the
        // pointer and unfolds the menu as it arrives. It is not a hit target: presses still
        // reach the row.
        let on_arrival = el().key(format!("menu-hover-{m}")).pin(Some(-12.0), Some(-8.0), Some(-30.0), Some(-8.0)).on_hover_change(move |s: &mut Studio, inside, _| {
            if inside {
                s.submenu = Some(m);
            }
        });
        menu = menu.item(MenuItem::new(*title).lead(on_arrival).submenu(entries, s.submenu == Some(m)).on_select(move |s: &mut Studio| s.submenu = Some(m)));
    }
    menu.open(s.menu).on_close(close_menu).build(k, layer)
}

// ---------------------------------------------------------------- the window

fn view(s: &Studio, cx: &mut Cx<Studio>) -> El {
    let lk = &s.looks[cx.system_dark() as usize];
    let k = &lk.k;
    let scale = cx.env().scale;
    let name = if s.fresh {
        "New project".to_string()
    } else if s.blank {
        "No project".to_string()
    } else { s.paths.project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() };

    let main = match (&s.doc, &s.error) {
        (Some(doc), _) => {
            let w = s.wave[0];
            let readout = match s.selection {
                Some((a, b)) => format!("{}  →  {}   ·   {}", timecode(a, doc.rate()), timecode(b, doc.rate()), duration(b - a, doc.rate())),
                None => format!("{} Hz · {} ch · {} splices · drag on the waveform to select", doc.rate(), doc.project.channels, doc.splices.len()),
            };
            let card = col()
                .key("timeline")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .radius(k.radius.lg + 2.0)
                .clip_rounded(k.radius.lg + 2.0)
                .fill(lk.lane)
                .border(1.0, k.c.border_muted)
                .child(overview(s, doc, lk, w, scale))
                .child(ruler(s, doc, lk, w))
                .child(wave_area(s, doc, lk, scale));
            col()
                .w_full()
                .h_full()
                .px(12.0)
                .child(row().w_full().h(30.0).center().child(text(readout).key("readout").font(lk.num(k.ty.ui)).color(if s.selection.is_some() { lk.you } else { k.c.text_faint })))
                .child(card)
                .child(transport(s, doc, lk))
        }
        // No project, or one that would not load: say how to start, and offer both ways in.
        (None, error) => {
            let (title, body) = match (&s.creating, error, s.fresh) {
                (Some(c), _, _) => ("Making a project", format!("Reading {}. A long or compressed file takes a little while.", c.name)),
                (None, Some(e), _) => ("This project could not be opened", e.clone()),
                (None, None, true) => (
                    "Add audio to begin",
                    "Find a recording in the Files tab and press Add. scrubr makes the project beside that file, with the same name. The recording itself is never changed.".to_string(),
                ),
                (None, None, false) => ("Start a project", "A project holds your edits over one or more recordings. The recordings themselves are never changed.".to_string()),
            };
            let busy = s.creating.is_some();
            // The way in a person most often wants is the filled button; the other is quieter.
            // New needs no dialog; Open asks for a file.
            let button = |key: &'static str, label: &'static str, filled: bool, ask: Option<Ask>| {
                let plate = row()
                    .key(key)
                    .h(32.0)
                    .px(14.0)
                    .center_y()
                    .radius(k.radius.md)
                    .fill(if filled { lk.you.alpha(if busy { 0.45 } else { 1.0 }) } else { k.c.text_base.alpha(0.07) })
                    .help(format!("{} ({})", label.trim_end_matches('\u{2026}'), command(label).keys.label()))
                    .child(text(label).font(k.ty.ui_medium).color(if filled { lk.on_you } else { k.c.text_base }));
                if busy {
                    plate
                } else {
                    plate.on_press_tx(move |s: &mut Studio, tx: &mut Tx| match ask {
                        Some(ask) => {
                            s.ask = Some(ask);
                            flush_asks(s, tx);
                        }
                        None => start_fresh(s),
                    })
                }
            };
            let mut plate = col()
                .key("empty")
                .w_full()
                .h_full()
                .center()
                .padding(32.0)
                // The toolbar above takes 48 points of the window; as much again below puts the
                // middle of this plate at the middle of the window.
                .pb(32.0 + 48.0)
                .gap(10.0)
                .child(icon(&LOGO, 44.0).color(lk.you))
                .child(el().h(2.0))
                .child(text(title).font(TextStyle { size: 19.0, weight: 620.0, line_height: 24.0, ..k.ty.ui }).color(k.c.text_base))
                .child(text(body).font(k.ty.body).color(k.c.text_muted).wrap().align(Align::Center).max_w(460.0));
            // A new project has its way forward in the Files tab beside this.
            if !s.fresh {
                plate = plate.child(el().h(4.0)).child(row().gap(10.0).center_y().child(button("new-project", NEW, true, None)).child(button("open-project", OPEN, false, Some(Ask::Open))));
                if !busy && error.is_none() {
                    plate = plate.child(el().h(2.0)).child(
                        text("Already have a .scrubr project, from an agent or an earlier session? Open it.").font(k.ty.small).color(k.c.text_faint).wrap().align(Align::Center).max_w(460.0),
                    );
                }
            }
            if let Some(notice) = &s.notice {
                plate = plate.child(text(notice.clone()).font(k.ty.small).color(k.c.text_muted).wrap().align(Align::Center).max_w(460.0));
            }
            plate
        }
    };

    let mut layer = Vec::new();
    let wordmark = TextStyle { size: 16.0, weight: 680.0, tracking: -0.2, ..k.ty.ui };
    let toolbar = Toolbar::new()
        .leading(
            row()
                .gap(8.0)
                .center_y()
                .child(el().w(2.0))
                .child(icon(&LOGO, 18.0).color(lk.you))
                .child(text("scrubr").font(wordmark).color(k.c.text_base))
                .child(el().w(2.0))
                .child(main_menu(s, k, &mut layer))
                .child(el().w(1.0).h(16.0).mx(4.0).fill(k.c.border_base))
                .child(text(name).font(k.ty.ui_medium).color(k.c.text_muted)),
        )
        .trailing(
            row()
                .gap(4.0)
                .center_y()
                .child(tool(s, k, &UNDO, "Undo"))
                .child(tool(s, k, &REDO, "Redo"))
                .child(el().w(6.0))
                .child({
                    // While an export runs the button shows how far it has got and does nothing.
                    let ready = command("Export\u{2026}").available(s);
                    let label = match &s.exporting {
                        Some(x) => format!("Exporting {}%", x.shown),
                        None => "Export".to_string(),
                    };
                    let button = row()
                        .key("export")
                        .h(28.0)
                        .px(12.0)
                        .gap(6.0)
                        .center_y()
                        .radius(k.radius.md)
                        .fill(lk.you.alpha(if ready { 1.0 } else { 0.45 }))
                        .help(format!("Export to FLAC or WAV ({})", command("Export\u{2026}").keys.label()))
                        .child(icon(&EXPORT, 14.0).color(lk.on_you))
                        .child(text(label).font(lk.num(k.ty.ui_medium)).color(lk.on_you));
                    if ready {
                        button.on_press_tx(|s: &mut Studio, tx: &mut Tx| {
                            s.ask = Some(Ask::Export);
                            flush_asks(s, tx);
                        })
                    } else {
                        button
                    }
                })
                .child(el().w(2.0))
                // The log's switch, while there is a log.
                .child(if s.doc.is_none() && !s.fresh {
                    el()
                } else {
                    IconButton::new(&shell::INSPECTOR)
                        .variant(IconButtonVariant::GhostMuted)
                        .expanded(s.inspector.is_open())
                        .label(if s.inspector.is_open() { "Hide the log" } else { "Show the log" })
                        .on_press(|s: &mut Studio| s.inspector.toggle())
                        .build(k)
                        .key("log-toggle")
                }),
        );
    let mut split = SplitView::new(main);
    // With no project there is no log, so its pane goes too and the empty state sits in the
    // middle of the window, not of what the pane left.
    if let Some(doc) = &s.doc {
        split = split.inspector(&s.inspector, |s: &mut Studio| &mut s.inspector, log(s, doc, lk));
    } else if s.fresh {
        split = split.inspector(&s.inspector, |s: &mut Studio| &mut s.inspector, fresh_panel(s, lk));
    }
    let window = split
        .toolbar(toolbar)
        .titlebar_inset(cx.titlebar_inset())
        .frame_inset(8.0)
        .toolbar_in_panel()
        .hide_sidebar_toggle()
        .reduced_motion(cx.is_reduced_motion())
        .build(k);
    rune_kit::root(window, layer)
}

fn on_key(s: &mut Studio, e: &KeyEvent, tx: &mut Tx) -> bool {
    if s.menu && matches!(e.key, Key::Escape) {
        close_menu(s);
        return true;
    }
    // While a label is being written, keys are its text. If the field has lost the keyboard,
    // Esc still closes the card and nothing else runs a command.
    if s.labeling.is_some() || s.finding {
        let escape = matches!(e.key, Key::Escape);
        if escape {
            cancel_label(s);
            cancel_find(s);
        }
        return escape;
    }
    match COMMANDS.iter().find(|c| c.keys.pressed(e)) {
        Some(c) if c.available(s) => {
            (c.run)(s);
            flush_asks(s, tx);
            true
        }
        _ => false,
    }
}

/// Every frame: follow the audio while it plays, and twice a second look at the project file,
/// reloading if an agent or another window wrote it.
fn poll(s: &mut Studio, t: f64, tx: &mut Tx) -> bool {
    let mut changed = false;

    if let (Some(seconds), Some(p)) = (s.play_check, &s.player) {
        match s.play_check_began {
            None => {
                s.play_check_began = Some(t);
                p.play();
            }
            Some(began) if t - began >= seconds => {
                let rate = s.doc.as_ref().map_or(48_000, |d| d.rate());
                println!(
                    "play-check: {:.1} s of wall time, playhead at {:.2} s, underruns {}, device {} at {} Hz, {} ch, meters reached {:.1} / {:.1} dBFS and show {:.1} / {:.1}",
                    t - began,
                    p.position() as f64 / rate as f64,
                    p.underruns(),
                    p.device,
                    p.dev_rate,
                    p.dev_channels,
                    s.meters.seen[0],
                    s.meters.seen[1],
                    s.meters.level[0],
                    s.meters.level[1]
                );
                std::process::exit(0);
            }
            Some(_) => {}
        }
    } else if s.play_check.is_some() {
        println!("play-check: no player ({})", s.audio_note);
        std::process::exit(1);
    }

    let playing = s.player.as_ref().map(|p| (p.is_playing(), p.finished(), p.position() as f64));
    if let Some((true, finished, at)) = playing {
        if finished {
            if let Some(p) = &s.player {
                p.pause();
            }
            s.cursor = total(s);
            changed = true;
        }
        // Page the view when the playhead leaves it.
        let w = s.wave[0] as f64;
        if w > 0.0 && (at < s.start || at > s.start + w * s.fpp) {
            s.start = at - 0.05 * w * s.fpp;
            clamp(s);
            changed = true;
        }
    }
    let still_playing = s.player.as_ref().is_some_and(Player::is_playing);
    // The meters go on falling for a moment after the audio stops.
    let meters_lit = step_meters(s, t);
    changed |= watch_export(s);
    changed |= watch_creating(s);
    changed |= watch_adding(s);
    changed |= watch_job(s);
    changed |= watch_spectrogram(s);
    // A file being listened to stops showing as playing when it ends.
    if s.files.playing.as_ref().is_some_and(|(_, p)| p.done()) {
        s.files.playing = None;
        changed = true;
    }
    tx.wake_after(Duration::from_millis(if still_playing || meters_lit { 4 } else if s.exporting.is_some() || s.creating.is_some() || s.adding.is_some() || s.job.is_some() || s.spectro.asked.is_some() || s.files.playing.is_some() { 100 } else { 500 }));

    if s.pretend_edit_at.is_some_and(|at| t >= at) {
        s.pretend_edit_at = None;
        s.stamp = None;
    }
    if t - s.checked_at >= 0.45 && !s.blank && !s.hold_reloads && s.adding.is_none() {
        s.checked_at = t;
        if stamp(&s.paths) != s.stamp {
            reload(s);
            if let Some((e, project)) = s.doc.as_ref().and_then(|d| d.project.head_index().map(|i| (&d.project.edits[i], &d.project))) {
                let word = format!("{} just edited · {}", shown_name(&e.author), describe(e, project));
                s.fleeting = Some((word.clone(), t + 8.0));
                s.notice = Some(word);
            }
            changed = true;
        } else if words_stamp(&s.paths) != s.words_stamp {
            // Something has listened: `scrubr transcribe`, run by an agent or from here.
            read_said(s);
            changed = true;
        }
    }
    // Word of an outside edit goes after a few seconds, unless something else has been said since.
    if let Some((word, until)) = &s.fleeting {
        if s.notice.as_ref() != Some(word) {
            s.fleeting = None;
        } else if t >= *until {
            s.notice = None;
            s.fleeting = None;
            changed = true;
        }
    }
    sync(s);
    changed
}

/// Opens the window on a project, or empty when there is none.
pub fn run(path: Option<PathBuf>) {
    // The path is handed to `init` through a cell: `AppSpec` takes a plain function.
    PATH.with(|p| *p.borrow_mut() = path);
    fn init() -> Studio {
        init_with(PATH.with(|p| p.borrow_mut().take()))
    }
    let spec = AppSpec::<Studio> { poll: Some(poll), on_key: Some(on_key), min_size: Some((960.0, 480.0)), ..AppSpec::new("scrubr", (1360.0, 760.0), init, view) };
    // On macOS the header runs under a clear title bar and leaves room at its left for the
    // window's close, minimise and zoom buttons. Elsewhere the native frame sits above the
    // header, so the header starts at the window's edge.
    let chrome = if cfg!(target_os = "macos") { Chrome::default() } else { Chrome { full_size_content: false, traffic_lights: None, drag_regions: false } };
    App::new(spec).chrome(chrome).run();
}

thread_local! {
    static PATH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}
