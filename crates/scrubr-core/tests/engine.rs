//! End-to-end tests of the engine on real files: projects on disk, rendered audio, exports.

use std::f32::consts::TAU;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use scrubr_core::analysis::{self, Options as Measure};
use scrubr_core::edit::{self, DEFAULT_CEILING_DBTP, Request, Revert, Target};
use scrubr_core::export::{self, Depth};
use scrubr_core::image;
use scrubr_core::project::{self, EditState, FadeDir, Op, Paths, Project, Who, WriteOpts};
use scrubr_core::pacing::{self, Landing, Tighten};
use scrubr_core::phrase::{self, Pick};
use scrubr_core::render::Renderer;
use scrubr_core::transcribe::{self, Heard};
use scrubr_core::words;
use scrubr_core::{Kind, Timeline, WavFile};

const RATE: u32 = 48_000;

fn who(author: &str, session: &str) -> Who {
    Who { author: author.into(), session: Some(session.into()) }
}

fn me() -> Who {
    who("test", "t1")
}

/// A small deterministic noise source in -1.0..1.0.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }
}

fn write_wav(path: &Path, rate: u32, channels: u16, bits: u16, frames: u64, mut sample: impl FnMut(u64, u16) -> f32) {
    let spec = hound::WavSpec { channels, sample_rate: rate, bits_per_sample: bits, sample_format: hound::SampleFormat::Int };
    let mut out = hound::WavWriter::create(path, spec).unwrap();
    let full = ((1i64 << (bits - 1)) - 1) as f32;
    for f in 0..frames {
        for c in 0..channels {
            out.write_sample((sample(f, c).clamp(-1.0, 1.0) * full).round() as i32).unwrap();
        }
    }
    out.finalize().unwrap();
}

/// Every sample of a WAV as the integer it stores.
fn read_ints(path: &Path) -> Vec<i32> {
    hound::WavReader::open(path).unwrap().samples::<i32>().map(Result::unwrap).collect()
}

fn read_floats(path: &Path) -> Vec<f32> {
    let mut wav = WavFile::open(path).unwrap();
    let mut out = Vec::new();
    wav.read(wav.info().frames as usize, &mut out).unwrap();
    out
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    paths: Paths,
}

impl Fixture {
    /// A project over one stereo 24-bit file of noise, `frames` long.
    fn noise(frames: u64) -> Fixture {
        Self::with(frames, 2, |_, _, noise| noise.next() * 0.5)
    }

    fn with(frames: u64, channels: u16, mut sample: impl FnMut(u64, u16, &mut Noise) -> f32) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut noise = Noise(0x9E37_79B9_7F4A_7C15);
        write_wav(&root.join("in.wav"), RATE, channels, 24, frames, |f, c| sample(f, c, &mut noise));
        let paths = Paths::new(root.join("p.scrubr"));
        edit::new_project(&paths, &[root.join("in.wav")], &me(), false).unwrap();
        Fixture { _dir: dir, root, paths }
    }

    fn source(&self) -> PathBuf {
        self.root.join("in.wav")
    }

    fn project(&self) -> Project {
        project::open(&self.paths).unwrap()
    }

    fn apply(&self, requests: Vec<Request>) -> edit::Applied {
        edit::apply(&self.paths, &WriteOpts::default(), &me(), false, |_, _| Ok((requests, None))).unwrap().value
    }

    fn batch(&self, requests: Vec<Request>) -> edit::Applied {
        edit::apply(&self.paths, &WriteOpts::default(), &me(), true, |_, _| Ok((requests, None))).unwrap().value
    }

    fn export(&self, name: &str) -> PathBuf {
        let out = self.root.join(name);
        export::export(&self.project(), &self.paths, &out, &Default::default()).unwrap();
        out
    }

    /// The whole timeline as rendered floats.
    fn render(&self, block: usize) -> Vec<f32> {
        let project = self.project();
        let mut renderer = Renderer::new(&project, &self.paths).unwrap();
        let total = renderer.total();
        let (mut all, mut buf, mut t) = (Vec::new(), Vec::new(), 0u64);
        while t < total {
            let n = ((total - t) as usize).min(block);
            renderer.read(t, n, &mut buf).unwrap();
            all.extend_from_slice(&buf);
            t += n as u64;
        }
        all
    }

    fn measure(&self, options: &Measure) -> analysis::Report {
        let project = self.project();
        let mut renderer = Renderer::new(&project, &self.paths).unwrap();
        let total = renderer.total();
        analysis::analyze(&mut renderer, 0, total, options, RATE).unwrap()
    }
}

fn cut(a: u64, b: u64, crossfade: u32) -> Request {
    Request::Cut { range: (a, b), crossfade }
}

// ---- accuracy --------------------------------------------------------------------------------

#[test]
fn an_untouched_project_exports_its_source_sample_for_sample() {
    let fx = Fixture::noise(20_000);
    assert_eq!(read_ints(&fx.export("out.wav")), read_ints(&fx.source()));
}

#[test]
fn a_hard_cut_lands_on_exact_frames() {
    let fx = Fixture::noise(20_000);
    fx.apply(vec![cut(5_000, 7_500, 0)]);
    let src = read_ints(&fx.source());
    let mut want = src[..5_000 * 2].to_vec();
    want.extend_from_slice(&src[7_500 * 2..]);
    assert_eq!(read_ints(&fx.export("out.wav")), want);
}

#[test]
fn a_crossfade_keeps_the_length_and_only_touches_its_window() {
    let fx = Fixture::noise(20_000);
    fx.apply(vec![cut(5_000, 7_500, 240)]);
    let src = read_floats(&fx.source());
    let out = fx.render(4096);
    assert_eq!(out.len(), 17_500 * 2);
    let at = |frame: usize, c: usize| src[frame * 2 + c];
    for t in 0..17_500usize {
        for c in 0..2 {
            let hard = if t < 5_000 { at(t, c) } else { at(t + 2_500, c) };
            let got = out[t * 2 + c];
            if !(4_880..5_120).contains(&t) {
                assert_eq!(got, hard, "frame {t} is outside the crossfade and must be untouched");
            } else {
                // Inside it, the audio before the cut fades out across the join while the audio
                // after the cut fades in.
                let fade_in = (t as f32 - 4_880.0 + 0.5) / 240.0;
                let want = at(t, c) * (1.0 - fade_in) + at(t + 2_500, c) * fade_in;
                assert!((got - want).abs() < 1e-6, "frame {t}: {got} vs {want}");
            }
        }
    }
}

#[test]
fn trimmed_ends_fade_from_and_to_silence() {
    let fx = Fixture::with(20_000, 1, |_, _, _| 0.5);
    fx.apply(vec![Request::Keep { range: (1_000, 19_000), crossfade: 240 }]);
    let out = fx.render(4096);
    assert_eq!(out.len(), 18_000);
    assert!(out[0] < 0.002 && out[17_999] < 0.002, "the ends start and finish near silence");
    assert!(out[..240].windows(2).all(|w| w[0] < w[1]), "the start ramps up");
    assert!(out[17_760..].windows(2).all(|w| w[0] > w[1]), "the end ramps down");
    assert!(out[240..17_760].iter().all(|&s| (s - 0.5).abs() < 1e-6));
    // With no crossfade the ends are left exactly as they are.
    project::update(&fx.paths, &WriteOpts::default(), |p| p.undo(1)).unwrap();
    fx.apply(vec![Request::Keep { range: (1_000, 19_000), crossfade: 0 }]);
    assert!(fx.render(4096).iter().all(|&s| (s - 0.5).abs() < 1e-6));
}

#[test]
fn rendering_does_not_depend_on_how_reads_are_chunked() {
    let fx = Fixture::noise(30_000);
    write_wav(&fx.root.join("sting.wav"), RATE, 1, 16, 3_000, |f, _| (TAU * 440.0 * f as f32 / RATE as f32).sin() * 0.4);
    fx.batch(vec![cut(2_000, 2_600, 240), cut(9_000, 9_010, 240), cut(15_000, 16_000, 0)]);
    fx.apply(vec![Request::Insert { file: fx.root.join("sting.wav"), at: 6_000, crossfade: 240 }]);
    fx.apply(vec![Request::Fade { dir: FadeDir::In, range: (0, 4_000) }]);
    fx.apply(vec![Request::Mute { range: (10_000, 11_000), crossfade: 240 }]);
    fx.apply(vec![Request::Gain { db: -3.0, range: Some((12_000, 20_000)) }]);
    fx.apply(vec![Request::Gain { db: 1.5, range: None }]);
    let whole = fx.render(1 << 20);
    assert_eq!(whole.len() as u64, (30_000 - 600 - 10 - 1_000 + 3_000) * 2);
    for block in [1, 7, 241, 4096] {
        assert!(fx.render(block) == whole, "blocks of {block} frames gave different audio");
    }
    // The mute is silent in its middle and the fade starts from near silence.
    assert!(whole[10_300 * 2..10_700 * 2].iter().all(|&s| s == 0.0));
    assert!(whole[0].abs() < 0.001);
}

// ---- the batch rule --------------------------------------------------------------------------

#[test]
fn a_batch_cuts_where_one_analysis_said_and_matches_cutting_one_at_a_time() {
    let ranges = [(2_000u64, 3_000u64), (8_000, 8_500), (15_000, 19_000)];
    let batch = Fixture::noise(24_000);
    let applied = batch.batch(ranges.iter().map(|&(a, b)| cut(a, b, 240)).collect());
    assert_eq!(applied.frames_before - applied.frames_after, 5_500);
    assert_eq!(batch.project().edits.len(), 2, "the batch is one edit");

    // The same cuts one command at a time need their positions shifted by what came before.
    let seq = Fixture::noise(24_000);
    let mut shift = 0;
    for &(a, b) in &ranges {
        seq.apply(vec![cut(a - shift, b - shift, 240)]);
        shift += b - a;
    }
    assert_eq!(fs::read(batch.export("out.wav")).unwrap(), fs::read(seq.export("out.wav")).unwrap());

    // One undo takes the whole batch back.
    edit::undo(&batch.paths, &WriteOpts::default(), 1).unwrap();
    assert_eq!(read_ints(&batch.export("undone.wav")), read_ints(&batch.source()));
}

#[test]
fn a_batch_refuses_a_range_past_the_end_and_writes_nothing() {
    let fx = Fixture::noise(10_000);
    let before = fs::read(&fx.paths.project).unwrap();
    let err = edit::apply(&fx.paths, &WriteOpts::default(), &me(), true, |_, _| Ok((vec![cut(100, 200, 0), cut(9_000, 10_001, 0)], None))).unwrap_err();
    assert_eq!((err.kind(), err.code()), (Kind::Usage, "range_out_of_bounds"));
    assert!(err.hint_text().unwrap().contains("scrubr info"));
    assert_eq!(fs::read(&fx.paths.project).unwrap(), before);
}

// ---- undo in the middle of the log -----------------------------------------------------------

