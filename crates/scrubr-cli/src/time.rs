//! Reading and writing times.
//!
//! A position is seconds (`723.25`, `0.5s`, `250ms`), a timecode (`12:03.250`, `1:02:03.250`),
//! a frame count (`34716000f`), or `end` with an optional offset (`end-2s`). A range is two
//! positions joined by `..`; an empty side means the start or the end of the timeline.

use scrubr_core::{Error, Result};

/// What a position is measured against.
#[derive(Clone, Copy, Debug)]
pub struct Scale {
    pub rate: u32,
    /// Timeline length in frames.
    pub total: u64,
}

const FORMS: &str = "Times are seconds (723.25), timecode (12:03.250 or 1:02:03.250), frames (34716000f), or end / end-2s.";

fn bad(text: &str, why: &str) -> Error {
    Error::usage("bad_time", format!("Cannot read \"{text}\" as a time: {why}.")).hint(FORMS)
}

/// The command line swaps the minus of a negative number for U+2212 so it is not read as a flag
/// (see `shield_negative_numbers`). Values are parsed, and quoted in errors, with a plain minus.
fn plain_minus(text: &str) -> String {
    text.replace("\u{2212}", "-")
}

/// A length of time in frames: seconds, timecode or frames.
pub fn parse_duration(text: &str, rate: u32) -> Result<u64> {
    let text = &plain_minus(text);
    let t = text.trim();
    if t.is_empty() {
        return Err(bad(text, "it is empty"));
    }
    if let Some(frames) = t.strip_suffix('f') {
        return frames.parse::<u64>().map_err(|_| bad(text, "a frame count is whole digits followed by f"));
    }
    let seconds = if t.contains(':') {
        let parts: Vec<&str> = t.split(':').collect();
        if parts.len() > 3 {
            return Err(bad(text, "a timecode has at most hours, minutes and seconds"));
        }
        let (last, lead) = parts.split_last().expect("split gives at least one part");
        let mut whole = 0u64;
        for part in lead {
            let n: u64 = part.parse().map_err(|_| bad(text, "hours and minutes must be whole numbers"))?;
            whole = whole * 60 + n;
        }
        let secs: f64 = last.parse().map_err(|_| bad(text, "the seconds part is not a number"))?;
        if !(0.0..60.0).contains(&secs) || last.starts_with(['+', '-']) {
            return Err(bad(text, "the seconds part of a timecode must be from 0 up to 60"));
        }
        whole as f64 * 60.0 + secs
    } else if let Some(ms) = t.strip_suffix("ms") {
        ms.parse::<f64>().map_err(|_| bad(text, "the part before ms is not a number"))? / 1000.0
    } else {
        t.strip_suffix('s').unwrap_or(t).parse::<f64>().map_err(|_| bad(text, "it is not a number"))?
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(bad(text, "a time cannot be negative"));
    }
    Ok((seconds * rate as f64).round() as u64)
}

/// A position on the timeline in frames.
pub fn parse_position(text: &str, scale: Scale) -> Result<u64> {
    let text = &plain_minus(text);
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("end") {
        let rest = rest.trim();
        if rest.is_empty() {
            return Ok(scale.total);
        }
        let Some(back) = rest.strip_prefix('-') else {
            return Err(bad(text, "after end comes a minus and a length, as in end-2s"));
        };
        let back = parse_duration(back, scale.rate)?;
        return scale.total.checked_sub(back).ok_or_else(|| {
            Error::usage(
                "range_out_of_bounds",
                format!("\"{text}\" is before the start of the timeline, which is {} long.", timecode(scale.total, scale.rate)),
            )
            .hint("Run `scrubr info <project>` to get the timeline length.")
        });
    }
    parse_duration(t, scale.rate)
}

/// A range on the timeline in frames, `start..end`. Bounds are checked by the engine.
pub fn parse_range(text: &str, scale: Scale) -> Result<(u64, u64)> {
    let text = &plain_minus(text);
    let Some(at) = text.find("..") else {
        return Err(Error::usage("bad_range", format!("Cannot read \"{text}\" as a range: it has no \"..\" between a start and an end."))
            .hint("Write a range as start..end, for example 12:03.25..12:09.9 or 0..end."));
    };
    let (start, end) = (text[..at].trim(), text[at + 2..].trim());
    let start = if start.is_empty() { 0 } else { parse_position(start, scale)? };
    let end = if end.is_empty() { scale.total } else { parse_position(end, scale)? };
    Ok((start, end))
}

/// A level in dB: `+2dB`, `-45dB`, `-1`.
pub fn parse_db(text: &str) -> Result<f64> {
    let text = &plain_minus(text);
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    let number = ["dbtp", "dbfs", "lufs", "db"].iter().find_map(|unit| lower.strip_suffix(unit)).unwrap_or(&lower).trim();
    match number.parse::<f64>() {
        Ok(db) if db.is_finite() => Ok(db),
        _ => Err(Error::usage("bad_level", format!("Cannot read \"{text}\" as a level in dB.")).hint("Write a level as a number with an optional unit, for example +2dB, -45dB or -16.")),
    }
}

/// `-45dB:1s`: a threshold and the shortest run that counts.
pub fn parse_silence(text: &str, rate: u32) -> Result<(f64, u64)> {
    let text = &plain_minus(text);
    let Some((level, length)) = text.split_once(':') else {
        return Err(Error::usage("bad_silence", format!("Cannot read \"{text}\" as a silence setting.")).hint("Write it as threshold:duration, for example -45dB:1s."));
    };
    Ok((parse_db(level)?, parse_duration(length, rate)?))
}

