//! The operations file `scrubr apply --ops` reads.
//!
//! A JSON array of operations, or an object with an `ops` array and an optional default
//! `crossfade`. Every range and position refers to the timeline before the batch:
//!
//! ```json
//! [
//!   { "op": "cut", "start": 3.2, "end": 4.85 },
//!   { "op": "cut", "range": "6:52.1..6:54.02" },
//!   { "op": "gain", "db": 2, "range": "0..0:41" },
//!   { "op": "normalize", "lufs": -16 }
//! ]
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

use scrubr_core::edit::{DEFAULT_CEILING_DBTP, DEFAULT_CROSSFADE_MS, Request, Target};
use scrubr_core::project::FadeDir;
use scrubr_core::{Error, Result};

use crate::time::{self, Scale};

const HINT: &str = "Each operation is an object like {\"op\": \"cut\", \"start\": 3.2, \"end\": 4.85} or {\"op\": \"cut\", \"range\": \"3.2..4.85\"}. Run `scrubr help --agent` for the full list.";

/// Reads the operations document from a file, or from stdin when the path is `-`.
pub fn read(path: &Path) -> Result<Value> {
    let text = if path.as_os_str() == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map_err(|e| Error::usage("bad_ops", format!("Could not read operations from stdin: {e}")))?;
        text
    } else {
        std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::not_found("file_not_found", format!("The operations file {} does not exist.", path.display())).hint("Check the path given to --ops.")
            } else {
                Error::usage("bad_ops", format!("Could not read {}: {e}", path.display()))
            }
        })?
    };
    // A UTF-8 byte order mark is what PowerShell's redirection puts in front of a file.
    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| Error::usage("bad_ops", format!("The operations are not valid JSON: {e}")).hint(HINT))
}

fn bad(index: usize, why: impl std::fmt::Display) -> Error {
    Error::usage("bad_ops", format!("ops[{index}]: {why}")).hint(HINT)
}

/// Adds which operation an error came from, keeping its code and hint.
fn prefix(index: usize, e: Error) -> Error {
    let hint = e.hint_text().map(str::to_string);
    let wrapped = Error::new(e.kind(), e.code(), format!("ops[{index}]: {e}"));
    match hint {
        Some(hint) => wrapped.hint(hint),
        None => wrapped,
    }
}

fn at(index: usize, result: Result<u64>) -> Result<u64> {
    result.map_err(|e| prefix(index, e))
}

/// A time given as a number of seconds or as text in any time form.
fn position(index: usize, value: &Value, scale: Scale) -> Result<u64> {
    match value {
        Value::Number(n) => at(index, time::parse_position(&n.to_string(), scale)),
        Value::String(s) => at(index, time::parse_position(s, scale)),
        other => Err(bad(index, format!("a time must be a number of seconds or a string, not {other}"))),
    }
}

fn duration(index: usize, value: &Value, rate: u32) -> Result<u64> {
    match value {
        Value::Number(n) => at(index, time::parse_duration(&n.to_string(), rate)),
        Value::String(s) => at(index, time::parse_duration(s, rate)),
        other => Err(bad(index, format!("a length must be a number of seconds or a string, not {other}"))),
    }
}

fn level(index: usize, value: &Value) -> Result<f64> {
    match value {
        Value::Number(n) => n.as_f64().ok_or_else(|| bad(index, "the level is not a number")),
        Value::String(s) => time::parse_db(s).map_err(|e| prefix(index, e)),
        other => Err(bad(index, format!("a level must be a number of dB or a string, not {other}"))),
    }
}

/// `range` as text, or `start` and `end`.
fn range(index: usize, op: &serde_json::Map<String, Value>, scale: Scale) -> Result<Option<(u64, u64)>> {
    match (op.get("range"), op.get("start"), op.get("end")) {
        (Some(Value::String(text)), None, None) => time::parse_range(text, scale).map(Some).map_err(|e| prefix(index, e)),
        (None, Some(start), Some(end)) => Ok(Some((position(index, start, scale)?, position(index, end, scale)?))),
        (None, None, None) => Ok(None),
        (Some(_), None, None) => Err(bad(index, "\"range\" must be a string like \"3.2..4.85\"")),
        _ => Err(bad(index, "give either \"range\" or both \"start\" and \"end\"")),
    }
}

