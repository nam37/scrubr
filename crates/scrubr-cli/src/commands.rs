//! One function per command. Each returns the fields of its JSON reply and the text a person
//! reads in a terminal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use scrubr_core::analysis::{self, Options as Measure};
use scrubr_core::edit::{self, Applied, DEFAULT_CEILING_DBTP, DEFAULT_CROSSFADE_MS, NormalizeReport, Request, Revert, Target};
use scrubr_core::export::{self, Depth};
use scrubr_core::image;
use scrubr_core::project::{self, EditState, FadeDir, Op, Paths, Project, Who, WriteOpts, Written};
use scrubr_core::render::Renderer;
use scrubr_core::timeline::TrackState;
use scrubr_core::{Error, Result, Timeline};

use crate::time::{self, Scale, range_text, seconds, timecode};
use crate::{Direction, EditOpts, LabelAction, Reply, TrackAction, ops};

fn non_empty(value: &Option<String>) -> Option<String> {
    value.as_ref().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The author and session of an edit. An author who gives no name is recorded as "cli".
fn who(author: &Option<String>, session: &Option<String>) -> Who {
    Who { author: non_empty(author).unwrap_or_else(|| "cli".into()), session: non_empty(session) }
}

fn write_opts(edit: &EditOpts) -> WriteOpts {
    WriteOpts { if_version: edit.if_version, dry_run: edit.dry_run, ..Default::default() }
}

fn scale(project: &Project, tl: &Timeline) -> Scale {
    Scale { rate: project.sample_rate, total: tl.total() }
}

/// A crossfade length in frames. Without one, the default of 5 ms.
fn crossfade(text: &Option<String>, rate: u32) -> Result<u32> {
    match text {
        Some(text) => Ok(time::parse_duration(text, rate)?.min(u32::MAX as u64) as u32),
        None => Ok((DEFAULT_CROSSFADE_MS / 1000.0 * rate as f64).round() as u32),
    }
}

/// The request, aimed at one track when one is named.
fn aimed(track: &Option<String>, request: Request) -> Request {
    match track {
        Some(track) => Request::OnTrack { track: track.clone(), request: Box::new(request) },
        None => request,
    }
}

/// " on track guest" for a description, or nothing.
fn on_track(track: &Option<String>) -> String {
    track.as_ref().map_or(String::new(), |t| format!(" on track {t}"))
}

/// The index of the track a command named, for hearing it alone.
fn alone(paths: &Paths, tl: &Timeline, track: &Option<String>) -> Result<Option<usize>> {
    match track {
        None => Ok(None),
        Some(key) => tl
            .find_track(key)
            .map(Some)
            .ok_or_else(|| Error::usage("unknown_track", format!("There is no track {key}.")).hint(format!("Run `scrubr track {} list` to see the tracks.", paths.shown.display()))),
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn ms(frames: u32, rate: u32) -> f64 {
    (frames as f64 / rate as f64 * 1e6).round() / 1e3
}

fn ranges_json(ranges: &[(u64, u64)], rate: u32) -> Value {
    ranges.iter().map(|&(a, b)| json!({ "start": seconds(a, rate), "end": seconds(b, rate) })).collect()
}

/// The fields every edit reply shares, and the line that ends its text.
fn edit_reply(written: &Written<Applied>, op: &str) -> (Map<String, Value>, String) {
    let a = &written.value;
    let mut fields = Map::new();
    if !written.wrote {
        fields.insert("dry_run".into(), json!(true));
    }
    fields.insert("edit".into(), json!(a.edit));
    fields.insert("op".into(), json!(op));
    fields.insert("timeline_seconds".into(), json!(seconds(a.frames_after, a.sample_rate)));
    fields.insert("timeline_seconds_before".into(), json!(seconds(a.frames_before, a.sample_rate)));
    let length = if a.frames_after == a.frames_before {
        format!("Timeline {}.", timecode(a.frames_after, a.sample_rate))
    } else {
        format!("Timeline {} -> {}.", timecode(a.frames_before, a.sample_rate), timecode(a.frames_after, a.sample_rate))
    };
    let tail = if written.wrote {
        format!("{length} Edit {}, version {}.", a.edit, written.version)
    } else {
        format!("{length} Dry run: nothing was written (version {}).", written.version)
    };
    (fields, tail)
}

fn spans_of(op: &Op) -> Option<Value> {
    match op {
        Op::Cut { spans, .. } | Op::Keep { spans, .. } | Op::Mute { spans, .. } | Op::Fade { spans, .. } | Op::Gain { spans: Some(spans), .. } => Some(json!(spans)),
        _ => None,
    }
}

fn finish(written: Written<Applied>, fields: Map<String, Value>, text: String) -> Reply {
    Reply { version: Some(written.version), fields, text }
}

// ---- new, info, history ----------------------------------------------------------------------

fn sources_json(project: &Project) -> Value {
    project
        .sources
        .iter()
        .map(|s| {
            if s.silence {
                return json!({ "id": s.id, "silence": true, "seconds": seconds(s.frames, project.sample_rate), "frames": s.frames });
            }
            json!({
                "id": s.id,
                "path": s.path,
                "seconds": seconds(s.frames, project.sample_rate),
                "frames": s.frames,
                "sample_rate": s.sample_rate,
                "channels": s.channels,
                "bits": s.bits,
                "float": s.float,
                "converted": s.conformed,
                "hash": s.hash,
            })
        })
        .collect()
}

pub fn new(project: &Path, from: &[PathBuf], force: bool, author: Option<String>, session: Option<String>) -> Result<Reply> {
    let paths = Paths::new(project);
    let created = edit::new_project(&paths, from, &who(&author, &session), force)?;
    let total = Timeline::build(&created)?.total();
    let rate = created.sample_rate;
    let text = format!(
        "Created {} from {} file{}: {} at {} Hz, {} channel{}. Peak cache in {}. Version {}.",
        paths.project.display(),
        from.len(),
        if from.len() == 1 { "" } else { "s" },
        timecode(total, rate),
        rate,
        created.channels,
        if created.channels == 1 { "" } else { "s" },
        paths.cache.display(),
        created.version
    );
    let fields = json!({
        "project": paths.project.to_string_lossy(),
        "timeline_seconds": seconds(total, rate),
        "timeline_frames": total,
        "sample_rate": rate,
        "channels": created.channels,
        "head": created.head,
        "sources": sources_json(&created),
        "peak_cache": paths.cache.to_string_lossy(),
    });
    Ok(Reply::new(Some(created.version), fields, text))
}

pub fn info(project: &Path) -> Result<Reply> {
    let paths = Paths::new(project);
    let p = project::open(&paths)?;
    let tl = Timeline::build(&p)?;
    let rate = p.sample_rate;
    let states = p.states();
    let count = |state: &str| states.iter().filter(|s| s.name() == state).count();
    let mut text = format!(
        "{}  version {}  head {}\ntimeline {} ({} frames)  {} Hz  {} ch  {} segment{}\n",
        paths.project.display(),
        p.version,
        p.head.as_deref().unwrap_or("-"),
        timecode(tl.total(), rate),
        tl.total(),
        rate,
        p.channels,
        tl.segs().len(),
        if tl.segs().len() == 1 { "" } else { "s" },
    );
    for s in p.sources.iter().filter(|s| s.silence) {
        text.push_str(&format!("  {}  (silence)  {}\n", s.id, timecode(s.frames, rate)));
    }
    let live: Vec<&TrackState> = tl.tracks().iter().filter(|t| !t.removed).collect();
    if live.len() > 1 {
        text.push_str(&format!("tracks: {}\n", live.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    for s in p.sources.iter().filter(|s| !s.silence) {
        text.push_str(&format!(
            "  {}  {}  {}  {} Hz {} ch {}-bit{}\n",
            s.id,
            s.path,
            timecode(s.frames, rate),
            s.sample_rate,
            s.channels,
            s.bits,
            if s.conformed { "  (converted copy in cache)" } else { "" }
        ));
    }
    text.push_str(&format!("edits: {} applied, {} undone, {} reverted; labels: {}", count("applied"), count("undone"), count("reverted"), p.labels.len()));
    let fields = json!({
        "project": paths.project.to_string_lossy(),
        "timeline_seconds": seconds(tl.total(), rate),
        "timeline_frames": tl.total(),
        "sample_rate": rate,
        "channels": p.channels,
        "head": p.head,
        "edits": { "applied": count("applied"), "undone": count("undone"), "reverted": count("reverted") },
        "labels": p.labels.len(),
        "segments": tl.segs().len(),
        "gain_db": round2(tl.global_gain_db()),
        "tracks": live.iter().map(|t| json!({ "id": t.id, "name": t.name })).collect::<Vec<_>>(),
        "sources": sources_json(&p),
    });
    Ok(Reply::new(Some(p.version), fields, text))
}

/// What an edit did, for a history line, when it carries no description of its own.
fn summary(op: &Op) -> String {
    match op {
        Op::Batch { ops } => {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            ops.iter().for_each(|op| *counts.entry(op.name()).or_default() += 1);
            counts.iter().map(|(name, n)| format!("{name} x{n}")).collect::<Vec<_>>().join(", ")
        }
        Op::Revert { edits } => format!("revert {}", edits.join(", ")),
        other => other.name().to_string(),
    }
}

pub fn history(project: &Path) -> Result<Reply> {
    let paths = Paths::new(project);
    let p = project::open(&paths)?;
    let states = p.states();
    let mut text = String::new();
    let mut list = Vec::new();
    for (e, state) in p.edits.iter().zip(&states) {
        let desc = e.desc.clone().unwrap_or_else(|| summary(&e.op));
        let by = match &e.session {
            Some(session) => format!("{}/{session}", e.author),
            None => e.author.clone(),
        };
        let mark = match state {
            EditState::Applied => String::new(),
            EditState::Undone => "  [undone]".into(),
            EditState::Reverted(by) => format!("  [reverted by {by}]"),
        };
        let head = if p.head.as_ref() == Some(&e.id) { "  <- head" } else { "" };
        text.push_str(&format!("{:<5} {:<44} {:<16} {}{mark}{head}\n", e.id, desc, by, e.time.as_deref().unwrap_or("")));
        let mut item = Map::new();
        item.insert("id".into(), json!(e.id));
        item.insert("op".into(), json!(e.op.name()));
        item.insert("desc".into(), json!(desc));
        item.insert("author".into(), json!(e.author));
        item.insert("session".into(), json!(e.session));
        item.insert("time".into(), json!(e.time));
        item.insert("state".into(), json!(state.name()));
        if let EditState::Reverted(by) = state {
            item.insert("reverted_by".into(), json!(by));
        }
        if let Op::Normalize { gain_db, .. } | Op::Gain { gain_db, .. } = &e.op {
            item.insert("gain_db".into(), json!(gain_db));
        }
        if let Op::Revert { edits } = &e.op {
            item.insert("reverts".into(), json!(edits));
        }
        list.push(Value::Object(item));
    }
    let fields = json!({ "head": p.head, "edits": list });
    Ok(Reply::new(Some(p.version), fields, text.trim_end()))
}

// ---- analyze, render, export -----------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn analyze(
    project: &Path,
    loudness: bool,
    peaks: bool,
    silence: Option<String>,
    envelope: Option<String>,
    quietest: Vec<String>,
    range: Option<String>,
    track: Option<String>,
) -> Result<Reply> {
    let paths = Paths::new(project);
    let p = project::open(&paths)?;
    let rate = p.sample_rate;
    let only = alone(&paths, &Timeline::build(&p)?, &track)?;
    let mut renderer = Renderer::of(&p, &paths, only)?;
    let total = renderer.total();
    let scale = Scale { rate, total };
    let (t0, t1) = match &range {
        Some(text) => {
            let range = time::parse_range(text, scale)?;
            edit::check_range(&p, &paths, renderer.timeline(), range)?;
            range
        }
        None => (0, total),
    };
    let mut options = Measure {
        loudness,
        peaks,
        silence: silence.as_deref().map(|s| time::parse_silence(s, rate)).transpose()?,
        envelope: envelope.as_deref().map(|w| time::parse_duration(w, rate)).transpose()?,
    };
    if options.envelope == Some(0) {
        return Err(Error::usage("bad_time", "The envelope window must be longer than zero.").hint("Use a window such as --envelope 1s."));
    }
    let mut searches = Vec::new();
    for text in &quietest {
        let search = time::parse_range(text, scale)?;
        edit::check_range(&p, &paths, renderer.timeline(), search)?;
        searches.push(search);
    }
    // With nothing asked for, report the two measures an agent needs first.
    let nothing_measured = !loudness && !peaks && options.silence.is_none() && options.envelope.is_none();
    if nothing_measured && searches.is_empty() {
        options.loudness = true;
        options.peaks = true;
    }
    // Quietest moments read only their own short ranges, so asking for them alone is quick.
    let report = match nothing_measured && !searches.is_empty() {
        true => analysis::Report::default(),
        false => analysis::analyze(&mut renderer, t0, t1, &options, rate)?,
    };

    let mut fields = Map::new();
    let mut text = format!("timeline {}", timecode(total, rate));
    fields.insert("timeline_seconds".into(), json!(seconds(total, rate)));
    if let Some(track) = &track {
        fields.insert("track".into(), json!(track));
        text.push_str(&format!(", track {track} alone"));
    }
    if range.is_some() {
        fields.insert("range".into(), json!({ "start": seconds(t0, rate), "end": seconds(t1, rate) }));
        text.push_str(&format!(", measured {}", range_text((t0, t1), rate)));
    }
    if let Some(l) = &report.loudness {
        fields.insert(
            "loudness".into(),
            json!({ "integrated_lufs": l.integrated_lufs.map(round2), "true_peak_dbtp": round2(l.true_peak_dbtp), "range_lu": round2(l.range_lu) }),
        );
        let lufs = l.integrated_lufs.map_or("too quiet to measure".to_string(), |v| format!("{v:.1} LUFS"));
        text.push_str(&format!("\nloudness: {lufs}, true peak {:.1} dBTP, range {:.1} LU", l.true_peak_dbtp, l.range_lu));
    }
    if let Some(peaks) = &report.peaks {
        fields.insert(
            "peaks".into(),
            json!({
                "peak_dbfs": peaks.peak_db.iter().map(|&p| round2(p)).collect::<Vec<_>>(),
                "clipped_samples": peaks.clipped_samples,
                "clipped_ranges": ranges_json(&peaks.clipped_ranges, rate),
                "clipped_range_count": peaks.clipped_range_count,
            }),
        );
        let per_channel: Vec<String> = peaks.peak_db.iter().map(|p| format!("{p:.1}")).collect();
        text.push_str(&format!("\npeaks: {} dBFS, {} clipped samples in {} ranges", per_channel.join(" / "), peaks.clipped_samples, peaks.clipped_range_count));
    }
    if let (Some(found), Some((db, min))) = (&report.silence, options.silence) {
        fields.insert("silence".into(), ranges_json(found, rate));
        fields.insert("silence_count".into(), json!(found.len()));
        fields.insert("silence_seconds".into(), json!(seconds(found.iter().map(|r| r.1 - r.0).sum(), rate)));
        text.push_str(&format!("\nsilence under {db} dBFS for {} or more: {} ranges", timecode(min, rate), found.len()));
        for &r in found.iter().take(40) {
            text.push_str(&format!("\n  {}", range_text(r, rate)));
        }
        if found.len() > 40 {
            text.push_str(&format!("\n  ... and {} more (use --json to list all)", found.len() - 40));
        }
    }
    if let (Some(levels), Some(window)) = (&report.envelope, options.envelope) {
        fields.insert(
            "envelope".into(),
            json!({ "start": seconds(t0, rate), "window": seconds(window, rate), "rms_dbfs": levels.iter().map(|&v| (v * 10.0).round() / 10.0).collect::<Vec<_>>() }),
        );
        text.push_str(&format!("\nenvelope: {} windows of {} (use --json for the levels)", levels.len(), timecode(window, rate)));
    }
    if !searches.is_empty() {
        let mut found = Vec::new();
        for &search in &searches {
            let quiet = analysis::quietest(&mut renderer, search.0, search.1, rate)?;
            let level = (quiet.rms_db * 10.0).round() / 10.0;
            found.push(json!({
                "range": { "start": seconds(search.0, rate), "end": seconds(search.1, rate) },
                "at": seconds(quiet.at(), rate),
                "start": seconds(quiet.start, rate),
                "end": seconds(quiet.end, rate),
                "rms_dbfs": level,
            }));
            text.push_str(&format!("\nquietest in {}: {} ({}, {level:.1} dBFS)", range_text(search, rate), timecode(quiet.at(), rate), range_text((quiet.start, quiet.end), rate)));
        }
        fields.insert("quietest".into(), Value::Array(found));
    }
    Ok(Reply { version: Some(p.version), fields, text })
}

pub fn render(project: &Path, png: &Path, range: Option<String>, width: u32, height: u32, track: Option<String>) -> Result<Reply> {
    let paths = Paths::new(project);
    let p = project::open(&paths)?;
    let rate = p.sample_rate;
    let tl = Timeline::build(&p)?;
    let range = range.as_deref().map(|text| time::parse_range(text, scale(&p, &tl))).transpose()?;
    let only = alone(&paths, &tl, &track)?;
    let report = image::render_png(&p, &paths, png, &image::Options { width, height, range, track: only })?;
    let seconds_per_pixel = report.frames_per_pixel / rate as f64;
    let text = format!(
        "Wrote {} ({}x{}): {}, {:.6} s per pixel.",
        png.display(),
        report.width,
        report.height,
        range_text((report.start, report.end), rate),
        seconds_per_pixel
    );
    let fields = json!({
        "png": png.to_string_lossy(),
        "width": report.width,
        "height": report.height,
        "start": seconds(report.start, rate),
        "end": seconds(report.end, rate),
        "seconds_per_pixel": seconds_per_pixel,
        "channels": p.channels,
        "drawn_from": if report.from_peaks { "peak_cache" } else { "audio" },
    });
    Ok(Reply::new(Some(p.version), fields, text))
}

pub fn export(project: &Path, out: &Path, bits: Option<String>, range: Option<String>, track: Option<String>) -> Result<Reply> {
    let paths = Paths::new(project);
    let p = project::open(&paths)?;
    let rate = p.sample_rate;
    let depth = match bits.as_deref() {
        None => None,
        Some("32f") | Some("float") => Some(Depth::Float),
        Some(text) => match text.parse::<u16>() {
            Ok(bits) => Some(Depth::Int(bits)),
            Err(_) => return Err(Error::usage("bad_depth", format!("\"{text}\" is not a sample format.")).hint("Use --bits 16, 24, 32 or 32f.")),
        },
    };
    let tl = Timeline::build(&p)?;
    let range = range.as_deref().map(|text| time::parse_range(text, scale(&p, &tl))).transpose()?;
    let only = alone(&paths, &tl, &track)?;
    let report = export::export(&p, &paths, out, &export::Options { depth, range, progress: None, track: only })?;
    let (bits, float) = match report.depth {
        Depth::Int(bits) => (bits, false),
        Depth::Float => (32, true),
    };
    let mut text = format!(
        "Exported {} to {}: {} {}-bit{}, {} Hz, {} ch, {} bytes.",
        timecode(report.frames, rate),
        out.display(),
        report.format.name().to_uppercase(),
        bits,
        if float { " float" } else { "" },
        rate,
        p.channels,
        report.bytes
    );
    if report.clipped_samples > 0 {
        text.push_str(&format!("\nwarning: {} samples were over full scale and were clipped. Lower the gain or normalize.", report.clipped_samples));
    }
    let fields = json!({
        "path": out.to_string_lossy(),
        "format": report.format.name(),
        "bits": bits,
        "float": float,
        "sample_rate": rate,
        "channels": p.channels,
        "seconds": seconds(report.frames, rate),
        "frames": report.frames,
        "bytes": report.bytes,
        "clipped_samples": report.clipped_samples,
    });
    Ok(Reply::new(Some(p.version), fields, text))
}

// ---- edits -----------------------------------------------------------------------------------

/// `cut`, or `keep` when `keep` is set.
pub fn cut(project: &Path, range: &str, xf: Option<String>, opts: &EditOpts, keep: bool) -> Result<Reply> {
    let paths = Paths::new(project);
    let name = if keep { "keep" } else { "cut" };
    let mut asked = (0, 0);
    let mut xf_frames = 0;
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), false, |p, tl| {
        asked = time::parse_range(range, scale(p, tl))?;
        xf_frames = crossfade(&xf, p.sample_rate)?;
        let request = if keep { Request::Keep { range: asked, crossfade: xf_frames } } else { Request::Cut { range: asked, crossfade: xf_frames } };
        Ok((vec![request], Some(format!("{name} {}", range_text(asked, p.sample_rate)))))
    })?;
    let rate = written.value.sample_rate;
    let removed = written.value.resolved[0].frames;
    let (mut fields, tail) = edit_reply(&written, name);
    fields.insert("range".into(), json!({ "start": seconds(asked.0, rate), "end": seconds(asked.1, rate) }));
    fields.insert("removed_seconds".into(), json!(seconds(removed, rate)));
    fields.insert("crossfade_ms".into(), json!(ms(xf_frames, rate)));
    fields.insert("source_spans".into(), spans_of(&written.value.resolved[0].op).unwrap_or(Value::Null));
    let text = format!("{name} {}: removed {}. {tail}", range_text(asked, rate), timecode(removed, rate));
    Ok(finish(written, fields, text))
}

pub fn mute(project: &Path, range: &str, xf: Option<String>, track: Option<String>, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    let mut asked = (0, 0);
    let mut xf_frames = 0;
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), false, |p, tl| {
        asked = time::parse_range(range, scale(p, tl))?;
        xf_frames = crossfade(&xf, p.sample_rate)?;
        Ok((vec![aimed(&track, Request::Mute { range: asked, crossfade: xf_frames })], Some(format!("mute {}{}", range_text(asked, p.sample_rate), on_track(&track)))))
    })?;
    let rate = written.value.sample_rate;
    let (mut fields, tail) = edit_reply(&written, "mute");
    fields.insert("track".into(), json!(track));
    fields.insert("range".into(), json!({ "start": seconds(asked.0, rate), "end": seconds(asked.1, rate) }));
    fields.insert("muted_seconds".into(), json!(seconds(written.value.resolved[0].frames, rate)));
    fields.insert("crossfade_ms".into(), json!(ms(xf_frames, rate)));
    fields.insert("source_spans".into(), spans_of(&written.value.resolved[0].op).unwrap_or(Value::Null));
    let text = format!("mute {}{}. {tail}", range_text(asked, rate), on_track(&track));
    Ok(finish(written, fields, text))
}

pub fn gain(project: &Path, db: &str, range: Option<String>, track: Option<String>, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    let db = time::parse_db(db)?;
    let mut asked = None;
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), false, |p, tl| {
        asked = range.as_deref().map(|text| time::parse_range(text, scale(p, tl))).transpose()?;
        let over = asked.map_or("whole timeline".to_string(), |r| range_text(r, p.sample_rate));
        Ok((vec![aimed(&track, Request::Gain { db, range: asked })], Some(format!("gain {db:+} dB {over}{}", on_track(&track)))))
    })?;
    let rate = written.value.sample_rate;
    let (mut fields, tail) = edit_reply(&written, "gain");
    fields.insert("track".into(), json!(track));
    fields.insert("gain_db".into(), json!(db));
    fields.insert("range".into(), asked.map_or(Value::Null, |r| json!({ "start": seconds(r.0, rate), "end": seconds(r.1, rate) })));
    if let Some(spans) = spans_of(&written.value.resolved[0].op) {
        fields.insert("source_spans".into(), spans);
    }
    let over = asked.map_or("the whole timeline".to_string(), |r| range_text(r, rate));
    let text = format!("gain {db:+} dB over {over}{}. {tail}", on_track(&track));
    Ok(finish(written, fields, text))
}

