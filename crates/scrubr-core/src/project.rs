//! The project file: sources, the edit log, a head pointer, labels and a version.
//!
//! Edits and labels are stored in source coordinates, as a source id and a frame range in that
//! source, so an edit keeps its meaning when an earlier edit is undone. The timeline is never
//! stored; [`crate::timeline::Timeline`] computes it by replaying the log up to head.
//!
//! A project has one or more tracks that share one timeline. Time is what the main track's
//! sources lay out: every cut, keep and insert acts on them. The audio of every other track is
//! attached to frames of those sources, so it is cut, restored and moved with them and the
//! tracks cannot drift apart. A stretch of time with no main audio is a silence source.
//!
//! Writes go through [`update`]: an OS file lock next to the project, a version check, a
//! temporary file and a rename, so two writers cannot both pass and a killed writer leaves the
//! previous version intact.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Kind, Result, io_at};

/// The format of a project with one track, as written since the first version.
pub const FORMAT: u32 = 1;

/// The format of a project that uses tracks. A build from before tracks refuses to read it.
pub const FORMAT_TRACKS: u32 = 2;

/// The id of the main track, which every project has: the audio that lays out time.
pub const MAIN_TRACK: &str = "t1";

/// A frame range `[start, end)` in one source: `["src1", 37932000, 38251200]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span(pub String, pub u64, pub u64);

impl Span {
    pub fn frames(&self) -> u64 {
        self.2.saturating_sub(self.1)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    /// Relative to the project file's folder when the file lives under it, absolute otherwise.
    /// Always written with forward slashes.
    pub path: String,
    /// Length in frames at the project's sample rate.
    pub frames: u64,
    pub bytes: u64,
    pub mtime: String,
    /// `blake3:<hex>` of the whole file.
    pub hash: String,
    /// The file's own sample rate and channel count.
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    #[serde(default, skip_serializing_if = "is_false")]
    pub float: bool,
    /// True when the file is not a WAV at the project's rate. Its audio is then read from a
    /// converted copy in the cache folder, which is rebuilt from the file when missing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub conformed: bool,
    /// True for a stretch of time with no audio of its own: there is no file, and `path`,
    /// `bytes`, `mtime` and `hash` are empty. It gives another track's audio time to sit on.
    #[serde(default, skip_serializing_if = "is_false")]
    pub silence: bool,
}

impl Source {
    /// A stretch of silence `frames` long.
    pub fn silence(id: String, frames: u64, sample_rate: u32, channels: u16) -> Source {
        Source { id, path: String::new(), frames, bytes: 0, mtime: String::new(), hash: String::new(), sample_rate, channels, bits: 0, float: false, conformed: false, silence: true }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FadeDir {
    In,
    Out,
}

/// One operation in the log. Every `spans` list is in source coordinates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Appends each source, whole, to the end of the timeline.
    Import { sources: Vec<String> },
    /// Removes `spans`. `crossfade` is the length in frames of the fade across each join.
    Cut {
        spans: Vec<Span>,
        #[serde(default)]
        crossfade: u32,
    },
    /// Removes everything outside a range. `spans` is the audio it removes, so audio an earlier
    /// edit had hidden inside the kept range comes back if that edit is reverted.
    Keep {
        spans: Vec<Span>,
        #[serde(default)]
        crossfade: u32,
    },
    /// A level change, over `spans` or, without them, over the whole timeline. With `track` it
    /// changes that track alone; without, every track.
    Gain {
        gain_db: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spans: Option<Vec<Span>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        track: Option<String>,
    },
    /// A linear ramp across `spans`, taken in order: up from silence for `in`, down to it for
    /// `out`.
    Fade {
        dir: FadeDir,
        spans: Vec<Span>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        track: Option<String>,
    },
    /// A whole-timeline level change. `gain_db` is the gain that was resolved when the edit was
    /// made, so replaying the log never measures the timeline again.
    Normalize {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lufs: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peak_db: Option<f64>,
        gain_db: f64,
        ceiling_dbtp: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        track: Option<String>,
    },
    /// Silence over `spans`, with a ramp of `crossfade` frames inside each end.
    Mute {
        spans: Vec<Span>,
        #[serde(default)]
        crossfade: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        track: Option<String>,
    },
    /// Places `source`, whole, just before frame `at.1` of source `at.0`. Without `at` it goes
    /// at the end of the timeline.
    Insert {
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<(String, u64)>,
        #[serde(default)]
        crossfade: u32,
    },
    /// Adds a track: a lane of audio beside the main one. Its audio arrives with `clip`.
    Track { track: String, name: String },
    /// Attaches `source`, from its first frame, to the main track's frames in `at`, taken in
    /// order. The clip is heard wherever those frames are on the timeline.
    Clip { track: String, source: String, at: Vec<Span> },
    /// Changes a track's name or its place in the mix. What is left out keeps its value.
    TrackSet {
        track: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gain_db: Option<f64>,
        /// From -1.0, full left, to 1.0, full right.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pan: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mute: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        solo: Option<bool>,
    },
    /// Takes a track and its audio out of the project. Time is not changed.
    TrackRemove { track: String },
    /// Several operations written as one edit and undone as one step.
    Batch { ops: Vec<Op> },
    /// Takes earlier edits out of the timeline without moving the edits around them.
    Revert { edits: Vec<String> },
}

impl Op {
    pub fn name(&self) -> &'static str {
        match self {
            Op::Import { .. } => "import",
            Op::Cut { .. } => "cut",
            Op::Keep { .. } => "keep",
            Op::Gain { .. } => "gain",
            Op::Fade { .. } => "fade",
            Op::Normalize { .. } => "normalize",
            Op::Mute { .. } => "mute",
            Op::Insert { .. } => "insert",
            Op::Track { .. } => "track",
            Op::Clip { .. } => "clip",
            Op::TrackSet { .. } => "track_set",
            Op::TrackRemove { .. } => "track_remove",
            Op::Batch { .. } => "batch",
            Op::Revert { .. } => "revert",
        }
    }

