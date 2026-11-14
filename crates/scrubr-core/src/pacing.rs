//! Pacing and checking a cut: where a recording pauses, shortening the pauses that only wait,
//! and reading back every join an edit has left.
//!
//! Quiet is judged against the recording itself, not a fixed level. A quiet recording's speech
//! can sit below another's room tone, so a threshold in dBFS that suits one misreads the other.
//! Here the levels of the whole timeline are measured first, and "quiet" is a little above where
//! that recording's own quiet sits. The whole timeline is measured even when a short range is
//! asked about, because a few seconds of speech hold too little quiet to judge by.

use crate::analysis::{self, Levels};
use crate::edit::{self, Applied, Request};
use crate::error::{Error, Result};
use crate::project::{Paths, Project, Who, WriteOpts, Written};
use crate::render::Renderer;
use crate::timeline::Timeline;
use crate::transcribe::Word;
use crate::{phrase, words};

/// How far either side of a join its level is read, in seconds.
const JOIN_REACH_SECONDS: f64 = 0.02;

fn track_index(paths: &Paths, tl: &Timeline, track: Option<&str>) -> Result<Option<usize>> {
    match track {
        None => Ok(None),
        Some(key) => tl.find_track(key).map(Some).ok_or_else(|| {
            Error::usage("unknown_track", format!("There is no track {key}.")).hint(format!("Run `scrubr track {} list` to see the tracks.", paths.shown.display()))
        }),
    }
}

fn measure(project: &Project, paths: &Paths, tl: &Timeline, track: Option<&str>) -> Result<Levels> {
    let listen = track_index(paths, tl, track)?;
    let mut renderer = Renderer::of(project, paths, listen)?;
    analysis::levels(&mut renderer, 0, tl.total(), project.sample_rate)
}

/// The shortest quiet that word times are moved out of, and the least audio measured to find
/// where quiet sits, in seconds.
const SETTLE_PAUSE_SECONDS: f64 = 0.1;
const SETTLE_WINDOW_SECONDS: f64 = 120.0;
/// The most sound after a word's end that is taken to be the rest of that word, in seconds.
const SETTLE_TAIL_SECONDS: f64 = 0.35;

/// Moves word times out of pauses, and says how many words it moved.
///
/// A speech model's word times are loose in one way that can be measured: it starts a word that
/// follows a pause early, inside the pause, and lets the word before run on into it. The level
/// knows where the pause is. So a word that starts in a pause and ends after it starts where the
/// pause ends, and one that starts before a pause and ends in it ends where the pause begins. On
/// the test recording that took the words after a pause from a median of 130 ms early to 20 ms.
/// The model also ends the last word before a pause too soon, with the word still sounding; when
/// a pause begins within a third of a second and no other word comes first, the word runs to it.
/// Words in running speech are left as they were heard.
///
/// `range` is the timeline range the words were heard over. Quiet is judged from that range,
/// widened to two minutes when it is shorter.
pub fn settle(project: &Project, paths: &Paths, track: Option<usize>, range: (u64, u64), words: &mut [Word]) -> Result<usize> {
    let rate = project.sample_rate;
    let mut renderer = Renderer::of(project, paths, track)?;
    let total = renderer.total();
    let window = (SETTLE_WINDOW_SECONDS * rate as f64) as u64;
    let length = range.1.saturating_sub(range.0).max(window);
    let t1 = (range.0.saturating_sub((length - (range.1.saturating_sub(range.0))) / 2) + length).min(total);
    let t0 = t1.saturating_sub(length);
    let levels = analysis::levels(&mut renderer, t0, t1, rate)?;
    let pauses = levels.pauses((SETTLE_PAUSE_SECONDS * rate as f64) as u64);
    let tail = (SETTLE_TAIL_SECONDS * rate as f64) as u64;
    let mut moved = 0;
    for i in 0..words.len() {
        if words[i].end <= words[i].start {
            continue;
        }
        // Where the next word begins as the model heard it; a tail is not taken from under it.
        let next = words.get(i + 1).map_or(u64::MAX, |w| w.start);
        let word = &mut words[i];
        let (start, end) = (word.start, word.end);
        // The pause the start is in, if the word runs on past it.
        let at = pauses.partition_point(|p| p.1 <= word.start);
        if let Some(&(_, z)) = pauses.get(at).filter(|p| p.0 <= start && end > p.1) {
            word.start = z;
        }
        // The pause the end is in, if the word began before it.
        let at = pauses.partition_point(|p| p.1 < word.end);
        if let Some(&(a, _)) = pauses.get(at).filter(|p| p.0 < end && word.start < p.0) {
            word.end = a;
        }
        // Sound that runs on from the word's end to a pause, with no word of its own.
        let at = pauses.partition_point(|p| p.0 < word.end);
        if let Some(&(a, z)) = pauses.get(at).filter(|p| p.0 >= word.end + 3 * levels.step && p.0 - word.end <= tail && next >= p.0) {
            // Not into the pause the word's own start was moved out of.
            if z > word.start {
                word.end = a;
            }
        }
        if (word.start, word.end) != (start, end) {
            moved += 1;
        }
    }
    Ok(moved)
}

