//! Transcription: the words spoken on the timeline, each with its start and end.
//!
//! The pipeline is the same whatever recognizes the speech. The timeline at head is rendered
//! with its edits, mixed to mono, resampled to 16 kHz and handed to an [`Engine`] in pieces of
//! at most five minutes, each split at the quietest moment near its end so no word straddles two
//! pieces. Word times come back in timeline frames. Audio streams through: only the piece in
//! hand is held in memory.
//!
//! The one engine, behind the `transcribe` Cargo feature, is Whisper in pure Rust
//! ([`whisper::Whisper`]). Its model is a local file; nothing is sent anywhere.

use crate::error::{Error, Result};
use crate::project::{Paths, Project};
use crate::render::Renderer;
use crate::source::{self, Frames};

/// The sample rate engines take, in Hz. Mono.
pub const ENGINE_RATE: u32 = 16_000;

/// The longest piece handed to an engine, in samples at [`ENGINE_RATE`].
const PIECE: usize = 300 * ENGINE_RATE as usize;
/// How far back from the end of a full piece to look for a quiet place to split it.
const SPLIT_SEARCH: usize = 30 * ENGINE_RATE as usize;
/// The length of quiet looked for.
const SPLIT_QUIET: usize = ENGINE_RATE as usize * 4 / 10;

/// A word as an engine heard it: times are seconds from the start of the audio it was given.
#[derive(Clone, Debug, PartialEq)]
pub struct Heard {
    pub text: String,
    pub start: f64,
    pub end: f64,
    pub probability: f64,
}

/// Something that turns speech into words with times.
pub trait Engine {
    /// The engine and model, for the reply.
    fn name(&self) -> String;

    /// Transcribes mono audio at [`ENGINE_RATE`], samples in -1.0..=1.0.
    fn transcribe(&mut self, audio: &[f32]) -> Result<Vec<Heard>>;
}

/// A word on the timeline. `start` and `end` are timeline frames.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub start: u64,
    pub end: u64,
    /// How sure the engine was, 0.0 to 1.0.
    pub probability: f64,
}

/// The timeline, or a range of it, as mono frames.
struct Mono {
    renderer: Renderer,
    pos: u64,
    end: u64,
    rate: u32,
    block: Vec<f32>,
}

impl Frames for Mono {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> usize {
        1
    }

    fn next(&mut self, out: &mut Vec<f32>) -> Result<()> {
        out.clear();
        let frames = (self.end - self.pos).min(1 << 15) as usize;
        if frames == 0 {
            return Ok(());
        }
        self.renderer.read(self.pos, frames, &mut self.block)?;
        let channels = self.renderer.channels();
        out.extend(self.block.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / channels as f32));
        self.pos += frames as u64;
        Ok(())
    }
}

/// Where to end a piece: the middle of the quietest stretch in the last part of `audio`.
fn split_point(audio: &[f32]) -> usize {
    let hop = ENGINE_RATE as usize / 100;
    let from = audio.len().saturating_sub(SPLIT_SEARCH);
    let energy: Vec<f32> = audio[from..].chunks(hop).map(|c| c.iter().map(|s| s * s).sum()).collect();
    let window = SPLIT_QUIET / hop;
    if energy.len() <= window {
        return audio.len();
    }
    let mut sum: f32 = energy[..window].iter().sum();
    let mut best = (sum, 0);
    for i in window..energy.len() {
        sum += energy[i] - energy[i - window];
        if sum < best.0 {
            best = (sum, i + 1 - window);
        }
    }
    from + (best.1 + window / 2) * hop
}

/// Collects engine-rate audio and hands it to the engine a piece at a time.
struct Pieces<'a> {
    engine: &'a mut dyn Engine,
    progress: &'a mut dyn FnMut(u64, u64),
    pending: Vec<f32>,
    /// Samples already handed to the engine.
    done: u64,
    words: Vec<Word>,
    /// The timeline range being transcribed, in frames, and the project's rate.
    start: u64,
    end: u64,
    rate: u32,
}

impl Pieces<'_> {
    fn push(&mut self, samples: &[f32]) -> Result<()> {
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= PIECE {
            let at = split_point(&self.pending[..PIECE]);
            self.flush(at)?;
        }
        Ok(())
    }

    fn flush(&mut self, samples: usize) -> Result<()> {
        let piece = &self.pending[..samples];
        // Digital silence has no words in it, and recognizers tend to invent some.
        if piece.iter().any(|s| s.abs() > 1e-4) {
            let base = self.done as f64 / ENGINE_RATE as f64;
            let frame = |seconds: f64| (self.start + ((base + seconds.max(0.0)) * self.rate as f64).round() as u64).min(self.end);
            for heard in self.engine.transcribe(piece)? {
                let text = heard.text.trim();
                if !text.is_empty() {
                    let start = frame(heard.start);
                    self.words.push(Word { text: text.to_string(), start, end: frame(heard.end).max(start), probability: heard.probability });
                }
            }
        }
        self.pending.drain(..samples);
        self.done += samples as u64;
        let frames = self.done * self.rate as u64 / ENGINE_RATE as u64;
        (self.progress)(frames.min(self.end - self.start), self.end - self.start);
        Ok(())
    }
}