/// The requests in an operations document, and the name of each for the edit's description.
pub fn parse(doc: &Value, scale: Scale) -> Result<Vec<(Request, &'static str)>> {
    let default_ms = |rate: u32| (DEFAULT_CROSSFADE_MS / 1000.0 * rate as f64).round() as u64;
    let (list, mut crossfade) = match doc {
        Value::Array(list) => (list, default_ms(scale.rate)),
        Value::Object(map) => match map.get("ops") {
            Some(Value::Array(list)) => (list, default_ms(scale.rate)),
            _ => return Err(Error::usage("bad_ops", "The operations document has no \"ops\" array.").hint(HINT)),
        },
        _ => return Err(Error::usage("bad_ops", "The operations document must be a JSON array, or an object with an \"ops\" array.").hint(HINT)),
    };
    if let Some(value) = doc.get("crossfade") {
        crossfade = duration(0, value, scale.rate)?;
    }
    let mut out = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        let Value::Object(op) = item else { return Err(bad(index, "an operation must be a JSON object")) };
        let Some(Value::String(name)) = op.get("op") else { return Err(bad(index, "it has no \"op\" name")) };
        let crossfade = match op.get("crossfade") {
            Some(value) => duration(index, value, scale.rate)?,
            None => crossfade,
        } as u32;
        let need_range = || range(index, op, scale)?.ok_or_else(|| bad(index, format!("{name} needs a \"range\", or \"start\" and \"end\"")));
        let known = |allowed: &[&str]| match op.keys().find(|k| *k != "op" && !allowed.contains(&k.as_str())) {
            Some(key) => Err(bad(index, format!("{name} has no field \"{key}\""))),
            None => Ok(()),
        };
        let (request, kind) = match name.as_str() {
            "cut" => {
                known(&["range", "start", "end", "crossfade"])?;
                (Request::Cut { range: need_range()?, crossfade }, "cut")
            }
            "keep" => {
                known(&["range", "start", "end", "crossfade"])?;
                (Request::Keep { range: need_range()?, crossfade }, "keep")
            }
            "mute" => {
                known(&["range", "start", "end", "crossfade", "track"])?;
                (Request::Mute { range: need_range()?, crossfade }, "mute")
            }
            "gain" => {
                known(&["db", "range", "start", "end", "track"])?;
                let db = level(index, op.get("db").ok_or_else(|| bad(index, "gain needs \"db\""))?)?;
                (Request::Gain { db, range: range(index, op, scale)? }, "gain")
            }
            "fade" => {
                known(&["dir", "length", "range", "start", "end", "track"])?;
                let dir = match op.get("dir").and_then(Value::as_str) {
                    Some("in") => FadeDir::In,
                    Some("out") => FadeDir::Out,
                    _ => return Err(bad(index, "fade needs \"dir\": \"in\" or \"out\"")),
                };
                let range = match (range(index, op, scale)?, op.get("length")) {
                    (Some(range), None) => range,
                    (None, Some(length)) => fade_range(dir, duration(index, length, scale.rate)?, scale).map_err(|e| prefix(index, e))?,
                    _ => return Err(bad(index, "fade needs either \"length\" or a range")),
                };
                (Request::Fade { dir, range }, "fade")
            }
            "insert" => {
                known(&["file", "at", "crossfade", "track"])?;
                let Some(Value::String(file)) = op.get("file") else { return Err(bad(index, "insert needs \"file\"")) };
                let at = position(index, op.get("at").ok_or_else(|| bad(index, "insert needs \"at\""))?, scale)?;
                (Request::Insert { file: PathBuf::from(file), at, crossfade }, "insert")
            }
            "normalize" => {
                known(&["lufs", "peak", "ceiling", "track"])?;
                let target = match (op.get("lufs"), op.get("peak")) {
                    (Some(lufs), None) => Target::Lufs(level(index, lufs)?),
                    (None, Some(peak)) => Target::Peak(level(index, peak)?),
                    _ => return Err(bad(index, "normalize needs either \"lufs\" or \"peak\"")),
                };
                let ceiling_dbtp = match op.get("ceiling") {
                    Some(value) => level(index, value)?,
                    None => DEFAULT_CEILING_DBTP,
                };
                (Request::Normalize { target, ceiling_dbtp }, "normalize")
            }
            other => return Err(bad(index, format!("there is no operation \"{other}\"; the operations are cut, keep, gain, fade, mute, insert and normalize"))),
        };
        // "track" aims the operation at one track. Cut and keep act on every track and take none.
        out.push(match op.get("track") {
            None => (request, kind),
            Some(Value::String(track)) => (Request::OnTrack { track: track.clone(), request: Box::new(request) }, kind),
            Some(_) => return Err(bad(index, "\"track\" must be a track's name or id")),
        });
    }
    Ok(out)
}