pub fn fade(project: &Path, direction: Direction, length: Option<String>, range: Option<String>, track: Option<String>, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    let dir = if direction == Direction::In { FadeDir::In } else { FadeDir::Out };
    let name = if dir == FadeDir::In { "in" } else { "out" };
    let mut asked = (0, 0);
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), false, |p, tl| {
        let scale = scale(p, tl);
        asked = match (&length, &range) {
            (Some(length), None) => ops::fade_range(dir, time::parse_duration(length, p.sample_rate)?, scale)?,
            (None, Some(range)) => time::parse_range(range, scale)?,
            _ => {
                return Err(Error::usage("bad_usage", "A fade needs a length or a range.")
                    .hint("For example `scrubr fade <project> in 0.5s`, or `scrubr fade <project> out --range 58:00..58:10`."));
            }
        };
        Ok((vec![aimed(&track, Request::Fade { dir, range: asked })], Some(format!("fade {name} {}{}", range_text(asked, p.sample_rate), on_track(&track)))))
    })?;
    let rate = written.value.sample_rate;
    let (mut fields, tail) = edit_reply(&written, "fade");
    fields.insert("track".into(), json!(track));
    fields.insert("direction".into(), json!(name));
    fields.insert("range".into(), json!({ "start": seconds(asked.0, rate), "end": seconds(asked.1, rate) }));
    fields.insert("fade_seconds".into(), json!(seconds(asked.1 - asked.0, rate)));
    fields.insert("source_spans".into(), spans_of(&written.value.resolved[0].op).unwrap_or(Value::Null));
    let text = format!("fade {name} over {}{}. {tail}", range_text(asked, rate), on_track(&track));
    Ok(finish(written, fields, text))
}

