//! Runs the built `scrubr` binary the way an agent does: arguments in, JSON and an exit code out.

use std::f32::consts::TAU;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const RATE: u32 = 48_000;

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.out).unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}\nstderr:\n{}", self.out, self.err))
    }

    /// The reply of a command that must succeed.
    fn ok(&self) -> Value {
        let reply = self.json();
        assert_eq!((self.code, &reply["ok"]), (0, &json!(true)), "{}\n{}", self.out, self.err);
        assert!(reply["project_version"].is_u64(), "every reply carries project_version: {}", self.out);
        reply
    }

    /// The error of a command that must fail with `code`.
    fn fails(&self, code: i32, error: &str) -> Value {
        let reply = self.json();
        assert_eq!(self.code, code, "{}\n{}", self.out, self.err);
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(reply["error"]["code"], json!(error), "{}", self.out);
        assert!(reply["error"]["message"].as_str().is_some_and(|m| !m.is_empty()));
        reply["error"].clone()
    }
}

struct Bench {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Bench {
    /// A folder with `ep.wav`: 12 s of tone with two silent gaps, at 3.0-4.5 s and 8.0-9.2 s.
    fn new() -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_wav(&root.join("ep.wav"), 12.0, |t| if (3.0..4.5).contains(&t) || (8.0..9.2).contains(&t) { 0.0 } else { (TAU * 220.0 * t).cos() * 0.1 });
        Bench { _dir: dir, root }
    }

    /// The same folder with a project `ep.scrubr` over the file.
    fn with_project() -> Bench {
        let bench = Bench::new();
        bench.run(&["new", "ep.scrubr", "--from", "ep.wav"]).ok();
        bench
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_scrubr"));
        // No test opens a real window: the window's program is pointed at a file that is not there.
        command.args(args).current_dir(&self.root).env_remove("SCRUBR_AUTHOR").env_remove("SCRUBR_SESSION").env("SCRUBR_GUI", "no-such-scrubr-window");
        command
    }

    fn run(&self, args: &[&str]) -> Run {
        self.finish(self.command(args))
    }

    fn run_as(&self, author: &str, session: &str, args: &[&str]) -> Run {
        let mut command = self.command(args);
        command.env("SCRUBR_AUTHOR", author).env("SCRUBR_SESSION", session);
        self.finish(command)
    }

    fn finish(&self, mut command: Command) -> Run {
        let output = command.output().unwrap();
        Run { code: output.status.code().unwrap(), out: String::from_utf8(output.stdout).unwrap(), err: String::from_utf8(output.stderr).unwrap() }
    }

    fn project_text(&self) -> String {
        fs::read_to_string(self.root.join("ep.scrubr")).unwrap()
    }

    fn project(&self) -> Value {
        serde_json::from_str(&self.project_text()).unwrap()
    }
}

fn write_wav(path: &Path, seconds: f32, sample: impl Fn(f32) -> f32) {
    let spec = hound::WavSpec { channels: 2, sample_rate: RATE, bits_per_sample: 24, sample_format: hound::SampleFormat::Int };
    let mut out = hound::WavWriter::create(path, spec).unwrap();
    for f in 0..(seconds * RATE as f32) as u32 {
        let s = (sample(f as f32 / RATE as f32) * 8_388_607.0) as i32;
        out.write_sample(s).unwrap();
        out.write_sample(s).unwrap();
    }
    out.finalize().unwrap();
}

