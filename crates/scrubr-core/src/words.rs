//! A transcript kept against the source audio, so it follows the audio through edits.
//!
//! Transcribing is slow, and an edit does not change what was said. So the words a model heard
//! are stored the way labels are: by the frames of the main track's sources they were heard
//! over. After a cut the words that remain are found again on the timeline and the words that
//! were cut are not, with no listening needed. The store lives in the project's cache folder: it
//! can always be made again.
//!
//! There is one set of words for the mix and one for each track that was transcribed alone.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Result, io_at};
use crate::project::{Paths, Span};
use crate::timeline::Timeline;
use crate::transcribe::Word;

/// A word as it is kept: the main-track source it was heard over and the frames of that source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    pub text: String,
    pub src: String,
    pub start: u64,
    pub end: u64,
    #[serde(default)]
    pub p: f64,
}

/// The words heard on the mix, or on one track alone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Set {
    /// The id of the track that was listened to, or `None` for the mix.
    pub track: Option<String>,
    /// The engine and model that heard the words last added.
    pub model: String,
    /// The audio that has been transcribed, in source frames. Audio outside it is not silent,
    /// only not listened to yet.
    pub covers: Vec<Span>,
    pub words: Vec<Kept>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Store {
    pub sets: Vec<Set>,
}

/// Where a project's transcript is kept.
pub fn file(paths: &Paths) -> PathBuf {
    paths.cache.join("words.json")
}

/// The transcript a project has, or an empty one. A file that cannot be read counts as none:
/// it is a cache.
pub fn load(paths: &Paths) -> Store {
    fs::read(file(paths)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Sorts spans and joins those that touch or overlap, source by source.
fn merged(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut out: Vec<Span> = Vec::new();
    for span in spans {
        match out.last_mut() {
            Some(last) if last.0 == span.0 && span.1 <= last.2 => last.2 = last.2.max(span.2),
            _ => out.push(span),
        }
    }
    out
}

/// Keeps what was heard over timeline `range` of `track` (the mix when `None`). Words already
/// kept for that audio are replaced, so transcribing a stretch again corrects it.
pub fn save(paths: &Paths, tl: &Timeline, track: Option<usize>, range: (u64, u64), model: &str, words: &[Word]) -> Result<()> {
    let covered = tl.stored(&tl.resolve(range.0, range.1));
    let id = track.map(|t| tl.tracks()[t].id.clone());
    let mut store = load(paths);
    let at = match store.sets.iter().position(|s| s.track == id) {
        Some(at) => at,
        None => {
            store.sets.push(Set { track: id, model: String::new(), covers: Vec::new(), words: Vec::new() });
            store.sets.len() - 1
        }
    };
    let set = &mut store.sets[at];
    set.model = model.to_string();
    let segs = tl.segs();
    let inside = |w: &Kept| covered.iter().any(|c| c.0 == w.src && w.start >= c.1 && w.start < c.2);
    // A word that began in audio since cut, and ran on into what was listened to just now, has
    // been heard again with the rest: it goes too, or it would be there twice.
    let plays = |w: &Kept| segs.iter().any(|s| tl.source_id(s.src) == w.src && s.start <= w.start && w.start < s.end);
    let runs_into = |w: &Kept| !plays(w) && covered.iter().any(|c| c.0 == w.src && w.start < c.2 && w.end > c.1);
    set.words.retain(|w| !inside(w) && !runs_into(w));
    for word in words.iter().filter(|w| w.start < tl.total()) {
        // The word belongs to the source it starts in, and ends there at the latest.
        let i = tl.seg_at(word.start);
        let seg = segs[i];
        let start = seg.start + (word.start - tl.seg_start(i));
        let end = (start + (word.end.max(word.start) - word.start)).min(seg.end);
        set.words.push(Kept { text: word.text.clone(), src: tl.source_id(seg.src).to_string(), start, end, p: (word.probability * 1000.0).round() / 1000.0 });
    }
    set.words.sort_by(|a, b| a.src.cmp(&b.src).then(a.start.cmp(&b.start)));
    set.covers = merged(set.covers.iter().cloned().chain(covered).collect());
    let path = file(paths);
    fs::create_dir_all(&paths.cache).map_err(|e| io_at(&paths.cache, e))?;
    // Through a temporary file, so a reader never sees half of it.
    let part = path.with_extension("json.part");
    fs::write(&part, serde_json::to_vec(&store).expect("a transcript is plain data")).map_err(|e| io_at(&part, e))?;
    fs::rename(&part, &path).map_err(|e| io_at(&path, e))
}

/// The set kept for `track` (the mix when `None`), if there is one.
pub fn set_for<'a>(store: &'a Store, tl: &Timeline, track: Option<usize>) -> Option<&'a Set> {
    let id = track.map(|t| tl.tracks()[t].id.as_str());
    store.sets.iter().find(|s| s.track.as_deref() == id)
}