/// One stretch that stays quiet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pause {
    /// Timeline frames.
    pub start: u64,
    pub end: u64,
    /// It runs to the start or the end of the timeline: the quiet before the first sound or
    /// after the last, not a wait between two sounds.
    pub at_edge: bool,
}

/// What [`pauses`] found.
#[derive(Clone, Debug)]
pub struct Pauses {
    pub found: Vec<Pause>,
    pub levels: Levels,
}

/// Every stretch of `range` (or of the timeline) that stays under the recording's own quiet
/// level for at least `min` frames. One that runs past an end of the range is given as far as
/// the range goes.
pub fn pauses(project: &Project, paths: &Paths, track: Option<&str>, range: Option<(u64, u64)>, min: u64) -> Result<Pauses> {
    let tl = Timeline::build(project)?;
    let (t0, t1) = range.unwrap_or((0, tl.total()));
    if range.is_some() {
        edit::check_range(project, paths, &tl, (t0, t1))?;
    }
    let levels = measure(project, paths, &tl, track)?;
    let found = levels
        .pauses(1)
        .into_iter()
        .filter(|&(a, b)| a < t1 && b > t0)
        .map(|(a, b)| Pause { start: a.max(t0), end: b.min(t1), at_edge: a == 0 || b >= tl.total() })
        .filter(|p| p.end - p.start >= min.max(1))
        .collect();
    Ok(Pauses { found, levels })
}

/// Timeline ranges, in frames.
type Spans = Vec<(u64, u64)>;

/// What [`tighten`] is asked to do, in frames.
#[derive(Clone, Copy, Debug)]
pub struct Tighten {
    /// Only pauses that lie wholly inside this range.
    pub range: Option<(u64, u64)>,
    /// Pauses longer than this are shortened,
    pub over: u64,
    /// to this.
    pub to: u64,
    pub crossfade: u32,
}

/// What [`tighten`] did.
#[derive(Clone, Debug)]
pub struct Tightened {
    pub applied: Applied,
    pub asked: Tighten,
    /// Each pause that was shortened, as it was, in timeline frames before the edit.
    pub pauses: Vec<(u64, u64)>,
    /// What was cut from each.
    pub cuts: Vec<(u64, u64)>,
    pub levels: Levels,
}

/// Shortens every pause longer than `over` frames to `to` frames, as one edit. A pause is a
/// stretch that stays under the recording's own quiet level, with sound either side of it: the
/// quiet before the first sound and after the last is left alone. The middle of a pause is cut
/// and what is left is shared between its two ends. `track` listens to one track alone; the cut
/// is made on every track, as cuts are. `ask` is given the project as it is when the edit is
/// made, to turn lengths and a range into frames.
pub fn tighten(paths: &Paths, opts: &WriteOpts, who: &Who, track: Option<&str>, ask: impl FnOnce(&Project, &Timeline) -> Result<Tighten>) -> Result<Written<Tightened>> {
    let mut found: Option<(Tighten, Spans, Spans, Levels)> = None;
    let written = edit::apply(paths, opts, who, true, |project, tl| {
        let asked = ask(project, tl)?;
        let (over, to) = (asked.over, asked.to);
        if to >= over {
            return Err(Error::usage("bad_pause_lengths", "The length pauses are shortened to has to be less than the length they are found by.").hint("For example `--over 0.6s --to 0.3s`."));
        }
        let (t0, t1) = asked.range.unwrap_or((0, tl.total()));
        if asked.range.is_some() {
            edit::check_range(project, paths, tl, (t0, t1))?;
        }
        let levels = measure(project, paths, tl, track)?;
        let all: Vec<(u64, u64)> = levels.pauses(1).into_iter().filter(|&(a, b)| a > 0 && b < tl.total() && a >= t0 && b <= t1).collect();
        let pauses: Vec<(u64, u64)> = all.iter().copied().filter(|(a, b)| b - a > over).collect();
        if pauses.is_empty() {
            let longest = all.iter().map(|(a, b)| b - a).max().unwrap_or(0);
            return Err(Error::usage(
                "no_long_pauses",
                format!("No pause is longer than {:.3}s. The longest is {:.3}s, with quiet taken as under {:.1} dBFS.", project.seconds(over), project.seconds(longest), levels.quiet_db),
            )
            .hint(format!("Give a shorter length with --over, or list the pauses with `scrubr analyze {} --pauses`.", paths.shown.display())));
        }
        let cuts: Vec<(u64, u64)> = pauses.iter().map(|&(a, b)| (a + to / 2, b - (to - to / 2))).collect();
        let requests = cuts.iter().map(|&range| Request::Cut { range, crossfade: asked.crossfade }).collect();
        let desc = format!("tighten {} pauses over {:.2}s to {:.2}s", pauses.len(), project.seconds(over), project.seconds(to));
        found = Some((asked, pauses, cuts, levels));
        Ok((requests, Some(desc)))
    })?;
    let (asked, pauses, cuts, levels) = found.expect("the edit was built");
    Ok(Written { value: Tightened { applied: written.value, asked, pauses, cuts, levels }, version: written.version, wrote: written.wrote })
}

