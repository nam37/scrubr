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
| `scrubr analyze P --quietest R [--quietest R ...]` | The quietest moment in each range, as `at`: where to put a cut between two words |
| `scrubr render P --png out.png [--range R] [--width 1600] [--height 480]` | Waveform image. The reply gives `start` and `seconds_per_pixel`, so pixel x is at `start + x * seconds_per_pixel` |
| `scrubr transcribe P [--range R] [--model base.en]` | Every word spoken, with `start` and `end`. Only in builds with the transcription feature; other builds say so |
| `scrubr cut P R` | Remove a range |
| `scrubr keep P R` | Remove everything outside a range |
| `scrubr mute P R` | Silence a range and keep its length |
| `scrubr gain P +2dB [--range R]` | Change level, of everything or of a range |
| `scrubr fade P in 0.5s` / `fade P out 2s` / `fade P in --range R` | Fade in from the start, out to the end, or across a range |
| `scrubr normalize P --lufs -16 [--ceiling -1]` / `--peak -1dB` | Set loudness or peak |
| `scrubr insert P sting.wav --at 0` | Insert a file at a position (`--at end` appends) |
| `scrubr track P add guest.wav [--name N] [--at T]` | Lay a file on a new track. Nothing on the timeline moves. `--track T` adds it to a track that is there |
| `scrubr track P set T [--gain -3dB] [--pan -0.3] [--mute\|--unmute] [--solo\|--unsolo] [--name N]` | A track's level, pan, mute and solo in the mix |
| `scrubr track P list` / `track P rm T` | Tracks and where their audio sits now; take a track out |
| `scrubr apply P --ops ops.json` | Several operations as one edit. `--ops -` reads stdin |
| `scrubr undo P` / `redo P` | Move head back or forward one edit (`--steps N`) |
| `scrubr undo P --edit e3` / `--of-session s7` | Take out specific edits, or a whole session, from anywhere in the log |
| `scrubr history P` | Every edit with author, session and state |
| `scrubr label P add R "text"` / `label P list` / `label P rm l1` | Notes for the reviewer. A single time instead of a range marks a point |
| `scrubr export P out.flac [--bits 16\|24\|32\|32f] [--range R]` | Render to `.wav` or `.flac` |
| `scrubr open P` | Open the project in the window, for the person to review. Returns at once |

Edit commands (`cut` to `redo`, and `label add`/`rm`) accept `--dry-run`, `--if-version N`,
`--author` and `--session`, and so do `track add`, `set` and `rm`. `scrubr open` needs the
window's program, `scrubr-gui`, beside `scrubr`; without it the reply is `gui_not_found`. Not in
this build: MP3 export.

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

## Tracks

A project has a main track, `main`, whose audio is the timeline, and any number of other tracks
laid over it: a second microphone, a music bed. Export and playback are the mix of them all. Name
a track by its name or its id (`t2`).

- **Time is shared.** `cut`, `keep` and `insert` act on every track, so tracks stay in step.
  Nothing cuts one track alone. To silence one track over a range and keep its length, use
  `mute R --track T`.
- **Level is per track.** `gain`, `fade`, `mute` and `normalize` take `--track T` and then change
  that track alone; without it they change every track. `normalize --track T` measures that track
  alone. In a batch, add `"track": "guest"` to the operation.
- **Adding audio.** `track add` lays a file over the timeline and moves nothing. A file that runs
  past the end makes the timeline longer, with silence on the other tracks.
  `insert --track T` is new time: the file on that track and silence on the others.
- **The mix.** `track set` gives a track a level, a pan from -1 (left) to 1 (right), mute and
  solo. At pan 0 a track is untouched; moving to one side turns the other side down. These are
  edits like any other: they are in `history` and can be undone.
- **Measuring one track.** `analyze`, `render`, `transcribe` and `export` take `--track T` and
  then work on that track heard alone, at its own level and pan, whatever is muted or soloed.
  With a microphone on each track, `transcribe --track T` gives one speaker's words.
- **The window** shows a lane for each track, with its mute, solo, level and pan. The person
  can change those there, and each change is an edit in `history` under their name.

## What edits do

- **Joins.** `cut`, `keep` and `insert` crossfade 5 ms across each join, and a trimmed start or
  end gets a 5 ms fade, because you cannot hear a click. `mute` ramps 5 ms inside each end of its
  range. `--crossfade 0` gives hard, sample-exact edges. Crossfades do not change the length.
- **Normalize** never pushes the true peak past its ceiling (-1 dBTP unless `--ceiling` says
  otherwise). If the target needs more gain than that, the gain stops there and the reply says
  `"limited_by": "true_peak"` with the `achieved_lufs`. Report that to the person; do not add gain
  on top.
- **Silence ranges** from `analyze --silence` are exact. Cutting them whole butts words together;
  leave some of each pause, for example 0.15 s at each end. The threshold is a fixed level, not
  relative to the recording. In a quiet recording, or under a quiet speaker, speech itself falls
  below -45 dB and is reported as silence. Run `analyze --loudness` first; if the integrated
  loudness is below about -30 LUFS, lower the threshold (-55 dB suited a recording at -41 LUFS)
  and check a few of the ranges with `--envelope` or `transcribe --range` before cutting.
- **Transcripts.** `transcribe` runs a local speech model, several times faster than real time,
  and prints progress on stderr. Expect some wrong words, and many where people talk over each
  other. A sound can come back as a word, such as `MUSIC`. A word can have the same `start` and
  `end`. Word times are typically within 0.1 s, but about one in ten is off by 0.5 s or more, so
  do not cut on them directly.
- **Removing a phrase.** Find it in `words`. For each end of the cut, take the word inside and
  the word outside, and run `analyze --quietest A..B`, with A 0.3 s before the end of the
  earlier word and B 0.3 s after the start of the later one. Cut at the `at` it returns: the
  middle of the quietest stretch between the two words (`start`, `end` and `rms_dbfs` describe
  that stretch). It uses no threshold, so it works on quiet recordings and where talk is too
  fast for `--silence` to find a pause. Give both ends in one command. Then run
  `transcribe --range` across each join and compare with the first transcript: the phrase must
  be gone and the words either side still there. If not, `undo` and label the place for the
  person. On hard recordings about one end in six lands a word out.
- **Models.** If the reply is `model_not_installed`, tell the person;
  `scrubr transcribe --fetch-model base.en` downloads it once.
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