/// The index of each kept word's source on this timeline, or `None` if the source is gone.
/// Words come sorted by source, so each source is looked up once.
fn sources<'a>(set: &'a Set, tl: &'a Timeline) -> impl Iterator<Item = (&'a Kept, Option<usize>)> {
    let mut known: Option<(&str, Option<usize>)> = None;
    set.words.iter().map(move |word| {
        let src = match known {
            Some((id, src)) if id == word.src => src,
            _ => {
                let src = (0..tl.source_count()).find(|&i| tl.source_id(i) == word.src);
                known = Some((&word.src, src));
                src
            }
        };
        (word, src)
    })
}

/// The parts of a word's audio that still play: the segment each is in, and its source frames.
fn pieces(tl: &Timeline, src: usize, word: &Kept) -> Vec<(usize, u64, u64)> {
    let segs = tl.segs();
    (0..segs.len()).filter(|&i| segs[i].src == src && segs[i].start < word.end && segs[i].end > word.start).map(|i| (i, segs[i].start.max(word.start), segs[i].end.min(word.end))).collect()
}

/// A set's words where they are on the timeline now, in order.
///
/// A word is still said when at least half of it still plays, and it is put over the longest
/// part that does. Half, and not all, because a model's word times are loose: it often starts a
/// word early, inside the pause before it, and a cut that tidies the pause must not lose the
/// word. A word with less than half left is a fragment and is left out.
pub fn on_timeline(set: &Set, tl: &Timeline) -> Vec<Word> {
    let segs = tl.segs();
    let mut out = Vec::new();
    for (word, src) in sources(set, tl) {
        let Some(src) = src else { continue };
        let (start, end) = if word.end <= word.start {
            // Heard at one moment: there if that moment is.
            let Some(i) = segs.iter().position(|s| s.src == src && s.start <= word.start && word.start < s.end) else { continue };
            let at = tl.seg_start(i) + (word.start - segs[i].start);
            (at, at)
        } else {
            let pieces = pieces(tl, src, word);
            let playing: u64 = pieces.iter().map(|&(_, a, b)| b - a).sum();
            if playing * 2 < word.end - word.start {
                continue;
            }
            let Some(&(i, a, b)) = pieces.iter().max_by_key(|&&(i, a, b)| (b - a, std::cmp::Reverse(i))) else { continue };
            let start = tl.seg_start(i) + (a - segs[i].start);
            (start, start + (b - a))
        };
        out.push(Word { text: word.text.clone(), start, end, probability: word.p });
    }
    out.sort_by_key(|w| w.start);
    out
}

/// Words a cut runs through, going by their times: for each, the timeline position of the join
/// where part of it was taken, and the word. A word with nothing left is not among them. Word
/// times are loose, so this says where to look, not what was lost.
pub fn broken(set: &Set, tl: &Timeline) -> Vec<(u64, String)> {
    let segs = tl.segs();
    let mut out = Vec::new();
    for (word, src) in sources(set, tl) {
        let Some(src) = src else { continue };
        for (i, a, b) in pieces(tl, src, word) {
            if a > word.start {
                out.push((tl.seg_start(i), word.text.clone()));
            }
            if b < word.end {
                out.push((tl.seg_start(i) + (segs[i].end - segs[i].start), word.text.clone()));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}
/// The parts of the timeline a set has words for, as timeline ranges in order. Outside these
/// the audio has not been listened to.
pub fn covered(set: &Set, tl: &Timeline) -> Vec<(u64, u64)> {
    let segs = tl.segs();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        let id = tl.source_id(seg.src);
        for cover in set.covers.iter().filter(|c| c.0 == id && c.1 < seg.end && c.2 > seg.start) {
            let (a, b) = (cover.1.max(seg.start), cover.2.min(seg.end));
            let range = (tl.seg_start(i) + (a - seg.start), tl.seg_start(i) + (b - seg.start));
            match out.last_mut() {
                Some(last) if last.1 >= range.0 => last.1 = last.1.max(range.1),
                _ => out.push(range),
            }
        }
    }
    out
}