    /// Source ids this operation brings into the project.
    fn collect_sources<'a>(&'a self, out: &mut HashSet<&'a str>) {
        match self {
            Op::Import { sources } => out.extend(sources.iter().map(String::as_str)),
            Op::Insert { source, .. } | Op::Clip { source, .. } => {
                out.insert(source);
            }
            Op::Batch { ops } => ops.iter().for_each(|op| op.collect_sources(out)),
            _ => {}
        }
    }

    /// True for an operation a build from before tracks would not understand.
    fn uses_tracks(&self) -> bool {
        match self {
            Op::Track { .. } | Op::Clip { .. } | Op::TrackSet { .. } | Op::TrackRemove { .. } => true,
            Op::Gain { track, .. } | Op::Fade { track, .. } | Op::Normalize { track, .. } | Op::Mute { track, .. } => track.is_some(),
            Op::Batch { ops } => ops.iter().any(Op::uses_tracks),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    pub id: String,
    #[serde(flatten)]
    pub op: Op,
    /// What the author asked for, in timeline terms at the moment of the edit. For display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub id: String,
    pub span: Span,
    pub text: String,
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
}

/// The last number handed out for each kind of id, so an id is never used twice even after
/// undone edits are discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ids {
    #[serde(default)]
    pub edit: u64,
    #[serde(default)]
    pub source: u64,
    #[serde(default)]
    pub label: u64,
    /// The main track is `t1`, so the first track added is `t2`.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub track: u64,
}

fn one() -> u64 {
    1
}

fn is_one(n: &u64) -> bool {
    *n == 1
}

impl Default for Ids {
    fn default() -> Ids {
        Ids { edit: 0, source: 0, label: 0, track: 1 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub format: u32,
    /// Increases on every write.
    pub version: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub sources: Vec<Source>,
    pub edits: Vec<Edit>,
    /// The last edit that is applied. Edits after it have been undone and can be redone.
    pub head: Option<String>,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub ids: Ids,
}

/// Who is making an edit.
#[derive(Clone, Debug)]
pub struct Who {
    pub author: String,
    pub session: Option<String>,
}

/// Where an edit stands relative to head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditState {
    Applied,
    /// After head: undone, and still redoable.
    Undone,
    /// At or before head, but taken out by the revert edit named here.
    Reverted(String),
}

impl EditState {
    pub fn name(&self) -> &'static str {
        match self {
            EditState::Applied => "applied",
            EditState::Undone => "undone",
            EditState::Reverted(_) => "reverted",
        }
    }
}

fn numeric_suffix(id: &str) -> u64 {
    let digits: String = id.chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect();
    digits.parse().unwrap_or(0)
}

impl Project {
    pub fn new(sample_rate: u32, channels: u16) -> Project {
        Project {
            format: FORMAT,
            version: 0,
            sample_rate,
            channels,
            sources: Vec::new(),
            edits: Vec::new(),
            head: None,
            labels: Vec::new(),
            ids: Ids::default(),
        }
    }

