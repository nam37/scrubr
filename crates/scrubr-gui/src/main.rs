//! M0, the Rune spike: open a WAV, build its peak pyramid, draw it, zoom and scroll it, and
//! play it through cpal with a moving playhead.
//!
//! `scrubr-gui <file.wav> [--mute] [--autoplay] [--no-audio] [--sync] [--bench SECONDS]`
//!
//! - `--mute` plays at zero gain: the stream and the dropout counter still run.
//! - `--sync` builds peaks before the window opens, for headless `--snap` runs.
//! - `--bench SECONDS` plays while zooming from the whole file down to single samples and back,
//!   prints frame and audio statistics, and exits.

mod audio;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rune::tok::{self, type_};
use rune::{
    App, AppSpec, Color, Cx, El, FrameEncoded, FrameOutcome, Key, KeyEvent, LineCap, Live, LiveText, Phase, Shape, Tx,
    canvas, col, el, memo, oklch, row, text,
};
use scrubr_core::peaks::{BASE, columns_from_samples};
use scrubr_core::{Peaks, WavFile, WavInfo};

use audio::Player;

/// The deepest zoom: points per frame.
const MAX_POINTS_PER_FRAME: f64 = 32.0;

struct Opts {
    path: Option<PathBuf>,
    mute: bool,
    autoplay: bool,
    no_audio: bool,
    sync: bool,
    bench: Option<f64>,
}

fn opts() -> Opts {
    let mut o = Opts { path: None, mute: false, autoplay: false, no_audio: false, sync: false, bench: None };
    let mut args = rune::extra_args().iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mute" => o.mute = true,
            "--autoplay" => o.autoplay = true,
            "--no-audio" => o.no_audio = true,
            "--sync" => o.sync = true,
            "--bench" => o.bench = args.next().and_then(|s| s.parse().ok()),
            other if !other.starts_with("--") && o.path.is_none() => o.path = Some(PathBuf::from(other)),
            _ => {}
        }
    }
    o
}

struct Doc {
    info: WavInfo,
    peaks: Peaks,
    /// A second handle on the file, for views finer than the peak cache.
    raw: RefCell<WavFile>,
    player: Option<Player>,
    audio_note: String,
    build_secs: f32,
}

type Loaded = (WavInfo, Peaks, f32);

struct Loading {
    rx: mpsc::Receiver<Result<Loaded, String>>,
    percent: Arc<AtomicU32>,
}

struct State {
    opts: Opts,
    loading: Option<Loading>,
    percent: u32,
    doc: Option<Doc>,
    error: Option<String>,
    /// First visible frame, and frames per point.
    start: f64,
    fpp: f64,
    /// The waveform area's size in points, from layout.
    wave: [f32; 2],
    fit_pending: bool,
    playhead: Live<[f32; 2]>,
    clock: LiveText,
    stats: LiveText,
    stats_at: f64,
    bench_began: Option<f64>,
}

// ---------------------------------------------------------------- measurements

#[derive(Default)]
struct FrameLog {
    last: Option<Instant>,
    /// Milliseconds between consecutive drawn frames.
    intervals: Vec<f32>,
    update: Vec<f32>,
    encode: Vec<f32>,
    refresh_hz: Option<f64>,
}

static FRAMES: Mutex<FrameLog> = Mutex::new(FrameLog { last: None, intervals: Vec::new(), update: Vec::new(), encode: Vec::new(), refresh_hz: None });
/// Milliseconds spent building each waveform shape.
static SHAPES: Mutex<Vec<f32>> = Mutex::new(Vec::new());

fn log_frame(f: &FrameEncoded) {
    if !matches!(f.outcome(), FrameOutcome::Drawn) {
        return;
    }
    let now = Instant::now();
    let mut log = FRAMES.lock().unwrap();
    if let Some(last) = log.last {
        log.intervals.push((now - last).as_secs_f32() * 1000.0);
    }
    log.last = Some(now);
    if let Some(ms) = f.update_ms() {
        log.update.push(ms as f32);
    }
    if let Some(ms) = f.encode_ms() {
        log.encode.push(ms as f32);
    }
    log.refresh_hz = f.refresh_hz().or(log.refresh_hz);
    if log.intervals.len() > 40_000 {
        log.intervals.drain(..20_000);
        log.update.clear();
        log.encode.clear();
    }
}

