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
use scrubr_core::render::Renderer;
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
    export::export(&project, &paths, &back, &export::Options { depth: Some(Depth::Int(24)), range: None }).unwrap();
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
    let err = export::export(&fx.project(), &fx.paths, &fx.root.join("out.mp3"), &Default::default()).unwrap_err();
    assert_eq!((err.code(), err.exit_code()), ("format_not_supported", 5));
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
    let report = image::render_png(&project, &fx.paths, &wide, &image::Options { width: 400, height: 200, range: None }).unwrap();
    assert!(report.from_peaks && report.end == 182_000);
    let near = fx.root.join("near.png");
    let report = image::render_png(&project, &fx.paths, &near, &image::Options { width: 400, height: 200, range: Some((39_000, 41_000)) }).unwrap();
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
    let err = image::render_png(&project, &fx.paths, &wide, &image::Options { width: 400, height: 200, range: Some((0, 999_999)) }).unwrap_err();
    assert_eq!(err.code(), "range_out_of_bounds");
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
