//! Cutting by words: finding a phrase in what was said, working out where to cut to take it
//! out, and checking the join before anything is written.
//!
//! Word times from a speech model are not exact enough to cut on, so a cut never lands on one.
//! Each end goes to the quietest moment between the word that goes and the word that stays.
//! The result is then listened to again across the join, as it would be after the cut: the
//! phrase must be gone and the words either side must still follow each other. If they do not,
//! nothing is written.

use std::ops::Range;

use crate::analysis::{self, Quiet};
use crate::edit::{self, Applied, Request};
use crate::error::{Error, Result};
use crate::project::{self, Paths, Project, Who, WriteOpts, Written};
use crate::render::Renderer;
use crate::timeline::Timeline;
use crate::transcribe::{self, Engine, Word};

/// How far past a word's reported edge the search for a pause reaches, in seconds. A word's
/// time is typically within a tenth of a second and sometimes half a second out.
pub const MARGIN_SECONDS: f64 = 0.3;
/// How far to look for a pause where there is no word on that side, in seconds.
const EDGE_SECONDS: f64 = 1.0;
/// How much is listened to again either side of a join, in seconds.
const CHECK_SECONDS: f64 = 6.0;
/// How many words of context are kept either side of a match.
const CONTEXT: usize = 2;

/// A word as it is compared: lower case, letters, digits and apostrophes only. "Well," and
/// "well" are the same word.
pub fn plain(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric() || *c == '\'').flat_map(char::to_lowercase).collect()
}

/// One place a phrase was heard.
#[derive(Clone, Debug)]
pub struct Found {
    /// The words of the phrase as they were heard.
    pub words: Vec<Word>,
    /// Up to two words heard just before, and just after.
    pub before: Vec<Word>,
    pub after: Vec<Word>,
    /// Where to cut, in timeline frames: the quietest moment either side.
    pub cut: (u64, u64),
    /// The pause each end of the cut sits in.
    pub quiet: (Quiet, Quiet),
}

/// Which of the places a phrase was heard to cut.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pick {
    /// The only one: more than one is an error that lists them.
    Only,
    /// The nth, counting from 1.
    Nth(usize),
    All,
}

/// Where the words of `phrase` occur in `words`, in order and without overlap.
fn locate(words: &[Word], phrase: &[String]) -> Vec<Range<usize>> {
    let heard: Vec<String> = words.iter().map(|w| plain(&w.text)).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + phrase.len() <= heard.len() {
        if heard[i..i + phrase.len()] == *phrase {
            out.push(i..i + phrase.len());
            i += phrase.len();
        } else {
            i += 1;
        }
    }
    out
}

fn words_of(phrase: &str) -> Result<Vec<String>> {
    let words: Vec<String> = phrase.split_whitespace().map(plain).filter(|w| !w.is_empty()).collect();
    if words.is_empty() {
        return Err(Error::usage("empty_phrase", "The phrase has no words in it.").hint("Give the words to look for, in quotes."));
    }
    Ok(words)
}