#[test]
fn the_prd_session_runs_end_to_end() {
    let bench = Bench::new();
    let created = bench.run(&["new", "ep.scrubr", "--from", "ep.wav"]).ok();
    assert_eq!((created["project_version"].as_u64(), created["timeline_seconds"].as_f64()), (Some(1), Some(12.0)));
    assert!(bench.root.join("ep.scrubr.cache").join("src1.peaks").exists());

    // The person trims both ends, then the agent takes over.
    bench.run_as("you", "manual", &["keep", "ep.scrubr", "0.5..11.5", "--crossfade", "0"]).ok();
    let report = bench.run_as("codex", "s7", &["analyze", "ep.scrubr", "--loudness", "--silence", "-45dB:1s"]).ok();
    assert_eq!(report["project_version"], json!(2));
    assert_eq!(report["timeline_seconds"], json!(11.0));
    assert_eq!(report["silence_count"], json!(2));
    assert_eq!(report["silence"], json!([{ "start": 2.5, "end": 4.0 }, { "start": 7.5, "end": 8.7 }]));
    let lufs = report["loudness"]["integrated_lufs"].as_f64().unwrap();
    // A sine at 0.1 is -20 dBFS; the gaps' hard edges overshoot a little under true-peak metering.
    assert!((-23.0..-20.0).contains(&lufs) && report["loudness"]["true_peak_dbtp"].as_f64().unwrap() < -18.0, "{report}");

    // The agent turns report.silence into cuts.json, straight from the reply's numbers.
    let cuts: Vec<Value> = report["silence"].as_array().unwrap().iter().map(|s| json!({ "op": "cut", "start": s["start"], "end": s["end"] })).collect();
    fs::write(bench.root.join("cuts.json"), serde_json::to_string(&cuts).unwrap()).unwrap();
    let applied = bench.run_as("codex", "s7", &["apply", "ep.scrubr", "--ops", "cuts.json", "--if-version", "2"]).ok();
    assert_eq!((applied["project_version"].as_u64(), applied["ops"].as_u64(), applied["removed_seconds"].as_f64()), (Some(3), Some(2), Some(2.7)));
    assert_eq!(applied["timeline_seconds"], json!(8.3));

    let normalized = bench.run_as("codex", "s7", &["normalize", "ep.scrubr", "--lufs", "-16"]).ok();
    assert_eq!((normalized["target_lufs"].as_f64(), normalized["limited_by"].clone(), normalized["ceiling_dbtp"].as_f64()), (Some(-16.0), Value::Null, Some(-1.0)));
    assert!((normalized["achieved_lufs"].as_f64().unwrap() + 16.0).abs() < 0.011, "{normalized}");
    // With the gaps gone the tone measures about -20 LUFS, so about +4 dB reaches -16.
    assert!((3.5..5.0).contains(&normalized["gain_db"].as_f64().unwrap()), "{normalized}");

    let cut = bench.run_as("codex", "s7", &["cut", "ep.scrubr", "0:05.25..0:05.9"]).ok();
    assert_eq!((cut["edit"].as_str(), cut["removed_seconds"].as_f64(), cut["crossfade_ms"].as_f64()), (Some("e5"), Some(0.65), Some(5.0)));
    let label = bench.run_as("codex", "s7", &["label", "ep.scrubr", "add", "0:06.5..0:07.2", "check: cough?"]).ok();
    assert_eq!((label["label"]["start"].as_f64(), label["label"]["text"].as_str(), label["label"]["author"].as_str()), (Some(6.5), Some("check: cough?"), Some("codex")));
    let image = bench.run_as("codex", "s7", &["render", "ep.scrubr", "--png", "check.png"]).ok();
    assert_eq!((image["width"].as_u64(), image["end"].as_f64()), (Some(1600), Some(7.65)));
    assert_eq!(&fs::read(bench.root.join("check.png")).unwrap()[1..4], b"PNG");
    let exported = bench.run_as("codex", "s7", &["export", "ep.scrubr", "out/ep42.flac"]).ok();
    assert_eq!((exported["format"].as_str(), exported["bits"].as_u64(), exported["seconds"].as_f64(), exported["clipped_samples"].as_u64()), (Some("flac"), Some(24), Some(7.65), Some(0)));
    assert_eq!(&fs::read(bench.root.join("out").join("ep42.flac")).unwrap()[..4], b"fLaC");

    // The file records who did what, in source coordinates.
    let project = bench.project();
    assert_eq!(project["version"], json!(6), "five edits and a label; reads do not write");
    let edits = project["edits"].as_array().unwrap();
    assert_eq!(edits.iter().map(|e| e["author"].as_str().unwrap()).collect::<Vec<_>>(), ["cli", "you", "codex", "codex", "codex"]);
    assert_eq!(edits[2]["session"], json!("s7"));
    assert_eq!(edits[2]["ops"][0]["spans"], json!([["src1", 144_000, 216_000]]), "timeline 2.5-4.0 s is source 3.0-4.5 s");
    assert_eq!(edits[3]["gain_db"], normalized["gain_db"]);
    assert_eq!(project["labels"][0]["span"][0], json!("src1"));

    // The person takes the agent's whole session back out; their own trim stays.
    let history = bench.run(&["history", "ep.scrubr"]).ok();
    assert_eq!(history["edits"].as_array().unwrap().len(), 5);
    let reverted = bench.run_as("you", "manual", &["undo", "ep.scrubr", "--of-session", "s7"]).ok();
    assert_eq!((reverted["reverted"].clone(), reverted["timeline_seconds"].as_f64()), (json!(["e3", "e4", "e5"]), Some(11.0)));
    let history = bench.run(&["history", "ep.scrubr"]).ok();
    assert_eq!(history["edits"][2]["state"], json!("reverted"));
    assert_eq!(history["edits"][1]["state"], json!("applied"));
}