    pub fn parse(text: &str, path: &Path) -> Result<Project> {
        let invalid = |what: String| {
            Error::codec("project_invalid", format!("{} is not a readable scrubr project: {what}", path.display()))
        };
        let mut project: Project = serde_json::from_str(text).map_err(|e| invalid(e.to_string()))?;
        if project.format == 0 || project.format > FORMAT_TRACKS {
            return Err(invalid(format!("it has format {}, and this build reads formats up to {FORMAT_TRACKS}", project.format))
                .hint("Open it with the scrubr version that wrote it."));
        }
        if project.sample_rate == 0 || project.channels == 0 {
            return Err(invalid("the sample rate and channel count must not be zero".into()));
        }
        if let Some(head) = &project.head
            && !project.edits.iter().any(|e| &e.id == head)
        {
            return Err(invalid(format!("head names edit {head}, which is not in the log")));
        }
        // Counters are missing from a hand-written file; never hand out an id already in use.
        let max = |ids: &mut dyn Iterator<Item = &str>| ids.map(numeric_suffix).max().unwrap_or(0);
        project.ids.edit = project.ids.edit.max(max(&mut project.edits.iter().map(|e| e.id.as_str())));
        project.ids.source = project.ids.source.max(max(&mut project.sources.iter().map(|s| s.id.as_str())));
        project.ids.label = project.ids.label.max(max(&mut project.labels.iter().map(|l| l.id.as_str())));
        Ok(project)
    }

    pub fn load(path: &Path) -> Result<Project> {
        Self::load_shown(path, path)
    }