/// Transcribes the timeline (or `within` of it) and finds every place `phrase` was said, each
/// with the cut that would take it out. `track` listens to one track alone. Nothing is changed.
pub fn find(
    project: &Project,
    paths: &Paths,
    track: Option<usize>,
    within: Option<(u64, u64)>,
    phrase: &str,
    engine: &mut dyn Engine,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Vec<Found>> {
    let wanted = words_of(phrase)?;
    let words = transcribe::transcribe_track(project, paths, track, within, engine, progress)?;
    let rate = project.sample_rate;
    let mut renderer = Renderer::of(project, paths, track)?;
    let (lo, hi) = within.unwrap_or((0, renderer.total()));
    let (margin, edge) = ((MARGIN_SECONDS * rate as f64) as u64, (EDGE_SECONDS * rate as f64) as u64);
    let mut out = Vec::new();
    for at in locate(&words, &wanted) {
        let (first, last) = (&words[at.start], &words[at.end - 1]);
        let (prev, next) = (at.start.checked_sub(1).map(|i| &words[i]), words.get(at.end));
        // Neither search may cross the middle of the phrase, or a short word's two ends could
        // both settle in the same pause.
        let middle = first.start + (last.end.max(first.start) - first.start) / 2;
        let from = prev.map_or(first.start.saturating_sub(edge), |p| p.end.saturating_sub(margin)).max(lo);
        let to = (first.start + margin).min(middle.max(first.start + 1)).min(hi).max(from + 1);
        let head = analysis::quietest(&mut renderer, from, to, rate)?;
        let from = last.end.saturating_sub(margin).max(middle).max(lo);
        let to = next.map_or(last.end + edge, |n| n.start + margin).min(hi).max(from + 1);
        let tail = analysis::quietest(&mut renderer, from, to, rate)?;
        if tail.at() <= head.at() {
            continue;
        }
        out.push(Found {
            words: words[at.clone()].to_vec(),
            before: words[at.start.saturating_sub(CONTEXT)..at.start].to_vec(),
            after: words[at.end..(at.end + CONTEXT).min(words.len())].to_vec(),
            cut: (head.at(), tail.at()),
            quiet: (head, tail),
        });
    }
    Ok(out)
}

/// What was heard across a join once the cut was made.
#[derive(Clone, Debug)]
pub struct Check {
    /// True when the phrase is gone and the words either side follow each other.
    pub clean: bool,
    /// The words heard around the join.
    pub heard: String,
    /// Where the join is on the timeline after the cut, in frames.
    pub join: u64,
}

fn has(heard: &[String], sequence: &[String]) -> bool {
    !sequence.is_empty() && heard.windows(sequence.len()).any(|w| w == sequence)
}

/// Listens across one join of the timeline `tl` (the project with the cut made, written or
/// not) and says whether the cut is clean.
fn check(project: &Project, paths: &Paths, tl: Timeline, track: Option<usize>, join: u64, found: &Found, engine: &mut dyn Engine) -> Result<Check> {
    let rate = project.sample_rate;
    let span = (CHECK_SECONDS * rate as f64) as u64;
    let renderer = match track {
        Some(track) => Renderer::for_track(project, paths, tl, track)?,
        None => Renderer::with_timeline(project, paths, tl)?,
    };
    let range = (join.saturating_sub(span), (join + span).min(renderer.total()));
    let words = transcribe::transcribe_renderer(renderer, rate, range, engine, &mut |_, _| {})?;
    let heard: Vec<String> = words.iter().map(|w| plain(&w.text)).collect();
    let phrase: Vec<String> = found.words.iter().map(|w| plain(&w.text)).collect();
    let before = found.before.last().map(|w| plain(&w.text));
    let after = found.after.first().map(|w| plain(&w.text));
    // The neighbours follow each other now.
    let joined = match (&before, &after) {
        (Some(b), Some(a)) => has(&heard, &[b.clone(), a.clone()]),
        _ => true,
    };
    // The phrase is not still there beside either neighbour.
    let left = before.as_ref().is_some_and(|b| has(&heard, &[vec![b.clone()], phrase.clone()].concat()));
    let right = after.as_ref().is_some_and(|a| has(&heard, &[phrase.clone(), vec![a.clone()]].concat()));
    let alone = before.is_none() && after.is_none() && has(&heard, &phrase);
    Ok(Check { clean: joined && !left && !right && !alone, heard: transcribe::text(&words), join })
}

/// What [`cut`] did.
#[derive(Clone, Debug)]
pub struct CutPhrase {
    pub applied: Applied,
    /// The places that were cut, with what was heard across each join afterwards. The check is
    /// `None` when it was not asked for.
    pub cuts: Vec<(Found, Option<Check>)>,
}

fn seconds(project: &Project, frames: u64) -> String {
    format!("{:.3}s", project.seconds(frames))
}

/// Finds `phrase` and cuts it out, as one edit. With `verify`, each join is listened to as it
/// would sound, before anything is written; a join that is not clean stops the whole cut with
/// `cut_not_clean`. A dry run does all of that and writes nothing.
#[allow(clippy::too_many_arguments)]
pub fn cut(
    paths: &Paths,
    opts: &WriteOpts,
    who: &Who,
    track: Option<&str>,
    within: Option<(u64, u64)>,
    phrase: &str,
    pick: Pick,
    crossfade: u32,
    verify: bool,
    engine: &mut dyn Engine,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Written<CutPhrase>> {
    // A version the caller no longer holds is refused before any listening is done.
    project::update(paths, &WriteOpts { if_version: opts.if_version, dry_run: true, ..Default::default() }, |_| Ok(()))?;
    let project = project::open(paths)?;
    let base = Timeline::build(&project)?;
    let listen = match track {
        None => None,
        Some(key) => Some(base.find_track(key).ok_or_else(|| {
            Error::usage("unknown_track", format!("There is no track {key}.")).hint(format!("Run `scrubr track {} list` to see the tracks.", paths.shown.display()))
        })?),
    };
    let found = find(&project, paths, listen, within, phrase, engine, progress)?;
    let listed = |found: &[Found]| found.iter().enumerate().map(|(i, f)| format!("{} at {}", i + 1, seconds(&project, f.words[0].start))).collect::<Vec<_>>().join(", ");
    let chosen: Vec<Found> = match (pick, found.len()) {
        (_, 0) => {
            return Err(Error::usage("phrase_not_found", format!("\"{phrase}\" was not heard{}.", if within.is_some() { " in that range" } else { "" }))
                .hint("Run `scrubr transcribe` to see the words as they were heard: the model may have heard them differently. Or cut by time."));
        }
        (Pick::All, _) | (Pick::Only, 1) => found,
        (Pick::Only, n) => {
            return Err(Error::usage("phrase_ambiguous", format!("\"{phrase}\" was heard {n} times: {}.", listed(&found)))
                .hint("Pick one with --occurrence N, cut them all with --all, or narrow the search with --within START..END."));
        }
        (Pick::Nth(n), count) if n == 0 || n > count => {
            return Err(Error::usage("no_such_occurrence", format!("\"{phrase}\" was heard {count} times, so there is no occurrence {n}: {}.", listed(&found))));
        }
        (Pick::Nth(n), _) => vec![found[n - 1].clone()],
    };

    let requests: Vec<Request> = chosen.iter().map(|f| Request::Cut { range: f.cut, crossfade }).collect();
    let mut checks: Vec<Option<Check>> = vec![None; chosen.len()];
    if verify {
        // The project as it would be, without writing it: the same operations, pending.
        let mut trial = project.clone();
        let mut ops = Vec::new();
        for request in &requests {
            ops.push(edit::resolve(&mut trial, paths, &base, &ops, request)?.op);
        }
        let mut removed = 0;
        for (i, one) in chosen.iter().enumerate() {
            let join = one.cut.0 - removed;
            removed += one.cut.1 - one.cut.0;
            let after = Timeline::build_with(&project, &ops)?;
            let result = check(&project, paths, after, listen, join, one, engine)?;
            if !result.clean {
                return Err(Error::usage(
                    "cut_not_clean",
                    format!(
                        "Cutting \"{phrase}\" at {}..{} would not leave the words either side as they were. Across the join the model heard: \"{}\". Nothing was cut.",
                        seconds(&project, one.cut.0),
                        seconds(&project, one.cut.1),
                        result.heard
                    ),
                )
                .hint("Leave a label there for the person with `scrubr label <project> add`, or look at the place with `analyze --envelope` and cut by time. `--no-verify` cuts without this check."));
            }
            checks[i] = Some(result);
        }
    }
    let desc = match chosen.as_slice() {
        [one] => format!("cut \"{phrase}\" {}..{}", seconds(&project, one.cut.0), seconds(&project, one.cut.1)),
        many => format!("cut \"{phrase}\" \u{d7}{}", many.len()),
    };
    // Positions were worked out for the version that was read: the write insists on it.
    let write = WriteOpts { if_version: Some(project.version), dry_run: opts.dry_run, ..opts.clone() };
    let batch = requests.len() > 1;
    let written = edit::apply(paths, &write, who, batch, |_, _| Ok((requests, Some(desc))))?;
    Ok(Written { value: CutPhrase { applied: written.value, cuts: chosen.into_iter().zip(checks).collect() }, version: written.version, wrote: written.wrote })
}
