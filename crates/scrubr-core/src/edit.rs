//! Turning what an author asks for, in timeline positions, into operations in source
//! coordinates.
//!
//! Every range in a request is resolved against one timeline, the `base`. For a single command
//! that is the timeline at head. For a batch it is the timeline before the batch, for every
//! operation in it, so positions worked out from one analysis stay valid through the whole batch.

use std::path::PathBuf;

use crate::analysis::{self, FLOOR_DB};
use crate::error::{Error, Result};
use crate::project::{self, FadeDir, Label, Op, Paths, Project, Span, Who, WriteOpts, Written};
use crate::render::Renderer;
use crate::source;
use crate::timeline::{SrcSpan, Timeline};

/// The default crossfade at joins, in milliseconds.
pub const DEFAULT_CROSSFADE_MS: f64 = 5.0;

/// The default true-peak ceiling for normalize, in dBTP.
pub const DEFAULT_CEILING_DBTP: f64 = -1.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    /// Integrated loudness in LUFS.
    Lufs(f64),
    /// Sample peak in dBFS.
    Peak(f64),
}

/// One requested change. Ranges and positions are timeline frames.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Cut { range: (u64, u64), crossfade: u32 },
    Keep { range: (u64, u64), crossfade: u32 },
    Gain { db: f64, range: Option<(u64, u64)> },
    Fade { dir: FadeDir, range: (u64, u64) },
    Mute { range: (u64, u64), crossfade: u32 },
    Insert { file: PathBuf, at: u64, crossfade: u32 },
    Normalize { target: Target, ceiling_dbtp: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalizeReport {
    pub target: Target,
    pub ceiling_dbtp: f64,
    pub gain_db: f64,
    /// True when the gain stopped at the true-peak ceiling, short of the target.
    pub limited: bool,
    pub measured_lufs: Option<f64>,
    pub measured_true_peak_dbtp: f64,
    pub measured_peak_db: f64,
}

impl NormalizeReport {
    pub fn achieved_lufs(&self) -> Option<f64> {
        self.measured_lufs.map(|l| l + self.gain_db)
    }

    pub fn achieved_true_peak_dbtp(&self) -> f64 {
        self.measured_true_peak_dbtp + self.gain_db
    }

    pub fn achieved_peak_db(&self) -> f64 {
        self.measured_peak_db + self.gain_db
    }
}

/// A request resolved to the operation the log stores.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub op: Op,
    /// Frames the operation removes, changes or adds.
    pub frames: u64,
    pub normalize: Option<NormalizeReport>,
    /// The id of the source an insert brought in.
    pub new_source: Option<String>,
}

fn seconds(project: &Project, frames: u64) -> String {
    format!("{:.3}s", project.seconds(frames))
}

/// Checks a timeline range against the base timeline.
pub fn check_range(project: &Project, paths: &Paths, base: &Timeline, (start, end): (u64, u64)) -> Result<()> {
    let total = base.total();
    let info_hint = format!("Run `scrubr info {}` to get the timeline length.", paths.shown.display());
    if end > total {
        return Err(Error::usage(
            "range_out_of_bounds",
            format!("End {} is past the end of the timeline ({}).", seconds(project, end), seconds(project, total)),
        )
        .hint(info_hint));
    }
    if start >= end {
        return Err(Error::usage(
            "empty_range",
            format!("The range {}..{} is empty or runs backwards.", seconds(project, start), seconds(project, end)),
        )
        .hint("Give the range as start..end with start before end."));
    }
    Ok(())
}

fn stored(base: &Timeline, spans: &[SrcSpan]) -> (Vec<Span>, u64) {
    (base.stored(spans), spans.iter().map(SrcSpan::frames).sum())
}