struct Summary {
    n: usize,
    mean: f32,
    p50: f32,
    p99: f32,
    max: f32,
}

fn summarize(values: &[f32]) -> Summary {
    if values.is_empty() {
        return Summary { n: 0, mean: 0.0, p50: 0.0, p99: 0.0, max: 0.0 };
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f32| v[((v.len() - 1) as f32 * q).round() as usize];
    Summary { n: v.len(), mean: v.iter().sum::<f32>() / v.len() as f32, p50: at(0.5), p99: at(0.99), max: v[v.len() - 1] }
}

// ---------------------------------------------------------------- loading

fn load(path: &PathBuf, percent: &AtomicU32) -> Result<Loaded, String> {
    let began = Instant::now();
    let (info, peaks) = scrubr_core::build_peaks(path, |p| percent.store((p * 100.0) as u32, Relaxed)).map_err(|e| e.to_string())?;
    Ok((info, peaks, began.elapsed().as_secs_f32()))
}

fn finish_load(s: &mut State, loaded: Result<Loaded, String>) {
    let (info, peaks, build_secs) = match loaded {
        Ok(l) => l,
        Err(e) => {
            s.error = Some(e);
            return;
        }
    };
    let raw = match WavFile::open(&info.path) {
        Ok(raw) => RefCell::new(raw),
        Err(e) => {
            s.error = Some(e.to_string());
            return;
        }
    };
    let (player, audio_note) = if s.opts.no_audio {
        (None, "audio off".to_string())
    } else {
        match Player::open(&info.path, if s.opts.mute { 0.0 } else { 1.0 }) {
            Ok(p) => {
                let note = format!("{} Hz out{}", p.dev_rate, if s.opts.mute { " · muted" } else { "" });
                (Some(p), note)
            }
            Err(e) => (None, format!("no audio: {e}")),
        }
    };
    eprintln!(
        "peaks: {:.2} s for {} frames ({:.1} MB, {} levels)",
        build_secs,
        info.frames,
        peaks.bytes() as f32 / 1e6,
        peaks.level_count()
    );
    s.doc = Some(Doc { info, peaks, raw, player, audio_note, build_secs });
    s.fit_pending = true;
    fit(s);
    if s.opts.autoplay && let Some(p) = s.doc.as_ref().and_then(|d| d.player.as_ref()) {
        p.play();
    }
}

fn init() -> State {
    let opts = opts();
    let mut s = State {
        opts,
        loading: None,
        percent: 0,
        doc: None,
        error: None,
        start: 0.0,
        fpp: 1.0,
        wave: [0.0; 2],
        fit_pending: true,
        playhead: Live::new([-100.0, 0.0]),
        clock: LiveText::new("0:00:00.000"),
        stats: LiveText::new(""),
        stats_at: 0.0,
        bench_began: None,
    };
    match s.opts.path.clone() {
        None => s.error = Some("Usage: scrubr-gui <file.wav> [--mute] [--autoplay] [--bench SECONDS]".into()),
        Some(path) if s.opts.sync => {
            let loaded = load(&path, &AtomicU32::new(0));
            finish_load(&mut s, loaded);
        }
        Some(path) => {
            let (tx, rx) = mpsc::channel();
            let percent = Arc::new(AtomicU32::new(0));
            let for_loader = percent.clone();
            std::thread::spawn(move || {
                let _ = tx.send(load(&path, &for_loader));
            });
            s.loading = Some(Loading { rx, percent });
        }
    }
    s
}

// ---------------------------------------------------------------- the view's camera

fn frames(s: &State) -> f64 {
    s.doc.as_ref().map_or(0.0, |d| d.info.frames as f64)
}

fn clamp(s: &mut State) {
    let w = s.wave[0].max(1.0) as f64;
    let total = frames(s).max(1.0);
    s.fpp = s.fpp.clamp(1.0 / MAX_POINTS_PER_FRAME, (total / w).max(1.0 / MAX_POINTS_PER_FRAME));
    s.start = s.start.clamp(0.0, (total - w * s.fpp).max(0.0));
}

/// Shows the whole file.
fn fit(s: &mut State) {
    if s.doc.is_none() || s.wave[0] <= 0.0 {
        return;
    }
    s.fpp = frames(s) / s.wave[0] as f64;
    s.start = 0.0;
    s.fit_pending = false;
    clamp(s);
}

/// Zooms by `factor`, keeping the frame under `x` (points from the left edge) where it is.
fn zoom_about(s: &mut State, x: f64, factor: f64) {
    let anchor = s.start + x * s.fpp;
    s.fpp /= factor.max(0.01);
    clamp(s);
    s.start = anchor - x * s.fpp;
    clamp(s);
}

fn pan(s: &mut State, points: f64) {
    s.start += points * s.fpp;
    clamp(s);
}

fn position(s: &State) -> Option<u64> {
    s.doc.as_ref()?.player.as_ref().map(Player::position)
}

/// Moves the playhead and the clock. Both are live values: neither re-runs the view.
fn sync_playhead(s: &State) {
    let Some(doc) = &s.doc else { return };
    let pos = position(s).unwrap_or(0);
    let x = ((pos as f64 - s.start) / s.fpp) as f32;
    let shown = doc.player.is_some() && x >= 0.0 && x <= s.wave[0];
    s.playhead.set([if shown { x } else { -100.0 }, 0.0]);
    s.clock.set(timecode(pos as f64, doc.info.sample_rate));
}

fn timecode(frame: f64, rate: u32) -> String {
    let ms = (frame.max(0.0) / rate as f64 * 1000.0).round() as u64;
    format!("{}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

// ---------------------------------------------------------------- drawing

fn wave_color() -> Color {
    oklch(0.80, 0.14, 75.0)
}

/// Reads the frames a view of `span` frames from `start` needs, with one frame of margin on
/// each side. Returns the index of the first frame read.
fn read_view(doc: &Doc, start: f64, span: f64, samples: &mut Vec<f32>) -> u64 {
    let first = (start.floor().max(0.0) as u64).saturating_sub(1);
    let count = span.ceil() as usize + 4;
    let mut raw = doc.raw.borrow_mut();
    if raw.seek(first).is_err() || raw.read(count, samples).is_err() {
        samples.clear();
    }
    first
}

/// The waveform for a `w` by `h` point area: one lane per channel. Above one frame per device
/// pixel each column is a vertical min-to-max line, from the peak cache while it is fine enough
/// and from the file below that. Under one frame per pixel the samples are joined by a line.
fn waveform(doc: &Doc, start: f64, fpp: f64, w: f32, h: f32, scale: f32) -> Shape {
    let began = Instant::now();
    let t = tok::theme(true);
    let channels = doc.peaks.channels();
    let lanes = channels.min(2);
    let cols = (w * scale).round().max(1.0) as usize;
    let col_w = w / cols as f32;
    let fpc = fpp * col_w as f64;
    let total = doc.info.frames as f64;
    let lane_h = h / lanes as f32;
    let color = wave_color();

    let mut shape = Shape::new();
    let mut columns = Vec::new();
    let mut samples = Vec::new();
    let mut first = 0u64;
    if fpc < BASE as f64 {
        first = read_view(doc, start, cols as f64 * fpc, &mut samples);
    }

    for c in 0..lanes {
        let mid = lane_h * (c as f32 + 0.5);
        let amp = lane_h * 0.5 - 6.0;
        shape = shape.line([0.0, mid], [w, mid], col_w, LineCap::Butt, t.separator);
        if fpc >= 1.0 {
            if !doc.peaks.columns(c, start, fpc, cols, &mut columns) {
                columns_from_samples(&samples, channels, c, first, start, fpc, cols, &mut columns);
            }
            for (i, m) in columns.iter().enumerate() {
                let frame = start + i as f64 * fpc;
                if frame + fpc <= 0.0 || frame >= total {
                    continue;
                }
                let x = (i as f32 + 0.5) * col_w;
                let (mut top, mut bottom) = (mid - m.max * amp, mid - m.min * amp);
                if bottom - top < col_w {
                    let centre = (top + bottom) * 0.5;
                    top = centre - col_w * 0.5;
                    bottom = centre + col_w * 0.5;
                }
                shape = shape.line([x, top], [x, bottom], col_w, LineCap::Butt, color);
            }
        } else {
            let points: Vec<[f32; 2]> = samples
                .chunks_exact(channels)
                .enumerate()
                .map(|(i, frame)| [(((first + i as u64) as f64 - start) / fpp) as f32, mid - frame[c] * amp])
                .collect();
            if points.len() >= 2 {
                shape = shape.stroke(points.clone(), 1.5, color);
            }
            // Past eight points per frame the samples themselves are worth seeing.
            if fpp <= 0.125 {
                for p in points {
                    shape = shape.line(p, p, 4.0, LineCap::Round, color);
                }
            }
        }
    }
    SHAPES.lock().unwrap().push(began.elapsed().as_secs_f32() * 1000.0);
    shape
}

fn message(s: impl Into<String>) -> El {
    let t = tok::theme(true);
    col().w_full().grow(1.0).min_h(0.0).center().child(text(s).font(type_::BODY).color(t.text_secondary))
}

fn wave_area(s: &State, doc: &Doc, scale: f32) -> El {
    let t = tok::theme(true);
    let [w, h] = s.wave;
    let mut area = el()
        .key("wave")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(t.card)
        .on_layout(|s: &mut State, r| {
            let size = [r[2], r[3]];
            if s.wave != size {
                s.wave = size;
                if s.fit_pending { fit(s) } else { clamp(s) }
                sync_playhead(s);
            }
        })
        .on_wheel(|s: &mut State, e, _| {
            pan(s, -(e.dx + e.dy) as f64);
            sync_playhead(s);
            true
        })
        .on_magnify(|s: &mut State, e, _| {
            zoom_about(s, e.x as f64, 1.0 + e.delta as f64);
            sync_playhead(s);
            true
        })
        .on_drag(|s: &mut State, d, _| {
            // Press or drag to move the playhead: scrubbing, minus the sound for now.
            let frame = (s.start + d.x as f64 * s.fpp).max(0.0) as u64;
            if d.phase != Phase::Up && let Some(p) = s.doc.as_ref().and_then(|d| d.player.as_ref()) {
                p.seek(frame);
            }
            sync_playhead(s);
        });
    if w > 0.0 && h > 0.0 {
        let version = (s.start.to_bits(), s.fpp.to_bits(), w.to_bits(), h.to_bits(), scale.to_bits());
        area = area
            .child(memo("wave-shape", version, || canvas(waveform(doc, s.start, s.fpp, w, h, scale)).key("wave-canvas").absolute(0.0, 0.0).size(w, h)))
            .child(el().key("playhead").absolute(0.0, 0.0).size(1.5, h).fill(t.text).live_offset(&s.playhead));
    }
    area
}

fn view(s: &State, cx: &mut Cx<State>) -> El {
    let t = tok::theme(true);
    let scale = cx.env().scale;
    let dim = |label: String| text(label).font(type_::MONO_CAPTION).color(t.text_secondary);

    let title = match &s.doc {
        Some(doc) => format!(
            "{} · {} kHz · {}-bit{} · {} ch · {}",
            doc.info.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            doc.info.sample_rate as f32 / 1000.0,
            doc.info.bits,
            if doc.info.float { " float" } else { "" },
            doc.info.channels,
            timecode(doc.info.frames as f64, doc.info.sample_rate),
        ),
        None => "scrubr".to_string(),
    };
    let header = row()
        .w_full()
        .padding(10.0)
        .gap(16.0)
        .justify_between()
        .items_center()
        .child(text(title).font(type_::TITLE).color(t.text))
        .child(dim("Space play · Ctrl+wheel zoom · wheel scroll · click seek · F fit".into()));

    let body = match (&s.doc, &s.error) {
        (_, Some(e)) => message(e.clone()),
        (None, None) => message(format!("Building peaks… {}%", s.percent)),
        (Some(doc), None) => wave_area(s, doc, scale),
    };

    let mut footer = row().w_full().padding(10.0).gap(20.0).items_center();
    if let Some(doc) = &s.doc {
        let rate = doc.info.sample_rate;
        let end = s.start + s.wave[0] as f64 * s.fpp;
        let zoom = if s.fpp >= 1.0 { format!("{:.0} frames/pt", s.fpp) } else { format!("{:.1} pt/frame", 1.0 / s.fpp) };
        footer = footer
            .child(text("0:00:00.000").live_text(&s.clock).font(type_::MONO_CAPTION).color(t.text))
            .child(dim(format!("{} – {} · {}", timecode(s.start, rate), timecode(end.min(doc.info.frames as f64), rate), zoom)))
            .child(dim(format!("peaks {:.2} s · {}", doc.build_secs, doc.audio_note)))
            // The placeholder sets the box a live text draws in, so it is as wide as the longest value.
            .child(text("frames: mean 00.0 ms · max 000.0 ms · underruns 0000").live_text(&s.stats).font(type_::MONO_CAPTION).color(t.text_secondary));
    }

    col().w_full().h_full().fill(t.bg).child(header).child(body).child(footer)
}

// ---------------------------------------------------------------- input and time

fn on_key(s: &mut State, e: &KeyEvent, _: &mut Tx) -> bool {
    let w = s.wave[0] as f64;
    match &e.key {
        Key::Text(k) if k == " " => {
            if let Some(p) = s.doc.as_ref().and_then(|d| d.player.as_ref()) {
                p.toggle();
            }
        }
        Key::Text(k) if k.eq_ignore_ascii_case("f") => fit(s),
        Key::Home => {
            if let Some(p) = s.doc.as_ref().and_then(|d| d.player.as_ref()) {
                p.seek(0);
            }
            s.start = 0.0;
        }
        Key::Left => pan(s, -0.2 * w),
        Key::Right => pan(s, 0.2 * w),
        Key::Up => zoom_about(s, w * 0.5, 1.5),
        Key::Down => zoom_about(s, w * 0.5, 1.0 / 1.5),
        _ => return false,
    }
    sync_playhead(s);
    true
}

/// `--bench`: play from ten minutes in while the view zooms from the whole file down to four
/// points per frame and back, keeping the playhead a third of the way across.
fn bench(s: &mut State, t: f64, tx: &mut Tx) -> bool {
    let Some(secs) = s.opts.bench else { return false };
    if s.doc.is_none() || s.wave[0] <= 0.0 {
        return false;
    }
    let w = s.wave[0] as f64;
    let total = frames(s);
    let Some(began) = s.bench_began else {
        s.bench_began = Some(t);
        if let Some(p) = s.doc.as_ref().and_then(|d| d.player.as_ref()) {
            p.seek((total * 0.17) as u64);
            p.play();
        }
        *FRAMES.lock().unwrap() = FrameLog::default();
        SHAPES.lock().unwrap().clear();
        tx.wake_after(Duration::from_millis(1));
        return true;
    };
    let u = (t - began) / secs;
    if u >= 1.0 {
        report(s);
        std::process::exit(0);
    }
    let depth = 1.0 - (2.0 * u - 1.0).abs();
    let fit = total / w;
    s.fpp = fit * (0.25 / fit).powf(depth);
    let pos = position(s).unwrap_or((total * 0.17) as u64) as f64;
    s.start = pos - 0.33 * w * s.fpp;
    clamp(s);
    tx.wake_after(Duration::from_millis(1));
    true
}

fn report(s: &State) {
    let log = FRAMES.lock().unwrap();
    let frames = summarize(&log.intervals);
    let update = summarize(&log.update);
    let encode = summarize(&log.encode);
    let shapes = summarize(&SHAPES.lock().unwrap());
    let budget = log.refresh_hz.map_or(1000.0 / 60.0, |hz| 1000.0 / hz as f32);
    let late = log.intervals.iter().filter(|&&ms| ms > budget * 1.5).count();
    let doc = s.doc.as_ref();
    println!("bench: {} s, window {:.0}x{:.0} pt", s.opts.bench.unwrap_or(0.0), s.wave[0], s.wave[1]);
    if let Some(doc) = doc {
        println!("file: {} frames, {} Hz, {} ch; peaks built in {:.2} s", doc.info.frames, doc.info.sample_rate, doc.info.channels, doc.build_secs);
    }
    println!("display: {} Hz, budget {budget:.2} ms", log.refresh_hz.map_or("unknown".into(), |hz| format!("{hz:.0}")));
    println!(
        "frames drawn: {}; interval mean {:.2} ms, p50 {:.2}, p99 {:.2}, max {:.2}; over 1.5x budget: {late}",
        frames.n + 1,
        frames.mean,
        frames.p50,
        frames.p99,
        frames.max
    );
    println!("view update: mean {:.2} ms, p99 {:.2}, max {:.2} (n {})", update.mean, update.p99, update.max, update.n);
    println!("encode: mean {:.2} ms, p99 {:.2}, max {:.2} (n {})", encode.mean, encode.p99, encode.max, encode.n);
    println!("waveform build: mean {:.2} ms, p99 {:.2}, max {:.2} (n {})", shapes.mean, shapes.p99, shapes.max, shapes.n);
    match doc.and_then(|d| d.player.as_ref()) {
        Some(p) => println!("audio: {} at {} Hz, {} ch; underruns {}", p.device, p.dev_rate, p.dev_channels, p.underruns()),
        None => println!("audio: off ({})", doc.map_or("no file", |d| d.audio_note.as_str())),
    }
}

fn poll(s: &mut State, t: f64, tx: &mut Tx) -> bool {
    let mut changed = false;

    let loaded = match &s.loading {
        None => None,
        Some(loading) => match loading.rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => {
                let percent = loading.percent.load(Relaxed);
                changed |= percent != s.percent;
                s.percent = percent;
                tx.wake_after(Duration::from_millis(50));
                None
            }
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("The loader stopped unexpectedly.".into())),
        },
    };
    if let Some(result) = loaded {
        s.loading = None;
        finish_load(s, result);
        changed = true;
    }

    changed |= bench(s, t, tx);

    let playing = s.doc.as_ref().and_then(|d| d.player.as_ref()).map(|p| (p.is_playing(), p.finished(), p.position()));
    if let Some((true, finished, pos)) = playing {
        if finished && let Some(p) = s.doc.as_ref().and_then(|d| d.player.as_ref()) {
            p.pause();
        }
        // Page the view when the playhead leaves it.
        let (pos, w) = (pos as f64, s.wave[0] as f64);
        if s.opts.bench.is_none() && w > 0.0 && (pos < s.start || pos > s.start + w * s.fpp) {
            s.start = pos - 0.05 * w * s.fpp;
            clamp(s);
            changed = true;
        }
        tx.wake_after(Duration::from_millis(4));
    }
    sync_playhead(s);

    if t - s.stats_at >= 0.5 {
        s.stats_at = t;
        let log = FRAMES.lock().unwrap();
        // The last couple of seconds of continuous drawing; longer gaps are the window at rest.
        let recent: Vec<f32> = log.intervals.iter().rev().take(120).copied().filter(|&ms| ms < 100.0).collect();
        if !recent.is_empty() {
            let f = summarize(&recent);
            let underruns = s.doc.as_ref().and_then(|d| d.player.as_ref()).map_or(0, Player::underruns);
            s.stats.set(format!("frames: mean {:.1} ms · max {:.1} ms · underruns {underruns}", f.mean, f.max));
        }
    }
    changed
}

fn main() {
    let spec = AppSpec::<State> {
        poll: Some(poll),
        on_key: Some(on_key),
        min_size: Some((720.0, 320.0)),
        ..AppSpec::new("scrubr", (1280.0, 520.0), init, view)
    };
    App::new(spec).on_frame_encoded(log_frame).run();
}
