//! The project view: a first look at scrubr's window on rune-kit.
//!
//! `scrubr-gui <project.scrubr>` opens a project and draws its timeline at head: an overview of
//! the whole recording, a ruler, a two-tone waveform (peaks around a loudness body), a mark at
//! every splice in its author's colour, and level edits and labels under the lanes. Beside it is
//! the activity log: every edit, by the person or an agent, with undo for one edit or a whole
//! session. Agents work from a shell; the window has no chat. It reloads when the project file
//! changes on disk. Every command is in one list, which the main menu, the tool buttons and the
//! keyboard all read. Playback, open and export are not wired yet.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rune::{App, AppSpec, Chrome, Color, Cx, El, Icon, Key, KeyEvent, LineCap, Phase, ScrollSpec, Shape, TextStyle, Tx, canvas, col, el, icon, memo, oklch, row, text};
use rune_kit::shell::{self, PaneState, SplitView, Toolbar};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Menu, MenuEntry, MenuItem, PaletteSeed, SegmentedControl};
use scrubr_core::edit::{self, DEFAULT_CROSSFADE_MS, Request, Revert};
use scrubr_core::project::{self, EditState, FadeDir, Op, Paths, Project, Who, WriteOpts};
use scrubr_core::{Peaks, Timeline, source};

// ---------------------------------------------------------------- icons