#[test]
fn undoing_a_session_in_the_middle_leaves_later_edits_and_labels_in_place() {
    let fx = Fixture::noise(48_000);
    let agent = who("codex", "s7");
    let opts = WriteOpts::default();
    // You trim, the agent cuts twice, then you cut and leave a label.
    edit::apply(&fx.paths, &opts, &who("you", "gui"), false, |_, _| Ok((vec![Request::Keep { range: (4_000, 44_000), crossfade: 0 }], None))).unwrap();
    edit::apply(&fx.paths, &opts, &agent, false, |_, _| Ok((vec![cut(10_000, 12_000, 0)], None))).unwrap();
    edit::apply(&fx.paths, &opts, &agent, false, |_, _| Ok((vec![cut(20_000, 21_000, 0)], None))).unwrap();
    let mine = edit::apply(&fx.paths, &opts, &who("you", "gui"), false, |_, _| Ok((vec![cut(30_000, 31_000, 0)], None))).unwrap().value;
    let (label, _) = edit::add_label(&fx.paths, &opts, &who("you", "gui"), |_, _| Ok(((33_000, 33_500), "keep this".into()))).unwrap().value;

    let before = fx.project();
    let my_cut = before.edits.iter().find(|e| e.id == mine.edit).unwrap().clone();
    // Timeline 30000 sat 4000 (trim) + 2000 + 1000 (agent cuts) frames later in the source.
    assert_eq!(my_cut.op, Op::Cut { spans: vec![project::Span("src1".into(), 37_000, 38_000)], crossfade: 0 });
    assert_eq!(label.span, project::Span("src1".into(), 40_000 + 1_000, 40_500 + 1_000));

    let (revert_id, moved) = edit::revert(&fx.paths, &opts, &who("you", "gui"), &Revert::Session("s7".into())).unwrap().value;
    assert_eq!(moved.edits, vec!["e3".to_string(), "e4".to_string()]);
    assert_eq!(moved.frames_after - moved.frames_before, 3_000);

    let after = fx.project();
    // Your later cut and your label are stored exactly as before...
    assert_eq!(after.edits.iter().find(|e| e.id == mine.edit).unwrap(), &my_cut);
    assert_eq!(after.labels, before.labels);
    // ...the agent's edits are still in the log, marked reverted, and yours are applied...
    let states = after.states();
    assert_eq!(states[2], EditState::Reverted(revert_id.clone()));
    assert_eq!(states[3], EditState::Reverted(revert_id));
    assert_eq!(states[4], EditState::Applied);
    // ...and the audio is the trim plus your cut, with the agent's audio back in between.
    let src = read_ints(&fx.source());
    let mut want = src[4_000 * 2..37_000 * 2].to_vec();
    want.extend_from_slice(&src[38_000 * 2..44_000 * 2]);
    assert_eq!(read_ints(&fx.export("out.wav")), want);
    // The label still marks the same audio, now 3000 frames later on the timeline.
    let tl = Timeline::build(&after).unwrap();
    assert_eq!(tl.locate(0, 41_000, 41_500), vec![(36_000, 36_500)]);
}

// ---- versions and the lock -------------------------------------------------------------------

#[test]
fn a_stale_version_is_refused_and_nothing_is_written() {
    let fx = Fixture::noise(10_000);
    assert_eq!(fx.project().version, 1);
    fx.apply(vec![cut(0, 100, 0)]);
    let before = fs::read(&fx.paths.project).unwrap();
    let stale = WriteOpts { if_version: Some(1), ..Default::default() };
    let err = edit::apply(&fx.paths, &stale, &me(), false, |_, _| Ok((vec![cut(0, 100, 0)], None))).unwrap_err();
    assert_eq!((err.kind(), err.code(), err.exit_code()), (Kind::Conflict, "version_conflict", 3));
    assert!(err.to_string().contains("version 2, not 1"));
    assert!(err.hint_text().unwrap().contains("--if-version 2"));
    assert_eq!(fs::read(&fx.paths.project).unwrap(), before);
    // The current version passes and moves the project on by one.
    let fresh = WriteOpts { if_version: Some(2), ..Default::default() };
    let written = edit::apply(&fx.paths, &fresh, &me(), false, |_, _| Ok((vec![cut(0, 100, 0)], None))).unwrap();
    assert_eq!((written.version, written.wrote), (3, true));
}

#[test]
fn a_dry_run_reports_and_writes_nothing() {
    let fx = Fixture::noise(10_000);
    let before = fs::read(&fx.paths.project).unwrap();
    let dry = WriteOpts { dry_run: true, ..Default::default() };
    let written = edit::apply(&fx.paths, &dry, &me(), false, |_, _| Ok((vec![cut(1_000, 3_000, 0)], None))).unwrap();
    assert_eq!((written.version, written.wrote), (1, false));
    assert_eq!((written.value.frames_before, written.value.frames_after), (10_000, 8_000));
    assert_eq!(fs::read(&fx.paths.project).unwrap(), before);
}

#[test]
fn of_two_writers_with_the_same_version_exactly_one_passes() {
    let fx = Fixture::noise(10_000);
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let (paths, barrier) = (fx.paths.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let opts = WriteOpts { if_version: Some(1), ..Default::default() };
                edit::apply(&paths, &opts, &me(), false, |_, _| Ok((vec![cut(0, 100, 0)], None))).map(|w| w.version).map_err(|e| e.code())
            })
        })
        .collect();
    let mut results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    results.sort();
    assert_eq!(results, vec![Ok(2), Err("version_conflict")]);
    assert_eq!(fx.project().edits.len(), 2);
}

#[test]
fn concurrent_writers_queue_on_the_lock_and_none_is_lost() {
    let fx = Fixture::noise(10_000);
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let paths = fx.paths.clone();
            std::thread::spawn(move || {
                let who = who("w", &format!("s{i}"));
                edit::apply(&paths, &WriteOpts::default(), &who, false, |_, _| Ok((vec![Request::Gain { db: 0.1, range: None }], None))).unwrap().version
            })
        })
        .collect();
    let mut versions: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    versions.sort();
    assert_eq!(versions, (2..=9).collect::<Vec<u64>>(), "every writer saw a different version");
    let project = fx.project();
    assert_eq!(project.version, 9);
    assert_eq!(project.edits.len(), 9);
    let mut sessions: Vec<_> = project.edits.iter().skip(1).map(|e| e.session.clone().unwrap()).collect();
    sessions.sort();
    sessions.dedup();
    assert_eq!(sessions.len(), 8);
    assert!((Timeline::build(&project).unwrap().global_gain_db() - 0.8).abs() < 1e-9);
}

#[test]
fn a_held_lock_makes_a_second_writer_wait_then_give_up() {
    let fx = Fixture::noise(10_000);
    let (paths, barrier) = (fx.paths.clone(), Arc::new(Barrier::new(2)));
    let holder = {
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            project::update(&paths, &WriteOpts::default(), |_| {
                barrier.wait();
                std::thread::sleep(Duration::from_millis(400));
                Ok(())
            })
            .unwrap()
        })
    };
    barrier.wait();
    let impatient = WriteOpts { lock_timeout: Duration::from_millis(50), ..Default::default() };
    let err = project::update(&fx.paths, &impatient, |_| Ok(())).unwrap_err();
    assert_eq!((err.kind(), err.code()), (Kind::Conflict, "project_locked"));
    // A patient writer gets in once the first is done.
    let written = project::update(&fx.paths, &WriteOpts::default(), |_| Ok(())).unwrap();
    holder.join().unwrap();
    assert_eq!(written.version, 3);
}

#[test]
fn a_failed_write_leaves_the_previous_version_and_no_stray_files() {
    let fx = Fixture::noise(10_000);
    let before = fs::read(&fx.paths.project).unwrap();
    let err = project::update(&fx.paths, &WriteOpts::default(), |p| -> scrubr_core::Result<()> {
        p.edits.clear();
        Err(scrubr_core::Error::usage("boom", "the command died"))
    });
    assert!(err.is_err());
    assert_eq!(fs::read(&fx.paths.project).unwrap(), before);
    let mut names: Vec<String> = fs::read_dir(&fx.root).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["in.wav", "p.scrubr", "p.scrubr.cache", "p.scrubr.lock"]);
}

// ---- source checks ---------------------------------------------------------------------------

#[test]
fn a_changed_source_stops_every_command() {
    let fx = Fixture::noise(10_000);
    // Rewriting the same audio changes the modified time but not the content: allowed, and a
    // writer records the new time.
    std::thread::sleep(Duration::from_millis(20));
    let bytes = fs::read(fx.source()).unwrap();
    fs::write(fx.source(), &bytes).unwrap();
    let stored = fx.project().sources[0].mtime.clone();
    fx.apply(vec![cut(0, 100, 0)]);
    assert_ne!(fx.project().sources[0].mtime, stored);

    // Different audio of the same length is caught by the hash.
    let mut changed = bytes.clone();
    let last = changed.len() - 1;
    changed[last] ^= 0x55;
    fs::write(fx.source(), &changed).unwrap();
    let err = project::open(&fx.paths).unwrap_err();
    assert_eq!((err.kind(), err.code(), err.exit_code()), (Kind::SourceChanged, "source_changed", 4));
    let err = edit::apply(&fx.paths, &WriteOpts::default(), &me(), false, |_, _| Ok((vec![cut(0, 100, 0)], None))).unwrap_err();
    assert_eq!(err.exit_code(), 4);

    // A missing source is "not found".
    fs::remove_file(fx.source()).unwrap();
    let err = project::open(&fx.paths).unwrap_err();
    assert_eq!((err.kind(), err.code(), err.exit_code()), (Kind::NotFound, "source_not_found", 2));
}

#[test]
fn export_checks_the_full_hash_even_when_size_and_time_match() {
    let fx = Fixture::noise(10_000);
    let meta = fs::metadata(fx.source()).unwrap();
    let mut bytes = fs::read(fx.source()).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x55;
    fs::write(fx.source(), &bytes).unwrap();
    fs::File::options().write(true).open(fx.source()).unwrap().set_modified(meta.modified().unwrap()).unwrap();
    // Size and modified time match, so the quick check passes...
    let project = project::open(&fx.paths).unwrap();
    // ...and export, which always hashes, refuses.
    let err = export::export(&project, &fx.paths, &fx.root.join("out.wav"), &Default::default()).unwrap_err();
    assert_eq!(err.code(), "source_changed");
    assert!(!fx.root.join("out.wav").exists());
}

// ---- export ----------------------------------------------------------------------------------

#[test]
fn exporting_the_same_version_twice_gives_identical_bytes() {
    let fx = Fixture::noise(60_000);
    fx.batch(vec![cut(5_000, 9_000, 240), cut(30_000, 30_500, 240)]);
    fx.apply(vec![Request::Normalize { target: Target::Lufs(-20.0), ceiling_dbtp: DEFAULT_CEILING_DBTP }]);
    fx.apply(vec![Request::Fade { dir: FadeDir::Out, range: (50_000, 55_500) }]);
    for name in ["a.wav", "a.flac"] {
        let first = fs::read(fx.export(name)).unwrap();
        let second = fs::read(fx.export(&format!("again-{name}"))).unwrap();
        assert!(!first.is_empty() && first == second, "{name} differs between two exports of one version");
    }
    // A different version gives different bytes, so the comparison above means something.
    fx.apply(vec![Request::Gain { db: -1.0, range: None }]);
    assert_ne!(fs::read(fx.export("b.wav")).unwrap(), fs::read(fx.root.join("a.wav")).unwrap());
}

#[test]
fn flac_and_wav_exports_hold_the_same_samples() {
    let fx = Fixture::noise(50_000);
    fx.apply(vec![cut(10_000, 12_345, 240)]);
    let wav = fx.export("out.wav");
    let flac = fx.export("out.flac");
    assert_eq!(&fs::read(&flac).unwrap()[..4], b"fLaC");
    // Full-scale noise does not compress, but the FLAC must not come out much larger than the
    // samples themselves.
    let (flac_len, wav_len) = (fs::metadata(&flac).unwrap().len(), fs::metadata(&wav).unwrap().len());
    assert!(flac_len < wav_len + wav_len / 50, "{flac_len} bytes of FLAC for {wav_len} bytes of WAV");
    // Decode the FLAC by importing it into a second project and exporting that untouched.
    let paths = Paths::new(fx.root.join("check.scrubr"));
    let project = edit::new_project(&paths, &[flac], &me(), false).unwrap();
    assert!(project.sources[0].conformed);
    assert_eq!((project.sources[0].frames, project.sources[0].bits), (47_655, 24));
    let back = fx.root.join("back.wav");
    export::export(&project, &paths, &back, &export::Options { depth: Some(Depth::Int(24)), ..Default::default() }).unwrap();
    assert_eq!(read_ints(&back), read_ints(&wav));
}