#[test]
fn exit_codes_and_error_shape_follow_the_prd() {
    let bench = Bench::with_project();
    // 1: bad usage.
    let error = bench.run(&["cut", "ep.scrubr", "soon..later"]).fails(1, "bad_time");
    assert!(error["hint"].as_str().unwrap().contains("12:03.250"));
    let error = bench.run(&["cut", "ep.scrubr", "5..3605"]).fails(1, "range_out_of_bounds");
    assert_eq!(error["message"], json!("End 3605.000s is past the end of the timeline (12.000s)."));
    assert!(error["hint"].as_str().unwrap().contains("scrubr info"));
    bench.run(&["cut", "ep.scrubr", "5..5"]).fails(1, "empty_range");
    bench.run(&["cut", "ep.scrubr", "1..2", "--frobnicate"]).fails(1, "bad_usage");
    bench.run(&["cut", "ep.scrubr"]).fails(1, "bad_usage");
    bench.run(&["slice", "ep.scrubr", "1..2"]).fails(1, "bad_usage");
    bench.run(&["undo", "ep.scrubr"]).fails(1, "nothing_to_undo");
    bench.run(&["redo", "ep.scrubr"]).fails(1, "nothing_to_redo");
    bench.run(&["new", "ep.scrubr", "--from", "ep.wav"]).fails(1, "project_exists");
    // 2: not found.
    let error = bench.run(&["info", "nope.scrubr"]).fails(2, "project_not_found");
    assert!(error["hint"].as_str().unwrap().contains("scrubr new"));
    bench.run(&["insert", "ep.scrubr", "missing.wav", "--at", "0"]).fails(2, "file_not_found");
    bench.run(&["apply", "ep.scrubr", "--ops", "missing.json"]).fails(2, "file_not_found");
    // 3: version conflict.
    bench.run(&["cut", "ep.scrubr", "1..2"]).ok();
    let before = bench.project_text();
    let error = bench.run(&["cut", "ep.scrubr", "1..2", "--if-version", "1"]).fails(3, "version_conflict");
    assert!(error["message"].as_str().unwrap().contains("version 2, not 1"));
    assert!(error["hint"].as_str().unwrap().contains("--if-version 2"));
    assert_eq!(bench.project_text(), before, "a refused edit writes nothing");
    // 5: decode and encode failures.
    bench.run(&["export", "ep.scrubr", "out.mp3"]).fails(5, "format_not_supported");
    fs::write(bench.root.join("junk.wav"), b"this is not audio").unwrap();
    bench.run(&["insert", "ep.scrubr", "junk.wav", "--at", "0"]).fails(5, "decode_failed");
    // 4: a source changed on disk. Every command stops, readers included.
    write_wav(&bench.root.join("ep.wav"), 12.0, |_| 0.25);
    let error = bench.run(&["info", "ep.scrubr"]).fails(4, "source_changed");
    assert!(error["hint"].as_str().unwrap().contains("tell the person"));
    bench.run(&["cut", "ep.scrubr", "1..2"]).fails(4, "source_changed");
    bench.run(&["export", "ep.scrubr", "out.wav"]).fails(4, "source_changed");
}

