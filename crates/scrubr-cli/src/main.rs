//! The `scrubr` command line: argument and time parsing over `scrubr-core`, with JSON output
//! for agents and text for people.

mod commands;
mod ops;
mod time;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

use scrubr_core::Error;

/// The agent guide. The repo ships the same file as `AGENTS.md`.
pub const AGENT_GUIDE: &str = include_str!("../../../AGENTS.md");

#[derive(Parser)]
#[command(
    name = "scrubr",
    version,
    about = "An audio editor for agents and people. Edits are non-destructive: commands change a project file, and audio is rendered only on export.",
    after_help = "Agents: run `scrubr help --agent` for the one-page guide.",
    disable_help_subcommand = true
)]
struct Cli {
    /// Print JSON even when stdout is a terminal. JSON is the default when it is not.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

/// Flags every command that writes the project accepts.
#[derive(Args, Clone, Debug)]
pub struct EditOpts {
    /// Report what would change without writing.
    #[arg(long)]
    pub dry_run: bool,

    /// Refuse the edit, with exit code 3, unless the project is at this version.
    #[arg(long, value_name = "N")]
    pub if_version: Option<u64>,

    /// Who is making the edit. Recorded on the edit.
    #[arg(long, env = "SCRUBR_AUTHOR", value_name = "NAME")]
    pub author: Option<String>,