/// Resolves one request. `pending` holds the operations already resolved in the same batch;
/// only normalize looks at them, to measure the timeline they produce.
pub fn resolve(project: &mut Project, paths: &Paths, base: &Timeline, pending: &[Op], request: &Request) -> Result<Resolved> {
    let plain = |op: Op, frames: u64| Resolved { op, frames, normalize: None, new_source: None };
    Ok(match request {
        Request::Cut { range, crossfade } => {
            check_range(project, paths, base, *range)?;
            let (spans, frames) = stored(base, &base.resolve(range.0, range.1));
            plain(Op::Cut { spans, crossfade: *crossfade }, frames)
        }
        Request::Keep { range, crossfade } => {
            check_range(project, paths, base, *range)?;
            let (spans, frames) = stored(base, &base.resolve_outside(range.0, range.1));
            plain(Op::Keep { spans, crossfade: *crossfade }, frames)
        }
        Request::Gain { db, range } => {
            if !db.is_finite() {
                return Err(Error::usage("bad_gain", "The gain must be a finite number of dB."));
            }
            match range {
                None => plain(Op::Gain { gain_db: *db, spans: None }, base.total()),
                Some(range) => {
                    check_range(project, paths, base, *range)?;
                    let (spans, frames) = stored(base, &base.resolve(range.0, range.1));
                    plain(Op::Gain { gain_db: *db, spans: Some(spans) }, frames)
                }
            }
        }
        Request::Fade { dir, range } => {
            check_range(project, paths, base, *range)?;
            let (spans, frames) = stored(base, &base.resolve(range.0, range.1));
            plain(Op::Fade { dir: *dir, spans }, frames)
        }
        Request::Mute { range, crossfade } => {
            check_range(project, paths, base, *range)?;
            let (spans, frames) = stored(base, &base.resolve(range.0, range.1));
            plain(Op::Mute { spans, crossfade: *crossfade }, frames)
        }
        Request::Insert { file, at, crossfade } => {
            if *at > base.total() {
                return Err(Error::usage(
                    "range_out_of_bounds",
                    format!("Position {} is past the end of the timeline ({}).", seconds(project, *at), seconds(project, base.total())),
                )
                .hint(format!("Run `scrubr info {}` to get the timeline length. Use `--at end` to append.", paths.shown.display())));
            }
            let id = project.next_source_id();
            let source = source::import(paths, &id, file, Some(project.sample_rate))?;
            let frames = source.frames;
            project.sources.push(source);
            let at = base.anchor(*at).map(|(src, frame)| (base.source_id(src).to_string(), frame));
            Resolved { op: Op::Insert { source: id.clone(), at, crossfade: *crossfade }, frames, normalize: None, new_source: Some(id) }
        }
        Request::Normalize { target, ceiling_dbtp } => {
            let report = measure_for_normalize(project, paths, pending, *target, *ceiling_dbtp)?;
            let (lufs, peak_db) = match target {
                Target::Lufs(l) => (Some(*l), None),
                Target::Peak(p) => (None, Some(*p)),
            };
            let total = base.total();
            Resolved {
                op: Op::Normalize { lufs, peak_db, gain_db: report.gain_db, ceiling_dbtp: *ceiling_dbtp },
                frames: total,
                normalize: Some(report),
                new_source: None,
            }
        }
    })
}

/// Measures the timeline at head plus `pending`, and works out the gain that reaches `target`
/// without taking the true peak past `ceiling_dbtp`.
fn measure_for_normalize(project: &Project, paths: &Paths, pending: &[Op], target: Target, ceiling_dbtp: f64) -> Result<NormalizeReport> {
    let nothing = || {
        Error::usage("nothing_to_normalize", "The timeline is silent or too short to measure, so there is no level to normalize.")
            .hint("Check the audio with `scrubr analyze <project> --loudness --peaks`.")
    };
    let tl = Timeline::build_with(project, pending)?;
    let total = tl.total();
    let mut renderer = Renderer::with_timeline(project, paths, tl)?;
    let options = analysis::Options { loudness: true, peaks: true, ..Default::default() };
    let report = analysis::analyze(&mut renderer, 0, total, &options, project.sample_rate)?;
    let loudness = report.loudness.expect("loudness was asked for");
    let peak_db = report.peaks.expect("peaks were asked for").peak_db.into_iter().fold(FLOOR_DB, f64::max);
    if peak_db <= FLOOR_DB {
        return Err(nothing());
    }
    let want = match target {
        Target::Lufs(lufs) => lufs - loudness.integrated_lufs.ok_or_else(nothing)?,
        Target::Peak(peak) => peak - peak_db,
    };
    // Gains are stored to a hundredth of a dB. The room under the ceiling is rounded down, so
    // the stored gain never takes the true peak past it.
    let room = ((ceiling_dbtp - loudness.true_peak_dbtp) * 100.0).floor() / 100.0;
    let want = (want * 100.0).round() / 100.0;
    let limited = want > room;
    Ok(NormalizeReport {
        target,
        ceiling_dbtp,
        gain_db: if limited { room } else { want },
        limited,
        measured_lufs: loudness.integrated_lufs,
        measured_true_peak_dbtp: loudness.true_peak_dbtp,
        measured_peak_db: peak_db,
    })
}

