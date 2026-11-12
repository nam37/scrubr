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
use scrubr_core::{Error, Result, Timeline};

use crate::time::{self, Scale, range_text, seconds, timecode};
use crate::{Direction, EditOpts, LabelAction, Reply, ops};

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
    for s in &p.sources {
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

pub fn analyze(project: &Path, loudness: bool, peaks: bool, silence: Option<String>, envelope: Option<String>, range: Option<String>) -> Result<Reply> {
    let paths = Paths::new(project);
    let p = project::open(&paths)?;
    let rate = p.sample_rate;
    let mut renderer = Renderer::new(&p, &paths)?;
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
    // With nothing asked for, report the two measures an agent needs first.
    if !loudness && !peaks && options.silence.is_none() && options.envelope.is_none() {
        options.loudness = true;
        options.peaks = true;
    }
    let report = analysis::analyze(&mut renderer, t0, t1, &options, rate)?;

    let mut fields = Map::new();
    let mut text = format!("timeline {}", timecode(total, rate));
    fields.insert("timeline_seconds".into(), json!(seconds(total, rate)));
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
    Ok(Reply { version: Some(p.version), fields, text })
}

pub fn render(project: &Path, png: &Path, range: Option<String>, width: u32, height: u32) -> Result<Reply> {
    let paths = Paths::new(project);
    let p = project::open(&paths)?;
    let rate = p.sample_rate;
    let tl = Timeline::build(&p)?;
    let range = range.as_deref().map(|text| time::parse_range(text, scale(&p, &tl))).transpose()?;
    let report = image::render_png(&p, &paths, png, &image::Options { width, height, range })?;
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

pub fn export(project: &Path, out: &Path, bits: Option<String>, range: Option<String>) -> Result<Reply> {
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
    let report = export::export(&p, &paths, out, &export::Options { depth, range })?;
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

pub fn mute(project: &Path, range: &str, xf: Option<String>, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    let mut asked = (0, 0);
    let mut xf_frames = 0;
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), false, |p, tl| {
        asked = time::parse_range(range, scale(p, tl))?;
        xf_frames = crossfade(&xf, p.sample_rate)?;
        Ok((vec![Request::Mute { range: asked, crossfade: xf_frames }], Some(format!("mute {}", range_text(asked, p.sample_rate)))))
    })?;
    let rate = written.value.sample_rate;
    let (mut fields, tail) = edit_reply(&written, "mute");
    fields.insert("range".into(), json!({ "start": seconds(asked.0, rate), "end": seconds(asked.1, rate) }));
    fields.insert("muted_seconds".into(), json!(seconds(written.value.resolved[0].frames, rate)));
    fields.insert("crossfade_ms".into(), json!(ms(xf_frames, rate)));
    fields.insert("source_spans".into(), spans_of(&written.value.resolved[0].op).unwrap_or(Value::Null));
    let text = format!("mute {}. {tail}", range_text(asked, rate));
    Ok(finish(written, fields, text))
}

pub fn gain(project: &Path, db: &str, range: Option<String>, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    let db = time::parse_db(db)?;
    let mut asked = None;
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), false, |p, tl| {
        asked = range.as_deref().map(|text| time::parse_range(text, scale(p, tl))).transpose()?;
        let over = asked.map_or("whole timeline".to_string(), |r| range_text(r, p.sample_rate));
        Ok((vec![Request::Gain { db, range: asked }], Some(format!("gain {db:+} dB {over}"))))
    })?;
    let rate = written.value.sample_rate;
    let (mut fields, tail) = edit_reply(&written, "gain");
    fields.insert("gain_db".into(), json!(db));
    fields.insert("range".into(), asked.map_or(Value::Null, |r| json!({ "start": seconds(r.0, rate), "end": seconds(r.1, rate) })));
    if let Some(spans) = spans_of(&written.value.resolved[0].op) {
        fields.insert("source_spans".into(), spans);
    }
    let over = asked.map_or("the whole timeline".to_string(), |r| range_text(r, rate));
    let text = format!("gain {db:+} dB over {over}. {tail}");
    Ok(finish(written, fields, text))
}

pub fn fade(project: &Path, direction: Direction, length: Option<String>, range: Option<String>, opts: &EditOpts) -> Result<Reply> {
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
        Ok((vec![Request::Fade { dir, range: asked }], Some(format!("fade {name} {}", range_text(asked, p.sample_rate)))))
    })?;
    let rate = written.value.sample_rate;
    let (mut fields, tail) = edit_reply(&written, "fade");
    fields.insert("direction".into(), json!(name));
    fields.insert("range".into(), json!({ "start": seconds(asked.0, rate), "end": seconds(asked.1, rate) }));
    fields.insert("fade_seconds".into(), json!(seconds(asked.1 - asked.0, rate)));
    fields.insert("source_spans".into(), spans_of(&written.value.resolved[0].op).unwrap_or(Value::Null));
    let text = format!("fade {name} over {}. {tail}", range_text(asked, rate));
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