#[test]
fn a_dry_run_reports_the_change_and_writes_nothing() {
    let bench = Bench::with_project();
    let before = bench.project_text();
    for args in [
        vec!["cut", "ep.scrubr", "1..2", "--dry-run"],
        vec!["keep", "ep.scrubr", "1..11", "--dry-run"],
        vec!["mute", "ep.scrubr", "1..2", "--dry-run"],
        vec!["gain", "ep.scrubr", "+2dB", "--dry-run"],
        vec!["fade", "ep.scrubr", "in", "0.5s", "--dry-run"],
        vec!["normalize", "ep.scrubr", "--lufs", "-16", "--dry-run"],
        vec!["insert", "ep.scrubr", "ep.wav", "--at", "end", "--dry-run"],
        vec!["label", "ep.scrubr", "add", "1..2", "note", "--dry-run"],
    ] {
        let reply = bench.run(&args).ok();
        assert_eq!((reply["dry_run"].clone(), reply["project_version"].clone()), (json!(true), json!(1)), "{args:?}");
        assert_eq!(bench.project_text(), before, "{args:?} wrote the project");
    }
    let reply = bench.run(&["cut", "ep.scrubr", "1..2", "--dry-run"]).ok();
    assert_eq!((reply["timeline_seconds_before"].as_f64(), reply["timeline_seconds"].as_f64()), (Some(12.0), Some(11.0)));
    let reply = bench.run(&["insert", "ep.scrubr", "ep.wav", "--at", "end", "--dry-run"]).ok();
    assert_eq!(reply["timeline_seconds"], json!(24.0));
    assert!(!bench.root.join("ep.scrubr.cache").join("src2.peaks").exists());
}

#[test]
fn the_quietest_moment_in_a_range_is_where_to_cut() {
    let bench = Bench::with_project();
    // The gaps are at 3.0-4.5 s and 8.0-9.2 s. Each search has its own range.
    let reply = bench.run(&["analyze", "ep.scrubr", "--quietest", "2.5..5", "--quietest", "7.9..9.5"]).ok();
    let found = reply["quietest"].as_array().unwrap();
    assert_eq!(found.len(), 2);
    let near = |value: &Value, seconds: f64| (value.as_f64().unwrap() - seconds).abs() < 0.011;
    assert!(near(&found[0]["at"], 3.75) && near(&found[0]["start"], 3.0) && near(&found[0]["end"], 4.5), "{reply}");
    assert!(near(&found[1]["at"], 8.6) && near(&found[1]["start"], 8.0) && near(&found[1]["end"], 9.2), "{reply}");
    assert_eq!((found[0]["range"].clone(), found[0]["rms_dbfs"].clone()), (json!({ "start": 2.5, "end": 5.0 }), json!(-120.0)));
    // Asked for alone, nothing else is measured.
    assert!(reply["loudness"].is_null() && reply["peaks"].is_null(), "{reply}");
    // Beside other measures it is added to them.
    let reply = bench.run(&["analyze", "ep.scrubr", "--peaks", "--quietest", "0..12"]).ok();
    assert!(reply["peaks"].is_object() && reply["quietest"][0]["rms_dbfs"] == json!(-120.0), "{reply}");
    // The reply's `at` cuts cleanly when passed straight back.
    let at = (found[0]["at"].as_f64().unwrap(), found[1]["at"].as_f64().unwrap());
    let cut = bench.run(&["cut", "ep.scrubr", &format!("{}..{}", at.0, at.1)]).ok();
    assert!((cut["timeline_seconds"].as_f64().unwrap() - (12.0 - (at.1 - at.0))).abs() < 1e-6, "{cut}");
    bench.run(&["analyze", "ep.scrubr", "--quietest", "5..99"]).fails(1, "range_out_of_bounds");
}