/// Transcribes the timeline at head, or `range` of it in frames. `progress` hears frames done
/// and frames in all after each piece.
pub fn transcribe(project: &Project, paths: &Paths, range: Option<(u64, u64)>, engine: &mut dyn Engine, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<Word>> {
    transcribe_track(project, paths, None, range, engine, progress)
}

/// [`transcribe`] for one track heard alone, by its index in the timeline's tracks, or the mix
/// when `track` is `None`. A track for each speaker gives a transcript for each speaker.
pub fn transcribe_track(
    project: &Project,
    paths: &Paths,
    track: Option<usize>,
    range: Option<(u64, u64)>,
    engine: &mut dyn Engine,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Vec<Word>> {
    let renderer = Renderer::of(project, paths, track)?;
    let total = renderer.total();
    if total == 0 {
        return Err(Error::usage("empty_timeline", "The timeline is empty, so there is nothing to transcribe."));
    }
    let (start, end) = range.unwrap_or((0, total));
    crate::edit::check_range(project, paths, renderer.timeline(), (start, end))?;
    transcribe_renderer(renderer, project.sample_rate, (start, end), engine, progress)
}

/// [`transcribe`] over what a renderer plays, from `start` to `end` in its timeline's frames.
/// The renderer need not be the project at head: it can play a change that is not written yet,
/// so the result can be listened to before it is made.
pub fn transcribe_renderer(renderer: Renderer, rate: u32, (start, end): (u64, u64), engine: &mut dyn Engine, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<Word>> {
    let mut mono = Mono { renderer, pos: start, end, rate, block: Vec::new() };
    let mut pieces = Pieces { engine, progress, pending: Vec::new(), done: 0, words: Vec::new(), start, end, rate };
    if rate == ENGINE_RATE {
        let mut block = Vec::new();
        loop {
            mono.next(&mut block)?;
            if block.is_empty() {
                break;
            }
            pieces.push(&block)?;
        }
    } else {
        source::resample(&mut mono, ENGINE_RATE, &mut |samples| pieces.push(samples))?;
    }
    let rest = pieces.pending.len();
    if rest > 0 {
        pieces.flush(rest)?;
    }
    Ok(pieces.words)
}

/// The words as running text.
pub fn text(words: &[Word]) -> String {
    words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
}

#[cfg(feature = "transcribe")]
pub mod whisper {
    //! OpenAI's Whisper in pure Rust, through `whisper-candle-core`. Models are the standard
    //! ones (`tiny.en`, `base.en`, `small.en`, ...), kept in the Hugging Face cache folder or
    //! given as a folder holding `config.json`, `model.safetensors` and
    //! `generation_config.json`. Loading never touches the network; [`fetch`] is the one call
    //! that downloads.

    use std::path::{Path, PathBuf};

    use whisper_core::{TranscribeOptions, WhichModel, WhisperModel};

    use super::{Engine, Heard};
    use crate::error::{Error, Result};

    /// The model used when none is named: the smallest with steady word timing.
    pub const DEFAULT_MODEL: &str = "base.en";

    struct Files {
        config: PathBuf,
        weights: PathBuf,
        generation_config: Option<PathBuf>,
    }

    fn which(name: &str) -> Result<WhichModel> {
        name.parse::<WhichModel>().map_err(|_| {
            Error::usage("unknown_model", format!("\"{name}\" is not a Whisper model or a folder that holds one."))
                .hint(format!("Use one of {}, or the path of a folder with config.json and model.safetensors.", WhichModel::ALL_NAMES.join(", ")))
        })
    }

    /// The model's files if they are on this machine.
    fn local(name: &str) -> Result<Option<Files>> {
        let dir = Path::new(name);
        if dir.is_dir() {
            let (config, weights, generation) = (dir.join("config.json"), dir.join("model.safetensors"), dir.join("generation_config.json"));
            return Ok((config.is_file() && weights.is_file()).then(|| Files { config, weights, generation_config: generation.is_file().then_some(generation) }));
        }
        let cached = hf_hub::Cache::from_env().model(which(name)?.hf_repo().to_string());
        Ok(match (cached.get("config.json"), cached.get("model.safetensors")) {
            (Some(config), Some(weights)) => Some(Files { config, weights, generation_config: cached.get("generation_config.json") }),
            _ => None,
        })
    }

    /// A model that can be asked for by name.
    #[derive(Clone, Debug)]
    pub struct Offered {
        pub name: &'static str,
        /// Where `fetch` downloads it from, on huggingface.co.
        pub repo: &'static str,
        /// The size of the download, in megabytes.
        pub megabytes: u32,
        /// Trained on English alone. The others hear any language.
        pub english_only: bool,
        pub installed: bool,
    }

    /// Every model that can be fetched, smallest first, English ones before the rest. A larger
    /// model hears more of what was said as it was said, and is slower in step with its size.
    pub fn offered() -> Vec<Offered> {
        const SIZES: [(&str, u32); 12] = [
            ("tiny.en", 151),
            ("base.en", 290),
            ("small.en", 967),
            ("medium.en", 3056),
            ("tiny", 151),
            ("base", 290),
            ("small", 967),
            ("medium", 3056),
            ("large-v3-turbo", 1618),
            ("large-v3", 3087),
            ("large-v2", 6173),
            ("large-v1", 6173),
        ];
        SIZES
            .iter()
            .filter_map(|&(name, megabytes)| {
                let model = name.parse::<WhichModel>().ok()?;
                Some(Offered { name, repo: model.hf_repo(), megabytes, english_only: !model.is_multilingual(), installed: is_installed(name).unwrap_or(false) })
            })
            .collect()
    }

    /// Whether `name` can be loaded without downloading anything.
    pub fn is_installed(name: &str) -> Result<bool> {
        Ok(local(name)?.is_some())
    }

    /// Downloads a model from the Hugging Face Hub into its cache folder and returns where the
    /// weights are. The only call here that uses the network.
    pub fn fetch(name: &str) -> Result<PathBuf> {
        let files = whisper_core::fetch_model(which(name)?)
            .map_err(|e| Error::codec("model_download_failed", format!("Could not download the {name} model: {e}")).hint("Check the network connection and try again."))?;
        Ok(files.weights)
    }

    fn load_failed(name: &str, e: impl std::fmt::Display) -> Error {
        Error::codec("model_load_failed", format!("Could not load the {name} model: {e}"))
    }

    pub struct Whisper {
        model: WhisperModel,
        name: String,
    }

    impl Whisper {
        /// Loads a model that is already on this machine.
        pub fn load(name: &str) -> Result<Whisper> {
            let Some(files) = local(name)? else {
                return Err(Error::not_found("model_not_installed", format!("The {name} speech model is not on this machine."))
                    .hint(format!("Run `scrubr transcribe --fetch-model {name}` once to download it from huggingface.co. Transcription itself runs locally.")));
            };
            let device = whisper_core::device("cpu").map_err(|e| load_failed(name, e))?;
            let mut model = WhisperModel::load(&files.config, &files.weights, &device).map_err(|e| load_failed(name, e))?;
            if let Some(generation_config) = &files.generation_config {
                // Carries the attention heads word times are read from.
                model.set_alignment_heads_from_file(generation_config).map_err(|e| load_failed(name, e))?;
            }
            Ok(Whisper { model, name: name.to_string() })
        }
    }

    impl Engine for Whisper {
        fn name(&self) -> String {
            format!("whisper {}", self.name)
        }

        fn transcribe(&mut self, audio: &[f32]) -> Result<Vec<Heard>> {
            let options = TranscribeOptions { word_timestamps: true, ..Default::default() };
            let result = whisper_core::transcribe(&mut self.model, audio, &options).map_err(|e| Error::codec("transcribe_failed", format!("The speech model failed: {e}")))?;
            Ok(result
                .segments
                .into_iter()
                .flat_map(|s| s.words.unwrap_or_default())
                .map(|w| Heard { text: w.word, start: w.start, end: w.end, probability: w.probability })
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_piece_is_split_in_the_quietest_stretch_near_its_end() {
        // Twenty seconds of tone with one second of silence starting at 12 s.
        let rate = ENGINE_RATE as usize;
        let audio: Vec<f32> = (0..20 * rate).map(|i| if (12 * rate..13 * rate).contains(&i) { 0.0 } else { (i as f32 * 0.05).sin() * 0.3 }).collect();
        let at = split_point(&audio);
        assert!((12 * rate + rate / 10..13 * rate - rate / 10).contains(&at), "split at {:.2} s", at as f32 / rate as f32);
        // Audio shorter than the quiet being looked for is not split.
        assert_eq!(split_point(&audio[..1000]), 1000);
    }
}
