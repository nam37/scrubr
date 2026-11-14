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
| `scrubr analyze P --pauses [0.3s] [--range R]` | Every pause of that length or more. Quiet is found from the recording, so there is no level to choose |
| `scrubr analyze P --joins [--range R]` | Every join the edits have left: in quiet or on sound, the words either side. `check` marks the ones to look at |
| `scrubr render P --png out.png [--range R] [--width 1600] [--height 480]` | Waveform image. The reply gives `start` and `seconds_per_pixel`, so pixel x is at `start + x * seconds_per_pixel` |
| `scrubr render P --png out.png --spectrogram [--min-hz 40] [--max-hz N]` | Spectrogram image: what kind of sound, not only how loud. For hum, hiss and clicks |
| `scrubr transcribe P [--range R] [--model base.en]` | Every word spoken, with `start` and `end`. What it hears is kept. Only in builds with the transcription feature; other builds say so |
| `scrubr transcribe P --saved [--range R]` | The words kept from earlier runs, where they are on the timeline now. Instant, in any build |
| `scrubr find P "words" [--within R]` | Where a phrase is said, and the cut that would take it out. Changes nothing. Needs the transcription feature |
| `scrubr cut P R` | Remove a range |
| `scrubr cut P --phrase "words" [--within R] [--occurrence N \| --all]` | Remove a phrase by its words, checked before it is written. Needs the transcription feature |
| `scrubr keep P R` | Remove everything outside a range |
| `scrubr tighten P [--over 0.6s] [--to 0.3s] [--range R]` | Shorten every pause longer than `--over` to `--to`, as one edit |
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
  can change those there, mute a selection on one track, and remove a track. Each change is an
  edit in `history` under their name.

## What edits do

- **Joins.** `cut`, `keep` and `insert` crossfade 5 ms across each join, and a trimmed start or
  end gets a 5 ms fade, because you cannot hear a click. `mute` ramps 5 ms inside each end of its
  range. `--crossfade 0` gives hard, sample-exact edges. Crossfades do not change the length.
- **Normalize** never pushes the true peak past its ceiling (-1 dBTP unless `--ceiling` says
  otherwise). If the target needs more gain than that, the gain stops there and the reply says
  `"limited_by": "true_peak"` with the `achieved_lufs`. Report that to the person; do not add gain
  on top.
- **Pauses.** `analyze --pauses` and `tighten` judge quiet against the recording itself. They
  measure where its quiet and its speech sit, and a moment is quiet when it is a little above
  the one and well under the other. The reply's `levels` gives all three figures. A soft
  recording is read the same way as a loud one, and there is no level for you to choose.
  - `scrubr tighten P` cuts the middle out of every pause longer than `--over` and leaves `--to`
    of it, half at each end. It is one edit and one undo step. Run it with `--dry-run` first:
    the reply lists each pause and the cut it would make.
  - The quiet before the first sound and after the last is not a pause, and `tighten` leaves it.
    `--pauses` lists it with `"at_edge": true`; trim it with `keep`.
  - A breath or a knock in a pause is sound, so that pause counts as two shorter ones.
  - With a microphone on each track, listen to the mix, which is the default: a pause is when
    nobody speaks. `--heard-on T` listens to one track; the cuts are still made on every track.
- **Checking joins.** After cutting, `scrubr analyze P --joins` reads every join back. `heard`
  is `quiet`, `low` or `sound`: what plays within 20 ms of the join, against the recording's own
  levels. From the kept transcript come `before` and `after`, the words either side, `doubled`
  when they are the same word, and `clipped`, the words the join runs through. `"check": true`
  marks a join that is not in quiet, or doubles a word, and `joins_to_check` counts them: look
  at those with `render` or `transcribe --range`, then fix or label them. A join in quiet names
  no clipped word, because word times are loose and the level is not. With no transcript kept,
  `has_words` is false and only the levels are given.