fn normalize_fields(report: &NormalizeReport) -> Map<String, Value> {
    let mut fields = Map::new();
    match report.target {
        Target::Lufs(lufs) => {
            fields.insert("target_lufs".into(), json!(lufs));
            fields.insert("achieved_lufs".into(), json!(report.achieved_lufs().map(round2)));
        }
        Target::Peak(peak) => {
            fields.insert("target_peak_dbfs".into(), json!(peak));
            fields.insert("achieved_peak_dbfs".into(), json!(round2(report.achieved_peak_db())));
            fields.insert("achieved_lufs".into(), json!(report.achieved_lufs().map(round2)));
        }
    }
    fields.insert("gain_db".into(), json!(report.gain_db));
    fields.insert("limited_by".into(), if report.limited { json!("true_peak") } else { Value::Null });
    fields.insert("ceiling_dbtp".into(), json!(report.ceiling_dbtp));
    fields.insert("true_peak_dbtp".into(), json!(round2(report.achieved_true_peak_dbtp())));
    fields.insert(
        "measured".into(),
        json!({ "integrated_lufs": report.measured_lufs.map(round2), "true_peak_dbtp": round2(report.measured_true_peak_dbtp), "peak_dbfs": round2(report.measured_peak_db) }),
    );
    fields
}