#[test]
fn negative_levels_are_values_not_flags() {
    let bench = Bench::with_project();
    let reply = bench.run(&["gain", "ep.scrubr", "-3dB", "--range", "0..0:02"]).ok();
    assert_eq!(reply["gain_db"], json!(-3.0));
    let reply = bench.run(&["gain", "ep.scrubr", "-1.5"]).ok();
    assert_eq!((reply["gain_db"].clone(), reply["range"].clone()), (json!(-1.5), Value::Null));
    let reply = bench.run(&["analyze", "ep.scrubr", "--silence", "-45dB:1s", "--peaks", "--envelope", "1s", "--range", "0..0:06"]).ok();
    assert_eq!((reply["silence_count"].clone(), reply["range"]["end"].clone()), (json!(1), json!(6.0)));
    assert_eq!(reply["envelope"]["rms_dbfs"].as_array().unwrap().len(), 6);
    // The tone peaks at -20 dBFS, and the loudest part left has the -1.5 dB gain only.
    assert!((reply["peaks"]["peak_dbfs"][0].as_f64().unwrap() + 21.5).abs() < 0.02, "{reply}");
    let reply = bench.run(&["normalize", "ep.scrubr", "--peak", "-6dB", "--ceiling", "-3"]).ok();
    assert_eq!((reply["target_peak_dbfs"].clone(), reply["ceiling_dbtp"].clone()), (json!(-6.0), json!(-3.0)));
    assert!((reply["achieved_peak_dbfs"].as_f64().unwrap() + 6.0).abs() < 0.011, "{reply}");
    // A bad level still reads back with a plain minus.
    let error = bench.run(&["gain", "ep.scrubr", "-3 apples"]).fails(1, "bad_level");
    assert!(error["message"].as_str().unwrap().contains("\"-3 apples\""));
}

#[test]
fn normalize_reports_when_the_ceiling_stops_it() {
    let bench = Bench::new();
    // Quiet tone with a full-scale click each second: the peaks leave no room for -16 LUFS.
    write_wav(&bench.root.join("ep.wav"), 6.0, |t| if (t * RATE as f32) as u32 % RATE < 8 { 0.9 } else { (TAU * 220.0 * t).sin() * 0.01 });
    bench.run(&["new", "ep.scrubr", "--from", "ep.wav"]).ok();
    let reply = bench.run(&["normalize", "ep.scrubr", "--lufs", "-16"]).ok();
    assert_eq!(reply["limited_by"], json!("true_peak"));
    assert_eq!(reply["target_lufs"], json!(-16.0));
    assert!(reply["achieved_lufs"].as_f64().unwrap() < -17.0, "{reply}");
    let peak = reply["true_peak_dbtp"].as_f64().unwrap();
    assert!(peak <= -1.0 && peak > -1.02, "{reply}");
    // Measuring afterwards agrees with the reply.
    let measured = bench.run(&["analyze", "ep.scrubr", "--loudness"]).ok();
    assert!(measured["loudness"]["true_peak_dbtp"].as_f64().unwrap() <= -1.0);
    assert!((measured["loudness"]["integrated_lufs"].as_f64().unwrap() - reply["achieved_lufs"].as_f64().unwrap()).abs() < 0.03);
}

