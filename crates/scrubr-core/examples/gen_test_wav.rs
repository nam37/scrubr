//! Writes a synthetic 48 kHz, 24-bit stereo WAV for the M0 spike: speech-like bursts with
//! pauses, so the waveform has shape at every zoom, over a quiet steady tone, so a playback
//! dropout is easy to hear.
//!
//! `cargo run --release -p scrubr-core --example gen_test_wav -- testdata/hour.wav 3600`

use std::f32::consts::TAU;

const RATE: u32 = 48_000;

struct Rng(u64);

impl Rng {
    /// xorshift64*, as a float in -1.0..1.0.
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (x >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }

    fn unit(&mut self) -> f32 {
        self.next() * 0.5 + 0.5
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "testdata/hour.wav".into());
    let seconds: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(3600);
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir)?;
    }

    let spec = hound::WavSpec { channels: 2, sample_rate: RATE, bits_per_sample: 24, sample_format: hound::SampleFormat::Int };
    let mut out = hound::WavWriter::create(&path, spec)?;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let full = (1i32 << 23) - 1;

    // A "phrase" is a run of syllables; between phrases there is a pause.
    let mut env = 0.0f32; // smoothed loudness
    let mut target = 0.0f32;
    let mut until = 0u64; // frame at which the next syllable or pause starts
    let mut syllables_left = 0u32;
    let mut low = [0.0f32; 2]; // one-pole low-pass state per channel
    let mut pan = 0.5f32;

    let frames = seconds * RATE as u64;
    for f in 0..frames {
        if f >= until {
            if syllables_left == 0 {
                // Pause of 0.3 to 2.5 s, then a new phrase somewhere in the stereo field.
                target = 0.0;
                until = f + (RATE as f32 * (0.3 + 2.2 * rng.unit() * rng.unit())) as u64;
                syllables_left = 4 + (rng.unit() * 24.0) as u32;
                pan = 0.3 + 0.4 * rng.unit();
            } else {
                target = 0.15 + 0.6 * rng.unit();
                until = f + (RATE as f32 * (0.08 + 0.25 * rng.unit())) as u64;
                syllables_left -= 1;
            }
        }
        env += (target - env) * 0.0008;
        let t = f as f32 / RATE as f32;
        let tone = 0.02 * (TAU * 220.0 * t).sin();
        for (c, low) in low.iter_mut().enumerate() {
            let gain = if c == 0 { 1.0 - pan } else { pan };
            *low += (rng.next() - *low) * 0.12;
            let s = (*low * 2.4 * env * gain * 1.6 + tone).clamp(-0.98, 0.98);
            out.write_sample((s * full as f32) as i32)?;
        }
    }
    out.finalize()?;
    println!("wrote {path}: {seconds} s, {RATE} Hz, 24-bit stereo, {frames} frames");
    Ok(())
}