#[test]
fn flac_compresses_ordinary_audio() {
    // A tone over a noise floor near -94 dBFS, like a quiet room: under half the size of the WAV.
    let fx = Fixture::with(RATE as u64 * 2, 2, |f, _, noise| (TAU * 220.0 * f as f32 / RATE as f32).sin() * 0.2 + noise.next() * 0.00002);
    let (wav, flac) = (fx.export("out.wav"), fx.export("out.flac"));
    let (flac_len, wav_len) = (fs::metadata(&flac).unwrap().len(), fs::metadata(&wav).unwrap().len());
    assert!(flac_len * 2 < wav_len, "{flac_len} bytes of FLAC for {wav_len} bytes of WAV");
}

#[test]
fn export_counts_clipped_samples_and_never_touches_a_source() {
    let fx = Fixture::noise(10_000);
    fx.apply(vec![Request::Gain { db: 12.0, range: None }]);
    let report = export::export(&fx.project(), &fx.paths, &fx.root.join("hot.wav"), &Default::default()).unwrap();
    assert!(report.clipped_samples > 1_000);
    let err = export::export(&fx.project(), &fx.paths, &fx.source(), &Default::default()).unwrap_err();
    assert_eq!(err.code(), "would_overwrite_source");
    let err = export::export(&fx.project(), &fx.paths, &fx.root.join("out.ogg"), &Default::default()).unwrap_err();
    assert_eq!((err.code(), err.exit_code()), ("format_not_supported", 5));
}

#[test]
fn an_mp3_export_plays_back_as_the_same_audio() {
    // Five seconds in stereo: a 440 Hz tone at -12 dBFS on the left and a 1 kHz tone at -18 on
    // the right, silent for the fourth second.
    let frames = 5 * RATE as u64;
    let fx = Fixture::with(frames, 2, |f, c, _| {
        let t = f as f32 / RATE as f32;
        if (3.0..4.0).contains(&t) {
            return 0.0;
        }
        if c == 0 { (TAU * 440.0 * t).sin() * 0.25 } else { (TAU * 1000.0 * t).sin() * 0.125 }
    });
    let out = fx.root.join("out").join("tone.mp3");
    let report = export::export(&fx.project(), &fx.paths, &out, &Default::default()).unwrap();
    assert_eq!((report.format, report.kbps, report.frames, report.clipped_samples), (export::Format::Mp3, Some(192), frames, 0));
    // 192 kbps is 24,000 bytes a second, whatever is in it.
    assert!((report.bytes as f64 - 5.0 * 24_000.0).abs() < 3_000.0, "{}", report.bytes);
    let again = fx.root.join("again.mp3");
    export::export(&fx.project(), &fx.paths, &again, &Default::default()).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&again).unwrap(), "the same version gives the same bytes");

    // Read back as a project of its own, it is the same length to the frame and the same sound.
    let back = Paths::new(fx.root.join("back.scrubr"));
    edit::new_project(&back, &[out], &me(), false).unwrap();
    let project = project::open(&back).unwrap();
    let mut renderer = Renderer::new(&project, &back).unwrap();
    assert_eq!(renderer.total(), frames);
    let mut heard = Vec::new();
    renderer.read(0, frames as usize, &mut heard).unwrap();
    let level = |channel: usize, from: f64, to: f64| {
        let (a, b) = ((from * RATE as f64) as usize, (to * RATE as f64) as usize);
        let sum: f64 = heard[a * 2..b * 2].as_chunks::<2>().0.iter().map(|frame| (frame[channel] as f64).powi(2)).sum();
        10.0 * (sum / (b - a) as f64).max(1e-12).log10()
    };
    // A sine's RMS is 3 dB under its peak.
    assert!((level(0, 0.5, 2.5) + 15.0).abs() < 0.3 && (level(1, 0.5, 2.5) + 21.0).abs() < 0.3, "{} {}", level(0, 0.5, 2.5), level(1, 0.5, 2.5));
    assert!(level(0, 3.2, 3.8) < -60.0 && level(1, 3.2, 3.8) < -60.0, "the silent second is silent: {} {}", level(0, 3.2, 3.8), level(1, 3.2, 3.8));

    // A bitrate can be asked for, if MP3 has it at this rate. A bit depth cannot.
    let small = export::export(&fx.project(), &fx.paths, &fx.root.join("small.mp3"), &export::Options { kbps: Some(96), ..Default::default() }).unwrap();
    assert!(small.kbps == Some(96) && small.bytes < report.bytes * 6 / 10);
    let options = |kbps, depth| export::Options { kbps, depth, ..Default::default() };
    let e = export::export(&fx.project(), &fx.paths, &fx.root.join("x.mp3"), &options(Some(100), None)).unwrap_err();
    assert!(e.code() == "bad_bitrate" && e.to_string().contains("100 kbps"));
    assert_eq!(export::export(&fx.project(), &fx.paths, &fx.root.join("x.mp3"), &options(None, Some(Depth::Int(16)))).unwrap_err().code(), "bad_depth");
    assert_eq!(export::export(&fx.project(), &fx.paths, &fx.root.join("x.flac"), &options(Some(128), None)).unwrap_err().code(), "bad_bitrate");
    assert!(!fx.root.join("x.mp3").exists() && !fx.root.join("x.mp3.part").exists());
    assert_eq!((export::default_kbps(22_050, 1), export::default_kbps(48_000, 1), export::mp3_bitrates(96_000).len()), (64, 128, 0));

    // Louder than full scale is clamped and counted, as it is for WAV and FLAC.
    fx.apply(vec![Request::Gain { db: 14.0, range: None }]);
    assert!(export::export(&fx.project(), &fx.paths, &fx.root.join("hot.mp3"), &Default::default()).unwrap().clipped_samples > 10_000);
}

// ---- normalize -------------------------------------------------------------------------------

#[test]
fn normalize_reaches_its_target_when_there_is_headroom() {
    // Steady noise: loudness sits well under its peaks' ceiling.
    let fx = Fixture::with(RATE as u64 * 4, 2, |_, _, noise| noise.next() * 0.05);
    let applied = fx.apply(vec![Request::Normalize { target: Target::Lufs(-23.0), ceiling_dbtp: -1.0 }]);
    let report = applied.resolved[0].normalize.unwrap();
    assert!(!report.limited);
    assert!((report.achieved_lufs().unwrap() + 23.0).abs() < 0.006);
    // The stored gain is the resolved one, and measuring the result agrees with the reply.
    match &fx.project().edits[1].op {
        Op::Normalize { lufs, gain_db, ceiling_dbtp, .. } => assert_eq!((*lufs, *gain_db, *ceiling_dbtp), (Some(-23.0), report.gain_db, -1.0)),
        other => panic!("{other:?}"),
    }
    let after = fx.measure(&Measure { loudness: true, ..Default::default() }).loudness.unwrap();
    assert!((after.integrated_lufs.unwrap() + 23.0).abs() < 0.02, "{after:?}");
    assert!(after.true_peak_dbtp <= -1.0);
}

#[test]
fn normalize_stops_at_the_true_peak_ceiling_and_says_so() {
    // Quiet noise with a loud click every half second: peaky, like a plosive-heavy voice.
    let fx = Fixture::with(RATE as u64 * 4, 2, |f, _, noise| {
        let click = if f % 24_000 < 12 { 0.7 * if f % 2 == 0 { 1.0 } else { -1.0 } } else { 0.0 };
        noise.next() * 0.02 + click
    });
    let before = fx.measure(&Measure { loudness: true, ..Default::default() }).loudness.unwrap();
    let applied = fx.apply(vec![Request::Normalize { target: Target::Lufs(-16.0), ceiling_dbtp: -1.0 }]);
    let report = applied.resolved[0].normalize.unwrap();
    assert!(report.limited, "the target needs more gain than the peaks allow: {report:?}");
    assert!(report.gain_db < -16.0 - before.integrated_lufs.unwrap());
    assert!(report.achieved_lufs().unwrap() < -16.0);
    assert!(report.achieved_true_peak_dbtp() <= -1.0 && report.achieved_true_peak_dbtp() > -1.011);
    // The rendered result really does sit at the ceiling, not over it.
    let after = fx.measure(&Measure { loudness: true, ..Default::default() }).loudness.unwrap();
    assert!(after.true_peak_dbtp <= -1.0 + 1e-6 && after.true_peak_dbtp > -1.02, "{after:?}");
    assert!((after.integrated_lufs.unwrap() - report.achieved_lufs().unwrap()).abs() < 0.02);

    // A lower ceiling stops sooner. Peak targets obey it too.
    edit::undo(&fx.paths, &WriteOpts::default(), 1).unwrap();
    let applied = fx.apply(vec![Request::Normalize { target: Target::Peak(-0.1), ceiling_dbtp: -3.0 }]);
    let report = applied.resolved[0].normalize.unwrap();
    assert!(report.limited && report.achieved_true_peak_dbtp() <= -3.0);
}

#[test]
fn normalize_in_a_batch_measures_the_audio_the_batch_leaves() {
    // Loud first second, quiet after. Cutting the loud part in the same batch changes the gain.
    let make = || Fixture::with(RATE as u64 * 3, 1, |f, _, noise| noise.next() * if f < RATE as u64 { 0.5 } else { 0.01 });
    let normalize = Request::Normalize { target: Target::Lufs(-30.0), ceiling_dbtp: -1.0 };
    let alone = make().apply(vec![normalize.clone()]).resolved[0].normalize.unwrap();
    let batched = make().batch(vec![cut(0, RATE as u64, 0), normalize]).resolved[1].normalize.unwrap();
    assert!(batched.gain_db > alone.gain_db + 20.0, "{} vs {}", batched.gain_db, alone.gain_db);
    assert!(!batched.limited);
}

// ---- analysis --------------------------------------------------------------------------------

#[test]
fn a_reference_tone_measures_at_its_level() {
    // EBU Tech 3341 case 1: a 1 kHz stereo sine at -23 dBFS reads -23.0 LUFS.
    let amp = 10f32.powf(-23.0 / 20.0);
    let fx = Fixture::with(RATE as u64 * 5, 2, |f, _, _| (TAU * 1000.0 * f as f32 / RATE as f32).sin() * amp);
    let report = fx.measure(&Measure { loudness: true, peaks: true, ..Default::default() });
    let loudness = report.loudness.unwrap();
    assert!((loudness.integrated_lufs.unwrap() + 23.0).abs() < 0.1, "{loudness:?}");
    assert!((loudness.true_peak_dbtp + 23.0).abs() < 0.1);
    let peaks = report.peaks.unwrap();
    assert!(peaks.peak_db.iter().all(|p| (p + 23.0).abs() < 0.01));
    assert_eq!((peaks.clipped_samples, peaks.clipped_range_count), (0, 0));
}

#[test]
fn silence_envelope_and_clipping_are_found_where_they_are() {
    // 0.0-1.0 s tone, 1.0-2.5 s near silence, 2.5-3.0 s tone, with a clipped burst at 0.5 s.
    let fx = Fixture::with(RATE as u64 * 3, 2, |f, _, noise| {
        let t = f as f32 / RATE as f32;
        if (24_000..24_010).contains(&f) {
            1.0
        } else if (1.0..2.5).contains(&t) {
            noise.next() * 0.0005
        } else {
            (TAU * 220.0 * t).cos() * 0.25
        }
    });
    let options = Measure { peaks: true, silence: Some((-45.0, RATE as u64)), envelope: Some(RATE as u64 / 2), ..Default::default() };
    let report = fx.measure(&options);
    assert_eq!(report.silence.unwrap(), vec![(48_000, 120_000)]);
    let envelope = report.envelope.unwrap();
    assert_eq!(envelope.len(), 6);
    assert!((envelope[1] + 15.05).abs() < 0.1, "a sine at 0.25 has an RMS of -15.05 dBFS: {envelope:?}");
    assert!(envelope[2] < -65.0 && envelope[4] < -65.0 && envelope[5] > -16.0);
    let peaks = report.peaks.unwrap();
    assert_eq!(peaks.clipped_samples, 20);
    assert_eq!(peaks.clipped_ranges, vec![(24_000, 24_010)]);
    assert!(peaks.peak_db.iter().all(|p| *p > -0.001));
    // A longer minimum finds nothing; a range limits what is measured.
    let longer = fx.measure(&Measure { silence: Some((-45.0, RATE as u64 * 2)), ..Default::default() });
    assert_eq!(longer.silence.unwrap(), vec![]);
    let project = fx.project();
    let mut renderer = Renderer::new(&project, &fx.paths).unwrap();
    let part = analysis::analyze(&mut renderer, 60_000, 100_000, &Measure { silence: Some((-45.0, 100)), ..Default::default() }, RATE).unwrap();
    assert_eq!(part.silence.unwrap(), vec![(60_000, 100_000)]);
}