    /// Reads the project at `path`, naming it `shown` in messages.
    fn load_shown(path: &Path, shown: &Path) -> Result<Project> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::not_found("project_not_found", format!("There is no project at {}.", shown.display()))
                    .hint(format!("Check the path, or create one with `scrubr new {} --from <audio file>`.", shown.display())));
            }
            Err(e) => return Err(io_at(shown, e)),
        };
        Project::parse(&text, shown)
    }

    /// The file's text: one source, edit or label per line, so a long log stays readable and
    /// diffs stay small.
    pub fn to_json(&self) -> String {
        fn list<T: Serialize>(out: &mut String, name: &str, items: &[T]) {
            if items.is_empty() {
                out.push_str(&format!("  \"{name}\": [],\n"));
                return;
            }
            out.push_str(&format!("  \"{name}\": [\n"));
            for (i, item) in items.iter().enumerate() {
                let comma = if i + 1 < items.len() { "," } else { "" };
                out.push_str(&format!("    {}{comma}\n", serde_json::to_string(item).expect("project data is plain JSON")));
            }
            out.push_str("  ],\n");
        }
        fn json<T: Serialize>(value: &T) -> String {
            serde_json::to_string(value).expect("project data is plain JSON")
        }
        let mut out = String::from("{\n");
        out.push_str(&format!("  \"format\": {},\n", self.format));
        out.push_str(&format!("  \"version\": {},\n", self.version));
        out.push_str(&format!("  \"sample_rate\": {},\n", self.sample_rate));
        out.push_str(&format!("  \"channels\": {},\n", self.channels));
        list(&mut out, "sources", &self.sources);
        list(&mut out, "edits", &self.edits);
        out.push_str(&format!("  \"head\": {},\n", json(&self.head)));
        list(&mut out, "labels", &self.labels);
        out.push_str(&format!("  \"ids\": {}\n", json(&self.ids)));
        out.push_str("}\n");
        out
    }

    pub fn head_index(&self) -> Option<usize> {
        let head = self.head.as_ref()?;
        self.edits.iter().position(|e| &e.id == head)
    }

    pub fn source_index(&self, id: &str) -> Option<usize> {
        self.sources.iter().position(|s| s.id == id)
    }

    pub fn seconds(&self, frames: u64) -> f64 {
        frames as f64 / self.sample_rate as f64
    }

    pub fn next_source_id(&mut self) -> String {
        self.ids.source += 1;
        format!("src{}", self.ids.source)
    }

    pub fn next_track_id(&mut self) -> String {
        self.ids.track = self.ids.track.max(1) + 1;
        format!("t{}", self.ids.track)
    }

    /// The state of every edit, in log order.
    pub fn states(&self) -> Vec<EditState> {
        let mut states = vec![EditState::Undone; self.edits.len()];
        let Some(head) = self.head_index() else { return states };
        let mut reverted: HashMap<&str, &str> = HashMap::new();
        for i in (0..=head).rev() {
            let edit = &self.edits[i];
            if let Some(by) = reverted.get(edit.id.as_str()) {
                states[i] = EditState::Reverted(by.to_string());
                continue;
            }
            states[i] = EditState::Applied;
            if let Op::Revert { edits } = &edit.op {
                for target in edits {
                    reverted.entry(target).or_insert(&edit.id);
                }
            }
        }
        states
    }

    /// The operations that make up the timeline at head, in order.
    pub fn applied_ops(&self) -> Vec<&Op> {
        self.edits
            .iter()
            .zip(self.states())
            .filter(|(edit, state)| *state == EditState::Applied && !matches!(edit.op, Op::Revert { .. }))
            .map(|(edit, _)| &edit.op)
            .collect()
    }

    /// Appends an edit at head and moves head to it. Edits that had been undone are discarded,
    /// along with sources and labels only they brought in; the ids of those sources are returned
    /// so their cache files can be removed.
    pub fn push_edit(&mut self, op: Op, desc: Option<String>, who: &Who) -> (String, Vec<String>) {
        let keep = self.head_index().map_or(0, |i| i + 1);
        let discarded = self.edits.len() > keep;
        self.edits.truncate(keep);
        // From the first edit that uses tracks, older builds must not read the file.
        if op.uses_tracks() {
            self.format = self.format.max(FORMAT_TRACKS);
        }
        self.ids.edit += 1;
        let id = format!("e{}", self.ids.edit);
        self.edits.push(Edit {
            id: id.clone(),
            op,
            desc,
            author: who.author.clone(),
            session: who.session.clone(),
            time: Some(now_string()),
        });
        self.head = Some(id.clone());
        let mut dropped = Vec::new();
        if discarded {
            let mut used = HashSet::new();
            self.edits.iter().for_each(|e| e.op.collect_sources(&mut used));
            let used: HashSet<String> = used.into_iter().map(str::to_string).collect();
            self.sources.retain(|s| {
                let keep = used.contains(&s.id);
                if !keep {
                    dropped.push(s.id.clone());
                }
                keep
            });
            self.labels.retain(|l| used.contains(&l.span.0));
        }
        (id, dropped)
    }

    /// Moves head back `steps` edits and returns the ids that were undone. The first edit, which
    /// created the project, stays.
    pub fn undo(&mut self, steps: usize) -> Result<Vec<String>> {
        let head = self.head_index().unwrap_or(0);
        if head == 0 || self.edits.is_empty() {
            return Err(Error::usage("nothing_to_undo", "There is nothing to undo: head is at the edit that created the project.")
                .hint("Run `scrubr history <project>` to see the log."));
        }
        let to = head.saturating_sub(steps.max(1));
        let undone = self.edits[to + 1..=head].iter().rev().map(|e| e.id.clone()).collect();
        self.head = Some(self.edits[to].id.clone());
        Ok(undone)
    }

    /// Moves head forward `steps` edits and returns the ids that were redone.
    pub fn redo(&mut self, steps: usize) -> Result<Vec<String>> {
        let head = self.head_index().map_or(0, |i| i + 1);
        if head >= self.edits.len() {
            return Err(Error::usage("nothing_to_redo", "There is nothing to redo: head is at the last edit.")
                .hint("Run `scrubr history <project>` to see the log."));
        }
        let to = (head + steps.max(1)).min(self.edits.len());
        let redone = self.edits[head..to].iter().map(|e| e.id.clone()).collect();
        self.head = Some(self.edits[to - 1].id.clone());
        Ok(redone)
    }

    /// Checks that each of `ids` can be reverted: it is in the log, applied, and not the edit
    /// that created the project.
    pub fn check_revertable(&self, ids: &[String]) -> Result<()> {
        let states = self.states();
        for id in ids {
            let Some(i) = self.edits.iter().position(|e| &e.id == id) else {
                return Err(Error::usage("unknown_edit", format!("There is no edit {id} in the log."))
                    .hint("Run `scrubr history <project>` to list edit ids."));
            };
            if i == 0 {
                return Err(Error::usage("cannot_revert", format!("Edit {id} created the project and cannot be reverted.")));
            }
            if states[i] != EditState::Applied {
                return Err(Error::usage("not_applied", format!("Edit {id} is already {}.", states[i].name()))
                    .hint("Run `scrubr history <project>` to see which edits are applied."));
            }
        }
        Ok(())
    }

    pub fn add_label(&mut self, span: Span, text: String, who: &Who) -> String {
        self.ids.label += 1;
        let id = format!("l{}", self.ids.label);
        self.labels.push(Label {
            id: id.clone(),
            span,
            text,
            author: who.author.clone(),
            session: who.session.clone(),
            time: Some(now_string()),
        });
        id
    }
}