#[test]
fn labels_follow_their_audio() {
    let bench = Bench::with_project();
    bench.run_as("codex", "s1", &["label", "ep.scrubr", "add", "0:06..0:07", "check this"]).ok();
    bench.run(&["label", "ep.scrubr", "add", "10", "a point"]).ok();
    bench.run(&["cut", "ep.scrubr", "1..3", "--crossfade", "0"]).ok();
    let list = bench.run(&["label", "ep.scrubr", "list"]).ok();
    let labels = list["labels"].as_array().unwrap();
    assert_eq!((labels[0]["start"].as_f64(), labels[0]["end"].as_f64(), labels[0]["hidden"].as_bool()), (Some(4.0), Some(5.0), Some(false)));
    assert_eq!((labels[1]["start"].as_f64(), labels[1]["end"].as_f64()), (Some(8.0), Some(8.0)));
    // Cutting the labelled audio hides the label without losing it; undo brings it back.
    bench.run(&["cut", "ep.scrubr", "3.5..5.5"]).ok();
    let list = bench.run(&["label", "ep.scrubr", "list"]).ok();
    assert_eq!((list["labels"][0]["hidden"].as_bool(), list["labels"][0]["start"].as_f64()), (Some(true), Some(3.5)));
    bench.run(&["undo", "ep.scrubr"]).ok();
    let removed = bench.run(&["label", "ep.scrubr", "rm", "l1"]).ok();
    assert_eq!(removed["removed"]["text"], json!("check this"));
    bench.run(&["label", "ep.scrubr", "rm", "l1"]).fails(1, "unknown_label");
    assert_eq!(bench.run(&["label", "ep.scrubr", "list"]).ok()["labels"].as_array().unwrap().len(), 1);
}

#[test]
fn undo_redo_and_single_edit_reverts() {
    let bench = Bench::with_project();
    bench.run(&["cut", "ep.scrubr", "1..2", "--crossfade", "0"]).ok();
    bench.run(&["cut", "ep.scrubr", "5..6", "--crossfade", "0"]).ok();
    let undone = bench.run(&["undo", "ep.scrubr", "--steps", "2"]).ok();
    assert_eq!((undone["undone"].clone(), undone["head"].clone(), undone["timeline_seconds"].clone()), (json!(["e3", "e2"]), json!("e1"), json!(12.0)));
    let redone = bench.run(&["redo", "ep.scrubr", "--steps", "5"]).ok();
    assert_eq!((redone["redone"].clone(), redone["head"].clone()), (json!(["e2", "e3"]), json!("e3")));
    // Take the first cut out from under the second.
    let reverted = bench.run(&["undo", "ep.scrubr", "--edit", "e2"]).ok();
    assert_eq!((reverted["edit"].clone(), reverted["reverted"].clone(), reverted["timeline_seconds"].clone()), (json!("e4"), json!(["e2"]), json!(11.0)));
    assert_eq!(bench.project()["edits"][2]["spans"], json!([["src1", 288_000, 336_000]]), "the second cut still names the audio it removed");
    bench.run(&["undo", "ep.scrubr", "--edit", "e2"]).fails(1, "not_applied");
    bench.run(&["undo", "ep.scrubr", "--edit", "e1"]).fails(1, "cannot_revert");
    bench.run(&["undo", "ep.scrubr", "--edit", "e99"]).fails(1, "unknown_edit");
    bench.run(&["undo", "ep.scrubr", "--of-session", "nobody"]).fails(1, "nothing_to_undo");
}