#[test]
fn the_quietest_stretch_is_the_pause_whatever_the_level() {
    // Tone, then 1.0-2.5 s of faint noise, then tone: once at a normal level and once 40 dB down,
    // where the "speech" itself is under any usual silence threshold.
    for scale in [1.0f32, 0.01] {
        let fx = Fixture::with(RATE as u64 * 3, 2, move |f, _, noise| {
            let t = f as f32 / RATE as f32;
            scale * if (1.0..2.5).contains(&t) { noise.next() * 0.0005 } else { (TAU * 220.0 * t).cos() * 0.25 }
        });
        let project = fx.project();
        let mut renderer = Renderer::new(&project, &fx.paths).unwrap();
        let quiet = analysis::quietest(&mut renderer, RATE as u64 / 2, RATE as u64 * 3, RATE).unwrap();
        let near = |frame: u64, seconds: f64| (frame as f64 / RATE as f64 - seconds).abs() < 0.011;
        assert!(near(quiet.start, 1.0) && near(quiet.end, 2.5) && near(quiet.at(), 1.75), "scale {scale}: {quiet:?}");
        assert!(quiet.rms_db < -65.0 + 20.0 * (scale as f64).log10(), "{quiet:?}");
        // A range inside the tone has no pause: the answer still lies inside the range.
        let none = analysis::quietest(&mut renderer, 0, RATE as u64 / 2, RATE).unwrap();
        assert!(none.start < none.end && none.end <= RATE as u64 / 2, "{none:?}");
        // A range shorter than the 0.1 s looked for comes back whole.
        let short = analysis::quietest(&mut renderer, 1000, 1100, RATE).unwrap();
        assert_eq!((short.start, short.end, short.at()), (1000, 1100, 1050));
    }
}

// ---- inserts and other formats ---------------------------------------------------------------

#[test]
fn an_insert_at_another_rate_is_resampled_to_the_project() {
    let fx = Fixture::with(48_000, 2, |_, _, _| 0.0);
    // One second of a 441 Hz mono tone at 44.1 kHz.
    write_wav(&fx.root.join("sting.wav"), 44_100, 1, 16, 44_100, |f, _| (TAU * 441.0 * f as f32 / 44_100.0).sin() * 0.5);
    let applied = fx.apply(vec![Request::Insert { file: fx.root.join("sting.wav"), at: 24_000, crossfade: 0 }]);
    assert_eq!(applied.resolved[0].frames, 48_000, "one second is one second at the project's rate");
    assert_eq!(applied.frames_after, 96_000);
    let project = fx.project();
    let source = &project.sources[1];
    assert!(source.conformed && source.sample_rate == 44_100 && source.channels == 1);
    assert_eq!(project.edits[1].op, Op::Insert { source: "src2".into(), at: Some(("src1".into(), 24_000)), crossfade: 0 });

    let out = fx.render(4096);
    let left: Vec<f32> = out.as_chunks::<2>().0.iter().map(|f| f[0]).collect();
    assert!(out.as_chunks::<2>().0.iter().all(|f| f[0] == f[1]), "mono fans out to both channels");
    assert!(left[..24_000].iter().chain(&left[72_000..]).all(|&s| s == 0.0));
    // Still a 441 Hz tone at half scale: 441 rising zero crossings in its second.
    let tone = &left[24_000..72_000];
    let crossings = tone.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
    assert!((440..=442).contains(&crossings), "{crossings} crossings");
    let peak = tone[4_800..43_200].iter().fold(0f32, |a, &s| a.max(s.abs()));
    assert!((peak - 0.5).abs() < 0.01, "peak {peak}");

    // The converted copy is a cache: delete it and it is rebuilt to the same audio.
    fs::remove_dir_all(&fx.paths.cache).unwrap();
    assert!(fx.render(4096) == out);

    // Undoing the insert and making another edit discards it, its source and its cache files.
    edit::undo(&fx.paths, &WriteOpts::default(), 1).unwrap();
    fx.apply(vec![cut(0, 100, 0)]);
    assert_eq!(fx.project().sources.len(), 1);
    assert!(!fx.paths.cache.join("src2.wav").exists());
}

#[test]
fn a_dry_run_insert_leaves_no_cache_files_behind() {
    let fx = Fixture::noise(10_000);
    write_wav(&fx.root.join("sting.wav"), 44_100, 1, 16, 4_410, |_, _| 0.1);
    let dry = WriteOpts { dry_run: true, ..Default::default() };
    let file = fx.root.join("sting.wav");
    let applied = edit::apply(&fx.paths, &dry, &me(), false, |_, _| Ok((vec![Request::Insert { file, at: 0, crossfade: 0 }], None))).unwrap().value;
    assert_eq!(applied.frames_after, 10_000 + 4_800);
    assert!(!fx.paths.cache.join("src2.wav").exists() && !fx.paths.cache.join("src2.peaks").exists());
    assert_eq!(fx.project().sources.len(), 1);
}

#[test]
fn a_project_from_two_files_plays_them_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.wav"), dir.path().join("sub").join("b.wav"));
    fs::create_dir_all(b.parent().unwrap()).unwrap();
    write_wav(&a, RATE, 2, 16, 1_000, |_, c| if c == 0 { 0.25 } else { -0.25 });
    write_wav(&b, RATE, 2, 16, 500, |_, _| 0.5);
    let paths = Paths::new(dir.path().join("two.scrubr"));
    let project = edit::new_project(&paths, &[a.clone(), b], &me(), false).unwrap();
    assert_eq!(project.sources.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(), ["a.wav", "sub/b.wav"]);
    assert!(project.sources[0].hash.starts_with("blake3:") && project.sources[0].hash.len() == 7 + 64);
    assert_eq!(Timeline::build(&project).unwrap().total(), 1_500);
    assert!(paths.cache.join("src1.peaks").exists() && paths.cache.join("src2.peaks").exists());
    // A second `new` on the same path is refused unless forced.
    let err = edit::new_project(&paths, std::slice::from_ref(&a), &me(), false).unwrap_err();
    assert_eq!(err.code(), "project_exists");
    assert_eq!(edit::new_project(&paths, &[a], &me(), true).unwrap().sources.len(), 1);
    let err = edit::new_project(&Paths::new(dir.path().join("x.scrubr")), &[dir.path().join("missing.wav")], &me(), false).unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

// ---- images ----------------------------------------------------------------------------------

#[test]
fn waveform_images_are_written_from_peaks_and_from_audio() {
    let fx = Fixture::with(RATE as u64 * 4, 2, |f, _, _| if f < RATE as u64 * 2 { (TAU * 100.0 * f as f32 / RATE as f32).sin() * 0.8 } else { 0.0 });
    fx.apply(vec![cut(40_000, 50_000, 240)]);
    let project = fx.project();
    let wide = fx.root.join("wide.png");
    let report = image::render_png(&project, &fx.paths, &wide, &image::Options { width: 400, height: 200, ..Default::default() }).unwrap();
    assert!(report.from_peaks && report.end == 182_000);
    let near = fx.root.join("near.png");
    let report = image::render_png(&project, &fx.paths, &near, &image::Options { width: 400, height: 200, range: Some((39_000, 41_000)), track: None }).unwrap();
    assert!(!report.from_peaks && (report.frames_per_pixel - 5.0).abs() < 1e-9);
    for path in [&wide, &near] {
        let bytes = fs::read(path).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!((u32::from_be_bytes(bytes[16..20].try_into().unwrap()), u32::from_be_bytes(bytes[20..24].try_into().unwrap())), (400, 200));
    }
    // The loud half is dark in the lanes and the silent half is not.
    let pixmap = tiny_skia::Pixmap::load_png(&wide).unwrap();
    let dark = |x: u32| (30..110).filter(|&y| pixmap.pixel(x, y).unwrap().red() < 0x40).count();
    assert!(dark(100) > 40 && dark(350) <= 1, "{} and {}", dark(100), dark(350));
    let err = image::render_png(&project, &fx.paths, &wide, &image::Options { width: 400, height: 200, range: Some((0, 999_999)), track: None }).unwrap_err();
    assert_eq!(err.code(), "range_out_of_bounds");
}

#[test]
fn a_file_can_be_listened_to_before_it_joins_a_project() {
    use scrubr_core::source::FileAudio;
    // The AAC fixture: read in small pieces from its start, it gives the audio an import gives.
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.m4a");
    let mut audio = FileAudio::open(&file).unwrap();
    assert_eq!((audio.sample_rate(), audio.channels(), audio.frames()), (48_000, 1, Some(49_024)));
    let (mut all, mut piece) = (Vec::new(), Vec::new());
    while audio.read(1000, &mut piece).unwrap() > 0 {
        assert!(piece.len() <= 1000);
        all.extend_from_slice(&piece);
    }
    assert_eq!(all.len(), 49_024);
    let body = &all[4_800..43_200];
    let crossings = body.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
    assert!((351..=353).contains(&crossings), "0.8 s of 440 Hz has 352 rising crossings, not {crossings}");
    // Starting again gives the same audio.
    audio.rewind().unwrap();
    audio.read(1000, &mut piece).unwrap();
    assert_eq!(piece[..], all[..1000]);

    // A WAV is read in place, and a file that is not there says so.
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("tone.wav");
    write_wav(&wav, RATE, 2, 16, 4_800, |f, _| (f as f32 * 0.01).sin() * 0.25);
    let mut audio = FileAudio::open(&wav).unwrap();
    assert_eq!((audio.sample_rate(), audio.channels(), audio.frames()), (RATE, 2, Some(4_800)));
    assert_eq!(audio.read(10_000, &mut piece).unwrap(), 4_800);
    assert_eq!(audio.read(10_000, &mut piece).unwrap(), 0);
    assert_eq!(FileAudio::open(&dir.path().join("nope.wav")).err().unwrap().exit_code(), 2);
}

#[test]
fn an_m4a_file_imports_as_a_source() {
    // One second of a 440 Hz tone at an eighth of full scale, encoded as AAC in an M4A file.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tone.m4a");
    fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.m4a"), &file).unwrap();
    let paths = Paths::new(dir.path().join("aac.scrubr"));
    let project = edit::new_project(&paths, &[file], &me(), false).unwrap();
    let source = &project.sources[0];
    assert!(source.conformed);
    assert_eq!((source.sample_rate, source.channels), (48_000, 1), "the file does not state its channels; the audio does");
    // The second of audio, with the 1024 frames an AAC encoder puts in front of it.
    assert_eq!(source.frames, 49_024);

    let mut renderer = Renderer::new(&project, &paths).unwrap();
    let mut audio = Vec::new();
    renderer.read(0, 49_024, &mut audio).unwrap();
    let body = &audio[4_800..43_200];
    let crossings = body.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
    assert!((351..=353).contains(&crossings), "0.8 s of 440 Hz has 352 rising crossings, not {crossings}");
    let peak = body.iter().fold(0f32, |m, &s| m.max(s.abs()));
    assert!((peak - 0.125).abs() < 0.02, "peak {peak}");
}