/// The span a label on timeline range `start..end` is stored with, and whether the range had to
/// be clipped to fit one source. A range inside one source keeps hidden audio between its ends,
/// so the label marks the same stretch if that audio comes back. `start == end` marks a point.
pub fn label_span(project: &Project, paths: &Paths, tl: &Timeline, (start, end): (u64, u64)) -> Result<(Span, bool)> {
    if start > end || end > tl.total() {
        check_range(project, paths, tl, (start, end))?;
    }
    if start == end {
        let (src, frame) = tl.anchor(start).ok_or_else(|| Error::usage("empty_timeline", "The timeline is empty, so there is nothing to label."))?;
        return Ok((Span(tl.source_id(src).to_string(), frame, frame), false));
    }
    let spans = tl.resolve(start, end);
    let first = spans[0];
    let same: Vec<&SrcSpan> = spans.iter().take_while(|s| s.src == first.src).collect();
    let last = same[same.len() - 1];
    Ok((Span(tl.source_id(first.src).to_string(), first.start, last.end), same.len() < spans.len()))
}

// ---- transactions ----------------------------------------------------------------------------

/// What one write did to the timeline.
#[derive(Clone, Debug)]
pub struct Applied {
    /// The id of the edit that was added.
    pub edit: String,
    pub resolved: Vec<Resolved>,
    pub frames_before: u64,
    pub frames_after: u64,
    pub sample_rate: u32,
}

/// Adds one edit to the project. `build` sees the project and its timeline at head, under the
/// write lock, and returns the requests and a description of them; every request is resolved
/// against that timeline. Several requests, or `batch`, are stored as one batch edit.
pub fn apply(
    paths: &Paths,
    opts: &WriteOpts,
    who: &Who,
    batch: bool,
    build: impl FnOnce(&Project, &Timeline) -> Result<(Vec<Request>, Option<String>)>,
) -> Result<Written<Applied>> {
    let mut brought_in: Vec<String> = Vec::new();
    let mut discarded: Vec<String> = Vec::new();
    let result = project::update(paths, opts, |project| {
        let base = Timeline::build(project)?;
        let (requests, desc) = build(project, &base)?;
        if requests.is_empty() {
            return Err(Error::usage("no_operations", "There are no operations to apply.").hint("Give at least one operation."));
        }
        let mut ops: Vec<Op> = Vec::with_capacity(requests.len());
        let mut resolved = Vec::with_capacity(requests.len());
        for request in &requests {
            let one = resolve(project, paths, &base, &ops, request)?;
            brought_in.extend(one.new_source.clone());
            ops.push(one.op.clone());
            resolved.push(one);
        }
        let op = if batch || ops.len() != 1 { Op::Batch { ops } } else { ops.remove(0) };
        let (edit, gone) = project.push_edit(op, desc, who);
        discarded = gone;
        let frames_after = Timeline::build(project)?.total();
        Ok(Applied { edit, resolved, frames_before: base.total(), frames_after, sample_rate: project.sample_rate })
    });
    // Cache files follow the project: drop those of sources a real write discarded, and those a
    // failed or dry run brought in.
    match &result {
        Ok(written) if written.wrote => discarded.iter().for_each(|id| source::forget(paths, id)),
        _ => brought_in.iter().for_each(|id| source::forget(paths, id)),
    }
    result
}

/// How head moved.
#[derive(Clone, Debug)]
pub struct Moved {
    /// The edits that were undone, redone or reverted, in the order it happened.
    pub edits: Vec<String>,
    pub head: Option<String>,
    pub frames_before: u64,
    pub frames_after: u64,
    pub sample_rate: u32,
}

fn moved(project: &mut Project, step: impl FnOnce(&mut Project) -> Result<Vec<String>>) -> Result<Moved> {
    let frames_before = Timeline::build(project)?.total();
    let edits = step(project)?;
    let frames_after = Timeline::build(project)?.total();
    Ok(Moved { edits, head: project.head.clone(), frames_before, frames_after, sample_rate: project.sample_rate })
}

/// Moves head back `steps` edits.
pub fn undo(paths: &Paths, opts: &WriteOpts, steps: usize) -> Result<Written<Moved>> {
    project::update(paths, opts, |project| moved(project, |p| p.undo(steps)))
}

/// Moves head forward `steps` edits.
pub fn redo(paths: &Paths, opts: &WriteOpts, steps: usize) -> Result<Written<Moved>> {
    project::update(paths, opts, |project| moved(project, |p| p.redo(steps)))
}