// ---- time stamps ----------------------------------------------------------------------------

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// `2026-11-10T09:14:02Z`, with a fraction when `nanos` is set and the time has one.
pub fn rfc3339(time: SystemTime, nanos: bool) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    let mut out = format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", rem / 3600, rem / 60 % 60, rem % 60);
    if nanos && since.subsec_nanos() != 0 {
        let frac = format!("{:09}", since.subsec_nanos());
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

pub fn now_string() -> String {
    rfc3339(SystemTime::now(), false)
}

// ---- paths ----------------------------------------------------------------------------------

/// The files that belong to one project: the project file, its cache folder and its lock file,
/// all side by side.
#[derive(Clone, Debug)]
pub struct Paths {
    /// The project path as it was given, for messages and hints.
    pub shown: PathBuf,
    /// The same path made absolute.
    pub project: PathBuf,
    /// The folder the project file is in. Relative source paths start here.
    pub dir: PathBuf,
    /// `<project>.cache/`: peak data and converted copies of sources. Rebuilt when missing.
    pub cache: PathBuf,
    /// `<project>.lock`: the file writers lock.
    pub lock: PathBuf,
}

impl Paths {
    pub fn new(project: impl AsRef<Path>) -> Paths {
        let given = project.as_ref();
        let project = std::path::absolute(given).unwrap_or_else(|_| given.to_path_buf());
        let dir = project.parent().map(Path::to_path_buf).unwrap_or_default();
        let sibling = |suffix: &str| {
            let mut name = project.file_name().unwrap_or_default().to_os_string();
            name.push(suffix);
            dir.join(name)
        };
        Paths { shown: given.to_path_buf(), cache: sibling(".cache"), lock: sibling(".lock"), dir, project }
    }

    /// Where a source's own file is.
    pub fn source_file(&self, source: &Source) -> PathBuf {
        let path = Path::new(&source.path);
        if path.is_absolute() { path.to_path_buf() } else { self.dir.join(path) }
    }

    /// The file a source's audio is read from: the file itself, or its converted copy.
    pub fn audio_file(&self, source: &Source) -> PathBuf {
        if source.conformed { self.cache.join(format!("{}.wav", source.id)) } else { self.source_file(source) }
    }

    pub fn peaks_file(&self, source: &Source) -> PathBuf {
        self.cache.join(format!("{}.peaks", source.id))
    }

    /// `file` as the project stores it: relative to the project folder when it is inside it.
    pub fn stored_path(&self, file: &Path) -> String {
        let absolute = std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf());
        let shown = absolute.strip_prefix(&self.dir).unwrap_or(&absolute);
        shown.to_string_lossy().replace('\\', "/")
    }
}