// ---- transcription pipeline, with a stand-in for the speech model -----------------------------

/// Hears one word, a second into every piece of audio it is given, and notes each piece's length.
#[derive(Default)]
struct OneWordPerPiece {
    pieces: Vec<usize>,
}

impl scrubr_core::transcribe::Engine for OneWordPerPiece {
    fn name(&self) -> String {
        "stand-in".into()
    }

    fn transcribe(&mut self, audio: &[f32]) -> scrubr_core::Result<Vec<scrubr_core::transcribe::Heard>> {
        self.pieces.push(audio.len());
        Ok(vec![scrubr_core::transcribe::Heard { text: format!(" word{} ", self.pieces.len()), start: 1.0, end: 1.5, probability: 0.9 }])
    }
}

#[test]
fn transcript_words_come_back_as_timeline_frames() {
    use scrubr_core::transcribe::{Word, transcribe};
    // A 48 kHz stereo project. The engine is given 16 kHz mono, and its seconds become frames.
    let fx = Fixture::noise(RATE as u64 * 6);
    let mut engine = OneWordPerPiece::default();
    let mut progress = Vec::new();
    let words = transcribe(&fx.project(), &fx.paths, Some((96_000, 240_000)), &mut engine, &mut |done, total| progress.push((done, total))).unwrap();
    assert_eq!(engine.pieces, vec![48_000], "three seconds of timeline is 48,000 samples at 16 kHz");
    assert_eq!(words, vec![Word { text: "word1".into(), start: 96_000 + 48_000, end: 96_000 + 72_000, probability: 0.9 }]);
    assert_eq!(progress, vec![(144_000, 144_000)]);

    // The transcript is of the edited timeline: after a cut, there is less to hear.
    fx.apply(vec![cut(0, 48_000, 0)]);
    let mut engine = OneWordPerPiece::default();
    let words = transcribe(&fx.project(), &fx.paths, None, &mut engine, &mut |_, _| {}).unwrap();
    assert_eq!(engine.pieces, vec![80_000]);
    assert_eq!((words[0].start, words[0].end), (48_000, 72_000));

    let err = transcribe(&fx.project(), &fx.paths, Some((0, 999_999)), &mut engine, &mut |_, _| {}).unwrap_err();
    assert_eq!(err.code(), "range_out_of_bounds");
}

#[test]
fn long_audio_reaches_the_engine_in_pieces_split_at_a_pause() {
    use scrubr_core::transcribe::transcribe;
    // Six minutes at 16 kHz mono: a tone, silent only from 4:50 to 4:51.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("long.wav");
    write_wav(&file, 16_000, 1, 16, 16_000 * 360, |f, _| if (16_000 * 290..16_000 * 291).contains(&f) { 0.0 } else { (f as f32 * 0.05).sin() * 0.3 });
    let paths = Paths::new(dir.path().join("long.scrubr"));
    let project = edit::new_project(&paths, &[file], &me(), false).unwrap();
    let mut engine = OneWordPerPiece::default();
    let words = transcribe(&project, &paths, None, &mut engine, &mut |_, _| {}).unwrap();
    // No piece is longer than five minutes, and the first ends inside the pause.
    assert_eq!(engine.pieces.len(), 2);
    let first = engine.pieces[0];
    assert!((16_000 * 290 + 1_600..16_000 * 291 - 1_600).contains(&first), "first piece ends at {:.2} s", first as f64 / 16_000.0);
    assert_eq!(engine.pieces[1], 16_000 * 360 - first);
    // The second piece's word is placed after the first piece, not at the start of the timeline.
    assert_eq!(words.len(), 2);
    assert_eq!(words[1].start, first as u64 + 16_000);
}

#[test]
fn silence_is_not_sent_to_the_engine() {
    use scrubr_core::transcribe::transcribe;
    let fx = Fixture::with(RATE as u64 * 2, 1, |_, _, _| 0.0);
    let mut engine = OneWordPerPiece::default();
    let words = transcribe(&fx.project(), &fx.paths, None, &mut engine, &mut |_, _| {}).unwrap();
    assert!(engine.pieces.is_empty() && words.is_empty());
}

// ---- tracks ----------------------------------------------------------------------------------

fn on(track: &str, request: Request) -> Request {
    Request::OnTrack { track: track.into(), request: Box::new(request) }
}

fn add_track(fx: &Fixture, file: &str, name: &str, at: u64) -> edit::TrackAdded {
    edit::track_add(&fx.paths, &WriteOpts::default(), &me(), &fx.root.join(file), Some(name), None, at).unwrap().value
}

fn set_track(fx: &Fixture, track: &str, change: edit::TrackChange) {
    edit::track_set(&fx.paths, &WriteOpts::default(), &me(), track, &change).unwrap();
}

/// True when every sample of `audio` is within a 24-bit step or two of `value`.
fn all_near(audio: &[f32], value: f32) -> bool {
    audio.iter().all(|s| (s - value).abs() < 3e-5)
}

#[test]
fn a_second_track_is_summed_with_the_first_and_moves_nothing() {
    // The main track is a steady 0.25 on both sides; the guest is 0.125 for 10,000 frames.
    let fx = Fixture::with(48_000, 2, |_, _, _| 0.25);
    write_wav(&fx.root.join("guest.wav"), RATE, 2, 24, 10_000, |_, _| 0.125);
    assert_eq!(fx.project().format, project::FORMAT);
    let added = add_track(&fx, "guest.wav", "guest", 5_000);
    assert_eq!((added.track.id.as_str(), added.track.name.as_str(), added.created), ("t2", "guest", true));
    assert_eq!((added.at, added.frames, added.extended, added.frames_before, added.frames_after), (5_000, 10_000, 0, 48_000, 48_000));
    // A project that uses tracks says so in its format, and reads back the same.
    let project = fx.project();
    assert_eq!(project.format, project::FORMAT_TRACKS);
    assert_eq!(Project::parse(&project.to_json(), Path::new("p.scrubr")).unwrap(), project);

    let audio = fx.render(4096);
    assert_eq!(audio.len(), 96_000);
    assert!(all_near(&audio[..10_000], 0.25) && all_near(&audio[10_000..30_000], 0.375) && all_near(&audio[30_000..], 0.25));
    // The same samples however the reads are cut up.
    assert_eq!(fx.render(977), audio);

    // Level: half as loud. Pan: full left leaves nothing of the guest on the right.
    set_track(&fx, "guest", edit::TrackChange { gain_db: Some(-6.0206), ..Default::default() });
    assert!(all_near(&fx.render(4096)[10_000..30_000], 0.3125));
    set_track(&fx, "t2", edit::TrackChange { pan: Some(-1.0), ..Default::default() });
    let audio = fx.render(4096);
    assert!(audio[10_000..30_000].as_chunks::<2>().0.iter().all(|f| (f[0] - 0.3125).abs() < 3e-5 && (f[1] - 0.25).abs() < 3e-5));
    // Solo leaves the guest alone; mute takes it out.
    set_track(&fx, "guest", edit::TrackChange { solo: Some(true), ..Default::default() });
    let audio = fx.render(4096);
    assert!(all_near(&audio[..10_000], 0.0) && audio[10_000..30_000].as_chunks::<2>().0.iter().all(|f| (f[0] - 0.0625).abs() < 3e-5 && f[1] == 0.0));
    set_track(&fx, "guest", edit::TrackChange { solo: Some(false), mute: Some(true), ..Default::default() });
    assert!(all_near(&fx.render(4096), 0.25));
    set_track(&fx, "guest", edit::TrackChange { mute: Some(false), pan: Some(0.0), gain_db: Some(0.0), ..Default::default() });

    // A level edit aimed at the guest changes the guest alone; one aimed at nothing changes both.
    fx.apply(vec![on("guest", Request::Gain { db: -6.0206, range: Some((5_000, 10_000)) })]);
    let audio = fx.render(4096);
    assert!(all_near(&audio[10_000..20_000], 0.3125) && all_near(&audio[20_000..30_000], 0.375) && all_near(&audio[..10_000], 0.25));
    fx.apply(vec![Request::Gain { db: -6.0206, range: Some((10_000, 12_000)) }]);
    let audio = fx.render(4096);
    assert!(all_near(&audio[20_000..24_000], 0.1875) && all_near(&audio[24_000..30_000], 0.375));
}

#[test]
fn tracks_stay_in_step_through_cuts_and_reverts() {
    // The main track counts up and the guest counts down, so wherever both are heard and in step
    // their sum is one fixed number. Any drift between them would change it.
    let fx = Fixture::with(48_000, 1, |f, _, _| f as f32 / 100_000.0);
    write_wav(&fx.root.join("guest.wav"), RATE, 1, 24, 20_000, |g, _| -(g as f32) / 100_000.0);
    // A cut made before the guest exists, then the guest laid at 10,000: source frame 12,000.
    let early = fx.apply(vec![cut(2_000, 4_000, 0)]).edit;
    add_track(&fx, "guest.wav", "guest", 10_000);
    let audio = fx.render(4096);
    assert_eq!(audio.len(), 46_000);
    assert!(all_near(&audio[10_000..30_000], 0.12), "the guest sits on main frames 12,000 to 32,000");
    assert!((audio[9_999] - 0.11999).abs() < 3e-5 && (audio[30_000] - 0.32).abs() < 3e-5);

    // Taking the early cut back gives the main track 2,000 frames more before the guest. The
    // guest moves with the audio it was laid on.
    edit::revert(&fx.paths, &WriteOpts::default(), &me(), &Revert::Edits(vec![early])).unwrap();
    let audio = fx.render(4096);
    assert_eq!(audio.len(), 48_000);
    assert!(all_near(&audio[12_000..32_000], 0.12) && (audio[11_999] - 0.11999).abs() < 3e-5);

    // A cut across the guest's start removes the same moment from both.
    fx.apply(vec![cut(11_000, 13_000, 0)]);
    let audio = fx.render(4096);
    assert_eq!(audio.len(), 46_000);
    assert!(all_near(&audio[11_000..30_000], 0.12) && (audio[10_999] - 0.10999).abs() < 3e-5 && (audio[30_000] - 0.32).abs() < 3e-5);
    // A cut with a crossfade fades every track across the join together, so the sum holds.
    fx.apply(vec![cut(20_000, 21_000, 240)]);
    assert!(all_near(&fx.render(4096)[11_000..29_000], 0.12));
    // Where the guest is now, as the timeline reports it.
    let tl = Timeline::build(&fx.project()).unwrap();
    let guest = tl.find_track("guest").unwrap();
    assert_eq!(tl.clips(guest).into_iter().map(|(_, ranges)| ranges).collect::<Vec<_>>(), vec![vec![(11_000, 29_000)]]);
    // Undo, twice, brings the audio back as it was.
    edit::undo(&fx.paths, &WriteOpts::default(), 2).unwrap();
    assert!(all_near(&fx.render(4096)[12_000..32_000], 0.12));
}