/// What is heard at a join.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Landing {
    /// Under the recording's own quiet level: a join nobody hears.
    Quiet,
    /// Above quiet and well under speech.
    Low,
    /// At speaking level: the join lands on sound.
    Sound,
}

impl Landing {
    pub fn as_str(self) -> &'static str {
        match self {
            Landing::Quiet => "quiet",
            Landing::Low => "low",
            Landing::Sound => "sound",
        }
    }
}

/// One join the edits have left, and what plays across it.
#[derive(Clone, Debug)]
pub struct Join {
    /// Where the join is on the timeline, in frames.
    pub at: u64,
    /// The loudest moment within 20 ms either side, in dBFS, and what that counts as.
    pub level_db: f64,
    pub heard: Landing,
    /// The last word before the join and the first after it, when a transcript is kept.
    pub before: Option<Word>,
    pub after: Option<Word>,
    /// Words the join runs through, going by their times. Empty when the join sits in quiet.
    pub clipped: Vec<String>,
    /// The words either side are the same word: a repeat the cut made or left.
    pub doubled: bool,
}

impl Join {
    /// Worth a second look: it does not sit in quiet, it runs through a word, or it leaves a
    /// word said twice.
    pub fn check(&self) -> bool {
        self.heard != Landing::Quiet || self.doubled || !self.clipped.is_empty()
    }
}

/// What [`joins`] found.
#[derive(Clone, Debug)]
pub struct JoinReport {
    pub joins: Vec<Join>,
    pub levels: Levels,
    /// False when no transcript is kept, so nothing is known about the words at each join.
    pub has_words: bool,
}

/// Reads back every join on the timeline (or in `range`): how loud it is against the
/// recording's own quiet, and, from the kept transcript, the words either side of it. A join is
/// where two pieces of audio that were not next to each other now meet: where something was
/// cut, where a file was inserted, where one file follows another.
pub fn joins(project: &Project, paths: &Paths, track: Option<&str>, range: Option<(u64, u64)>) -> Result<JoinReport> {
    let tl = Timeline::build(project)?;
    let listen = track_index(paths, &tl, track)?;
    let (t0, t1) = range.unwrap_or((0, tl.total()));
    if range.is_some() {
        edit::check_range(project, paths, &tl, (t0, t1))?;
    }
    let rate = project.sample_rate;
    let levels = measure(project, paths, &tl, track)?;
    let store = words::load(paths);
    let set = words::set_for(&store, &tl, listen).or_else(|| if listen.is_none() { words::set_for(&store, &tl, Some(0)) } else { None });
    let said = set.map(|set| words::on_timeline(set, &tl)).unwrap_or_default();
    let broken = set.map(|set| words::broken(set, &tl)).unwrap_or_default();
    let reach = (JOIN_REACH_SECONDS * rate as f64) as u64;
    let segs = tl.segs();
    let mut out = Vec::new();
    for i in 1..segs.len() {
        let at = tl.seg_start(i);
        // Two pieces that follow each other in their file are not a join.
        if at < t0 || at > t1 || (segs[i - 1].src == segs[i].src && segs[i - 1].end == segs[i].start) {
            continue;
        }
        let level_db = levels.loudest(at.saturating_sub(reach), at + reach);
        let heard = levels.landing(level_db);
        let next = said.partition_point(|w| w.start < at);
        let before = next.checked_sub(1).map(|i| &said[i]);
        let after = said.get(next);
        // Word times are loose and the level is not: a join that sits in quiet ran through no
        // word, whatever the times say.
        let clipped: Vec<String> = match heard {
            Landing::Quiet => Vec::new(),
            _ => broken.iter().filter(|(join, _)| *join == at).map(|(_, text)| text.clone()).collect(),
        };
        let doubled = match (before, after) {
            (Some(b), Some(a)) => !phrase::plain(&b.text).is_empty() && phrase::plain(&b.text) == phrase::plain(&a.text),
            _ => false,
        };
        out.push(Join { at, level_db, heard, before: before.cloned(), after: after.cloned(), clipped, doubled });
    }
    Ok(JoinReport { joins: out, levels, has_words: set.is_some() })
}

impl Levels {
    /// What a level counts as, against this recording's own quiet and speech.
    pub fn landing(&self, db: f64) -> Landing {
        if db < self.quiet_db {
            Landing::Quiet
        } else if db >= self.speech_db - 18.0 {
            Landing::Sound
        } else {
            Landing::Low
        }
    }
}