fn normalize_text(report: &NormalizeReport) -> String {
    let reached = match report.target {
        Target::Lufs(lufs) => format!("target {lufs} LUFS, reached {:.2} LUFS", report.achieved_lufs().unwrap_or(f64::NAN)),
        Target::Peak(peak) => format!("target peak {peak} dBFS, reached {:.2} dBFS", report.achieved_peak_db()),
    };
    let limit = if report.limited { format!(" Gain stopped at the true-peak ceiling of {} dBTP, short of the target.", report.ceiling_dbtp) } else { String::new() };
    format!("normalize: {:+.2} dB, {reached}, true peak {:.2} dBTP.{limit}", report.gain_db, report.achieved_true_peak_dbtp())
}

pub fn normalize(project: &Path, lufs: Option<String>, peak: Option<String>, ceiling: Option<String>, track: Option<String>, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    let target = match (&lufs, &peak) {
        (Some(lufs), None) => Target::Lufs(time::parse_db(lufs)?),
        (None, Some(peak)) => Target::Peak(time::parse_db(peak)?),
        _ => {
            return Err(Error::usage("bad_usage", "Normalize needs a target: --lufs or --peak.")
                .hint("For example `scrubr normalize <project> --lufs -16`, or `--peak -1dB`."));
        }
    };
    let ceiling_dbtp = ceiling.as_deref().map(time::parse_db).transpose()?.unwrap_or(DEFAULT_CEILING_DBTP);
    let desc = match target {
        Target::Lufs(lufs) => format!("normalize {lufs} LUFS{}", on_track(&track)),
        Target::Peak(peak) => format!("normalize peak {peak} dBFS{}", on_track(&track)),
    };
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), false, |_, _| {
        Ok((vec![aimed(&track, Request::Normalize { target, ceiling_dbtp })], Some(desc)))
    })?;
    let report = written.value.resolved[0].normalize.expect("normalize reports what it measured");
    let (mut fields, tail) = edit_reply(&written, "normalize");
    fields.insert("track".into(), json!(track));
    fields.extend(normalize_fields(&report));
    let text = format!("{}{} {tail}", normalize_text(&report), track.as_ref().map_or(String::new(), |t| format!(" Track {t} alone was measured and changed.")));
    Ok(finish(written, fields, text))
}