// ---- locked, versioned, atomic writes --------------------------------------------------------

#[derive(Clone, Debug)]
pub struct WriteOpts {
    /// Refuse the write unless the project is at this version.
    pub if_version: Option<u64>,
    /// Run the change and report it, but write nothing.
    pub dry_run: bool,
    /// How long to wait for another writer to finish.
    pub lock_timeout: Duration,
}

impl Default for WriteOpts {
    fn default() -> Self {
        WriteOpts { if_version: None, dry_run: false, lock_timeout: Duration::from_secs(10) }
    }
}

#[derive(Debug)]
pub struct Written<T> {
    pub value: T,
    /// The project's version after the call: one more than before, or unchanged on a dry run.
    pub version: u64,
    pub wrote: bool,
}

/// Runs `f` while holding the project's write lock.
fn with_lock<T>(paths: &Paths, timeout: Duration, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)
        .map_err(|e| io_at(&paths.lock, e))?;
    let mut lock = fd_lock::RwLock::new(file);
    let started = Instant::now();
    let guard = loop {
        match lock.try_write() {
            Ok(guard) => break guard,
            Err(e) => {
                if started.elapsed() >= timeout {
                    return Err(Error::conflict(
                        "project_locked",
                        format!("Another scrubr command has held the write lock on {} for {:.1} s ({e}).", paths.shown.display(), timeout.as_secs_f64()),
                    )
                    .hint("Wait for the other command to finish, then re-read the project with `scrubr info` and retry."));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };
    let result = f();
    drop(guard);
    result
}

/// Writes `text` to `path` through a temporary file and a rename. On Windows the rename is
/// retried briefly when another program has the file open.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}", std::process::id()));
    let tmp = path.with_file_name(name);
    let write = || -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(io_at(&tmp, e));
    }
    let started = Instant::now();
    loop {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && started.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(io_at(path, e));
            }
        }
    }
}

fn version_conflict(paths: &Paths, expected: u64, actual: u64) -> Error {
    Error::conflict(
        "version_conflict",
        format!("The project is at version {actual}, not {expected}: it changed since the version this command expected."),
    )
    .hint(format!(
        "Run `scrubr info {}` and `scrubr history {}` to see what changed, recompute positions, and retry with --if-version {actual}.",
        paths.shown.display(),
        paths.shown.display()
    ))
}

/// Creates the project file. Fails if one is already there, unless `force` is set.
pub fn create(paths: &Paths, project: &mut Project, force: bool) -> Result<()> {
    with_lock(paths, WriteOpts::default().lock_timeout, || {
        if paths.project.exists() && !force {
            return Err(Error::usage("project_exists", format!("{} already exists.", paths.shown.display()))
                .hint("Pick another name, or pass --force to replace it."));
        }
        project.version = 1;
        write_atomic(&paths.project, &project.to_json())
    })
}

/// Changes the project in place: lock, read, check the version and the sources, run `f`, then
/// write the next version through a temporary file and a rename. If `f` fails, or `dry_run` is
/// set, nothing is written.
pub fn update<T>(paths: &Paths, opts: &WriteOpts, f: impl FnOnce(&mut Project) -> Result<T>) -> Result<Written<T>> {
    with_lock(paths, opts.lock_timeout, || {
        let mut project = Project::load_shown(&paths.project, &paths.shown)?;
        if let Some(expected) = opts.if_version
            && expected != project.version
        {
            return Err(version_conflict(paths, expected, project.version));
        }
        for refresh in verify_sources(&project, paths, false)? {
            project.sources[refresh.index].mtime = refresh.mtime;
        }
        let value = f(&mut project)?;
        if opts.dry_run {
            return Ok(Written { value, version: project.version, wrote: false });
        }
        project.version += 1;
        write_atomic(&paths.project, &project.to_json())?;
        Ok(Written { value, version: project.version, wrote: true })
    })
}

/// Reads the project without the lock. A write is a rename, so a reader sees one whole version.
/// Sources are checked by size and modified time, as on every command.
pub fn open(paths: &Paths) -> Result<Project> {
    let project = Project::load_shown(&paths.project, &paths.shown)?;
    verify_sources(&project, paths, false)?;
    Ok(project)
}