#[test]
fn a_file_past_the_end_and_an_insert_on_a_track_make_time_for_every_track() {
    let fx = Fixture::with(10_000, 1, |_, _, _| 0.25);
    write_wav(&fx.root.join("a.wav"), RATE, 1, 24, 2_000, |_, _| 0.125);
    write_wav(&fx.root.join("b.wav"), RATE, 1, 24, 3_000, |_, _| 0.0625);
    // A file laid over the last 1,000 frames runs 1,000 past the end: the timeline grows.
    let added = add_track(&fx, "a.wav", "bed", 9_000);
    assert_eq!((added.extended, added.frames_after), (1_000, 11_000));
    let audio = fx.render(4096);
    assert!(all_near(&audio[..9_000], 0.25) && all_near(&audio[9_000..10_000], 0.375) && all_near(&audio[10_000..], 0.125));

    // An insert on the bed track is new time: silence on the main track, the file on the bed.
    let applied = fx.apply(vec![on("bed", Request::Insert { file: fx.root.join("b.wav"), at: 5_000, crossfade: 0 })]);
    assert_eq!((applied.frames_before, applied.frames_after), (11_000, 14_000));
    let audio = fx.render(4096);
    assert!(all_near(&audio[..5_000], 0.25) && all_near(&audio[5_000..8_000], 0.0625) && all_near(&audio[8_000..12_000], 0.25));
    assert!(all_near(&audio[12_000..13_000], 0.375) && all_near(&audio[13_000..], 0.125));
    // The bed alone, and the main track alone.
    let project = fx.project();
    let tl = Timeline::build(&project).unwrap();
    let bed = tl.find_track("bed").unwrap();
    let mut alone = Vec::new();
    Renderer::for_track(&project, &fx.paths, tl.clone(), bed).unwrap().read(0, 14_000, &mut alone).unwrap();
    assert!(all_near(&alone[..5_000], 0.0) && all_near(&alone[5_000..8_000], 0.0625) && all_near(&alone[8_000..12_000], 0.0) && all_near(&alone[12_000..], 0.125));
    Renderer::for_track(&project, &fx.paths, tl, 0).unwrap().read(0, 14_000, &mut alone).unwrap();
    assert!(all_near(&alone[..5_000], 0.25) && all_near(&alone[5_000..8_000], 0.0) && all_near(&alone[13_000..], 0.0));

    // Normalize aimed at the bed measures the bed alone and changes the bed alone.
    let applied = fx.apply(vec![on("bed", Request::Normalize { target: Target::Peak(-12.0412), ceiling_dbtp: 0.0 })]);
    let report = applied.resolved[0].normalize.unwrap();
    assert!((report.measured_peak_db + 18.0618).abs() < 0.01 && (report.gain_db - 6.02).abs() < 0.011, "{report:?}");
    let audio = fx.render(4096);
    assert!(all_near(&audio[..5_000], 0.25) && audio[13_000..].iter().all(|s| (s - 0.25).abs() < 1e-3));

    // Removing the track takes its audio out and leaves time alone; undo puts it back.
    edit::track_remove(&fx.paths, &WriteOpts::default(), &me(), "bed").unwrap();
    let audio = fx.render(4096);
    assert_eq!(audio.len(), 14_000);
    assert!(all_near(&audio[5_000..8_000], 0.0) && all_near(&audio[8_000..13_000], 0.25) && all_near(&audio[13_000..], 0.0));
    assert!(Timeline::build(&fx.project()).unwrap().find_track("bed").is_none());
    edit::undo(&fx.paths, &WriteOpts::default(), 1).unwrap();
    assert!(Timeline::build(&fx.project()).unwrap().find_track("bed").is_some());
}

#[test]
fn track_mistakes_are_refused_with_a_reason() {
    let fx = Fixture::with(10_000, 1, |_, _, _| 0.25);
    write_wav(&fx.root.join("a.wav"), RATE, 1, 24, 2_000, |_, _| 0.125);
    add_track(&fx, "a.wav", "bed", 0);
    let opts = WriteOpts::default();
    let code = |e: scrubr_core::Error| (e.exit_code(), e.code().to_string());
    let request = |r: Request| edit::apply(&fx.paths, &opts, &me(), false, |_, _| Ok((vec![r], None))).map(|_| ());
    assert_eq!(code(request(on("nope", Request::Gain { db: 1.0, range: None })).unwrap_err()), (1, "unknown_track".into()));
    assert_eq!(code(request(on("bed", cut(0, 100, 0))).unwrap_err()), (1, "track_not_allowed".into()));
    assert_eq!(code(edit::track_add(&fx.paths, &opts, &me(), &fx.root.join("a.wav"), Some("bed"), None, 0).unwrap_err()), (1, "track_exists".into()));
    assert_eq!(code(edit::track_add(&fx.paths, &opts, &me(), &fx.root.join("a.wav"), None, Some("main"), 0).unwrap_err()), (1, "main_track".into()));
    assert_eq!(code(edit::track_add(&fx.paths, &opts, &me(), &fx.root.join("a.wav"), Some("late"), None, 10_001).unwrap_err()), (1, "range_out_of_bounds".into()));
    assert_eq!(code(edit::track_remove(&fx.paths, &opts, &me(), "t1").unwrap_err()), (1, "main_track".into()));
    assert_eq!(code(edit::track_set(&fx.paths, &opts, &me(), "bed", &edit::TrackChange { pan: Some(2.0), ..Default::default() }).unwrap_err()), (1, "bad_pan".into()));
    assert_eq!(code(edit::track_set(&fx.paths, &opts, &me(), "bed", &edit::TrackChange::default()).unwrap_err()), (1, "no_operations".into()));
    assert_eq!(code(edit::track_set(&fx.paths, &opts, &me(), "bed", &edit::TrackChange { name: Some("main".into()), ..Default::default() }).unwrap_err()), (1, "track_exists".into()));
    // None of those wrote anything, and a second file can join the track that is there.
    assert_eq!(fx.project().version, 2);
    let added = edit::track_add(&fx.paths, &opts, &me(), &fx.root.join("a.wav"), None, Some("bed"), 4_000).unwrap().value;
    assert!(!added.created && added.track.id == "t2");
    let audio = fx.render(4096);
    assert!(all_near(&audio[..2_000], 0.375) && all_near(&audio[2_000..4_000], 0.25) && all_near(&audio[4_000..6_000], 0.375));
    // A file from a newer scrubr is refused, not misread.
    let newer = fx.project().to_json().replacen("\"format\": 2", "\"format\": 3", 1);
    assert!(Project::parse(&newer, Path::new("p.scrubr")).unwrap_err().to_string().contains("format 3"));
}

// ---- cutting by words ------------------------------------------------------------------------

const SPOKEN: [&str; 6] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

/// The pitch a stand-in word is "spoken" at.
fn pitch(word: usize) -> f32 {
    400.0 + 300.0 * word as f32
}

/// A recording of "alpha bravo charlie delta echo bravo foxtrot": each word a burst of its own
/// pitch 0.35 s long, with 0.25 s of silence before the next. The first starts at 0.5 s.
fn spoken() -> Fixture {
    let said = [0usize, 1, 2, 3, 4, 1, 5];
    Fixture::with(5 * RATE as u64, 1, move |f, _, _| {
        let t = f as f32 / RATE as f32 - 0.5;
        let slot = (t / 0.6).floor();
        let into = t - slot * 0.6;
        if t < 0.0 || slot as usize >= said.len() || into >= 0.35 {
            return 0.0;
        }
        // A few milliseconds to rise and fall, as a voice does.
        let edge = (into.min(0.35 - into) / 0.005).min(1.0);
        (TAU * pitch(said[slot as usize]) * into).sin() * 0.4 * edge
    })
}

/// A stand-in for the speech model: it hears each burst as the word of its pitch. `late` is
/// added to every time it reports, as a real model's times are sometimes out.
struct Tones {
    late: f64,
}

impl transcribe::Engine for Tones {
    fn name(&self) -> String {
        "tones".into()
    }

    fn transcribe(&mut self, audio: &[f32]) -> scrubr_core::Result<Vec<Heard>> {
        let rate = transcribe::ENGINE_RATE as f64;
        let hop = 160;
        let loud: Vec<bool> = audio.chunks(hop).map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt() > 0.02).collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < loud.len() {
            if !loud[i] {
                i += 1;
                continue;
            }
            let from = i;
            while i < loud.len() && loud[i] {
                i += 1;
            }
            if i - from < 8 {
                continue;
            }
            let burst = &audio[from * hop..(i * hop).min(audio.len())];
            let crossings = burst.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count() as f32;
            let hz = crossings / 2.0 / (burst.len() as f32 / rate as f32);
            let word = (((hz - 400.0) / 300.0).round().max(0.0) as usize).min(SPOKEN.len() - 1);
            let (start, end) = ((from * hop) as f64 / rate, (i * hop) as f64 / rate);
            out.push(Heard { text: SPOKEN[word].to_string(), start: start + self.late, end: end + self.late, probability: 1.0 });
        }
        Ok(out)
    }
}

fn said(fx: &Fixture) -> String {
    let words = transcribe::transcribe(&fx.project(), &fx.paths, None, &mut Tones { late: 0.0 }, &mut |_, _| {}).unwrap();
    transcribe::text(&words)
}

fn cut_phrase(fx: &Fixture, phrase: &str, pick: Pick, late: f64, opts: &WriteOpts) -> scrubr_core::Result<project::Written<phrase::CutPhrase>> {
    phrase::cut(&fx.paths, opts, &me(), None, None, phrase, pick, 240, true, &mut Tones { late }, &mut |_, _| {})
}

#[test]
fn a_phrase_is_cut_in_the_pauses_either_side_and_checked() {
    let fx = spoken();
    assert_eq!(said(&fx), "alpha bravo charlie delta echo bravo foxtrot");
    let secs = |frames: u64| frames as f64 / RATE as f64;

    // Finding changes nothing. The cut sits in the silence before "charlie" and after "delta",
    // not on the times the words were heard at, and the same when those times are a little out.
    for late in [0.0, 0.08, -0.08] {
        let found = phrase::find(&fx.project(), &fx.paths, None, None, "Charlie, delta!", &mut Tones { late }, &mut |_, _| {}).unwrap();
        assert_eq!(found.len(), 1);
        let one = &found[0];
        let texts = |words: &[transcribe::Word]| words.iter().map(|w| w.text.clone()).collect::<Vec<_>>().join(" ");
        assert_eq!((texts(&one.before), texts(&one.words), texts(&one.after)), ("alpha bravo".into(), "charlie delta".into(), "echo bravo".into()));
        assert!((1.46..1.69).contains(&secs(one.cut.0)) && (2.66..2.89).contains(&secs(one.cut.1)), "cut {:.3}..{:.3} with times {late} out", secs(one.cut.0), secs(one.cut.1));
        assert!(one.quiet.0.rms_db < -100.0 && one.quiet.1.rms_db < -100.0);
    }
    assert_eq!(fx.project().edits.len(), 1);

    // A dry run listens and checks, and writes nothing.
    let dry = cut_phrase(&fx, "charlie delta", Pick::Only, 0.0, &WriteOpts { dry_run: true, ..Default::default() }).unwrap();
    assert!(!dry.wrote && dry.value.cuts[0].1.as_ref().unwrap().clean);
    assert_eq!(said(&fx), "alpha bravo charlie delta echo bravo foxtrot");

    let done = cut_phrase(&fx, "charlie delta", Pick::Only, 0.08, &WriteOpts::default()).unwrap();
    assert!(done.wrote);
    let (found, check) = &done.value.cuts[0];
    let check = check.as_ref().unwrap();
    assert!(check.clean && check.heard.contains("bravo echo") && check.join == found.cut.0);
    assert_eq!(said(&fx), "alpha bravo echo bravo foxtrot");
    let project = fx.project();
    assert_eq!(project.edits.len(), 2, "one edit");
    assert_eq!(Timeline::build(&project).unwrap().total(), 5 * RATE as u64 - (found.cut.1 - found.cut.0));
    assert!(project.edits[1].desc.as_deref().unwrap().starts_with("cut \"charlie delta\" "));
}