- **Spectrograms.** `render --spectrogram` draws time across and frequency up the image, low
  at the bottom, on a logarithmic scale; brighter is louder. A hum is a level line low down
  (50 or 60 Hz and its multiples), hiss a haze across the top that stays through the pauses,
  a click a thin upright line, speech a stack of wavering bands. The picture has a ruler above
  and a scale at the left, so read a pixel through the reply's `plot`: time is
  `start + (x - plot.left) * seconds_per_pixel`, and frequency is
  `top_hz * (bottom_hz / top_hz) ^ ((y - plot.top) / (plot.height - 1))`. `level` says which
  levels are black and brightest. Each column shows the most that sounded at each pitch in its
  time, so a click still shows in a wide view. `--track T` and `--range R` work as for a waveform.
- **Silence ranges** from `analyze --silence` are exact, and their threshold is a fixed level
  that you choose. In a quiet recording speech itself falls below -45 dB and is reported as
  silence, so for pauses in speech use `--pauses`. Cutting a range whole butts words together;
  leave some of each pause, for example 0.15 s at each end.
- **Transcripts.** `transcribe` runs a local speech model, several times faster than real time,
  and prints progress on stderr. Expect some wrong words, and many where people talk over each
  other. A sound can come back as a word, such as `MUSIC`. A word can have the same `start` and
  `end`. Word times are typically within 0.1 s, but about one in ten is off by 0.5 s or more, so
  do not cut on them directly. The model starts a word early when a pause comes before it, so
  `transcribe` moves those times to the edge of the pause as the level shows it
  (`times_moved_out_of_pauses` counts them), and runs the last word before a pause on to where
  the pause begins. The start of a word after a pause is the truest time in a transcript; times
  inside running speech are as the model gave them.
- **Kept transcripts.** What `transcribe` hears is kept against the audio, the way labels are.
  After a cut, an insert or an undo, `scrubr transcribe P --saved` gives the words where they are
  now without listening again: the words that were cut are gone and the rest have moved with
  their audio. A word with less than half of it left counts as cut. Its `covered` lists the ranges that have been listened to; audio outside them,
  such as a file inserted since, has no words yet, so run `transcribe --range` over it. Listening
  to a range again replaces its words. `--track T` keeps a transcript of that track by itself.
  The window draws the kept words along the waveform for the person.
- **Removing a phrase.** `scrubr cut P --phrase "you know"` does it in one step. It transcribes,
  finds the words, cuts at the quietest moment either side (never on the word times), and
  listens across the join as it would sound before writing anything. If the words either side
  would not be left as they were, it writes nothing and replies `cut_not_clean` (exit 1): label
  the place for the person and move on. A sentence between pauses cuts cleanly; a few words in
  the middle of running speech often do not, and are refused.
  - On a long recording give `--within START..END`, a window of a minute or so around the
    phrase from an earlier `transcribe`. Without it the whole timeline is transcribed first.
  - Said more than once: the reply is `phrase_ambiguous` and lists the places. Pick one with
    `--occurrence N` or take them all with `--all`, which is one edit.
  - `--dry-run` listens and checks and writes nothing. `scrubr find` only lists the places.
  - `--heard-on T` listens for the phrase on one track. The cut is still made on every track.
  - Each cut shortens the timeline, so times after it move earlier by `removed_seconds`.
  - The model has to hear the words as you wrote them. If the reply is `phrase_not_found`, look
    at `transcribe` for how they were heard.
  - By hand, the same rule: for each end run `analyze --quietest A..B`, with A 0.3 s before the
    end of the earlier word and B 0.3 s after the start of the later one, cut at the `at` it
    returns, then `transcribe --range` across the join to check.
- **Models.** If the reply is `model_not_installed`, tell the person;
  `scrubr transcribe --fetch-model base.en` downloads it once. `scrubr transcribe --models`
  lists the models there are, the size of each download and whether it is installed.
  `--model NAME` on `transcribe`, `find` and `cut --phrase` uses one. Downloading is the
  person's call: the larger ones are one to six gigabytes.
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

`scrubr tighten ep42.scrubr --over 1s --to 0.3s` does the first three steps as one command,
with quiet found from the recording. Follow either with `scrubr analyze ep42.scrubr --joins`
and look at any join it marks.