pub fn insert(project: &Path, file: &Path, at: &str, xf: Option<String>, track: Option<String>, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    let mut position = 0;
    let mut xf_frames = 0;
    let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), false, |p, tl| {
        position = time::parse_position(at, scale(p, tl))?;
        xf_frames = crossfade(&xf, p.sample_rate)?;
        let request = aimed(&track, Request::Insert { file: file.to_path_buf(), at: position, crossfade: xf_frames });
        Ok((vec![request], Some(format!("insert {name} at {}{}", timecode(position, p.sample_rate), on_track(&track)))))
    })?;
    let rate = written.value.sample_rate;
    let resolved = &written.value.resolved[0];
    let (mut fields, tail) = edit_reply(&written, "insert");
    fields.insert("track".into(), json!(track));
    fields.insert("source".into(), json!(resolved.new_source));
    fields.insert("at".into(), json!(seconds(position, rate)));
    fields.insert("inserted_seconds".into(), json!(seconds(resolved.frames, rate)));
    fields.insert("crossfade_ms".into(), json!(ms(xf_frames, rate)));
    let text = format!("insert {name} at {}{}: added {}. {tail}", timecode(position, rate), on_track(&track), timecode(resolved.frames, rate));
    Ok(finish(written, fields, text))
}

pub fn apply(project: &Path, ops_file: &Path, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    let doc = ops::read(ops_file)?;
    let mut names: Vec<&'static str> = Vec::new();
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), true, |p, tl| {
        let parsed = ops::parse(&doc, scale(p, tl))?;
        names = parsed.iter().map(|(_, name)| *name).collect();
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        names.iter().for_each(|name| *counts.entry(name).or_default() += 1);
        let desc = counts.iter().map(|(name, n)| format!("{name} x{n}")).collect::<Vec<_>>().join(", ");
        Ok((parsed.into_iter().map(|(request, _)| request).collect(), Some(desc)))
    })?;
    let rate = written.value.sample_rate;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    names.iter().for_each(|name| *counts.entry(name).or_default() += 1);
    let (mut fields, tail) = edit_reply(&written, "batch");
    fields.insert("ops".into(), json!(names.len()));
    fields.insert("by_op".into(), json!(counts));
    let before = written.value.frames_before;
    let after = written.value.frames_after;
    fields.insert("removed_seconds".into(), json!(seconds(before.saturating_sub(after), rate)));
    fields.insert("added_seconds".into(), json!(seconds(after.saturating_sub(before), rate)));
    let mut text = format!("applied {} operations ({}).", names.len(), counts.iter().map(|(name, n)| format!("{name} x{n}")).collect::<Vec<_>>().join(", "));
    let reports: Vec<&NormalizeReport> = written.value.resolved.iter().filter_map(|r| r.normalize.as_ref()).collect();
    if let Some(last) = reports.last() {
        fields.insert("normalize".into(), Value::Object(normalize_fields(last)));
        text.push_str(&format!(" {}", normalize_text(last)));
    }
    let text = format!("{text} {tail}");
    Ok(finish(written, fields, text))
}

fn moved_reply(written: Written<edit::Moved>, verb: &str, key: &str, extra: Option<(&str, Value)>) -> Reply {
    let m = &written.value;
    let rate = m.sample_rate;
    let mut fields = Map::new();
    if !written.wrote {
        fields.insert("dry_run".into(), json!(true));
    }
    if let Some((name, value)) = extra {
        fields.insert(name.into(), value);
    }
    fields.insert(key.into(), json!(m.edits));
    fields.insert("head".into(), json!(m.head));
    fields.insert("timeline_seconds".into(), json!(seconds(m.frames_after, rate)));
    fields.insert("timeline_seconds_before".into(), json!(seconds(m.frames_before, rate)));
    let dry = if written.wrote { format!("Version {}.", written.version) } else { format!("Dry run: nothing was written (version {}).", written.version) };
    let text = format!(
        "{verb} {}. Head is {}. Timeline {} -> {}. {dry}",
        m.edits.join(", "),
        m.head.as_deref().unwrap_or("-"),
        timecode(m.frames_before, rate),
        timecode(m.frames_after, rate)
    );
    Reply { version: Some(written.version), fields, text }
}

pub fn undo(project: &Path, steps: usize, edits: Vec<String>, of_session: Option<String>, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    let which = match (edits.is_empty(), of_session) {
        (false, _) => Some(Revert::Edits(edits)),
        (true, Some(session)) => Some(Revert::Session(session)),
        (true, None) => None,
    };
    match which {
        None => Ok(moved_reply(edit::undo(&paths, &write_opts(opts), steps)?, "Undid", "undone", None)),
        Some(which) => {
            let written = edit::revert(&paths, &write_opts(opts), &who(&opts.author, &opts.session), &which)?;
            let (id, moved) = written.value;
            let written = Written { value: moved, version: written.version, wrote: written.wrote };
            Ok(moved_reply(written, "Reverted", "reverted", Some(("edit", json!(id)))))
        }
    }
}

pub fn redo(project: &Path, steps: usize, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    Ok(moved_reply(edit::redo(&paths, &write_opts(opts), steps)?, "Redid", "redone", None))
}

// ---- labels ----------------------------------------------------------------------------------

/// A label with where it sits on the timeline now. A label whose audio is hidden reports the
/// position that audio would come back at.
fn label_json(label: &project::Label, p: &Project, tl: &Timeline) -> (Value, String) {
    let rate = p.sample_rate;
    let src = p.source_index(&label.span.0);
    let visible = src.map(|src| tl.locate(src, label.span.1, label.span.2.max(label.span.1 + 1))).unwrap_or_default();
    let point = label.span.1 == label.span.2;
    let (start, end, hidden) = match (visible.first(), visible.last()) {
        (Some(first), Some(last)) => (Some(first.0), Some(if point { first.0 } else { last.1 }), false),
        _ => {
            let at = src.and_then(|src| tl.position_from(src, label.span.1)).or(src.map(|_| tl.total()));
            (at, at, true)
        }
    };
    let value = json!({
        "id": label.id,
        "text": label.text,
        "start": start.map(|t| seconds(t, rate)),
        "end": end.map(|t| seconds(t, rate)),
        "hidden": hidden,
        "author": label.author,
        "session": label.session,
        "time": label.time,
        "span": label.span,
    });
    let place = match (start, end) {
        (Some(a), Some(b)) if !hidden && a != b => range_text((a, b), rate),
        (Some(a), _) if !hidden => timecode(a, rate),
        (Some(a), _) => format!("(audio removed; was at {})", timecode(a, rate)),
        _ => "(audio removed)".into(),
    };
    (value, format!("{:<4} {:<24} {}  [{}]", label.id, place, label.text, label.author))
}