    /// The run this edit belongs to. Recorded on the edit, and what a session undo goes by.
    #[arg(long, env = "SCRUBR_SESSION", value_name = "ID")]
    pub session: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Direction {
    In,
    Out,
}

#[derive(Subcommand)]
enum Command {
    /// Create a project from one or more audio files and build the peak cache.
    New {
        project: PathBuf,
        /// Audio files, placed end to end. The first sets the sample rate and channel count.
        #[arg(long, required = true, num_args = 1.., value_name = "FILE")]
        from: Vec<PathBuf>,
        /// Replace an existing project file.
        #[arg(long)]
        force: bool,
        #[arg(long, env = "SCRUBR_AUTHOR", value_name = "NAME")]
        author: Option<String>,
        #[arg(long, env = "SCRUBR_SESSION", value_name = "ID")]
        session: Option<String>,
    },
    /// Timeline length, format, sources, head and version.
    Info { project: PathBuf },
    /// Measurements as JSON: loudness, peaks, silence, level envelope.
    Analyze {
        project: PathBuf,
        /// Integrated loudness, loudness range and true peak (EBU R128).
        #[arg(long)]
        loudness: bool,
        /// Sample peak per channel, clipped sample count and ranges.
        #[arg(long)]
        peaks: bool,
        /// Ranges under a threshold for at least a duration, as THRESHOLD:DURATION (-45dB:1s).
        #[arg(long, value_name = "SPEC")]
        silence: Option<String>,
        /// RMS level in dBFS per window of this length (1s).
        #[arg(long, value_name = "WINDOW")]
        envelope: Option<String>,
        /// Measure only this part of the timeline.
        #[arg(long, value_name = "RANGE")]
        range: Option<String>,
    },
    /// Waveform image of the timeline or a range.
    Render {
        project: PathBuf,
        /// Where to write the PNG.
        #[arg(long, value_name = "FILE")]
        png: PathBuf,
        #[arg(long, value_name = "RANGE")]
        range: Option<String>,
        #[arg(long, default_value_t = 1600)]
        width: u32,
        #[arg(long, default_value_t = 480)]
        height: u32,
    },
    /// Words with start and end times as JSON. Needs the optional transcription feature.
    Transcribe { project: PathBuf },
    /// Remove a range. The join gets a short crossfade.
    Cut {
        project: PathBuf,
        range: String,
        /// Crossfade length at the join. 0 gives a hard, sample-exact join. [default: 5ms]
        #[arg(long, value_name = "LENGTH")]
        crossfade: Option<String>,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// Remove everything outside a range.
    Keep {
        project: PathBuf,
        range: String,
        /// Fade length at each trimmed end. 0 leaves the ends as they are. [default: 5ms]
        #[arg(long, value_name = "LENGTH")]
        crossfade: Option<String>,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// Change the level of the timeline or a range.
    Gain {
        project: PathBuf,
        /// The change in dB, for example +2dB or -3.
        db: String,
        #[arg(long, value_name = "RANGE")]
        range: Option<String>,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// Fade in at the start, out at the end, or across a range.
    Fade {
        project: PathBuf,
        direction: Direction,
        /// How long the fade is, from the start for `in` or up to the end for `out`.
        length: Option<String>,
        /// Fade across this range instead.
        #[arg(long, value_name = "RANGE", conflicts_with = "length")]
        range: Option<String>,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// Normalize to integrated loudness or to a peak. Gain stops at a true-peak ceiling.
    Normalize {
        project: PathBuf,
        /// Target integrated loudness in LUFS, for example -16.
        #[arg(long, value_name = "LUFS", conflicts_with = "peak")]
        lufs: Option<String>,
        /// Target sample peak in dBFS, for example -1dB.
        #[arg(long, value_name = "DB")]
        peak: Option<String>,
        /// The true-peak ceiling in dBTP. [default: -1]
        #[arg(long, value_name = "DBTP")]
        ceiling: Option<String>,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// Replace a range with silence and keep its length.
    Mute {
        project: PathBuf,
        range: String,
        /// Ramp length inside each end of the range. 0 gives hard edges. [default: 5ms]
        #[arg(long, value_name = "LENGTH")]
        crossfade: Option<String>,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// Insert another file, resampled if its rate differs.
    Insert {
        project: PathBuf,
        file: PathBuf,
        /// Where on the timeline the file starts. `end` appends.
        #[arg(long, value_name = "TIME")]
        at: String,
        /// Crossfade length at both joins. [default: 5ms]
        #[arg(long, value_name = "LENGTH")]
        crossfade: Option<String>,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// Several operations in one write and one undo step. Every range refers to the timeline
    /// as it was before the batch.
    Apply {
        project: PathBuf,
        /// A JSON file of operations, or - for stdin.
        #[arg(long, value_name = "FILE")]
        ops: PathBuf,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// Move head back one edit, or take specific edits or a whole session out of the log.
    Undo {
        project: PathBuf,
        /// How many edits to move head back.
        #[arg(long, default_value_t = 1, value_name = "N")]
        steps: usize,
        /// Revert these edits wherever they are in the log, leaving later edits in place.
        #[arg(long = "edit", value_name = "ID", value_delimiter = ',', conflicts_with = "steps")]
        edits: Vec<String>,
        /// Revert every edit made in this session, leaving other edits in place.
        #[arg(long, value_name = "SESSION", conflicts_with_all = ["steps", "edits"])]
        of_session: Option<String>,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// Move head forward again after an undo.
    Redo {
        project: PathBuf,
        #[arg(long, default_value_t = 1, value_name = "N")]
        steps: usize,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// List edits with author and session.
    History { project: PathBuf },
    /// Leave, list or remove notes for the person reviewing.
    Label {
        project: PathBuf,
        #[command(subcommand)]
        action: LabelAction,
    },
    /// Render head to WAV or FLAC.
    Export {
        project: PathBuf,
        /// The output file. Its extension picks the format: .wav or .flac.
        out: PathBuf,
        /// Sample format: 16, 24, 32 or 32f (float, WAV only). The default follows the source.
        #[arg(long, value_name = "BITS")]
        bits: Option<String>,
        /// Export only this part of the timeline.
        #[arg(long, value_name = "RANGE")]
        range: Option<String>,
    },
    /// Open the GUI. Not in this build yet.
    Open { project: Option<PathBuf> },
    /// Print help, or the agent guide with --agent.
    Help {
        /// Print the one-page guide for agents.
        #[arg(long)]
        agent: bool,
        /// A command to describe.
        command: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum LabelAction {
    /// Add a note on a range, or at a single time.
    Add {
        range: String,
        text: String,
        #[command(flatten)]
        edit: EditOpts,
    },
    /// List labels with where they sit on the timeline now.
    List,
    /// Remove a label by id.
    Rm {
        id: String,
        #[command(flatten)]
        edit: EditOpts,
    },
}

/// What a command prints: fields for JSON, and lines for a terminal.
#[derive(Debug)]
pub struct Reply {
    pub version: Option<u64>,
    pub fields: Map<String, Value>,
    pub text: String,
}

impl Reply {
    pub fn new(version: Option<u64>, fields: Value, text: impl Into<String>) -> Reply {
        let Value::Object(fields) = fields else { unreachable!("replies are JSON objects") };
        Reply { version, fields, text: text.into() }
    }
}

/// JSON with one top-level field per line and compact values, which stays readable for a
/// person and short for an agent.
fn print_json(fields: &Map<String, Value>) {
    let mut out = String::from("{\n");
    for (i, (key, value)) in fields.iter().enumerate() {
        let comma = if i + 1 < fields.len() { "," } else { "" };
        out.push_str(&format!("  {}: {value}{comma}\n", Value::String(key.clone())));
    }
    out.push('}');
    println!("{out}");
}

fn print_error(json: bool, code: &str, message: &str, hint: Option<&str>) {
    if json {
        let mut error = Map::new();
        error.insert("code".into(), json!(code));
        error.insert("message".into(), json!(message));
        if let Some(hint) = hint {
            error.insert("hint".into(), json!(hint));
        }
        let mut fields = Map::new();
        fields.insert("ok".into(), json!(false));
        fields.insert("error".into(), Value::Object(error));
        print_json(&fields);
    } else {
        eprintln!("error: {message}");
        if let Some(hint) = hint {
            eprintln!("hint: {hint}");
        }
    }
}

/// A leading minus on a number (`-3dB`, `-45dB:1s`, `-16`) would read as a flag. Swap it for the
/// minus sign U+2212, which the value parsers accept, so levels can be typed the natural way.
fn shield_negative_numbers(args: impl Iterator<Item = String>) -> Vec<String> {
    args.map(|arg| {
        let mut chars = arg.chars();
        match (chars.next(), chars.next(), chars.next()) {
            (Some('-'), Some(c), next) if c.is_ascii_digit() || (c == '.' && next.is_some_and(|n| n.is_ascii_digit())) => {
                format!("\u{2212}{}", &arg[1..])
            }
            _ => arg,
        }
    })
    .collect()
}

fn run(cli: Cli) -> Result<Reply, Error> {
    let Some(command) = cli.command else {
        return commands::open(None);
    };
    match command {
        Command::New { project, from, force, author, session } => commands::new(&project, &from, force, author, session),
        Command::Info { project } => commands::info(&project),
        Command::Analyze { project, loudness, peaks, silence, envelope, range } => commands::analyze(&project, loudness, peaks, silence, envelope, range),
        Command::Render { project, png, range, width, height } => commands::render(&project, &png, range, width, height),
        Command::Transcribe { project } => commands::transcribe(&project),
        Command::Cut { project, range, crossfade, edit } => commands::cut(&project, &range, crossfade, &edit, false),
        Command::Keep { project, range, crossfade, edit } => commands::cut(&project, &range, crossfade, &edit, true),
        Command::Gain { project, db, range, edit } => commands::gain(&project, &db, range, &edit),
        Command::Fade { project, direction, length, range, edit } => commands::fade(&project, direction, length, range, &edit),
        Command::Normalize { project, lufs, peak, ceiling, edit } => commands::normalize(&project, lufs, peak, ceiling, &edit),
        Command::Mute { project, range, crossfade, edit } => commands::mute(&project, &range, crossfade, &edit),
        Command::Insert { project, file, at, crossfade, edit } => commands::insert(&project, &file, &at, crossfade, &edit),
        Command::Apply { project, ops, edit } => commands::apply(&project, &ops, &edit),
        Command::Undo { project, steps, edits, of_session, edit } => commands::undo(&project, steps, edits, of_session, &edit),
        Command::Redo { project, steps, edit } => commands::redo(&project, steps, &edit),
        Command::History { project } => commands::history(&project),
        Command::Label { project, action } => commands::label(&project, action),
        Command::Export { project, out, bits, range } => commands::export(&project, &out, bits, range),
        Command::Open { project } => commands::open(project.as_deref()),
        Command::Help { agent: true, .. } => Ok(Reply::new(None, json!({ "guide": AGENT_GUIDE }), AGENT_GUIDE.trim_end())),
        Command::Help { command, .. } => {
            let mut app = Cli::command();
            let help = match &command {
                Some(name) => match app.find_subcommand_mut(name) {
                    Some(sub) => sub.render_long_help().to_string(),
                    None => {
                        return Err(Error::usage("unknown_command", format!("There is no command named \"{name}\"."))
                            .hint("Run `scrubr help` to list commands."));
                    }
                },
                None => app.render_long_help().to_string(),
            };
            Ok(Reply::new(None, json!({ "help": help }), help.trim_end()))
        }
    }
}

fn main() -> ExitCode {
    let args = shield_negative_numbers(std::env::args());
    let json = args.iter().any(|a| a == "--json") || !std::io::stdout().is_terminal();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind::{DisplayHelp, DisplayHelpOnMissingArgumentOrSubcommand, DisplayVersion};
            if matches!(e.kind(), DisplayHelp | DisplayVersion) {
                print!("{e}");
                return ExitCode::SUCCESS;
            }
            if json && e.kind() != DisplayHelpOnMissingArgumentOrSubcommand {
                let text = e.render().to_string().replace('\u{2212}', "-");
                let message = text.lines().next().unwrap_or("bad usage").trim_start_matches("error: ").to_string();
                print_error(true, "bad_usage", &message, Some("Run `scrubr help <command>` for the command's arguments, or `scrubr help --agent` for the guide."));
            } else {
                eprint!("{}", e.render().to_string().replace('\u{2212}', "-"));
            }
            return ExitCode::from(1);
        }
    };
    let json = json || cli.json;
    // The guide and help are text for reading: print them as they are unless JSON was asked for.
    let plain = matches!(cli.command, Some(Command::Help { .. })) && !cli.json;
    match run(cli) {
        Ok(reply) => {
            if json && !plain {
                let mut fields = Map::new();
                fields.insert("ok".into(), json!(true));
                if let Some(version) = reply.version {
                    fields.insert("project_version".into(), json!(version));
                }
                fields.extend(reply.fields);
                print_json(&fields);
            } else {
                println!("{}", reply.text);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            print_error(json, e.code(), &e.to_string(), e.hint_text());
            ExitCode::from(e.exit_code() as u8)
        }
    }
}