/// The range a fade of `length` covers: from the start for `in`, up to the end for `out`.
pub fn fade_range(dir: FadeDir, length: u64, scale: Scale) -> Result<(u64, u64)> {
    if length > scale.total {
        return Err(Error::usage(
            "range_out_of_bounds",
            format!("A fade of {} is longer than the timeline ({}).", time::timecode(length, scale.rate), time::timecode(scale.total, scale.rate)),
        )
        .hint("Run `scrubr info <project>` to get the timeline length."));
    }
    Ok(match dir {
        FadeDir::In => (0, length),
        FadeDir::Out => (scale.total - length, scale.total),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const S: Scale = Scale { rate: 48_000, total: 48_000 * 600 };

    #[test]
    fn every_operation_reads() {
        let doc = json!([
            { "op": "cut", "start": 3.2, "end": 4.85 },
            { "op": "cut", "range": "6:52.1..6:54.02", "crossfade": 0 },
            { "op": "keep", "start": "0:01", "end": "end-1s" },
            { "op": "gain", "db": "+2dB", "range": "0..0:41" },
            { "op": "gain", "db": -3 },
            { "op": "fade", "dir": "in", "length": "0.5s" },
            { "op": "fade", "dir": "out", "length": 2 },
            { "op": "mute", "range": "100f..200f" },
            { "op": "insert", "file": "sting.wav", "at": 0 },
            { "op": "normalize", "lufs": -16 },
            { "op": "normalize", "peak": "-1dB", "ceiling": -2 }
        ]);
        let requests: Vec<Request> = parse(&doc, S).unwrap().into_iter().map(|(r, _)| r).collect();
        assert_eq!(requests[0], Request::Cut { range: (153_600, 232_800), crossfade: 240 });
        assert_eq!(requests[1], Request::Cut { range: (19_780_800, 19_872_960), crossfade: 0 });
        assert_eq!(requests[2], Request::Keep { range: (48_000, S.total - 48_000), crossfade: 240 });
        assert_eq!(requests[3], Request::Gain { db: 2.0, range: Some((0, 41 * 48_000)) });
        assert_eq!(requests[4], Request::Gain { db: -3.0, range: None });
        assert_eq!(requests[5], Request::Fade { dir: FadeDir::In, range: (0, 24_000) });
        assert_eq!(requests[6], Request::Fade { dir: FadeDir::Out, range: (S.total - 96_000, S.total) });
        assert_eq!(requests[7], Request::Mute { range: (100, 200), crossfade: 240 });
        assert_eq!(requests[8], Request::Insert { file: "sting.wav".into(), at: 0, crossfade: 240 });
        assert_eq!(requests[9], Request::Normalize { target: Target::Lufs(-16.0), ceiling_dbtp: -1.0 });
        assert_eq!(requests[10], Request::Normalize { target: Target::Peak(-1.0), ceiling_dbtp: -2.0 });
    }

    #[test]
    fn a_wrapper_object_sets_the_default_crossfade() {
        let doc = json!({ "crossfade": "10ms", "ops": [{ "op": "cut", "range": "1..2" }, { "op": "cut", "range": "3..4", "crossfade": 0 }] });
        let requests = parse(&doc, S).unwrap();
        assert_eq!(requests[0].0, Request::Cut { range: (48_000, 96_000), crossfade: 480 });
        assert_eq!(requests[1].0, Request::Cut { range: (144_000, 192_000), crossfade: 0 });
    }

    #[test]
    fn mistakes_name_the_operation_they_are_in() {
        for (doc, want) in [
            (json!([{ "op": "cut" }]), "ops[0]: cut needs a \"range\""),
            (json!([{ "op": "cut", "range": "1..2" }, { "op": "slice", "range": "1..2" }]), "ops[1]: there is no operation \"slice\""),
            (json!([{ "op": "cut", "start": 1 }]), "ops[0]: give either \"range\" or both"),
            (json!([{ "op": "cut", "range": "1..2", "fade": 3 }]), "ops[0]: cut has no field \"fade\""),
            (json!([{ "op": "cut", "start": "soon", "end": 2 }]), "ops[0]: Cannot read \"soon\" as a time"),
            (json!([{ "op": "fade", "dir": "up", "length": 1 }]), "ops[0]: fade needs \"dir\""),
            (json!([{ "range": "1..2" }]), "ops[0]: it has no \"op\" name"),
            (json!({ "cuts": [] }), "no \"ops\" array"),
            (json!("cut"), "must be a JSON array"),
        ] {
            let err = parse(&doc, S).unwrap_err();
            assert_eq!(err.exit_code(), 1, "{doc}");
            assert!(err.to_string().contains(want), "{doc}: {err}");
        }
    }
}
