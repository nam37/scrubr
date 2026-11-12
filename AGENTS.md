# scrubr agent guide

scrubr is an audio editor you drive from a shell. A project file (`ep42.scrubr`) holds a list of
edits over source audio files. The sources are never modified, and audio is rendered only by
`export`, so commands are fast and every edit can be undone. A person reviews the same project
file, so say who you are and leave notes where you are unsure.

`scrubr help --agent` prints this guide. `scrubr help <command>` lists a command's arguments.

## Rules

- **Output.** Replies are JSON when stdout is not a terminal, or with `--json`. Every reply has
  `ok` and `project_version`. On failure `ok` is false and `error` holds `code`, `message` and
  `hint`. The hint says what to run next.
- **Say who you are.** Set `SCRUBR_AUTHOR` and `SCRUBR_SESSION` once per run (or pass `--author`
  and `--session`). Every edit records them, and the person can undo your whole session at once.
- **Versions.** `project_version` goes up on every write. Positions you worked out from a reply
  are valid only for that version. Pass it back with `--if-version N` on the edit. If the project
  changed in between, the command exits with code 3 and writes nothing: re-read, recompute, retry.
- **Dry runs.** `--dry-run` on any edit reports what would change and writes nothing.
- **You cannot listen.** Measure before and after with `analyze` and `render --png`, and label
  anything you are unsure of instead of guessing.

## Time

Commands address the timeline: the audio as it plays now, after all edits.

| Form | Example | Meaning |
|---|---|---|
| Seconds | `723.25`, `0.5s`, `250ms` | From the start of the timeline |
| Timecode | `12:03.250`, `1:02:03.250` | Minutes or hours, seconds, milliseconds |
| Frames | `34716000f` | Exact sample position |
| End | `end`, `end-2s` | Relative to the end of the timeline |
| Range | `12:03.25..12:09.9`, `..0:41`, `58:10..` | Start and end in any form; an empty side is the start or the end |

Times in replies are seconds to the microsecond. Pass them back unchanged and they land on the
same frame. Lengths (`--crossfade`, fade length, `--envelope`) take the same forms; a bare number
is seconds, so write `5ms`, not `5`.

## Commands

| Command | What it does |
|---|---|
| `scrubr new P --from a.wav [b.wav ...]` | Create a project. WAV, FLAC, MP3, OGG and AAC (.m4a) are read; files at another rate are resampled |
| `scrubr info P` | Timeline length, format, sources, head, version |
| `scrubr analyze P [--loudness] [--peaks] [--silence -45dB:1s] [--envelope 1s] [--range R]` | Measurements. With no flags: loudness and peaks |
| `scrubr render P --png out.png [--range R] [--width 1600] [--height 480]` | Waveform image. The reply gives `start` and `seconds_per_pixel`, so pixel x is at `start + x * seconds_per_pixel` |
| `scrubr transcribe P [--range R] [--model base.en]` | Every word spoken, with `start` and `end`. Only in builds with the transcription feature; other builds say so |
| `scrubr cut P R` | Remove a range |
| `scrubr keep P R` | Remove everything outside a range |
| `scrubr mute P R` | Silence a range and keep its length |
| `scrubr gain P +2dB [--range R]` | Change level, of everything or of a range |
| `scrubr fade P in 0.5s` / `fade P out 2s` / `fade P in --range R` | Fade in from the start, out to the end, or across a range |
| `scrubr normalize P --lufs -16 [--ceiling -1]` / `--peak -1dB` | Set loudness or peak |
| `scrubr insert P sting.wav --at 0` | Insert a file at a position (`--at end` appends) |
| `scrubr apply P --ops ops.json` | Several operations as one edit. `--ops -` reads stdin |
| `scrubr undo P` / `redo P` | Move head back or forward one edit (`--steps N`) |
| `scrubr undo P --edit e3` / `--of-session s7` | Take out specific edits, or a whole session, from anywhere in the log |
| `scrubr history P` | Every edit with author, session and state |
| `scrubr label P add R "text"` / `label P list` / `label P rm l1` | Notes for the reviewer. A single time instead of a range marks a point |
| `scrubr export P out.flac [--bits 16\|24\|32\|32f] [--range R]` | Render to `.wav` or `.flac` |

Edit commands (`cut` to `redo`, and `label add`/`rm`) accept `--dry-run`, `--if-version N`,
`--author` and `--session`. Not in this build: `scrubr open` (the GUI) and MP3 export.