#[test]
fn a_phrase_said_more_than_once_has_to_be_picked() {
    let fx = spoken();
    let e = cut_phrase(&fx, "bravo", Pick::Only, 0.0, &WriteOpts::default()).unwrap_err();
    assert_eq!((e.kind(), e.code()), (Kind::Usage, "phrase_ambiguous"));
    assert!(e.to_string().contains("heard 2 times") && e.hint_text().unwrap().contains("--occurrence"));
    assert_eq!(cut_phrase(&fx, "bravo", Pick::Nth(3), 0.0, &WriteOpts::default()).unwrap_err().code(), "no_such_occurrence");
    assert_eq!(cut_phrase(&fx, "zulu", Pick::Only, 0.0, &WriteOpts::default()).unwrap_err().code(), "phrase_not_found");
    assert_eq!(cut_phrase(&fx, " ,. ", Pick::Only, 0.0, &WriteOpts::default()).unwrap_err().code(), "empty_phrase");
    assert_eq!(fx.project().edits.len(), 1, "nothing was written by any of those");

    // The second one alone.
    cut_phrase(&fx, "bravo", Pick::Nth(2), 0.0, &WriteOpts::default()).unwrap();
    assert_eq!(said(&fx), "alpha bravo charlie delta echo foxtrot");
    edit::undo(&fx.paths, &WriteOpts::default(), 1).unwrap();

    // Both, as one edit, each join checked where it lands after the cuts before it.
    let all = cut_phrase(&fx, "bravo", Pick::All, 0.0, &WriteOpts::default()).unwrap();
    assert_eq!(all.value.cuts.len(), 2);
    assert!(all.value.cuts.iter().all(|(_, check)| check.as_ref().unwrap().clean));
    assert_eq!(said(&fx), "alpha charlie delta echo foxtrot");
    assert_eq!(fx.project().edits.iter().filter(|e| matches!(e.op, Op::Batch { .. })).count(), 1);

    // A search kept to a range hears only what is in it.
    let found = phrase::find(&fx.project(), &fx.paths, None, Some((0, RATE as u64)), "charlie", &mut Tones { late: 0.0 }, &mut |_, _| {}).unwrap();
    assert!(found.is_empty());
}

#[test]
fn a_cut_that_would_take_the_wrong_word_is_not_made() {
    let fx = spoken();
    let version = fx.project().version;
    // A model whose times are a whole word late: "charlie" is reported where "delta" is. The cut
    // worked out from that would take "delta" and leave "charlie". Listening across the join
    // before writing catches it.
    let e = cut_phrase(&fx, "charlie", Pick::Only, 0.6, &WriteOpts::default()).unwrap_err();
    assert_eq!((e.kind(), e.code()), (Kind::Usage, "cut_not_clean"));
    assert!(e.to_string().contains("Nothing was cut") && e.hint_text().unwrap().contains("label"));
    let project = fx.project();
    assert_eq!((project.version, project.edits.len()), (version, 1));
    assert_eq!(said(&fx), "alpha bravo charlie delta echo bravo foxtrot");

    // A version the caller no longer holds is refused before anything is listened to.
    let e = cut_phrase(&fx, "charlie", Pick::Only, 0.0, &WriteOpts { if_version: Some(version + 5), ..Default::default() }).unwrap_err();
    assert_eq!(e.kind(), Kind::Conflict);
}

// ---- a transcript kept against the audio -----------------------------------------------------

fn listen(fx: &Fixture, range: Option<(u64, u64)>) -> Vec<transcribe::Word> {
    transcribe::transcribe(&fx.project(), &fx.paths, range, &mut Tones { late: 0.0 }, &mut |_, _| {}).unwrap()
}

/// Listens to a range of the timeline (all of it when `None`) and keeps what was heard.
fn listen_and_keep(fx: &Fixture, range: Option<(u64, u64)>) {
    let tl = Timeline::build(&fx.project()).unwrap();
    let heard = listen(fx, range);
    words::save(&fx.paths, &tl, None, range.unwrap_or((0, tl.total())), "tones", &heard).unwrap();
}

/// The kept words where they are now, without listening.
fn kept(fx: &Fixture) -> Vec<transcribe::Word> {
    let tl = Timeline::build(&fx.project()).unwrap();
    let store = words::load(&fx.paths);
    words::set_for(&store, &tl, None).map(|set| words::on_timeline(set, &tl)).unwrap_or_default()
}

/// True when two lists of words say the same and sit within 20 ms of each other.
fn same_words(a: &[transcribe::Word], b: &[transcribe::Word]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.text == y.text && x.start.abs_diff(y.start) <= 960 && x.end.abs_diff(y.end) <= 960)
}

#[test]
fn a_kept_transcript_follows_its_audio_through_edits() {
    let fx = spoken();
    assert!(kept(&fx).is_empty(), "nothing is kept until something has listened");
    listen_and_keep(&fx, None);
    let first = kept(&fx);
    assert_eq!(first, listen(&fx, None));
    assert_eq!(transcribe::text(&first), "alpha bravo charlie delta echo bravo foxtrot");

    // A phrase is cut: the words that remain are where a fresh listen finds them, and nothing
    // had to listen again.
    cut_phrase(&fx, "charlie delta", Pick::Only, 0.0, &WriteOpts::default()).unwrap();
    assert_eq!(transcribe::text(&kept(&fx)), "alpha bravo echo bravo foxtrot");
    assert!(same_words(&kept(&fx), &listen(&fx, None)));
    // Undo, and they are all back.
    edit::undo(&fx.paths, &WriteOpts::default(), 1).unwrap();
    assert_eq!(kept(&fx), first);

    // New audio in the middle moves the later words along. It has not been listened to, and the
    // store says so.
    write_wav(&fx.root.join("sting.wav"), RATE, 1, 24, RATE as u64 / 2, |f, _| if (2_400..21_600).contains(&f) { (TAU * pitch(5) * f as f32 / RATE as f32).sin() * 0.4 } else { 0.0 });
    fx.apply(vec![Request::Insert { file: fx.root.join("sting.wav"), at: RATE as u64, crossfade: 0 }]);
    let moved = kept(&fx);
    assert_eq!(transcribe::text(&moved), "alpha bravo charlie delta echo bravo foxtrot");
    assert_eq!((moved[0].start, moved[1].start), (first[0].start, first[1].start + RATE as u64 / 2));
    let covered = |fx: &Fixture| {
        let tl = Timeline::build(&fx.project()).unwrap();
        words::covered(words::set_for(&words::load(&fx.paths), &tl, None).unwrap(), &tl)
    };
    assert_eq!(covered(&fx), [(0, RATE as u64), (RATE as u64 * 3 / 2, RATE as u64 * 11 / 2)]);
    // Listening to that stretch alone fills it in, and doing it twice adds nothing twice.
    for _ in 0..2 {
        listen_and_keep(&fx, Some((RATE as u64, RATE as u64 * 3 / 2)));
        assert_eq!(transcribe::text(&kept(&fx)), "alpha foxtrot bravo charlie delta echo bravo foxtrot");
        assert_eq!(covered(&fx), [(0, RATE as u64 * 11 / 2)]);
    }
    assert!(same_words(&kept(&fx), &listen(&fx, None)));
}

#[test]
fn a_word_a_cut_runs_through_ends_at_the_cut() {
    let fx = spoken();
    listen_and_keep(&fx, None);
    // "bravo" is heard from 1.10 s to 1.45 s. Cut from 1.25 s to 1.40 s: it still starts where
    // it did and now ends at the cut. Cut its start as well, so that less than half of it is
    // left, and it is gone.
    let bravo = kept(&fx)[1].clone();
    assert_eq!((bravo.text.as_str(), bravo.start, bravo.end), ("bravo", 52_800, 69_600));
    fx.apply(vec![cut(60_000, 67_200, 0)]);
    let now = kept(&fx);
    assert_eq!((now[1].text.as_str(), now[1].start, now[1].end), ("bravo", 52_800, 60_000));
    assert_eq!(now.len(), 7);
    fx.apply(vec![cut(50_000, 56_000, 0)]);
    assert_eq!(transcribe::text(&kept(&fx)), "alpha charlie delta echo bravo foxtrot");

    // A model often starts a word early, in the pause before it. So a cut that takes a little
    // of a word's start leaves the word, beginning at the join.
    let fx = spoken();
    listen_and_keep(&fx, None);
    fx.apply(vec![cut(48_000, 55_200, 0)]);
    let now = kept(&fx);
    assert_eq!((now.len(), now[1].text.as_str(), now[1].start, now[1].end), (7, "bravo", 48_000, 62_400));
    let tl = Timeline::build(&fx.project()).unwrap();
    let store = words::load(&fx.paths);
    assert_eq!(words::broken(words::set_for(&store, &tl, None).unwrap(), &tl), [(48_000, "bravo".to_string())]);
    // Listening again hears it afresh, and the word kept from before is not there twice.
    listen_and_keep(&fx, None);
    assert_eq!(transcribe::text(&kept(&fx)), "alpha bravo charlie delta echo bravo foxtrot");

    // The mix and a track heard alone are kept apart.
    let tl = Timeline::build(&fx.project()).unwrap();
    words::save(&fx.paths, &tl, Some(0), (0, tl.total()), "tones", &listen(&fx, None)[..2]).unwrap();
    let store = words::load(&fx.paths);
    assert_eq!(words::on_timeline(words::set_for(&store, &tl, Some(0)).unwrap(), &tl).len(), 2);
    assert_eq!(words::on_timeline(words::set_for(&store, &tl, None).unwrap(), &tl).len(), 7);
    assert_eq!(store.sets.iter().map(|s| s.track.as_deref()).collect::<Vec<_>>(), [None, Some("t1")]);
}

// ---- pacing and checking a cut ---------------------------------------------------------------

/// Speech-like bursts over room tone: a burst every 0.6 s, 0.35 s long, with a wait of 1.2 s
/// after the third and 0.9 s after the fifth. `level` scales the whole recording.
fn paced(level: f32) -> Fixture {
    // Where each burst starts, in seconds.
    let starts = [0.5f32, 1.1, 1.7, 3.25, 3.85, 5.1, 5.7];
    Fixture::with(7 * RATE as u64, 1, move |f, _, noise| {
        let t = f as f32 / RATE as f32;
        let room = noise.next() * 0.002;
        match starts.iter().find(|&&s| t >= s && t < s + 0.35) {
            Some(&s) => ((TAU * 500.0 * (t - s)).sin() * 0.4 + room) * level,
            None => room * level,
        }
    })
}

fn levels_of(fx: &Fixture) -> analysis::Levels {
    let project = fx.project();
    let mut renderer = Renderer::new(&project, &fx.paths).unwrap();
    let total = renderer.total();
    analysis::levels(&mut renderer, 0, total, RATE).unwrap()
}

#[test]
fn pauses_are_found_against_the_recordings_own_quiet() {
    let secs = |r: (u64, u64)| (r.0 as f64 / RATE as f64, r.1 as f64 / RATE as f64);
    // The same recording at full level and 34 dB down, where its speech is quieter than the first
    // one's room: the same pauses are found in both, with no level given.
    let mut found = Vec::new();
    for level in [1.0f32, 0.02] {
        let fx = paced(level);
        let levels = levels_of(&fx);
        assert!(levels.quiet_db > levels.floor_db && levels.quiet_db < levels.speech_db - 10.0, "{levels:?}");
        let long: Vec<(f64, f64)> = levels.pauses(RATE as u64 / 2).into_iter().map(secs).collect();
        assert_eq!(long.len(), 4, "{long:?}");
        // The lead-in, the wait after the third burst, the one after the fifth, and the tail.
        assert!(long[0].0 == 0.0 && (long[0].1 - 0.5).abs() < 0.03, "{long:?}");
        assert!((long[1].0 - 2.05).abs() < 0.03 && (long[1].1 - 3.25).abs() < 0.03, "{long:?}");
        assert!((long[2].0 - 4.20).abs() < 0.03 && (long[2].1 - 5.10).abs() < 0.03, "{long:?}");
        assert!((long[3].0 - 6.05).abs() < 0.03 && long[3].1 == 7.0, "{long:?}");
        // The quarter-second gaps between bursts are pauses too, only shorter.
        assert_eq!(levels.pauses(RATE as u64 / 10).len(), 8);
        found.push((levels.floor_db, long));
    }
    assert!((found[0].0 - found[1].0 - 34.0).abs() < 1.0, "the floor moved with the level: {found:?}");
    assert_eq!(found[0].1, found[1].1);
}