/// Seconds for JSON, to the microsecond. That is finer than one frame at any sample rate scrubr
/// handles, so a time read from a reply and passed back lands on the same frame.
pub fn seconds(frames: u64, rate: u32) -> f64 {
    (frames as f64 / rate as f64 * 1e6).round() / 1e6
}

/// `12:03.250`, or `1:02:03.250` past an hour.
pub fn timecode(frames: u64, rate: u32) -> String {
    let ms = (frames as f64 / rate as f64 * 1000.0).round() as u64;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{ms:03}") } else { format!("{m}:{s:02}.{ms:03}") }
}

pub fn range_text(range: (u64, u64), rate: u32) -> String {
    format!("{}..{}", timecode(range.0, rate), timecode(range.1, rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Scale = Scale { rate: 48_000, total: 48_000 * 3449 };

    #[test]
    fn every_form_in_the_prd_table_reads() {
        assert_eq!(parse_position("723.25", S).unwrap(), 34_716_000);
        assert_eq!(parse_position("12:03.250", S).unwrap(), 34_716_000);
        assert_eq!(parse_position("1:02:03.250", S).unwrap(), 48_000 * 3723 + 12_000);
        assert_eq!(parse_position("34716000f", S).unwrap(), 34_716_000);
        assert_eq!(parse_range("12:03.25..12:09.9", S).unwrap(), (34_716_000, 35_035_200));
        assert_eq!(parse_position("end", S).unwrap(), S.total);
        assert_eq!(parse_position("end-2s", S).unwrap(), S.total - 96_000);
    }

    #[test]
    fn the_other_spellings_read_too() {
        assert_eq!(parse_position("0", S).unwrap(), 0);
        assert_eq!(parse_position("0:41", S).unwrap(), 41 * 48_000);
        assert_eq!(parse_duration("0.5s", 48_000).unwrap(), 24_000);
        assert_eq!(parse_duration("5ms", 48_000).unwrap(), 240);
        assert_eq!(parse_duration("0", 48_000).unwrap(), 0);
        assert_eq!(parse_position("end-0:02", S).unwrap(), S.total - 96_000);
        assert_eq!(parse_position("end-96000f", S).unwrap(), S.total - 96_000);
        assert_eq!(parse_range("..0:41", S).unwrap(), (0, 41 * 48_000));
        assert_eq!(parse_range("58:10..", S).unwrap().1, S.total);
        assert_eq!(parse_range("0..end", S).unwrap(), (0, S.total));
        assert_eq!(parse_range("3.5..4.5", S).unwrap(), (168_000, 216_000));
        assert_eq!(parse_range("100f..200f", S).unwrap(), (100, 200));
    }

    #[test]
    fn unreadable_times_are_usage_errors_with_a_hint() {
        for text in ["", "abc", "-3", "1:2:3:4", "12:75", "1.5f", "end+2s", "endx", "1:-3"] {
            let err = parse_position(text, S).unwrap_err();
            assert_eq!((err.exit_code(), err.code()), (1, "bad_time"), "{text}");
            assert!(err.hint_text().unwrap().contains("12:03.250"));
        }
        assert_eq!(parse_range("12:03", S).unwrap_err().code(), "bad_range");
        assert_eq!(parse_position("end-2h", S).unwrap_err().code(), "bad_time");
        assert_eq!(parse_position("end-9999:00", S).unwrap_err().code(), "range_out_of_bounds");
    }

    #[test]
    fn levels_read_with_or_without_units() {
        assert_eq!(parse_db("+2dB").unwrap(), 2.0);
        assert_eq!(parse_db("-45dB").unwrap(), -45.0);
        assert_eq!(parse_db("-16").unwrap(), -16.0);
        assert_eq!(parse_db("-1dBTP").unwrap(), -1.0);
        assert_eq!(parse_db("-16 LUFS").unwrap(), -16.0);
        assert!(parse_db("loud").is_err());
        assert_eq!(parse_silence("-45dB:1s", 48_000).unwrap(), (-45.0, 48_000));
        assert_eq!(parse_silence("-30:250ms", 48_000).unwrap(), (-30.0, 12_000));
        // As the command line hands negative numbers over.
        assert_eq!(parse_silence("\u{2212}45dB:1s", 48_000).unwrap(), (-45.0, 48_000));
        assert_eq!(parse_db("\u{2212}3dB").unwrap(), -3.0);
        assert!(parse_duration("\u{2212}3", 48_000).unwrap_err().to_string().contains("-3"));
        assert!(parse_silence("-45dB", 48_000).is_err());
    }

    #[test]
    fn a_time_from_a_reply_lands_on_the_same_frame() {
        for rate in [8_000u32, 44_100, 48_000, 96_000, 192_000] {
            for frame in [0u64, 1, 7, 44_099, 153_601, 34_716_001, 172_703_999, 9_999_999_937] {
                let text = format!("{}", seconds(frame, rate));
                assert_eq!(parse_duration(&text, rate).unwrap(), frame, "{frame} at {rate} Hz printed as {text}");
            }
        }
        assert_eq!(timecode(34_716_000, 48_000), "12:03.250");
        assert_eq!(timecode(48_000 * 3723 + 12_000, 48_000), "1:02:03.250");
        assert_eq!(range_text((0, 24_000), 48_000), "0:00.000..0:00.500");
    }
}