// ---- source checks ---------------------------------------------------------------------------

/// A source whose modified time changed while its content did not. Writers store the new time
/// so the next command can skip the hash again.
pub struct Refresh {
    pub index: usize,
    pub mtime: String,
}

pub fn file_mtime(meta: &fs::Metadata) -> String {
    rfc3339(meta.modified().unwrap_or(UNIX_EPOCH), true)
}

/// `blake3:<hex>` of the whole file, streamed.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|e| io_at(path, e))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(&mut file).map_err(|e| io_at(path, e))?;
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// Compares every source with what the project recorded. Size and modified time are compared
/// first; the full hash runs when either differs, or for every source when `full` is set (before
/// export). A source whose content changed stops the command.
pub fn verify_sources(project: &Project, paths: &Paths, full: bool) -> Result<Vec<Refresh>> {
    let mut refreshes = Vec::new();
    for (index, source) in project.sources.iter().enumerate().filter(|(_, s)| !s.silence) {
        let file = paths.source_file(source);
        let meta = match fs::metadata(&file) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::not_found("source_not_found", format!("Source {} is missing: {} does not exist.", source.id, file.display()))
                    .hint("Put the file back where the project expects it. The project stores paths relative to its own folder."));
            }
            Err(e) => return Err(io_at(&file, e)),
        };
        let mtime = file_mtime(&meta);
        let same_stat = meta.len() == source.bytes && mtime == source.mtime;
        if same_stat && !full {
            continue;
        }
        if meta.len() != source.bytes || hash_file(&file)? != source.hash {
            return Err(Error::new(
                Kind::SourceChanged,
                "source_changed",
                format!("Source {} changed on disk: {} no longer matches the audio this project was built on.", source.id, file.display()),
            )
            .hint("Stop and tell the person. Restore the original file, or start a new project from the changed one with `scrubr new`."));
        }
        if !same_stat {
            refreshes.push(Refresh { index, mtime });
        }
    }
    Ok(refreshes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_civil() {
        assert_eq!(rfc3339(UNIX_EPOCH, true), "1970-01-01T00:00:00Z");
        let t = UNIX_EPOCH + Duration::new(1_794_302_042, 500_000_000);
        assert_eq!(rfc3339(t, false), "2026-11-10T09:14:02Z");
        assert_eq!(rfc3339(t, true), "2026-11-10T09:14:02.5Z");
        // A leap day.
        assert_eq!(rfc3339(UNIX_EPOCH + Duration::from_secs(1_709_164_800), false), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn the_prd_example_parses_and_round_trips() {
        let text = r#"{
          "format": 1, "version": 7, "sample_rate": 48000, "channels": 2,
          "sources": [
            { "id": "src1", "path": "raw/ep42.wav", "frames": 172704000, "bytes": 1036224044,
              "mtime": "2026-11-10T09:14:02Z", "hash": "blake3:9f2c", "sample_rate": 48000, "channels": 2, "bits": 24 }
          ],
          "edits": [
            { "id": "e1", "op": "import", "sources": ["src1"], "author": "you" },
            { "id": "e4", "op": "normalize", "lufs": -16.0, "gain_db": 5.4, "ceiling_dbtp": -1.0, "author": "codex", "session": "s7" },
            { "id": "e5", "op": "cut", "spans": [["src1", 37932000, 38251200]], "author": "codex", "session": "s7" }
          ],
          "head": "e5",
          "labels": [
            { "id": "l1", "span": ["src1", 44935200, 45016800], "text": "check: cough?", "author": "codex" }
          ]
        }"#;
        let project = Project::parse(text, Path::new("ep42.scrubr")).unwrap();
        assert_eq!(project.edits[2].op, Op::Cut { spans: vec![Span("src1".into(), 37_932_000, 38_251_200)], crossfade: 0 });
        assert_eq!(project.ids, Ids { edit: 5, source: 1, label: 1, track: 1 });
        let again = Project::parse(&project.to_json(), Path::new("ep42.scrubr")).unwrap();
        assert_eq!(again, project);
        assert!(project.to_json().contains(r#"{"id":"e5","op":"cut","spans":[["src1",37932000,38251200]],"crossfade":0,"author":"codex","session":"s7"}"#));
    }
}