## Batches

`apply` takes a JSON array of operations, or `{"crossfade": "5ms", "ops": [...]}`:

```json
[
  { "op": "cut", "start": 3.2, "end": 4.85 },
  { "op": "cut", "range": "6:52.1..6:54.02" },
  { "op": "keep", "range": "0:41..58:10" },
  { "op": "mute", "range": "30:12..30:13.4" },
  { "op": "gain", "db": 2, "range": "0..0:41" },
  { "op": "fade", "dir": "in", "length": "0.5s" },
  { "op": "insert", "file": "sting.wav", "at": 0 },
  { "op": "normalize", "lufs": -16 }
]
```

**Batch rule: every range in a batch refers to the timeline as it was before the batch.** Take
all the ranges from one `analyze` reply and cut them in one `apply`; do not shift later ranges to
allow for earlier cuts. Outside a batch each command sees the timeline the previous one left, so
positions after a cut move earlier by its length. A batch is one edit and one undo step.
`normalize` in a batch measures the audio as the operations before it leave it.

## What edits do

- **Joins.** `cut`, `keep` and `insert` crossfade 5 ms across each join, and a trimmed start or
  end gets a 5 ms fade, because you cannot hear a click. `mute` ramps 5 ms inside each end of its
  range. `--crossfade 0` gives hard, sample-exact edges. Crossfades do not change the length.
- **Normalize** never pushes the true peak past its ceiling (-1 dBTP unless `--ceiling` says
  otherwise). If the target needs more gain than that, the gain stops there and the reply says
  `"limited_by": "true_peak"` with the `achieved_lufs`. Report that to the person; do not add gain
  on top.
- **Silence ranges** from `analyze --silence` are exact. Cutting them whole butts words together;
  leave some of each pause, for example 0.15 s at each end.
- **Transcripts.** `transcribe` runs a local speech model, at several times real time, and
  prints progress on stderr. Word times are typically within 0.1 s but can be off by more, so do
  not cut on them directly: find the phrase in `words`, then move each end to the middle of the
  nearest pause from `analyze --silence -45dB:0.25s`. If the reply is `model_not_installed`,
  tell the person; `scrubr transcribe --fetch-model base.en` downloads it once.
- **Undo.** After `undo`, a new edit discards the undone ones. `undo --edit` and `--of-session`
  add a revert instead, so later edits and labels stay where they are; undo the revert to bring
  the edits back.
- **Labels** stick to the audio, not to a time. They move when earlier audio is cut.

## Exit codes

| Code | Meaning | What to do |
|---|---|---|
| 0 | Success | Continue |
| 1 | Bad usage: unknown flag, unreadable time, range outside the timeline, nothing to undo | Fix the command; read `error.hint` |
| 2 | Project or file not found | Check the path |
| 3 | Version conflict, or another writer held the project lock | Re-read the project, recompute positions, retry |
| 4 | A source file changed on disk | Stop and tell the person |
| 5 | Decode, encode or file I/O failure | Report the file and format |

## Worked example

Clean up a podcast: cut long pauses, set loudness, fade the ends, export.

```sh
export SCRUBR_AUTHOR=codex SCRUBR_SESSION=s7
scrubr analyze ep42.scrubr --loudness --silence -45dB:1s > report.json
# report.json: {"ok": true, "project_version": 2, "timeline_seconds": 3449.0,
#   "loudness": {"integrated_lufs": -21.4, "true_peak_dbtp": -7.2, "range_lu": 8.1},
#   "silence": [{"start": 3.2, "end": 4.85}, {"start": 412.1, "end": 414.02}], "silence_count": 38}
# Write cuts.json from report.silence, keeping 0.15 s of each pause at both ends:
#   [{"op": "cut", "start": 3.35, "end": 4.7}, {"op": "cut", "start": 412.25, "end": 413.87}]
scrubr apply ep42.scrubr --ops cuts.json --if-version 2
scrubr normalize ep42.scrubr --lufs -16
scrubr fade ep42.scrubr in 0.5s
scrubr fade ep42.scrubr out 2s
scrubr label ep42.scrubr add 14:18.5..14:20.2 "check: cough?"
scrubr render ep42.scrubr --png check.png
scrubr analyze ep42.scrubr --loudness --peaks
scrubr export ep42.scrubr out/ep42.flac
```

If `apply` exits 3, someone edited the project after your analysis. Run `analyze` again and
build the cuts from the new reply.