/// Which edits a revert takes out.
#[derive(Clone, Debug)]
pub enum Revert {
    Edits(Vec<String>),
    /// Every applied edit made in this session.
    Session(String),
}

/// Takes edits out from anywhere in the log by adding a revert edit at head. Later edits and
/// labels are stored in source coordinates, so they stay where they were. Returns the revert's
/// own id and the edits it reverted.
pub fn revert(paths: &Paths, opts: &WriteOpts, who: &Who, which: &Revert) -> Result<Written<(String, Moved)>> {
    project::update(paths, opts, |project| {
        let ids: Vec<String> = match which {
            Revert::Edits(ids) => ids.clone(),
            Revert::Session(session) => {
                let states = project.states();
                let ids: Vec<String> = project
                    .edits
                    .iter()
                    .zip(&states)
                    .skip(1)
                    .filter(|(e, state)| **state == project::EditState::Applied && e.session.as_deref() == Some(session) && !matches!(e.op, Op::Revert { .. }))
                    .map(|(e, _)| e.id.clone())
                    .collect();
                if ids.is_empty() {
                    return Err(Error::usage("nothing_to_undo", format!("No applied edit was made in session {session}."))
                        .hint(format!("Run `scrubr history {}` to see sessions.", paths.shown.display())));
                }
                ids
            }
        };
        project.check_revertable(&ids)?;
        let desc = match which {
            Revert::Edits(ids) => format!("revert {}", ids.join(", ")),
            Revert::Session(session) => format!("revert session {session} ({} edits)", ids.len()),
        };
        let mut id = String::new();
        let moved = moved(project, |p| {
            id = p.push_edit(Op::Revert { edits: ids.clone() }, Some(desc), who).0;
            Ok(ids)
        })?;
        Ok((id, moved))
    })
}

/// Adds a label. `build` sees the project and its timeline under the write lock and returns the
/// timeline range and the text. Returns the label and whether its range was clipped to one source.
pub fn add_label(
    paths: &Paths,
    opts: &WriteOpts,
    who: &Who,
    build: impl FnOnce(&Project, &Timeline) -> Result<((u64, u64), String)>,
) -> Result<Written<(Label, bool)>> {
    project::update(paths, opts, |project| {
        let tl = Timeline::build(project)?;
        let (range, text) = build(project, &tl)?;
        let (span, clipped) = label_span(project, paths, &tl, range)?;
        project.add_label(span, text, who);
        Ok((project.labels.last().expect("just added").clone(), clipped))
    })
}

/// Removes a label by id.
pub fn remove_label(paths: &Paths, opts: &WriteOpts, id: &str) -> Result<Written<Label>> {
    project::update(paths, opts, |project| {
        let Some(at) = project.labels.iter().position(|l| l.id == id) else {
            return Err(Error::usage("unknown_label", format!("There is no label {id}."))
                .hint(format!("Run `scrubr label {} list` to list label ids.", paths.shown.display())));
        };
        Ok(project.labels.remove(at))
    })
}

/// Creates a project from one or more audio files, placed end to end. The first file sets the
/// project's sample rate and channel count; later files are converted to match. Builds the peak
/// cache for each.
pub fn new_project(paths: &Paths, files: &[PathBuf], who: &Who, force: bool) -> Result<Project> {
    if files.is_empty() {
        return Err(Error::usage("no_sources", "A project needs at least one audio file.").hint("Pass one or more files with --from."));
    }
    if paths.project.exists() && !force {
        return Err(Error::usage("project_exists", format!("{} already exists.", paths.shown.display()))
            .hint("Pick another name, or pass --force to replace it."));
    }
    let first = source::probe(&files[0])?;
    let mut project = Project::new(first.sample_rate, first.channels);
    let mut ids = Vec::new();
    for file in files {
        let id = project.next_source_id();
        match source::import(paths, &id, file, Some(project.sample_rate)) {
            Ok(source) => project.sources.push(source),
            Err(e) => {
                ids.iter().chain([&id]).for_each(|id| source::forget(paths, id));
                return Err(e);
            }
        }
        ids.push(id);
    }
    let names: Vec<String> = files.iter().map(|f| f.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
    project.push_edit(Op::Import { sources: ids }, Some(format!("import {}", names.join(", "))), who);
    project::create(paths, &mut project, force)?;
    Ok(project)
}