pub fn normalize(project: &Path, lufs: Option<String>, peak: Option<String>, ceiling: Option<String>, opts: &EditOpts) -> Result<Reply> {
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
        Target::Lufs(lufs) => format!("normalize {lufs} LUFS"),
        Target::Peak(peak) => format!("normalize peak {peak} dBFS"),
    };
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), false, |_, _| {
        Ok((vec![Request::Normalize { target, ceiling_dbtp }], Some(desc)))
    })?;
    let report = written.value.resolved[0].normalize.expect("normalize reports what it measured");
    let (mut fields, tail) = edit_reply(&written, "normalize");
    fields.extend(normalize_fields(&report));
    let text = format!("{} {tail}", normalize_text(&report));
    Ok(finish(written, fields, text))
}

pub fn insert(project: &Path, file: &Path, at: &str, xf: Option<String>, opts: &EditOpts) -> Result<Reply> {
    let paths = Paths::new(project);
    let mut position = 0;
    let mut xf_frames = 0;
    let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let written = edit::apply(&paths, &write_opts(opts), &who(&opts.author, &opts.session), false, |p, tl| {
        position = time::parse_position(at, scale(p, tl))?;
        xf_frames = crossfade(&xf, p.sample_rate)?;
        let request = Request::Insert { file: file.to_path_buf(), at: position, crossfade: xf_frames };
        Ok((vec![request], Some(format!("insert {name} at {}", timecode(position, p.sample_rate)))))
    })?;
    let rate = written.value.sample_rate;
    let resolved = &written.value.resolved[0];
    let (mut fields, tail) = edit_reply(&written, "insert");
    fields.insert("source".into(), json!(resolved.new_source));
    fields.insert("at".into(), json!(seconds(position, rate)));
    fields.insert("inserted_seconds".into(), json!(seconds(resolved.frames, rate)));
    fields.insert("crossfade_ms".into(), json!(ms(xf_frames, rate)));
    let text = format!("insert {name} at {}: added {}. {tail}", timecode(position, rate), timecode(resolved.frames, rate));
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

// ---- not in this build -----------------------------------------------------------------------

pub fn open(project: Option<&Path>) -> Result<Reply> {
    let what = match project {
        Some(project) => format!("`scrubr open {}`", project.display()),
        None => "The scrubr window".to_string(),
    };
    Err(Error::usage("gui_not_available", format!("{what} is not available yet: the GUI arrives in milestone M2, and this build is the command line only."))
        .hint("Run `scrubr help` for the commands, or `scrubr help --agent` for the agent guide."))
}

pub fn transcribe(project: &Path) -> Result<Reply> {
    // Make sure the project is real before talking about features, so a wrong path reads as one.
    project::open(&Paths::new(project))?;
    if cfg!(feature = "transcribe") {
        Err(Error::usage(
            "feature_not_built",
            "Transcription is not built yet: the `transcribe` feature is switched on, but scrubr has no speech-to-text engine behind it. The engine has not been chosen.",
        )
        .hint("Transcribe outside scrubr for now and cut by time, or leave a label for the person with `scrubr label <project> add`."))
    } else {
        Err(Error::usage(
            "feature_not_built",
            "This build of scrubr has no transcription. It is an optional feature, off by default, and its speech-to-text engine has not been chosen yet.",
        )
        .hint("Transcribe outside scrubr for now and cut by time, or leave a label for the person with `scrubr label <project> add`."))
    }
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

        let text = new(&project, &[wav.clone()], false, None, None).unwrap().text;
        assert!(text.starts_with("Created ") && text.contains("from 1 file: 0:04.000 at 48000 Hz, 2 channels.") && text.ends_with("Version 1."), "{text}");
        assert_eq!(cut(&project, "0.5..1", None, &opts(false), false).unwrap().text, "cut 0:00.500..0:01.000: removed 0:00.500. Timeline 0:04.000 -> 0:03.500. Edit e2, version 2.");
        assert_eq!(gain(&project, "-3dB", None, &opts(false)).unwrap().text, "gain -3 dB over the whole timeline. Timeline 0:03.500. Edit e3, version 3.");
        assert_eq!(
            fade(&project, Direction::Out, Some("0.5s".into()), None, &opts(true)).unwrap().text,
            "fade out over 0:03.000..0:03.500. Timeline 0:03.500. Dry run: nothing was written (version 3)."
        );
        let text = normalize(&project, Some("-12".into()), None, None, &opts(false)).unwrap().text;
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
        let text = analyze(&project, true, true, Some("-50dB:0.5s".into()), Some("1s".into()), None).unwrap().text;
        assert!(text.starts_with("timeline 0:03.500\nloudness: -12.0 LUFS, true peak "), "{text}");
        assert!(text.contains("\nsilence under -50 dBFS for 0:00.500 or more: 1 ranges\n  0:01.500..0:02.500\n"), "{text}");
        assert!(text.contains("clipped samples") && text.ends_with("envelope: 4 windows of 0:01.000 (use --json for the levels)"), "{text}");
        let text = undo(&project, 1, vec![], None, &opts(false)).unwrap().text;
        assert_eq!(text, "Undid e4. Head is e3. Timeline 0:03.500 -> 0:03.500. Version 6.");
        let text = export(&project, &dir.path().join("out.wav"), None, Some("0..1".into())).unwrap().text;
        assert!(text.starts_with("Exported 0:01.000 to ") && text.contains("WAV 16-bit, 48000 Hz, 2 ch, 192044 bytes."), "{text}");
        let err = open(Some(&project)).unwrap_err();
        assert!(err.to_string().contains("milestone M2") && err.hint_text().is_some());
    }
}