#[test]
fn a_batch_reads_stdin_and_refuses_a_bad_operation_whole() {
    let bench = Bench::with_project();
    let before = bench.project_text();
    let mut command = bench.command(&["apply", "ep.scrubr", "--ops", "-"]);
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"crossfade": 0, "ops": [{"op": "cut", "range": "1..2"}, {"op": "cut", "range": "11..13"}]}"#).unwrap();
    let output = child.wait_with_output().unwrap();
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!((output.status.code(), reply["error"]["code"].as_str()), (Some(1), Some("range_out_of_bounds")));
    assert_eq!(bench.project_text(), before);

    let mut child = bench.command(&["apply", "ep.scrubr", "--ops", "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"[{"op": "cut", "range": "1..2"}, {"op": "cut", "range": "10..11"}, {"op": "fade", "dir": "out", "length": "0.5s"}, {"op": "normalize", "peak": -3}]"#)
        .unwrap();
    let reply: Value = serde_json::from_slice(&child.wait_with_output().unwrap().stdout).unwrap();
    assert_eq!((reply["ok"].clone(), reply["ops"].clone(), reply["timeline_seconds"].clone()), (json!(true), json!(4), json!(10.0)));
    assert_eq!(reply["by_op"], json!({ "cut": 2, "fade": 1, "normalize": 1 }));
    assert!(reply["normalize"]["gain_db"].as_f64().unwrap() > 10.0);
    assert_eq!(bench.project()["edits"].as_array().unwrap().len(), 2, "one batch, one edit");
    bench.run(&["apply", "ep.scrubr", "--ops", "ep.wav"]).fails(1, "bad_ops");
}

#[test]
fn the_agent_guide_is_agents_md() {
    let bench = Bench::new();
    let guide = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../AGENTS.md")).unwrap();
    let printed = bench.run(&["help", "--agent"]);
    assert_eq!(printed.code, 0);
    assert_eq!(printed.out.replace("\r\n", "\n").trim_end(), guide.replace("\r\n", "\n").trim_end());
    for needle in ["--if-version", "Batch rule", "Exit codes", "Worked example", "end-2s"] {
        assert!(guide.contains(needle), "the guide covers {needle}");
    }
    let as_json = bench.run(&["help", "--agent", "--json"]).json();
    assert_eq!(as_json["guide"].as_str().unwrap().replace("\r\n", "\n"), guide.replace("\r\n", "\n"));
    assert!(bench.run(&["help"]).out.contains("Usage: scrubr"));
    assert!(bench.run(&["help", "cut"]).out.contains("--crossfade"));
    assert!(bench.run(&["--help"]).out.contains("help --agent"));
}

#[test]
fn features_not_in_this_build_say_so() {
    let bench = Bench::with_project();
    let error = bench.run(&["transcribe", "ep.scrubr"]).fails(1, "feature_not_built");
    assert!(error["message"].as_str().unwrap().contains("optional feature"));
    assert!(error["hint"].as_str().unwrap().contains("--features transcribe"));
    bench.run(&["transcribe", "nope.scrubr"]).fails(2, "project_not_found");
    // The window is a program of its own; without it `open` says where it looked.
    let error = bench.run(&["open", "ep.scrubr"]).fails(2, "gui_not_found");
    assert!(error["hint"].as_str().unwrap().contains("SCRUBR_GUI"));
    bench.run(&["open", "nope.scrubr"]).fails(2, "project_not_found");
    bench.run(&[]).fails(2, "gui_not_found");
}

#[test]
fn exports_of_one_version_are_identical_and_wav_matches_the_request() {
    let bench = Bench::with_project();
    bench.run(&["cut", "ep.scrubr", "1..2"]).ok();
    bench.run(&["fade", "ep.scrubr", "out", "1s"]).ok();
    for (a, b) in [("a.flac", "b.flac"), ("a.wav", "b.wav")] {
        bench.run(&["export", "ep.scrubr", a]).ok();
        bench.run(&["export", "ep.scrubr", b]).ok();
        assert_eq!(fs::read(bench.root.join(a)).unwrap(), fs::read(bench.root.join(b)).unwrap(), "{a} and {b}");
    }
    let reply = bench.run(&["export", "ep.scrubr", "part.wav", "--bits", "16", "--range", "0:02..0:04"]).ok();
    assert_eq!((reply["bits"].clone(), reply["frames"].clone()), (json!(16), json!(96_000)));
    let spec = hound::WavReader::open(bench.root.join("part.wav")).unwrap().spec();
    assert_eq!((spec.bits_per_sample, spec.channels, spec.sample_rate), (16, 2, RATE));
    bench.run(&["export", "ep.scrubr", "x.flac", "--bits", "32f"]).fails(1, "bad_depth");
    bench.run(&["export", "ep.scrubr", "ep.wav"]).fails(1, "would_overwrite_source");
}