pub fn label(project: &Path, action: LabelAction) -> Result<Reply> {
    let paths = Paths::new(project);
    match action {
        LabelAction::Add { range, text, edit: opts } => {
            // A note that starts with a negative number arrives with its minus swapped; put it back.
            let text = text.replacen('\u{2212}', "-", 1);
            let written = edit::add_label(&paths, &write_opts(&opts), &who(&opts.author, &opts.session), |p, tl| {
                let scale = scale(p, tl);
                let range = if range.contains("..") { time::parse_range(&range, scale)? } else { time::parse_position(&range, scale).map(|t| (t, t))? };
                Ok((range, text.clone()))
            })?;
            let p = Project::load(&paths.project)?;
            let (label, clipped) = &written.value;
            // On a dry run the label is not in the file, so place it on the current timeline.
            let tl = Timeline::build(&p)?;
            let (value, line) = label_json(label, &p, &tl);
            let mut fields = Map::new();
            if !written.wrote {
                fields.insert("dry_run".into(), json!(true));
            }
            fields.insert("label".into(), value);
            if *clipped {
                fields.insert("clipped".into(), json!(true));
            }
            let note = if *clipped { " The range crossed into another source file, so the label covers the part in the first." } else { "" };
            let dry = if written.wrote { format!("Version {}.", written.version) } else { format!("Dry run: nothing was written (version {}).", written.version) };
            Ok(Reply { version: Some(written.version), fields, text: format!("Added label {line}.{note} {dry}") })
        }
        LabelAction::List => {
            let p = project::open(&paths)?;
            let tl = Timeline::build(&p)?;
            let (values, lines): (Vec<Value>, Vec<String>) = p.labels.iter().map(|l| label_json(l, &p, &tl)).unzip();
            let text = if lines.is_empty() { "No labels.".to_string() } else { lines.join("\n") };
            Ok(Reply::new(Some(p.version), json!({ "labels": values }), text))
        }
        LabelAction::Rm { id, edit: opts } => {
            let written = edit::remove_label(&paths, &write_opts(&opts), &id)?;
            let mut fields = Map::new();
            if !written.wrote {
                fields.insert("dry_run".into(), json!(true));
            }
            fields.insert("removed".into(), json!({ "id": written.value.id, "text": written.value.text }));
            let dry = if written.wrote { format!("Version {}.", written.version) } else { format!("Dry run: nothing was written (version {}).", written.version) };
            Ok(Reply { version: Some(written.version), fields, text: format!("Removed label {} ({}). {dry}", written.value.id, written.value.text) })
        }
    }
}

// ---- tracks ----------------------------------------------------------------------------------

/// A track for a reply, and the line that lists it. `clips` gives where its audio sits now.
fn track_json(t: &TrackState, p: &Project, tl: &Timeline, clips: bool) -> (Value, String) {
    let rate = p.sample_rate;
    let main = t.id == project::MAIN_TRACK;
    let mut fields = Map::new();
    fields.insert("id".into(), json!(t.id));
    fields.insert("name".into(), json!(t.name));
    fields.insert("main".into(), json!(main));
    fields.insert("gain_db".into(), json!(t.gain_db));
    fields.insert("pan".into(), json!(t.pan));
    fields.insert("mute".into(), json!(t.mute));
    fields.insert("solo".into(), json!(t.solo));
    let mut mix = vec![format!("{:+} dB", t.gain_db)];
    if t.pan != 0.0 {
        mix.push(format!("pan {} {}", t.pan.abs(), if t.pan < 0.0 { "left" } else { "right" }));
    }
    if t.mute {
        mix.push("muted".into());
    }
    if t.solo {
        mix.push("solo".into());
    }
    let mut text = format!("{:<4}  {:<16}  {}", t.id, t.name, mix.join(", "));
    if main {
        text.push_str("  (the timeline's own audio)");
    }
    if clips && let Some(index) = tl.tracks().iter().position(|x| x.id == t.id) {
        let mut list = Vec::new();
        for (clip, ranges) in tl.clips(index) {
            let source = &p.sources[clip];
            let heard: u64 = ranges.iter().map(|r| r.1 - r.0).sum();
            list.push(json!({ "source": source.id, "file": source.path, "seconds": seconds(heard, rate), "ranges": ranges_json(&ranges, rate) }));
            let at = ranges.iter().map(|&r| range_text(r, rate)).collect::<Vec<_>>().join(", ");
            text.push_str(&format!("\n      {}  {}  {}", source.id, source.path, if at.is_empty() { "not on the timeline now".to_string() } else { at }));
        }
        fields.insert("clips".into(), Value::Array(list));
    }
    (Value::Object(fields), text)
}

/// The end of an edit command's text: the edit and version, or that nothing was written.
fn wrote_tail(wrote: bool, edit: &str, version: u64) -> String {
    if wrote { format!("Edit {edit}, version {version}.") } else { format!("Dry run: nothing was written (version {version}).") }
}

/// A pan from the command line: a number from -1 to 1.
fn parse_pan(text: &str) -> Result<f64> {
    text.trim().replace('\u{2212}', "-").parse::<f64>().map_err(|_| {
        Error::usage("bad_pan", format!("Cannot read \"{text}\" as a pan.")).hint("Pan runs from -1, full left, to 1, full right. Use a number such as -0.3, 0 or 0.5.")
    })
}