macro_rules! stroke_icon {
    ($name:ident, $id:literal, $body:literal) => {
        static $name: Icon = Icon { name: $id, view: 16.0, body: concat!(r#"<g stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" fill="none">"#, $body, "</g>") };
    };
}

static PLAY: Icon = Icon { name: "scrubr-play", view: 16.0, body: r#"<path d="M5.2 3.2L12.6 8L5.2 12.8Z" fill="currentColor"/>"# };
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
stroke_icon!(FIT, "scrubr-fit", r#"<path d="M2.5 6V3.5H5M11 3.5H13.5V6M13.5 10V12.5H11M5 12.5H2.5V10"/>"#);

const OVERVIEW_H: f32 = 46.0;
const RULER_H: f32 = 26.0;
const MARKS_H: f32 = 46.0;
const TRANSPORT_H: f32 = 64.0;
/// The deepest zoom of this first look, in frames per point. Closer views need the renderer.
const MIN_FPP: f64 = 32.0;

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
}

fn look(dark: bool) -> Look {
    if dark {
        let you = oklch(0.80, 0.14, 75.0);
        let k = Kit::from_palette(PaletteSeed { bg: oklch(0.195, 0.006, 70.0), fg: oklch(0.94, 0.012, 85.0), accent: you });
        Look {
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
        let k = Kit::from_palette(PaletteSeed { bg: oklch(0.962, 0.006, 80.0), fg: oklch(0.24, 0.012, 70.0), accent: you });
        Look {
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
    /// Index of the edit that made the join.
    edit: Option<usize>,
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

struct Doc {
    project: Project,
    tl: Timeline,
    peaks: Vec<Option<Peaks>>,
    states: Vec<EditState>,
    splices: Vec<Splice>,
    levels: Vec<LevelMark>,
    notes: Vec<Note>,
    groups: Vec<Group>,
}

fn load(paths: &Paths) -> Result<Doc, String> {
    let project = project::open(paths).map_err(|e| e.to_string())?;
    let tl = Timeline::build(&project).map_err(|e| e.to_string())?;
    let mut peaks: Vec<Option<Peaks>> = project.sources.iter().map(|_| None).collect();
    for seg in tl.segs() {
        if peaks[seg.src].is_none() {
            peaks[seg.src] = Some(source::peaks(paths, &project.sources[seg.src], project.sample_rate).map_err(|e| e.to_string())?);
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
            Op::Fade { dir, spans: s } => spans(s, Some(if *dir == FadeDir::In { LevelKind::FadeIn } else { LevelKind::FadeOut })),
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
        let (edit, removed) = if a.src == b.src && b.start >= a.end {
            // The latest cut that took out any of the audio between the two pieces.
            let edit = cuts.iter().rev().find(|c| c.0 == a.src && c.1 < b.start && c.2 > a.end).map(|c| c.3);
            (edit, Some((a.src, a.end, b.start)))
        } else {
            (inserted[b.src].or(inserted[a.src]), None)
        };
        let agent = edit.is_some_and(|i| is_agent(&project.edits[i].author));
        splices.push(Splice { at: tl.seg_start(i), edit, agent, removed });
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
            Some(Note { start, end, text: l.text.clone(), author: l.author.clone(), agent: is_agent(&l.author), hidden })
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
    Ok(Doc { project, tl, peaks, states, splices, levels, notes, groups })
}

/// One column of a waveform: its peaks and its loudness, after the level edits.
#[derive(Clone, Copy, Default)]
struct Column {
    min: f32,
    max: f32,
    rms: f32,
    any: bool,
}

impl Doc {
    fn rate(&self) -> u32 {
        self.project.sample_rate
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

pub struct Studio {
    paths: Paths,
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
    cursor: f64,
    selected: Option<usize>,
    notice: Option<String>,
    /// The project file's modified time when it was last read.
    stamp: Option<SystemTime>,
    checked_at: f64,
    /// Bumped on every reload, so drawings cached for the old document are rebuilt.
    generation: u64,
}

fn stamp(paths: &Paths) -> Option<SystemTime> {
    std::fs::metadata(&paths.project).and_then(|m| m.modified()).ok()
}

fn reload(s: &mut Studio) {
    s.stamp = stamp(&s.paths);
    s.generation += 1;
    match load(&s.paths) {
        Ok(doc) => {
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
}

fn init_with(path: std::path::PathBuf) -> Studio {
    let args = rune::extra_args();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let mut s = Studio {
        paths: Paths::new(path),
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
        notice: None,
        stamp: None,
        checked_at: 0.0,
        generation: 0,
    };
    reload(&mut s);
    // For stills: `--view START..END` in seconds, `--select START..END`, `--edit ID`,
    // `--cursor SECONDS`, `--tab labels`, `--menu edit`.
    let rate = s.doc.as_ref().map_or(48_000.0, |d| d.rate() as f64);
    let range = |text: String| {
        let (a, b) = text.split_once("..")?;
        Some((a.parse::<f64>().ok()? * rate, b.parse::<f64>().ok()? * rate))
    };
    s.view_request = flag("--view").and_then(range);
    s.selection = flag("--select").and_then(range);
    if let Some(at) = flag("--cursor").and_then(|t| t.parse::<f64>().ok()) {
        s.cursor = at * rate;
    }
    if let (Some(id), Some(doc)) = (flag("--edit"), &s.doc) {
        s.selected = doc.project.edits.iter().position(|e| e.id == id);
    }
    if flag("--tab").as_deref() == Some("labels") {
        s.tab = 1;
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
    const STEPS: [(f64, usize, u32); 19] = [
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
        Op::Revert { .. } => ("Undo", &UNDO),
        Op::Batch { ops } => match ops.first() {
            Some(first) if ops.iter().all(|op| op.name() == first.name()) => kind(first),
            _ => ("Batch", &LAYERS),
        },
    }
}

/// An edit's own description, with a batch's count set as a multiplication sign.
fn describe(e: &project::Edit) -> String {
    let text = e.desc.clone().unwrap_or_else(|| e.op.name().to_string());
    if matches!(e.op, Op::Batch { .. }) { text.replace(" x", " \u{00d7}") } else { text }
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

fn wave_shape(s: &Studio, doc: &Doc, lk: &Look, w: f32, h: f32, scale: f32) -> Shape {
    let lanes = doc.project.channels.clamp(1, 2) as usize;
    let cols = (w * scale).round().max(1.0) as usize;
    let col_w = w / cols as f32;
    let fpc = s.fpp * col_w as f64;
    let lane_h = h / lanes as f32;
    let selected = s.selection.map(|(a, b)| (((a - s.start) / fpc) as isize, ((b - s.start) / fpc) as isize));
    let mut shape = Shape::new();
    for c in 0..lanes {
        let mid = lane_h * (c as f32 + 0.5);
        shape = shape.line([0.0, mid], [w, mid], 1.0, LineCap::Butt, lk.rule);
        if c > 0 {
            shape = shape.line([0.0, lane_h * c as f32], [w, lane_h * c as f32], 1.0, LineCap::Butt, lk.rule);
        }
        let columns = doc.columns(c, s.start, fpc, cols);
        shape = lane(shape, &columns, col_w, mid, lane_h * 0.5 - 10.0, lk.ink, lk.ink_soft, |i| match selected {
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
        let chosen = splice.edit.is_some() && splice.edit == s.selected;
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
    let splice = doc.splices.iter().find(|j| j.edit == Some(edit) && (j.at as f64) >= s.start && (j.at as f64) <= s.start + w as f64 * s.fpp)?;
    let removed = splice.removed?;
    let e = &doc.project.edits[edit];
    let x = ((splice.at as f64 - s.start) / s.fpp) as f32;
    let (cw, gh) = (236.0, 40.0);
    let by = match &e.session {
        Some(session) => format!("{} · {session}", shown_name(&e.author)),
        None => shown_name(&e.author),
    };
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
        .child(text(format!("{} · {by} · still in the source", e.id)).font(k.ty.small).color(k.c.text_muted));
    for shadow in &k.elevation.floating {
        card = card.box_shadow(*shadow);
    }
    Some(card)
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
            pan(s, -(e.dx + e.dy) as f64);
            true
        })
        .on_magnify(|s: &mut Studio, e, _| {
            zoom_about(s, e.x as f64, 1.0 + e.delta as f64);
            true
        })
        .on_drag(|s: &mut Studio, d, _| {
            // A drag selects a range; a press without one moves the cursor.
            let frame = |x: f32| (s.start + x as f64 * s.fpp).clamp(0.0, total(s));
            if (d.x - d.start_x).abs() > 2.0 {
                let (a, b) = (frame(d.start_x), frame(d.x));
                s.selection = Some((a.min(b), a.max(b)));
            } else if d.phase == Phase::Up {
                s.cursor = frame(d.x);
                s.selection = None;
            }
        });
    if w <= 0.0 || h <= 0.0 {
        return area;
    }
    let version = (s.generation, s.start.to_bits(), s.fpp.to_bits(), w.to_bits(), h.to_bits(), scale.to_bits(), s.selection.map(|(a, b)| (a.to_bits(), b.to_bits())), s.selected, k.dark);
    area = area.child(memo("wave-shape", version, || canvas(wave_shape(s, doc, lk, w, h, scale)).absolute(0.0, 0.0).size(w, h)));
    if let Some((a, b)) = s.selection {
        let (x0, x1) = ((((a - s.start) / s.fpp) as f32).max(0.0), (((b - s.start) / s.fpp) as f32).min(w));
        if x1 > x0 {
            area = area.child(el().key("selection").absolute(x0, 0.0).size(x1 - x0, h).fill(lk.you.alpha(0.10)).border_sides([0.0, 1.0, 0.0, 1.0], lk.you.alpha(0.75)));
        }
    }
    let lanes = doc.project.channels.clamp(1, 2) as usize;
    if lanes == 2 {
        for (c, name) in ["L", "R"].into_iter().enumerate() {
            area = area.child(
                row()
                    .key(format!("lane-{name}"))
                    .absolute(8.0, h / 2.0 * c as f32 + 18.0)
                    .size(18.0, 18.0)
                    .radius(5.0)
                    .center()
                    .fill(k.c.text_base.alpha(0.07))
                    .child(text(name).font(k.ty.small_medium).color(k.c.text_muted)),
            );
        }
    }
    let cx = ((s.cursor - s.start) / s.fpp) as f32;
    if (0.0..=w).contains(&cx) {
        area = area.child(el().key("cursor").absolute(cx - 0.75, 0.0).size(1.5, h).fill(k.c.text_base));
    }
    if let Some(card) = callout(s, doc, lk, w) {
        area = area.child(card);
    }
    area
}

/// Under the lanes: level edits as bars in their author's colour, then the labels.
fn marks(s: &Studio, doc: &Doc, lk: &Look, w: f32) -> El {
    let k = &lk.k;
    let end = s.start + w as f64 * s.fpp;
    let x_of = |t: u64| ((t as f64 - s.start) / s.fpp) as f32;
    let mut shape = Shape::new();
    for m in doc.levels.iter().filter(|m| (m.end as f64) > s.start && (m.start as f64) < end) {
        let (x0, x1) = (x_of(m.start).max(0.0), x_of(m.end).min(w));
        let color = lk.author(m.agent).alpha(if s.selected == Some(m.edit) { 1.0 } else { 0.75 });
        let (top, bottom) = (6.0, 13.0);
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
    let mut lane = el()
        .key("marks")
        .w_full()
        .h(MARKS_H)
        .border_sides([1.0, 0.0, 0.0, 0.0], lk.rule)
        .child(canvas(shape).key("marks-canvas").absolute(0.0, 0.0).size(w, MARKS_H));
    for (i, note) in doc.notes.iter().enumerate().filter(|(_, n)| !n.hidden && (n.end as f64) >= s.start && (n.start as f64) <= end) {
        let color = lk.author(note.agent);
        let (x0, x1) = (x_of(note.start).max(0.0), x_of(note.end).min(w));
        if x1 - x0 > 2.0 {
            lane = lane.child(el().key(format!("note-span-{i}")).absolute(x0, 19.0).size(x1 - x0, 2.0).fill(color.alpha(0.8)));
        }
        lane = lane.child(
            row()
                .key(format!("note-{i}"))
                .absolute(x0.min(w - 60.0), 23.0)
                .h(18.0)
                .px(7.0)
                .gap(5.0)
                .center_y()
                .radius(9.0)
                .fill(color.alpha(0.14))
                .border(1.0, color.alpha(0.45))
                .child(text(note.text.clone()).font(k.ty.small_medium).color(k.c.text_base)),
        );
    }
    lane
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

fn transport(s: &Studio, doc: &Doc, lk: &Look) -> El {
    let k = &lk.k;
    let rate = doc.rate();
    let counter = TextStyle { size: 27.0, weight: 600.0, line_height: 30.0, tracking: -0.4, ..lk.num(k.ty.ui) };
    let last = doc.project.head_index().map(|i| &doc.project.edits[i]);
    let status = s.notice.clone().unwrap_or_else(|| match last {
        Some(e) => format!("{} · {} · {}", shown_name(&e.author), describe(e), ago(e.time.as_deref())),
        None => String::new(),
    });
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
                .help("Play (not wired in this first look)")
                .child(icon(&PLAY, 18.0).color(lk.on_you)),
        )
        .child(plate(lk, vec![tool(s, k, &PREV, "Previous splice"), tool(s, k, &NEXT, "Next splice")]))
        .child(
            row()
                .gap(10.0)
                .items_end()
                .child(text(timecode(s.cursor, rate)).key("counter").font(counter).color(k.c.text_base))
                .child(text(format!("/ {}", timecode(doc.tl.total() as f64, rate))).font(lk.num(k.ty.ui)).color(k.c.text_faint).pb(3.0)),
        )
        .child(el().w(4.0))
        .child(plate(lk, vec![tool(s, k, &SCISSORS, "Cut selection"), tool(s, k, &MUTE, "Mute selection")]))
        .child(plate(lk, vec![tool(s, k, &ZOOM_OUT, "Zoom out"), tool(s, k, &ZOOM_IN, "Zoom in"), tool(s, k, &FIT, "Fit timeline")]))
        .child(el().grow(1.0))
        .child(
            row()
                .gap(7.0)
                .center_y()
                .min_w(0.0)
                .child(el().size(7.0, 7.0).radius(4.0).fill(lk.author(last.is_some_and(|e| is_agent(&e.author)))))
                .child(text(status).font(lk.num(k.ty.small)).color(k.c.text_muted).truncate()),
        )
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
                .child(text(describe(e)).font(lk.num(k.ty.ui)).color(if live { k.c.text_base } else { k.c.text_faint }).truncate().grow(1.0).min_w(0.0))
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
    let mut list = col().key("label-list").w_full().gap(2.0);
    if doc.notes.is_empty() {
        return list.child(text("No labels yet. An agent leaves one with `scrubr label add`.").font(k.ty.small).color(k.c.text_faint).wrap());
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
                            .child(text(place).font(lk.num(k.ty.small)).color(k.c.text_faint)),
                    )
                    .child(text(note.text.clone()).font(k.ty.ui).color(k.c.text_base).wrap()),
            )
            .on_press(move |s: &mut Studio| {
                s.cursor = at;
                reveal(s, at);
            });
        list = list.child(k.interactive(item));
    }
    list
}

fn log(s: &Studio, doc: &Doc, lk: &Look) -> El {
    let k = &lk.k;
    let applied = doc.states.iter().filter(|s| **s == EditState::Applied).count();
    let tabs = [format!("Activity  {applied}"), format!("Labels  {}", doc.notes.len())];
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
                .child(SegmentedControl::new(&tabs, Some(s.tab)).fit().on_pick(|s: &mut Studio, i, _| s.tab = i).build(k).key("log-tabs"))
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
                .child(if s.tab == 0 { activity(s, doc, lk) } else { labels(doc, lk) }),
        )
}

// ---------------------------------------------------------------- actions

fn you() -> Who {
    Who { author: "you".into(), session: None }
}

/// Runs a change to the project and reloads, or says why it could not be made.
fn act(s: &mut Studio, change: impl FnOnce(&Paths) -> scrubr_core::Result<()>) {
    match change(&s.paths) {
        Ok(()) => {
            s.notice = None;
            reload(s);
        }
        Err(e) => s.notice = Some(e.to_string()),
    }
}

fn select(s: &mut Studio, i: usize) {
    s.selected = if s.selected == Some(i) { None } else { Some(i) };
    let at = s.doc.as_ref().and_then(|d| d.splices.iter().find(|j| j.edit == Some(i)).map(|j| j.at as f64));
    if let (Some(_), Some(at)) = (s.selected, at) {
        reveal(s, at);
    }
}

fn undo(s: &mut Studio) {
    act(s, |paths| edit::undo(paths, &WriteOpts::default(), 1).map(|_| ()));
}

fn redo(s: &mut Studio) {
    act(s, |paths| edit::redo(paths, &WriteOpts::default(), 1).map(|_| ()));
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
    let Some((range, crossfade, desc)) = selected_range(s, "cut") else { return };
    act(s, move |paths| edit::apply(paths, &WriteOpts::default(), &you(), false, |_, _| Ok((vec![Request::Cut { range, crossfade }], Some(desc)))).map(|_| ()));
    if s.notice.is_none() {
        s.selection = None;
        s.cursor = range.0 as f64;
    }
}

fn mute_selection(s: &mut Studio) {
    let Some((range, crossfade, desc)) = selected_range(s, "mute") else { return };
    act(s, move |paths| edit::apply(paths, &WriteOpts::default(), &you(), false, |_, _| Ok((vec![Request::Mute { range, crossfade }], Some(desc)))).map(|_| ()));
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
        s.cursor = at;
        reveal(s, at);
    }
}

// ---------------------------------------------------------------- commands

/// The key that runs a command. `Cmd` is Command on macOS and Ctrl elsewhere.
#[derive(Clone, Copy, PartialEq)]
enum Keys {
    Char(char),
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
            Keys::Char(c) => matches!(&e.key, Key::Text(t) if !e.mods.cmd && t.chars().map(|x| x.to_ascii_lowercase()).eq([c])),
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

impl Command {
    fn available(&self, s: &Studio) -> bool {
        self.built && s.doc.is_some() && (self.enabled)(s)
    }
}

const MENUS: [&str; 4] = ["File", "Edit", "View", "Transport"];

fn always(_: &Studio) -> bool {
    true
}

fn has_selection(s: &Studio) -> bool {
    s.selection.is_some()
}

fn has_splices(s: &Studio) -> bool {
    s.doc.as_ref().is_some_and(|d| !d.splices.is_empty())
}

fn not_yet(_: &mut Studio) {}

static COMMANDS: [Command; 18] = [
    Command { menu: 0, label: "Open project\u{2026}", keys: Keys::Cmd('o'), divide: false, built: false, enabled: always, run: not_yet },
    Command { menu: 0, label: "Export\u{2026}", keys: Keys::Cmd('e'), divide: false, built: false, enabled: always, run: not_yet },
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
    Command { menu: 3, label: "Play or pause", keys: Keys::Char(' '), divide: false, built: false, enabled: always, run: not_yet },
    Command {
        menu: 3,
        label: "Go to start",
        keys: Keys::Home,
        divide: true,
        built: true,
        enabled: always,
        run: |s| {
            s.cursor = 0.0;
            s.start = 0.0;
        },
    },
    Command { menu: 3, label: "Previous splice", keys: Keys::Char('['), divide: false, built: true, enabled: has_splices, run: |s| step_splice(s, false) },
    Command { menu: 3, label: "Next splice", keys: Keys::Char(']'), divide: false, built: true, enabled: has_splices, run: |s| step_splice(s, true) },
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
            let mut item = MenuItem::new(c.label).shortcut(c.keys.label()).disabled(!c.available(s)).on_select(move |s: &mut Studio| {
                close_menu(s);
                (c.run)(s);
            });
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
    let name = s.paths.project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

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
                .child(wave_area(s, doc, lk, scale))
                .child(marks(s, doc, lk, w));
            col()
                .w_full()
                .h_full()
                .px(12.0)
                .pb(4.0)
                .child(row().w_full().h(30.0).center().child(text(readout).key("readout").font(lk.num(k.ty.ui)).color(if s.selection.is_some() { lk.you } else { k.c.text_faint })))
                .child(card)
                .child(transport(s, doc, lk))
        }
        (None, error) => col().w_full().h_full().center().padding(32.0).child(text(error.clone().unwrap_or_default()).font(k.ty.body).color(k.c.text_muted).wrap()),
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
                .child(
                    row()
                        .key("export")
                        .h(28.0)
                        .px(12.0)
                        .gap(6.0)
                        .center_y()
                        .radius(k.radius.md)
                        .fill(lk.you)
                        .help("Export (not wired in this first look)")
                        .child(icon(&EXPORT, 14.0).color(lk.on_you))
                        .child(text("Export").font(k.ty.ui_medium).color(lk.on_you)),
                )
                .child(el().w(2.0))
                .child(
                    IconButton::new(&shell::INSPECTOR)
                        .variant(IconButtonVariant::GhostMuted)
                        .expanded(s.inspector.is_open())
                        .label(if s.inspector.is_open() { "Hide the log" } else { "Show the log" })
                        .on_press(|s: &mut Studio| s.inspector.toggle())
                        .build(k)
                        .key("log-toggle"),
                ),
        );
    let inspector = match &s.doc {
        Some(doc) => log(s, doc, lk),
        None => el(),
    };
    let window = SplitView::new(main)
        .inspector(&s.inspector, |s: &mut Studio| &mut s.inspector, inspector)
        .toolbar(toolbar)
        .titlebar_inset(cx.titlebar_inset())
        .frame_inset(8.0)
        .toolbar_in_panel()
        .hide_sidebar_toggle()
        .reduced_motion(cx.is_reduced_motion())
        .build(k);
    rune_kit::root(window, layer)
}

fn on_key(s: &mut Studio, e: &KeyEvent, _: &mut Tx) -> bool {
    if s.menu && matches!(e.key, Key::Escape) {
        close_menu(s);
        return true;
    }
    match COMMANDS.iter().find(|c| c.keys.pressed(e)) {
        Some(c) if c.available(s) => {
            (c.run)(s);
            true
        }
        _ => false,
    }
}

/// Twice a second, look at the project file: if an agent or another window wrote it, reload.
fn poll(s: &mut Studio, t: f64, tx: &mut Tx) -> bool {
    tx.wake_after(Duration::from_millis(500));
    if t - s.checked_at < 0.45 {
        return false;
    }
    s.checked_at = t;
    let now = stamp(&s.paths);
    if now == s.stamp {
        return false;
    }
    reload(s);
    if let Some(e) = s.doc.as_ref().and_then(|d| d.project.head_index().map(|i| &d.project.edits[i])) {
        s.notice = Some(format!("Live · {} edited just now · {}", shown_name(&e.author), describe(e)));
    }
    true
}

pub fn run(path: std::path::PathBuf) {
    // The path is handed to `init` through a cell: `AppSpec` takes a plain function.
    PATH.with(|p| *p.borrow_mut() = Some(path));
    fn init() -> Studio {
        init_with(PATH.with(|p| p.borrow_mut().take()).expect("the project path is set before the window opens"))
    }
    let spec = AppSpec::<Studio> { poll: Some(poll), on_key: Some(on_key), min_size: Some((960.0, 480.0)), ..AppSpec::new("scrubr", (1360.0, 760.0), init, view) };
    // On macOS the header runs under a clear title bar and leaves room at its left for the
    // window's close, minimise and zoom buttons. Elsewhere the native frame sits above the
    // header, so the header starts at the window's edge.
    let chrome = if cfg!(target_os = "macos") { Chrome::default() } else { Chrome { full_size_content: false, traffic_lights: None, drag_regions: false } };
    App::new(spec).chrome(chrome).run();
}

thread_local! {
    static PATH: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}