#[test]
fn long_pauses_are_shortened_as_one_edit() {
    let fx = paced(1.0);
    let (over, to) = (RATE as u64 * 6 / 10, RATE as u64 * 3 / 10);
    let tighten = |opts: &WriteOpts, over: u64, to: u64| pacing::tighten(&fx.paths, opts, &me(), None, |_, _| Ok(Tighten { range: None, over, to, crossfade: 240 }));

    // A dry run says what would be cut and writes nothing.
    let dry = tighten(&WriteOpts { dry_run: true, ..Default::default() }, over, to).unwrap();
    assert!(!dry.wrote && dry.value.pauses.len() == 2 && fx.project().edits.len() == 1);

    let done = tighten(&WriteOpts::default(), over, to).unwrap().value;
    assert_eq!((done.pauses.len(), done.cuts.len()), (2, 2));
    let project = fx.project();
    assert_eq!(project.edits.len(), 2, "one edit");
    assert!(project.edits[1].desc.as_deref().unwrap().starts_with("tighten 2 pauses"));
    // Each of those waits is now three tenths of a second. The short gaps are as they were, and
    // so are the quiet before the first sound and after the last, which are not pauses.
    let after: Vec<u64> = levels_of(&fx).pauses(RATE as u64 / 10).into_iter().map(|(a, b)| b - a).collect();
    assert_eq!(after.len(), 8);
    let near = |frames: u64, seconds: f64| (frames as f64 / RATE as f64 - seconds).abs() < 0.03;
    assert!(near(after[3], 0.3) && near(after[5], 0.3), "{after:?}");
    assert!(near(after[1], 0.25) && near(after[2], 0.25) && near(after[4], 0.25), "{after:?}");
    assert!(near(after[0], 0.5) && near(after[7], 0.95), "{after:?}");
    let removed: u64 = done.cuts.iter().map(|(a, b)| b - a).sum();
    assert_eq!(Timeline::build(&project).unwrap().total(), 7 * RATE as u64 - removed);

    // Nothing longer left to shorten: the reply says how long the longest is.
    let e = tighten(&WriteOpts::default(), over, to).unwrap_err();
    assert_eq!((e.kind(), e.code()), (Kind::Usage, "no_long_pauses"));
    assert!(e.to_string().contains("The longest is 0.3"), "{e}");
    assert_eq!(tighten(&WriteOpts::default(), to, over).unwrap_err().code(), "bad_pause_lengths");

    // Asked about a range, the pauses are still judged against the whole recording: two seconds
    // that are all sound have no pause in them, and a pause is given as far as the range goes.
    let fx = paced(1.0);
    let half = RATE as u64 / 2;
    let found = pacing::pauses(&fx.project(), &fx.paths, None, Some((half, RATE as u64 * 5 / 2)), RATE as u64 / 10).unwrap().found;
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(found[2].end == RATE as u64 * 5 / 2 && !found[2].at_edge);
    let all = pacing::pauses(&fx.project(), &fx.paths, None, None, half).unwrap().found;
    assert_eq!(all.iter().map(|p| p.at_edge).collect::<Vec<_>>(), [true, false, false, true]);
    // Only the pause wholly inside the range is shortened.
    let ranged = pacing::tighten(&fx.paths, &WriteOpts::default(), &me(), None, |_, _| Ok(Tighten { range: Some((0, 4 * RATE as u64)), over, to, crossfade: 240 })).unwrap();
    assert_eq!(ranged.value.pauses.len(), 1);
}

#[test]
fn every_join_is_read_back_with_what_plays_across_it() {
    let fx = spoken();
    listen_and_keep(&fx, None);
    let report = |fx: &Fixture| pacing::joins(&fx.project(), &fx.paths, None, None).unwrap();
    assert!(report(&fx).joins.is_empty() && report(&fx).has_words);

    // A clean cut between words: quiet, with the words either side.
    cut_phrase(&fx, "charlie delta", Pick::Only, 0.0, &WriteOpts::default()).unwrap();
    let found = report(&fx);
    let said = |word: &Option<transcribe::Word>| word.as_ref().map(|w| w.text.clone()).unwrap_or_default();
    assert_eq!(found.joins.len(), 1);
    let join = &found.joins[0];
    assert_eq!((join.heard, said(&join.before).as_str(), said(&join.after).as_str(), join.doubled, join.clipped.len()), (Landing::Quiet, "bravo", "echo", false, 0));

    // Cut "echo" too and the same word is said twice across the join: "bravo bravo".
    cut_phrase(&fx, "echo", Pick::Only, 0.0, &WriteOpts::default()).unwrap();
    let found = report(&fx);
    assert_eq!(found.joins.len(), 1, "two cuts side by side make one join");
    assert!(found.joins[0].doubled && found.joins[0].heard == Landing::Quiet);

    // A cut that starts inside a word lands on sound, and names the word it clipped. One that
    // ends inside a word names that one.
    let fx = spoken();
    listen_and_keep(&fx, None);
    fx.apply(vec![cut(60_000, 76_800, 0)]);
    let found = report(&fx);
    assert_eq!((found.joins[0].heard, found.joins[0].clipped.clone()), (Landing::Sound, vec!["bravo".to_string()]));
    fx.apply(vec![cut(24_000, 28_800, 0)]);
    let found = report(&fx);
    assert_eq!(found.joins.len(), 2);
    assert_eq!((found.joins[0].heard, found.joins[0].clipped.clone()), (Landing::Sound, vec!["alpha".to_string()]));

    // With no transcript kept the levels are still read, and the reply says words are unknown.
    let fx = spoken();
    fx.apply(vec![cut(60_000, 76_800, 0)]);
    let found = report(&fx);
    assert!(!found.has_words && found.joins[0].heard == Landing::Sound && found.joins[0].before.is_none());
}

#[test]
fn word_times_are_moved_out_of_the_pauses_either_side() {
    use scrubr_core::transcribe::Word;
    // Sound from 0.5 to 0.85 s, 1.1 to 1.45 s, 1.7 to 2.05 s and on; room tone between.
    let fx = paced(1.0);
    let project = fx.project();
    let at = |seconds: f64| (seconds * RATE as f64).round() as u64;
    let word = |text: &str, start: f64, end: f64| Word { text: text.into(), start: at(start), end: at(end), probability: 1.0 };
    let mut words = vec![
        // Started early, in the pause before it, and run on into the pause after it.
        word("one", 0.3, 1.0),
        // Heard where it is.
        word("two", 1.1, 1.45),
        // Begun inside a stretch of sound, where there is nothing to go by. Ended a little soon,
        // with the sound running on to 2.05 s and a pause after it: it runs to the pause.
        word("three", 1.75, 1.95),
        // Wholly inside the long pause, as a noise the model took for a word can be: left.
        word("four", 2.3, 2.9),
        // Heard at one moment: left.
        word("five", 3.0, 3.0),
        // Ended too soon, with the sound running on to 4.2 s and a pause after it: it runs to
        // the pause.
        word("six", 3.85, 4.0),
        // The sound after this one is the next word's, so it is left.
        word("seven", 5.1, 5.3),
        word("eight", 5.32, 5.45),
    ];
    let before = words.clone();
    let moved = pacing::settle(&project, &fx.paths, None, (0, 7 * RATE as u64), &mut words).unwrap();
    assert_eq!(moved, 3);
    let near = |frames: u64, seconds: f64| (frames as f64 / RATE as f64 - seconds).abs() < 0.021;
    assert!(near(words[0].start, 0.5) && near(words[0].end, 0.85), "{:?}", words[0]);
    assert!(words[2].start == before[2].start && near(words[2].end, 2.05), "{:?}", words[2]);
    assert_eq!((&words[1], &words[3..5]), (&before[1], &before[3..5]));
    assert!(words[5].start == before[5].start && near(words[5].end, 4.2), "{:?}", words[5]);
    assert_eq!(words[6..], before[6..]);
}

// ---- spectrograms ----------------------------------------------------------------------------

#[test]
fn a_spectrogram_shows_what_sounds_where_and_when() {
    use scrubr_core::spectrum;
    // Four seconds: a 60 Hz hum all the way through at -40 dBFS, a 1 kHz tone at -12 dBFS from
    // 1 s to 2 s, and one click at 3 s.
    let fx = Fixture::with(4 * RATE as u64, 1, |f, _, _| {
        let t = f as f32 / RATE as f32;
        let hum = (TAU * 60.0 * t).sin() * 0.01;
        let tone = if (1.0..2.0).contains(&t) { (TAU * 1000.0 * t).sin() * 0.25 } else { 0.0 };
        let click = if f == 3 * RATE as u64 { 0.9 } else { 0.0 };
        hum + tone + click
    });
    let grid = spectrum::measure(&fx.project(), &fx.paths, None, None, 400, 240, 40.0, None).unwrap();
    assert_eq!((grid.db.len(), grid.rows(), grid.max_hz), (400, 240, 20_000.0));
    let at = |seconds: f64, hz: f64| grid.db[(seconds * 100.0) as usize][grid.row_of_hz(hz).round() as usize];
    // Rows and frequencies turn into each other.
    assert!((grid.hz_of_row(grid.row_of_hz(1000.0)) - 1000.0).abs() < 1e-6);
    assert!((grid.hz_of_row(0.0) - 20_000.0).abs() < 1e-6 && (grid.hz_of_row(239.0) - 40.0).abs() < 1e-6);
    // The tone reads at its level, where and when it sounds, and nowhere else.
    assert!((at(1.5, 1000.0) + 12.0).abs() < 1.5, "{}", at(1.5, 1000.0));
    assert!(at(0.5, 1000.0) < -90.0 && at(2.6, 1000.0) < -90.0 && at(1.5, 5000.0) < -90.0, "{} {} {}", at(0.5, 1000.0), at(2.6, 1000.0), at(1.5, 5000.0));
    // The hum is a level line low down, from start to end.
    for seconds in [0.2, 1.5, 2.5, 3.8] {
        assert!((at(seconds, 60.0) + 40.0).abs() < 2.5, "{} at {seconds}", at(seconds, 60.0));
        assert!(at(seconds, 200.0) < -80.0, "{} at {seconds}", at(seconds, 200.0));
    }
    // The click is an upright line: every pitch at once, for a moment.
    let click = (3.0f64 * 100.0) as usize;
    let lit = |column: usize| grid.db[column].iter().filter(|&&db| db > -65.0).count();
    assert_eq!(lit(click), 240);
    // Away from it only the hum's own rows are lit.
    assert!(lit(click - 30) < 60 && lit(click + 30) < 60, "{} {}", lit(click - 30), lit(click + 30));

    // The image is written, and says how to read a pixel.
    let png = fx.root.join("out").join("spec.png");
    let report = spectrum::render_png(&fx.project(), &fx.paths, &png, &spectrum::Options { width: 466, height: 264, ..Default::default() }).unwrap();
    assert_eq!((report.plot_left, report.plot_top, report.plot_width, report.plot_height), (66, 24, 400, 240));
    assert_eq!((report.bottom_hz, report.top_hz), (40.0, 20_000.0));
    assert!(std::fs::metadata(&png).unwrap().len() > 1000);
    // Closer in, the window is shorter, so the click is a thinner line.
    let close = spectrum::measure(&fx.project(), &fx.paths, None, Some((RATE as u64 * 29 / 10, RATE as u64 * 31 / 10)), 400, 120, 40.0, None).unwrap();
    assert!(close.window < grid.window);
    let e = spectrum::measure(&fx.project(), &fx.paths, None, None, 400, 240, 500.0, Some(100.0)).unwrap_err();
    assert_eq!((e.kind(), e.code()), (Kind::Usage, "bad_frequency"));
}