pub fn track(project: &Path, action: TrackAction) -> Result<Reply> {
    let paths = Paths::new(project);
    match action {
        TrackAction::List => {
            let p = project::open(&paths)?;
            let tl = Timeline::build(&p)?;
            let (list, lines): (Vec<Value>, Vec<String>) = tl.tracks().iter().filter(|t| !t.removed).map(|t| track_json(t, &p, &tl, true)).unzip();
            Ok(Reply::new(Some(p.version), json!({ "timeline_seconds": seconds(tl.total(), p.sample_rate), "tracks": list }), lines.join("\n")))
        }
        TrackAction::Add { file, name, track, at, edit } => {
            // The position is read against the timeline the write sees, under its lock.
            let p = project::open(&paths)?;
            let tl = Timeline::build(&p)?;
            let position = match &at {
                Some(at) => time::parse_position(at, scale(&p, &tl))?,
                None => 0,
            };
            let opts = WriteOpts { if_version: write_opts(&edit).if_version.or(Some(p.version)), ..write_opts(&edit) };
            let written = edit::track_add(&paths, &opts, &who(&edit.author, &edit.session), &file, name.as_deref(), track.as_deref(), position)?;
            let a = &written.value;
            let p = project::open(&paths)?;
            let rate = a.sample_rate;
            let mut fields = Map::new();
            if !written.wrote {
                fields.insert("dry_run".into(), json!(true));
            }
            fields.insert("edit".into(), json!(a.edit));
            fields.insert("op".into(), json!("track_add"));
            fields.insert("track".into(), json!({ "id": a.track.id, "name": a.track.name, "created": a.created }));
            fields.insert("source".into(), json!(a.source));
            fields.insert("at".into(), json!(seconds(a.at, rate)));
            fields.insert("seconds".into(), json!(seconds(a.frames, rate)));
            fields.insert("extended_seconds".into(), json!(seconds(a.extended, rate)));
            fields.insert("timeline_seconds".into(), json!(seconds(a.frames_after, rate)));
            fields.insert("timeline_seconds_before".into(), json!(seconds(a.frames_before, rate)));
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            let grew = if a.extended > 0 { format!(" The timeline grew by {} to {}.", timecode(a.extended, rate), timecode(a.frames_after, rate)) } else { String::new() };
            let text = format!(
                "{} track {} ({}): {name} at {}, {} long.{grew} {}",
                if a.created { "Added" } else { "Added to" },
                a.track.name,
                a.track.id,
                timecode(a.at, rate),
                timecode(a.frames, rate),
                wrote_tail(written.wrote, &a.edit, written.version)
            );
            let _ = p;
            Ok(Reply { version: Some(written.version), fields, text })
        }
        TrackAction::Set { track, gain, pan, mute, unmute, solo, unsolo, name, edit } => {
            let change = edit::TrackChange {
                name,
                gain_db: gain.as_deref().map(time::parse_db).transpose()?,
                pan: pan.as_deref().map(parse_pan).transpose()?,
                mute: if mute { Some(true) } else if unmute { Some(false) } else { None },
                solo: if solo { Some(true) } else if unsolo { Some(false) } else { None },
            };
            let written = edit::track_set(&paths, &write_opts(&edit), &who(&edit.author, &edit.session), &track, &change)?;
            let p = project::open(&paths)?;
            let tl = Timeline::build(&p)?;
            let (id, state) = &written.value;
            let (json_track, line) = track_json(state, &p, &tl, false);
            let mut fields = Map::new();
            if !written.wrote {
                fields.insert("dry_run".into(), json!(true));
            }
            fields.insert("edit".into(), json!(id));
            fields.insert("op".into(), json!("track_set"));
            fields.insert("track".into(), json_track);
            Ok(Reply { version: Some(written.version), fields, text: format!("{line}\n{}", wrote_tail(written.wrote, id, written.version)) })
        }
        TrackAction::Rm { track, edit } => {
            let written = edit::track_remove(&paths, &write_opts(&edit), &who(&edit.author, &edit.session), &track)?;
            let (id, state) = &written.value;
            let mut fields = Map::new();
            if !written.wrote {
                fields.insert("dry_run".into(), json!(true));
            }
            fields.insert("edit".into(), json!(id));
            fields.insert("op".into(), json!("track_rm"));
            fields.insert("track".into(), json!({ "id": state.id, "name": state.name }));
            let text = format!("Removed track {} ({}). Its audio is out of the mix; the timeline's length is unchanged. {}", state.name, state.id, wrote_tail(written.wrote, id, written.version));
            Ok(Reply { version: Some(written.version), fields, text })
        }
    }
}

// ---- the window ------------------------------------------------------------------------------

/// The window is its own program, `scrubr-gui`, kept beside this one. `SCRUBR_GUI` names another.
fn gui_program() -> PathBuf {
    match std::env::var_os("SCRUBR_GUI") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.join(format!("scrubr-gui{}", std::env::consts::EXE_SUFFIX))))
            .unwrap_or_else(|| PathBuf::from("scrubr-gui")),
    }
}

/// Stops programs this one starts from inheriting its input and output.
#[cfg(windows)]
fn keep_own_handles() {
    use std::os::windows::io::AsRawHandle;
    unsafe extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    let handles = [std::io::stdin().as_raw_handle(), std::io::stdout().as_raw_handle(), std::io::stderr().as_raw_handle()];
    for handle in handles {
        if !handle.is_null() {
            // SAFETY: the handle is this process's own and stays open while it runs. The call
            // clears one flag on it and touches nothing else.
            unsafe { SetHandleInformation(handle.cast(), HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

/// Starts the window on a project, or empty, and returns at once: the window runs on its own.
pub fn open(project: Option<&Path>) -> Result<Reply> {
    open_with(project, &gui_program())
}

fn open_with(project: Option<&Path>, gui: &Path) -> Result<Reply> {
    // A wrong project path should read as one here, not in a window that opens on an error.
    let opened = match project {
        Some(project) => Some((std::path::absolute(project).unwrap_or_else(|_| project.to_path_buf()), project::open(&Paths::new(project))?.version)),
        None => None,
    };
    if !gui.is_file() {
        return Err(Error::not_found("gui_not_found", format!("The scrubr window's program was not found at {}.", gui.display()))
            .hint("Build it with `cargo build --release -p scrubr-gui` so that it sits beside `scrubr`, or set SCRUBR_GUI to its path."));
    }
    let mut command = std::process::Command::new(gui);
    if let Some((path, _)) = &opened {
        command.arg(path);
    }
    // The window must not hold on to this command's output. On Windows a child inherits the
    // caller's handles unless told not to, and a caller reading the reply, such as an agent,
    // would then wait until the window closed.
    #[cfg(windows)]
    keep_own_handles();
    use std::process::Stdio;
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::codec("gui_failed", format!("Could not start {}: {e}", gui.display())).hint("Check that the file is the scrubr window's program and can be run."))?;
    let text = match &opened {
        Some((path, _)) => format!("Opened {} in the scrubr window.", path.display()),
        None => "Opened the scrubr window.".to_string(),
    };
    let fields = json!({ "opened": opened.as_ref().map(|(path, _)| path.to_string_lossy()), "pid": child.id() });
    Ok(Reply::new(opened.map(|(_, version)| version), fields, text))
}

#[cfg(not(feature = "transcribe"))]
pub fn transcribe(project: Option<&Path>, _model: Option<String>, _range: Option<String>, _fetch_model: Option<String>, _track: Option<String>) -> Result<Reply> {
    // Make sure the project is real before talking about features, so a wrong path reads as one.
    if let Some(project) = project {
        project::open(&Paths::new(project))?;
    }
    Err(Error::usage("feature_not_built", "This build of scrubr has no transcription. It is an optional feature, off by default.")
        .hint("Build scrubr with `--features transcribe`, or transcribe outside scrubr and cut by time. You can also leave a label for the person with `scrubr label <project> add`."))
}

#[cfg(feature = "transcribe")]
pub fn transcribe(project: Option<&Path>, model: Option<String>, range: Option<String>, fetch_model: Option<String>, track: Option<String>) -> Result<Reply> {
    use scrubr_core::transcribe::{self as speech, Engine, whisper};

    if let Some(name) = fetch_model {
        let weights = whisper::fetch(&name)?;
        let bytes = std::fs::metadata(&weights).map(|m| m.len()).unwrap_or(0);
        let text = format!("The {name} model is installed: {} ({bytes} bytes).", weights.display());
        return Ok(Reply::new(None, json!({ "model": name, "path": weights.to_string_lossy(), "bytes": bytes }), text));
    }
    let Some(project) = project else {
        return Err(Error::usage("bad_usage", "Transcribe needs a project.").hint("For example `scrubr transcribe <project>`, or `scrubr transcribe --fetch-model base.en` to install a model."));
    };
    let paths = Paths::new(project);
    let p = project::open(&paths)?;
    let rate = p.sample_rate;
    let tl = Timeline::build(&p)?;
    let range = range.as_deref().map(|text| time::parse_range(text, scale(&p, &tl))).transpose()?;
    let name = model.unwrap_or_else(|| whisper::DEFAULT_MODEL.to_string());
    let mut engine = whisper::Whisper::load(&name)?;
    let began = std::time::Instant::now();
    // Progress goes to stderr, so stdout stays one JSON reply.
    let only = alone(&paths, &tl, &track)?;
    let words = speech::transcribe_track(&p, &paths, only, range, &mut engine, &mut |done, total| {
        eprintln!("transcribe: {} of {}", timecode(done, rate), timecode(total, rate));
    })?;
    let took = began.elapsed().as_secs_f64();
    let (start, end) = range.unwrap_or((0, tl.total()));
    let text = speech::text(&words);
    let listed: Vec<Value> = words
        .iter()
        .map(|w| json!({ "word": w.text, "start": seconds(w.start, rate), "end": seconds(w.end, rate), "p": (w.probability * 1000.0).round() / 1000.0 }))
        .collect();
    let mut fields = Map::new();
    fields.insert("timeline_seconds".into(), json!(seconds(tl.total(), rate)));
    fields.insert("range".into(), json!({ "start": seconds(start, rate), "end": seconds(end, rate) }));
    fields.insert("engine".into(), json!(engine.name()));
    fields.insert("took_seconds".into(), json!((took * 10.0).round() / 10.0));
    fields.insert("word_count".into(), json!(words.len()));
    fields.insert("text".into(), json!(text));
    fields.insert("words".into(), Value::Array(listed));
    let shown = format!("{} words in {} by {}, in {took:.1} s (use --json for word times):\n{text}", words.len(), range_text((start, end), rate), engine.name());
    Ok(Reply { version: Some(p.version), fields, text: shown })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// stdout is never a terminal under `cargo test`, so the binary's tests only see JSON. This
    /// covers the text a person gets instead.
    #[test]
    fn replies_read_as_text_for_a_person() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("ep.wav");
        let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut out = hound::WavWriter::create(&wav, spec).unwrap();
        for f in 0..48_000 * 4 {
            let s = if (96_000..144_000).contains(&f) { 0 } else { ((f as f32 * 0.03).sin() * 8_000.0) as i16 };
            out.write_sample(s).unwrap();
            out.write_sample(s).unwrap();
        }
        out.finalize().unwrap();
        let project = dir.path().join("ep.scrubr");
        let opts = |dry_run: bool| EditOpts { dry_run, if_version: None, author: Some("you".into()), session: Some("s1".into()) };

        let text = new(&project, std::slice::from_ref(&wav), false, None, None).unwrap().text;
        assert!(text.starts_with("Created ") && text.contains("from 1 file: 0:04.000 at 48000 Hz, 2 channels.") && text.ends_with("Version 1."), "{text}");
        assert_eq!(cut(&project, "0.5..1", None, &opts(false), false).unwrap().text, "cut 0:00.500..0:01.000: removed 0:00.500. Timeline 0:04.000 -> 0:03.500. Edit e2, version 2.");
        assert_eq!(gain(&project, "-3dB", None, None, &opts(false)).unwrap().text, "gain -3 dB over the whole timeline. Timeline 0:03.500. Edit e3, version 3.");
        assert_eq!(
            fade(&project, Direction::Out, Some("0.5s".into()), None, None, &opts(true)).unwrap().text,
            "fade out over 0:03.000..0:03.500. Timeline 0:03.500. Dry run: nothing was written (version 3)."
        );
        let text = normalize(&project, Some("-12".into()), None, None, None, &opts(false)).unwrap().text;
        assert!(text.starts_with("normalize: +") && text.contains("target -12 LUFS, reached -12.00 LUFS") && text.ends_with("Edit e4, version 4."), "{text}");
        let text = label(&project, LabelAction::Add { range: "0:02..0:03".into(), text: "check".into(), edit: opts(false) }).unwrap().text;
        assert_eq!(text, "Added label l1   0:02.000..0:03.000       check  [you]. Version 5.");

        let text = info(&project).unwrap().text;
        assert!(text.contains("version 5  head e4\ntimeline 0:03.500 (168000 frames)  48000 Hz  2 ch  2 segments\n  src1  ep.wav  0:04.000  48000 Hz 2 ch 16-bit\n"), "{text}");
        assert!(text.ends_with("edits: 4 applied, 0 undone, 0 reverted; labels: 1"));
        let text = history(&project).unwrap().text;
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].starts_with("e2    cut 0:00.500..0:01.000") && lines[1].contains("you/s1"), "{text}");
        assert!(lines[3].starts_with("e4    normalize -12 LUFS") && lines[3].ends_with("<- head"), "{text}");
        let text = analyze(&project, false, false, None, None, vec!["1..3".into()], None, None).unwrap().text;
        assert_eq!(text, "timeline 0:03.500
quietest in 0:01.000..0:03.000: 0:02.000 (0:01.500..0:02.500, -120.0 dBFS)");
        let text = analyze(&project, true, true, Some("-50dB:0.5s".into()), Some("1s".into()), Vec::new(), None, None).unwrap().text;
        assert!(text.starts_with("timeline 0:03.500\nloudness: -12.0 LUFS, true peak "), "{text}");
        assert!(text.contains("\nsilence under -50 dBFS for 0:00.500 or more: 1 ranges\n  0:01.500..0:02.500\n"), "{text}");
        assert!(text.contains("clipped samples") && text.ends_with("envelope: 4 windows of 0:01.000 (use --json for the levels)"), "{text}");
        let text = undo(&project, 1, vec![], None, &opts(false)).unwrap().text;
        assert_eq!(text, "Undid e4. Head is e3. Timeline 0:03.500 -> 0:03.500. Version 6.");
        let text = export(&project, &dir.path().join("out.wav"), None, Some("0..1".into()), None).unwrap().text;
        assert!(text.starts_with("Exported 0:01.000 to ") && text.contains("WAV 16-bit, 48000 Hz, 2 ch, 192044 bytes."), "{text}");
        let err = open_with(Some(&project), &dir.path().join("no-such-window")).unwrap_err();
        assert!(err.to_string().contains("was not found at") && err.hint_text().is_some_and(|h| h.contains("SCRUBR_GUI")), "{err}");
        let err = open_with(Some(&dir.path().join("nope.scrubr")), &dir.path().join("no-such-window")).unwrap_err();
        assert!(err.to_string().contains("nope.scrubr"), "{err}");
    }
}
