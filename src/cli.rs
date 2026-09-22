//! Local configuration and roster inspection. Roster discovery reads only the
//! documented skill roots; there are no child, network, or state writes.
use crate::authorized_read::{AuthorizedRoot, AuthorizedRoots, ReadError};
use crate::config::{
    ConfigSources, MAX_LAYER_ENTRIES, RawValue, ResolvedConfig, SettingKey, ValueSource,
};
use crate::limits::{CONFIG_FILE_BYTES, DurationMillis};
use crate::output::OutputDocument;
use crate::runtime::EntryClock;
use clap::{Arg, ArgAction, Command};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

const HELP: &str = "SkillRanker — powered by TypeSafe.ai Jev\n\nUsage: sr [rank] [--context FILE | --transcript FILE --harness NAME | --session PATH | --latest]\n                 [--roster FILE] [--require-skill ID] [--dry-run [--shortlist-ids ID,...]]\n                 [--offline | --allow-network] [--json | --table]\n                 [--top N] [--shortlist M] [--gate FLOAT] [--fits FLOAT]\n                 [--explain] [--why-not ID] [--cursor TOKEN] [--no-tools] [--no-cache]\n                 [--save-case FILE]\n       sr doctor [--json | --table] [--offline | --allow-network]\n       sr doctor --config [--json | --table] [--top N] [--shortlist N]\n       sr roster [--json] [--limit N] [--cursor TOKEN]\n       sr roster --snapshot FILE | --diff FILE\n       sr capabilities [--json]\n       sr demo --case <useful|none|explicit|unavailable> [--json | --table]\n       sr replay FILE [--policy FILE] [--compare-policy FILE] [--json | --table]\n       sr eval --dataset FILE [--allow-network] [--max-runtime-ms MS] [--timeout-ms MS] [--policy FILE] [--compare-policy FILE] [--json | --table]\n       sr feedback <EVENT_ID> --skill ID [--instead ID] [--verdict <useful|not-useful|unknown>] [--reason CODE] [--provenance TEXT] [--expected-version GEN] [--dir DIR] [--json]\n       sr ledger <init|migrate|status|prune|clear> [--before TIME] [--apply] [--json]\n       sr observe [--context FILE | --transcript FILE --harness NAME | --session PATH] [--branch NAME] [--roster FILE] [--dir DIR] [--json]\n       sr stats [--since DURATION] [--by-skill] [--dir DIR] [--json | --table]\n       sr hook <claude> [--shadow] [--offline | --allow-network] [--dir DIR]\n       sr install-hook <claude> [--settings FILE] [--target-dir DIR] [--apply] [--json]\n       sr uninstall-hook <claude> [--settings FILE] [--target-dir DIR] [--apply] [--json]\n       sr --help | --version\n\nRank the next step of an agent session using TypeSafe Jev.\nRequires your own TypeSafe API key (TYPESAFE_API_KEY) and network consent (--allow-network).\n";

const EVAL_HELP: &str = "sr eval --dataset FILE [--allow-network] [--max-runtime-ms MS] [--timeout-ms MS] [--policy FILE] [--compare-policy FILE] [--json | --table]\n\nEvaluate recorded or synthetic replay batches against local or comparison policies with bounded runtime and explicit accounting.\n";

const STATS_HELP: &str = "sr stats [--since DURATION] [--by-skill] [--dir DIR] [--json | --table]\n\nReport observation and operational metrics across honest cohorts (evaluations, suggestions, abstentions, latency, loads, judgments, tokens, and cost).\n";

const FEEDBACK_HELP: &str = "sr feedback <EVENT_ID> --skill ID [--instead ID] [--verdict <useful|not-useful|unknown>] [--reason CODE] [--provenance TEXT] [--expected-version GEN] [--dir DIR] [--json]\n\nRecord explicit feedback or paired corrective labels for a historical ranking event.\n";

const OBSERVE_HELP: &str = "sr observe [--context FILE | --transcript FILE --harness NAME | --session PATH] [--branch NAME] [--roster FILE] [--dir DIR] [--json]\n\nIngest session tool events, record loaded skill observations, attribute to recent emissions, and advance observation watermark.\n";

fn command() -> Command {
    let mut doctor = Command::new("doctor")
        .disable_help_flag(true)
        .arg(
            Arg::new("help")
                .long("help")
                .short('h')
                .action(ArgAction::SetTrue),
        )
        .arg(Arg::new("config").long("config").action(ArgAction::SetTrue))
        .arg(
            Arg::new("json")
                .long("json")
                .conflicts_with("table")
                .action(ArgAction::SetTrue),
        )
        .arg(Arg::new("table").long("table").action(ArgAction::SetTrue))
        .arg(
            Arg::new("offline")
                .long("offline")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("allow-network")
                .long("allow-network")
                .action(ArgAction::SetTrue),
        );
    for key in SettingKey::ALL {
        if let Some(flag) = key.spec().cli_flag {
            let name = flag.trim_start_matches('-');
            let action = if matches!(name, "shadow" | "no-tools") {
                ArgAction::SetTrue
            } else {
                ArgAction::Set
            };
            doctor = doctor.arg(Arg::new(name).long(name).action(action));
        }
    }

    let mut rank = Command::new("rank")
        .disable_help_flag(true)
        .arg(
            Arg::new("help")
                .long("help")
                .short('h')
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("context")
                .long("context")
                .action(ArgAction::Set)
                .conflicts_with_all(["transcript", "session"]),
        )
        .arg(
            Arg::new("transcript")
                .long("transcript")
                .action(ArgAction::Set)
                .requires("harness")
                .conflicts_with_all(["context", "session"]),
        )
        .arg(
            Arg::new("harness")
                .long("harness")
                .action(ArgAction::Set)
                .requires("transcript")
                .conflicts_with_all(["context", "session"]),
        )
        .arg(
            Arg::new("session")
                .long("session")
                .action(ArgAction::Set)
                .conflicts_with_all(["context", "transcript", "harness"]),
        )
        .arg(
            Arg::new("latest")
                .long("latest")
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["context", "transcript", "session"]),
        )
        .arg(Arg::new("roster").long("roster").action(ArgAction::Set))
        .arg(
            Arg::new("require-skill")
                .long("require-skill")
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("explain")
                .long("explain")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("why-not")
                .long("why-not")
                .action(ArgAction::Set)
                .requires("explain"),
        )
        .arg(
            Arg::new("cursor")
                .long("cursor")
                .action(ArgAction::Set)
                .requires("explain"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .conflicts_with("table")
                .action(ArgAction::SetTrue),
        )
        .arg(Arg::new("table").long("table").action(ArgAction::SetTrue))
        .arg(
            Arg::new("offline")
                .long("offline")
                .conflicts_with("allow-network")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("allow-network")
                .long("allow-network")
                .conflicts_with("offline")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .conflicts_with_all(["allow-network", "save-case"])
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("shortlist-ids")
                .long("shortlist-ids")
                .value_name("ID")
                .num_args(1..)
                .value_delimiter(',')
                .action(ArgAction::Append)
                .requires("dry-run"),
        )
        .arg(
            Arg::new("no-cache")
                .long("no-cache")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("no-ledger")
                .long("no-ledger")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("no-persist")
                .long("no-persist")
                .conflicts_with("save-case")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("save-case")
                .long("save-case")
                .value_name("FILE")
                .help("Save a recorded case file atomically to the given path")
                .conflicts_with_all(["dry-run", "no-persist"])
                .action(ArgAction::Set),
        );
    for key in SettingKey::ALL {
        if let Some(flag) = key.spec().cli_flag {
            let name = flag.trim_start_matches('-');
            let action = if matches!(name, "shadow" | "no-tools") {
                ArgAction::SetTrue
            } else {
                ArgAction::Set
            };
            rank = rank.arg(Arg::new(name).long(name).action(action));
        }
    }

    let mut app = Command::new("sr")
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .arg(
            Arg::new("help")
                .long("help")
                .short('h')
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("version")
                .long("version")
                .short('V')
                .action(ArgAction::SetTrue)
                .conflicts_with("help"),
        )
        .subcommand(doctor)
        .subcommand(rank)
        .subcommand(
            Command::new("capabilities")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("json").long("json").action(ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("roster")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("json").long("json").action(ArgAction::SetTrue))
                .arg(Arg::new("limit").long("limit").action(ArgAction::Set))
                .arg(Arg::new("cursor").long("cursor").action(ArgAction::Set))
                .arg(
                    Arg::new("snapshot")
                        .long("snapshot")
                        .action(ArgAction::Set)
                        .conflicts_with_all(["diff", "limit", "cursor"]),
                )
                .arg(
                    Arg::new("diff")
                        .long("diff")
                        .action(ArgAction::Set)
                        .conflicts_with_all(["limit", "cursor"]),
                ),
        )
        .subcommand(
            Command::new("demo")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("case").long("case").value_parser([
                    "useful",
                    "none",
                    "explicit",
                    "unavailable",
                ]))
                .arg(
                    Arg::new("json")
                        .long("json")
                        .conflicts_with("table")
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("table").long("table").action(ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("replay")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("file")
                        .help("Path to the recorded or synthetic case file")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("policy")
                        .long("policy")
                        .value_name("FILE")
                        .help("Path to a local policy override file")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("compare-policy")
                        .long("compare-policy")
                        .value_name("FILE")
                        .help("Path to a comparison policy file")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .conflicts_with("table")
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("table").long("table").action(ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("eval")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("dataset")
                        .long("dataset")
                        .value_name("FILE")
                        .help("Path to the evaluation dataset (JSONL / JSON)")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("allow-network")
                        .long("allow-network")
                        .help("Explicit network authorization consent")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("max-runtime-ms")
                        .long("max-runtime-ms")
                        .value_name("MS")
                        .help("Maximum runtime deadline for the batch in milliseconds")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("timeout-ms")
                        .long("timeout-ms")
                        .value_name("MS")
                        .help("Optional per-case deadline in milliseconds")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("policy")
                        .long("policy")
                        .value_name("FILE")
                        .help("Path to a local policy override file")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("compare-policy")
                        .long("compare-policy")
                        .value_name("FILE")
                        .help("Path to a comparison policy file")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .conflicts_with("table")
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("table").long("table").action(ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("ledger")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("json").long("json").action(ArgAction::SetTrue))
                .subcommand(
                    Command::new("init")
                        .disable_help_flag(true)
                        .arg(
                            Arg::new("help")
                                .long("help")
                                .short('h')
                                .action(ArgAction::SetTrue),
                        )
                        .arg(Arg::new("json").long("json").action(ArgAction::SetTrue))
                        .arg(
                            Arg::new("dir")
                                .long("dir")
                                .help("Custom ledger directory")
                                .action(ArgAction::Set),
                        ),
                )
                .subcommand(
                    Command::new("migrate")
                        .disable_help_flag(true)
                        .arg(
                            Arg::new("help")
                                .long("help")
                                .short('h')
                                .action(ArgAction::SetTrue),
                        )
                        .arg(
                            Arg::new("apply")
                                .long("apply")
                                .help("Apply migrations after backup")
                                .action(ArgAction::SetTrue),
                        )
                        .arg(Arg::new("json").long("json").action(ArgAction::SetTrue))
                        .arg(
                            Arg::new("dir")
                                .long("dir")
                                .help("Custom ledger directory")
                                .action(ArgAction::Set),
                        ),
                )
                .subcommand(
                    Command::new("status")
                        .disable_help_flag(true)
                        .arg(
                            Arg::new("help")
                                .long("help")
                                .short('h')
                                .action(ArgAction::SetTrue),
                        )
                        .arg(Arg::new("json").long("json").action(ArgAction::SetTrue))
                        .arg(
                            Arg::new("dir")
                                .long("dir")
                                .help("Custom ledger directory")
                                .action(ArgAction::Set),
                        ),
                )
                .subcommand(
                    Command::new("prune")
                        .disable_help_flag(true)
                        .arg(
                            Arg::new("help")
                                .long("help")
                                .short('h')
                                .action(ArgAction::SetTrue),
                        )
                        .arg(
                            Arg::new("before")
                                .long("before")
                                .help("Cutoff date (YYYY-MM-DD), timestamp, or duration (30d)")
                                .action(ArgAction::Set),
                        )
                        .arg(
                            Arg::new("apply")
                                .long("apply")
                                .help("Apply the retention cleanup")
                                .action(ArgAction::SetTrue),
                        )
                        .arg(Arg::new("json").long("json").action(ArgAction::SetTrue))
                        .arg(
                            Arg::new("dir")
                                .long("dir")
                                .help("Custom ledger directory")
                                .action(ArgAction::Set),
                        ),
                )
                .subcommand(
                    Command::new("clear")
                        .disable_help_flag(true)
                        .arg(
                            Arg::new("help")
                                .long("help")
                                .short('h')
                                .action(ArgAction::SetTrue),
                        )
                        .arg(
                            Arg::new("apply")
                                .long("apply")
                                .help("Apply clearing all history")
                                .action(ArgAction::SetTrue),
                        )
                        .arg(Arg::new("json").long("json").action(ArgAction::SetTrue))
                        .arg(
                            Arg::new("dir")
                                .long("dir")
                                .help("Custom ledger directory")
                                .action(ArgAction::Set),
                        ),
                ),
        )
        .subcommand(
            Command::new("feedback")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("event_id")
                        .help("Attributed ranking event ID")
                        .index(1)
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("event")
                        .long("event")
                        .help("Attributed ranking event ID")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("skill")
                        .long("skill")
                        .help("Original target skill ID")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("instead")
                        .long("instead")
                        .help("Better alternative skill ID for paired correction")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("verdict")
                        .long("verdict")
                        .help("Single skill judgment verdict (useful, not-useful, or unknown)")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("reason")
                        .long("reason")
                        .help("Optional bounded reason code")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("provenance")
                        .long("provenance")
                        .help("Optional user provenance or tag")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("expected-version")
                        .long("expected-version")
                        .help("Expected schema/data generation for revision fencing")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("dir")
                        .long("dir")
                        .help("Custom ledger directory")
                        .action(ArgAction::Set),
                )
                .arg(Arg::new("json").long("json").action(ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("observe")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("context")
                        .long("context")
                        .action(ArgAction::Set)
                        .conflicts_with_all(["transcript", "session"]),
                )
                .arg(
                    Arg::new("transcript")
                        .long("transcript")
                        .action(ArgAction::Set)
                        .requires("harness")
                        .conflicts_with_all(["context", "session"]),
                )
                .arg(
                    Arg::new("harness")
                        .long("harness")
                        .action(ArgAction::Set)
                        .requires("transcript")
                        .conflicts_with_all(["context", "session"]),
                )
                .arg(
                    Arg::new("session")
                        .long("session")
                        .action(ArgAction::Set)
                        .conflicts_with_all(["context", "transcript", "harness"]),
                )
                .arg(Arg::new("branch").long("branch").action(ArgAction::Set))
                .arg(Arg::new("roster").long("roster").action(ArgAction::Set))
                .arg(Arg::new("dir").long("dir").action(ArgAction::Set))
                .arg(Arg::new("json").long("json").action(ArgAction::SetTrue))
                .arg(
                    Arg::new("no-ledger")
                        .long("no-ledger")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("no-persist")
                        .long("no-persist")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("hook")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .subcommand(
                    Command::new("claude")
                        .disable_help_flag(true)
                        .arg(
                            Arg::new("help")
                                .long("help")
                                .short('h')
                                .action(ArgAction::SetTrue),
                        )
                        .arg(Arg::new("shadow").long("shadow").action(ArgAction::SetTrue))
                        .arg(Arg::new("dir").long("dir").action(ArgAction::Set))
                        .arg(
                            Arg::new("workspace")
                                .long("workspace")
                                .action(ArgAction::Set),
                        )
                        .arg(
                            Arg::new("user-config-root")
                                .long("user-config-root")
                                .action(ArgAction::Set),
                        )
                        .arg(
                            Arg::new("allow-network")
                                .long("allow-network")
                                .conflicts_with("offline")
                                .action(ArgAction::SetTrue),
                        )
                        .arg(
                            Arg::new("offline")
                                .long("offline")
                                .conflicts_with("allow-network")
                                .action(ArgAction::SetTrue),
                        )
                        .arg(
                            Arg::new("no-cache")
                                .long("no-cache")
                                .action(ArgAction::SetTrue),
                        )
                        .arg(
                            Arg::new("no-ledger")
                                .long("no-ledger")
                                .action(ArgAction::SetTrue),
                        )
                        .arg(
                            Arg::new("no-persist")
                                .long("no-persist")
                                .action(ArgAction::SetTrue),
                        )
                        .arg(
                            Arg::new("timeout-ms")
                                .long("timeout-ms")
                                .action(ArgAction::Set),
                        ),
                ),
        )
        .subcommand(
            Command::new("install-hook")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .subcommand(
                    Command::new("claude")
                        .disable_help_flag(true)
                        .arg(
                            Arg::new("help")
                                .long("help")
                                .short('h')
                                .action(ArgAction::SetTrue),
                        )
                        .arg(Arg::new("apply").long("apply").action(ArgAction::SetTrue))
                        .arg(
                            Arg::new("settings-file")
                                .long("settings-file")
                                .action(ArgAction::Set),
                        )
                        .arg(
                            Arg::new("timeout-secs")
                                .long("timeout-secs")
                                .action(ArgAction::Set),
                        )
                        .arg(
                            Arg::new("binary-path")
                                .long("binary-path")
                                .action(ArgAction::Set),
                        ),
                ),
        )
        .subcommand(
            Command::new("uninstall-hook")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .subcommand(
                    Command::new("claude")
                        .disable_help_flag(true)
                        .arg(
                            Arg::new("help")
                                .long("help")
                                .short('h')
                                .action(ArgAction::SetTrue),
                        )
                        .arg(Arg::new("apply").long("apply").action(ArgAction::SetTrue))
                        .arg(
                            Arg::new("settings-file")
                                .long("settings-file")
                                .action(ArgAction::Set),
                        )
                        .arg(
                            Arg::new("timeout-secs")
                                .long("timeout-secs")
                                .action(ArgAction::Set),
                        )
                        .arg(
                            Arg::new("binary-path")
                                .long("binary-path")
                                .action(ArgAction::Set),
                        ),
                ),
        )
        .subcommand(
            Command::new("stats")
                .disable_help_flag(true)
                .arg(
                    Arg::new("help")
                        .long("help")
                        .short('h')
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("since")
                        .long("since")
                        .value_name("DURATION")
                        .help("Time window cutoff, e.g. 7d, 24h, 30m, or ISO timestamp")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("by-skill")
                        .long("by-skill")
                        .help("Break down metrics per skill")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("dir")
                        .long("dir")
                        .help("Custom ledger directory")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .conflicts_with("table")
                        .action(ArgAction::SetTrue),
                )
                .arg(Arg::new("table").long("table").action(ArgAction::SetTrue)),
        );
    for planned in crate::capabilities::PLANNED_FLAGS {
        app = app.mut_subcommand(planned.command, |command| {
            command.arg(Arg::new(planned.flag).long(planned.flag).hide(true).action(
                if planned.takes_value {
                    ArgAction::Set
                } else {
                    ArgAction::SetTrue
                },
            ))
        });
    }
    app
}

fn is_hook_claude_invocation(args: &[OsString]) -> bool {
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if let Some(s) = arg.to_str() {
            if s.starts_with('-') {
                continue;
            }
            if s == "hook"
                && let Some(sub) = iter.next()
            {
                return sub.to_str() == Some("claude");
            }
            break;
        }
    }
    false
}

/// Exit and streams are deliberately separate; diagnostics never echo clap/TOML input.
pub fn run(clock: EntryClock) -> u8 {
    let args: Vec<OsString> = std::env::args_os().collect();
    let is_hook_claude = is_hook_claude_invocation(&args);
    let is_help = args.iter().any(|arg| arg == "--help" || arg == "-h");
    let wants_json = args.iter().any(|arg| arg == "--json") || !io::stdout().is_terminal();
    let location = if let Some(dir) = try_extract_dir(&args) {
        crate::storage::LedgerLocation::Directory(dir)
    } else {
        crate::storage::LedgerLocation::Platform
    };
    match execute(&clock, args) {
        Ok(output) => {
            if is_hook_claude && !is_help {
                0
            } else {
                let bytes_len = output.len();
                match io::stdout().lock().write_all(output.as_bytes()) {
                    Ok(()) => {
                        if bytes_len > 0
                            && let Some(event_id) = try_extract_event_id(&output)
                        {
                            let _ = try_record_cli_emission(&clock, location, &event_id, bytes_len);
                        }
                        0
                    }
                    Err(_) => 1,
                }
            }
        }
        Err((code, kind, message)) => {
            if is_hook_claude {
                let _ = writeln!(io::stderr().lock(), "sr: {message}");
                return 0;
            } else if wants_json {
                if message.trim_start().starts_with('{')
                    && crate::output::OutputDocument::from_json(message.as_bytes()).is_ok()
                {
                    let _ = writeln!(io::stdout().lock(), "{message}");
                } else {
                    let error_kind = crate::output::ErrorKind::ALL
                        .iter()
                        .copied()
                        .find(|k| k.as_str() == kind)
                        .unwrap_or(match code {
                            2 => crate::output::ErrorKind::InvalidUsage,
                            3 => crate::output::ErrorKind::MissingSession,
                            4 => crate::output::ErrorKind::ProviderFailure,
                            5 => crate::output::ErrorKind::EmptyRoster,
                            6 => crate::output::ErrorKind::Timeout,
                            7 => crate::output::ErrorKind::MalformedInput,
                            8 => crate::output::ErrorKind::NetworkDenied,
                            9 => crate::output::ErrorKind::StorageFailure,
                            10 => crate::output::ErrorKind::InvalidProviderResponse,
                            11 => crate::output::ErrorKind::CacheMiss,
                            _ => crate::output::ErrorKind::InvalidUsage,
                        });
                    let doc = crate::output::OutputDocument::failure_with_details(
                        error_kind,
                        &message,
                        "Use sr --help; inspect trusted-user and project configuration.",
                        false,
                    );
                    let wire = doc
                        .to_json()
                        .unwrap_or_else(|_| serde_json::to_vec(doc.as_value()).unwrap());
                    let _ = io::stdout().lock().write_all(&wire);
                    let _ = writeln!(io::stdout().lock());
                }
            } else {
                let _ = writeln!(io::stderr().lock(), "sr: {message}");
            }
            code
        }
    }
}

/// Turns a storage refusal into something a person can act on.
///
/// The permission case is the one that was undiagnosable: "cache path does not satisfy owner-only
/// permissions" named no path, and the generic hint said to inspect trusted-user and project
/// configuration when the fix is a chmod on one directory. Locating it took four attempts in one
/// review and two in another, both times on a directory the harness had just created (sr-488b).
///
/// Everything else keeps its existing wording: this adds a sentence where one was missing rather
/// than rewriting error text across the CLI.
fn storage_failure(
    prefix: &str,
    error: &crate::storage::StoreError,
    location: &crate::storage::LedgerLocation,
) -> String {
    let base = format!("{prefix}: {error}");
    if !matches!(error, crate::storage::StoreError::Permissions) {
        return base;
    }
    let path = match location {
        crate::storage::LedgerLocation::Directory(dir) => Some(dir.clone()),
        crate::storage::LedgerLocation::Platform => crate::storage::default_ledger_directory().ok(),
    };
    // No path, or a chain that passes its own rules, means the refusal came from somewhere this
    // cannot see -- a read-only filesystem, say. Better to say nothing extra than to guess.
    let Some(refusal) = path
        .as_deref()
        .and_then(crate::storage::diagnose_owner_only)
    else {
        return base;
    };
    match refusal.remedy() {
        Some(remedy) => format!("{base}. {}. Fix with: {remedy}", refusal.describe()),
        None => format!("{base}. {}", refusal.describe()),
    }
}

fn try_extract_dir(args: &[OsString]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--dir" {
            if let Some(next) = iter.next() {
                return Some(PathBuf::from(next));
            }
        } else if let Some(s) = arg.to_str()
            && let Some(stripped) = s.strip_prefix("--dir=")
        {
            return Some(PathBuf::from(stripped));
        }
    }
    None
}

fn try_extract_event_id(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.starts_with('{')
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
    {
        if let Some(id) = val.get("event_id").and_then(|v| v.as_str()) {
            return Some(id.to_string());
        }
        if let Some(id) = val
            .get("local_decision")
            .and_then(|local| local.get("event_id"))
            .and_then(|v| v.as_str())
        {
            return Some(id.to_string());
        }
    }
    None
}

fn try_record_cli_emission(
    clock: &EntryClock,
    location: crate::storage::LedgerLocation,
    event_id: &str,
    bytes_written: usize,
) -> bool {
    let Ok(invocation) = crate::runtime::ProcessInvocation::from_clock(*clock) else {
        return false;
    };
    let Ok(cx) = invocation.request_cx() else {
        return false;
    };
    crate::storage::record_emission(
        &invocation,
        &cx,
        crate::storage::LedgerAccess::ExistingOnly,
        location,
        event_id,
        bytes_written,
    )
    .unwrap_or(false)
}

pub type Failure = (u8, &'static str, String);

fn finish_invocation<T>(
    invocation: crate::runtime::ProcessInvocation,
    outcome: Result<T, Failure>,
) -> Result<T, Failure> {
    let clock = invocation.clock();
    // Capture operation errors before consuming the runtime too: an early `?`
    // here would bypass its bounded shutdown and fall back to Runtime::drop.
    if invocation.shutdown() && clock.now() < clock.deadline().expires_at() {
        outcome
    } else {
        Err((
            6,
            "timeout",
            "Runtime cleanup did not finish within the invocation deadline".into(),
        ))
    }
}

/// The work cutoff prevents late advice, not a bounded error receipt. Runtime
/// shutdown still has to finish before the total deadline in finish_invocation.
/// In particular, a follower can spend its work budget waiting for its leader;
/// do not replace that unavailable document and its usage with a preflight error.
fn validate_rank_completion(
    completed_in_time: Result<(), Failure>,
    document: &OutputDocument,
) -> Result<(), Failure> {
    if matches!(
        document.kind(),
        crate::output::OutputKind::Decision(crate::output::Decision::Unavailable)
    ) {
        Ok(())
    } else {
        completed_in_time
    }
}

fn invalid(message: impl Into<String>) -> Failure {
    (2, "invalid-configuration", message.into())
}

fn timely(clock: &EntryClock) -> Result<(), Failure> {
    clock
        .admit_new_work()
        .map(|_| ())
        .map_err(|_| (6, "timeout", "Local inspection deadline exceeded".into()))
}

fn execute(clock: &EntryClock, mut args: Vec<OsString>) -> Result<String, Failure> {
    timely(clock)?;
    // Bare `sr` is `sr rank`: rank flags may follow the program name directly.
    if args
        .get(1)
        .and_then(|first| first.to_str())
        .is_some_and(|first| {
            first.starts_with('-') && !matches!(first, "--help" | "-h" | "--version" | "-V")
        })
    {
        args.insert(1, OsString::from("rank"));
    }
    // A command this build plans but does not implement has no subcommand, so
    // the parser would refuse it as unrecognized arguments. Name the phase it
    // waits on instead, from the same inventory `sr capabilities` publishes.
    let planned = args
        .get(1)
        .and_then(|first| first.to_str())
        .and_then(crate::capabilities::planned_command_phase);
    let matches = command().try_get_matches_from(args).map_err(|_| match planned {
        Some(phase) => (
            2,
            "invalid-usage",
            format!(
                "This build does not implement that command; it is planned for phase {}. Run sr capabilities for command status.",
                phase.to_uppercase()
            ),
        ),
        None => (
            2,
            "invalid-usage",
            "Unsupported or conflicting arguments; use --help".into(),
        ),
    })?;
    if matches.get_flag("help") && matches.subcommand().is_none() {
        return Ok(HELP.into());
    }
    if matches.get_flag("version") && matches.subcommand().is_none() {
        return Ok(format!("sr {}\n", env!("CARGO_PKG_VERSION")));
    }
    if matches.get_flag("help") || matches.get_flag("version") {
        return Err((
            2,
            "invalid-usage",
            "Top-level flags cannot accompany a command".into(),
        ));
    }
    // Use parsed argument identity, not a raw argv scan: values may themselves
    // look like flags, and the same flag can be implemented on another command.
    if let Some((name, subcommand)) = matches.subcommand()
        && !subcommand.get_flag("help")
        && let Some(planned) = crate::capabilities::PLANNED_FLAGS.iter().find(|entry| {
            entry.command == name
                && subcommand.value_source(entry.flag)
                    == Some(clap::parser::ValueSource::CommandLine)
        })
    {
        return Err((
            2,
            "invalid-usage",
            format!(
                "This build does not implement {name} --{}; it is planned for phase {}. Run sr capabilities for flag status.",
                planned.flag,
                crate::capabilities::phase_name(planned.phase).to_uppercase()
            ),
        ));
    }
    if let Some(("roster", roster)) = matches.subcommand() {
        if roster.get_flag("help") {
            return Ok(HELP.into());
        }
        return roster_listing(clock, roster);
    }
    if let Some(("doctor", doctor)) = matches.subcommand() {
        if doctor.get_flag("help") {
            return Ok(HELP.into());
        }
        return doctor_command(clock, doctor);
    }
    if let Some(("rank", rank_matches)) = matches.subcommand() {
        if rank_matches.get_flag("help") {
            return Ok(HELP.into());
        }
        return rank_command(clock, Some(rank_matches));
    }
    if let Some(("capabilities", capabilities)) = matches.subcommand() {
        if capabilities.get_flag("help") {
            return Ok(HELP.into());
        }
        // Always JSON: this is a machine-readable registry.
        return Ok(format!("{}\n", crate::capabilities::registry()));
    }
    if let Some(("demo", demo_matches)) = matches.subcommand() {
        if demo_matches.get_flag("help") {
            return Ok(HELP.into());
        }
        return demo_command(clock, demo_matches);
    }
    if let Some(("replay", replay_matches)) = matches.subcommand() {
        if replay_matches.get_flag("help") {
            return Ok(HELP.into());
        }
        return replay_command(clock, replay_matches);
    }
    if let Some(("eval", eval_matches)) = matches.subcommand() {
        if eval_matches.get_flag("help") {
            return Ok(EVAL_HELP.into());
        }
        return eval_command(clock, eval_matches);
    }
    if let Some(("ledger", ledger_matches)) = matches.subcommand() {
        if ledger_matches.get_flag("help") {
            return Ok(HELP.into());
        }
        return ledger_command(clock, ledger_matches);
    }
    if let Some(("feedback", feedback_matches)) = matches.subcommand() {
        if feedback_matches.get_flag("help") {
            return Ok(FEEDBACK_HELP.into());
        }
        return feedback_command(clock, feedback_matches);
    }
    if let Some(("observe", observe_matches)) = matches.subcommand() {
        if observe_matches.get_flag("help") {
            return Ok(OBSERVE_HELP.into());
        }
        return observe_command(clock, observe_matches);
    }
    if let Some(("stats", stats_matches)) = matches.subcommand() {
        if stats_matches.get_flag("help") {
            return Ok(STATS_HELP.into());
        }
        return stats_command(clock, stats_matches);
    }
    if let Some(("hook", hook_matches)) = matches.subcommand() {
        if hook_matches.get_flag("help") && hook_matches.subcommand().is_none() {
            return Ok(HELP.into());
        }
        if let Some(("claude", claude_matches)) = hook_matches.subcommand() {
            if claude_matches.get_flag("help") {
                return Ok(HELP.into());
            }
            return hook_claude_command(clock, claude_matches);
        }
        return Err((2, "invalid-usage", "Use sr hook claude".into()));
    }
    if let Some(("install-hook", install_matches)) = matches.subcommand() {
        if install_matches.get_flag("help") && install_matches.subcommand().is_none() {
            return Ok(HELP.into());
        }
        return install_hook_command(clock, install_matches);
    }
    if let Some(("uninstall-hook", uninstall_matches)) = matches.subcommand() {
        if uninstall_matches.get_flag("help") && uninstall_matches.subcommand().is_none() {
            return Ok(HELP.into());
        }
        return uninstall_hook_command(clock, uninstall_matches);
    }
    // Bare `sr` ranks once, as documented.
    rank_command(clock, None)
}

fn feedback_command(
    clock: &EntryClock,
    feedback_matches: &clap::ArgMatches,
) -> Result<String, Failure> {
    timely(clock)?;
    let invocation = crate::runtime::ProcessInvocation::from_clock(*clock)
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;
    let cx = invocation
        .request_cx()
        .map_err(|_| (6u8, "timeout", "Runtime context unavailable".into()))?;

    let event_id = feedback_matches
        .get_one::<String>("event_id")
        .or_else(|| feedback_matches.get_one::<String>("event"))
        .ok_or_else(|| {
            (
                2u8,
                "invalid-arguments",
                "Missing event ID for feedback".into(),
            )
        })?;

    let skill_id = feedback_matches.get_one::<String>("skill").ok_or_else(|| {
        (
            2u8,
            "invalid-arguments",
            "Missing original skill ID (--skill)".into(),
        )
    })?;

    let instead = feedback_matches.get_one::<String>("instead");
    let verdict = feedback_matches.get_one::<String>("verdict");

    if instead.is_none() && verdict.is_none() {
        return Err((
            2u8,
            "invalid-arguments",
            "Must specify either --instead for paired correction or --verdict for single feedback"
                .into(),
        ));
    }
    if instead.is_some() && verdict.is_some() {
        return Err((
            2u8,
            "invalid-arguments",
            "Cannot specify both --instead and --verdict".into(),
        ));
    }

    let reason = feedback_matches
        .get_one::<String>("reason")
        .map(|s| s.to_string());
    let provenance = feedback_matches
        .get_one::<String>("provenance")
        .map(|s| s.to_string());
    let expected_version = feedback_matches
        .get_one::<String>("expected-version")
        .map(|s| {
            s.parse::<u32>().map_err(|_| {
                (
                    2u8,
                    "invalid-arguments",
                    "Invalid expected version number".into(),
                )
            })
        })
        .transpose()?;

    let location = if let Some(dir) = feedback_matches.get_one::<String>("dir") {
        crate::storage::LedgerLocation::Directory(PathBuf::from(dir))
    } else {
        crate::storage::LedgerLocation::Platform
    };

    let req = if let Some(alt_id) = instead {
        crate::storage::FeedbackRequest::Paired(crate::storage::PairedCorrectionRequest {
            event_id: event_id.to_string(),
            original_skill_id: skill_id.to_string(),
            alternative_skill_id: alt_id.to_string(),
            reason_code: reason,
            provenance,
            expected_version,
        })
    } else {
        let label_str = verdict.unwrap();
        let label = match label_str.to_ascii_lowercase().replace('_', "-").as_str() {
            "useful" => crate::storage::JudgmentLabel::Useful,
            "harmful" | "not-useful" => crate::storage::JudgmentLabel::Harmful,
            "neutral" | "unknown" => crate::storage::JudgmentLabel::Neutral,
            _ => {
                return Err((
                    2u8,
                    "invalid-arguments",
                    format!(
                        "Invalid verdict '{label_str}'; expected 'useful', 'not-useful' (or 'harmful'), or 'unknown' (or 'neutral')"
                    ),
                ));
            }
        };
        crate::storage::FeedbackRequest::Single(crate::storage::SingleFeedbackRequest {
            event_id: event_id.to_string(),
            skill_id: skill_id.to_string(),
            verdict: label,
            reason_code: reason,
            provenance,
            expected_version,
        })
    };

    let outcome = crate::storage::submit_feedback(&invocation, &cx, location, req).map_err(
        |err| match err {
            crate::storage::FeedbackError::MissingSnapshot => (
                crate::output::ErrorKind::IncompleteRoster.exit_code() as u8,
                crate::output::ErrorKind::IncompleteRoster.as_str(),
                "Roster snapshot missing or incomplete for event. For a single judgment, supply a retained stable skill ID. Otherwise create a new ranking with an explicit --roster; this cannot repair the old event's history".into(),
            ),
            crate::storage::FeedbackError::IneligibleAlternative { skill_id, reason } => {
                let r = reason.as_deref().unwrap_or("ineligible");
                (
                    2u8,
                    "ineligible-alternative",
                    format!("Alternative skill '{skill_id}' is ineligible: {r}"),
                )
            }
            crate::storage::FeedbackError::RevisionConflict { expected, actual } => (
                11u8,
                "revision-conflict",
                format!("Ledger revision conflict: expected {expected}, actual {actual}"),
            ),
            crate::storage::FeedbackError::IdenticalSkills => (
                2u8,
                "invalid-arguments",
                "Original and alternative skill IDs must be distinct".into(),
            ),
            crate::storage::FeedbackError::InvalidSkillId(msg) => {
                (2u8, "invalid-arguments", format!("Invalid skill ID: {msg}"))
            }
            crate::storage::FeedbackError::OriginalSkillNotFound(id) => (
                2u8,
                "skill-not-found",
                format!("Original skill '{id}' not found in event or roster snapshot"),
            ),
            crate::storage::FeedbackError::EventNotFound(id) => (
                2u8,
                "event-not-found",
                format!("Ranking event '{id}' not found in ledger"),
            ),
            crate::storage::FeedbackError::StaleStamp => {
                (11u8, "revision-conflict", "Ledger stamp is stale".into())
            }
            crate::storage::FeedbackError::Store(err) => (
                9u8,
                "storage-failure",
                format!("Ledger storage error: {err}"),
            ),
        },
    )?;

    let wants_json = feedback_matches.get_flag("json");
    if wants_json {
        let json_val = serde_json::to_string_pretty(&outcome).map_err(|_| {
            (
                9u8,
                "serialization-failure",
                "Failed to serialize outcome".into(),
            )
        })?;
        Ok(format!("{json_val}\n"))
    } else {
        match outcome {
            crate::storage::FeedbackOutcome::PairedCorrection {
                group_id,
                data_generation,
                original_judgment_id,
                alternative_judgment_id,
                alternative_skill_id,
                ..
            } => Ok(format!(
                "Recorded paired correction (group: {group_id}, data generation: {data_generation})\n  Original '{skill_id}' judged harmful ({original_judgment_id})\n  Alternative '{alternative_skill_id}' judged useful ({alternative_judgment_id})\n"
            )),
            crate::storage::FeedbackOutcome::ProspectiveProposal {
                proposal_id,
                alternative_skill_id,
                reason,
                ..
            } => Ok(format!(
                "Alternative '{alternative_skill_id}' was historically absent from roster snapshot ({reason}).\nRecorded prospective library proposal: {proposal_id}\nNo historical usefulness judgments committed.\n"
            )),
            crate::storage::FeedbackOutcome::SingleJudgment {
                judgment_id,
                verdict,
                data_generation,
                ..
            } => Ok(format!(
                "Recorded feedback for '{skill_id}': {verdict:?} ({judgment_id}, data generation: {data_generation})\n"
            )),
        }
    }
}

fn observe_command(clock: &EntryClock, matches: &clap::ArgMatches) -> Result<String, Failure> {
    timely(clock)?;

    if matches.get_flag("no-ledger") || matches.get_flag("no-persist") {
        return Err((
            2u8,
            "invalid-usage",
            "sr observe requires ledger persistence; --no-ledger and --no-persist are rejected"
                .into(),
        ));
    }

    let context_file = matches.get_one::<String>("context");
    let transcript_file = matches.get_one::<String>("transcript");
    let harness_opt = matches.get_one::<String>("harness");
    let session_file = matches.get_one::<String>("session");

    if context_file.is_none()
        && (transcript_file.is_none() || harness_opt.is_none())
        && session_file.is_none()
    {
        return Err((
            2u8,
            "invalid-usage",
            "sr observe requires an explicit source: --context FILE, --transcript FILE --harness NAME, or --session PATH".into(),
        ));
    }

    let invocation = crate::runtime::ProcessInvocation::from_clock(*clock)
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;
    let cx = invocation
        .request_cx()
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;

    let location = if let Some(dir) = matches.get_one::<String>("dir") {
        crate::storage::LedgerLocation::Directory(PathBuf::from(dir))
    } else {
        crate::storage::LedgerLocation::Platform
    };

    let workspace = std::env::current_dir().map_err(|_| invalid("Workspace is unavailable"))?;
    let workspace_id = crate::identity::WorkspaceId::new(workspace.to_string_lossy().as_ref())
        .map_err(|_| invalid("Invalid workspace root path"))?;

    // Set by whichever ingestion path runs, so the command can say that it did not reach the end
    // of its input instead of implying it did.
    let mut unread_backlog = false;
    let (normalized_context, bytes_scanned) = if let Some(context_path) = context_file {
        let path = PathBuf::from(context_path);
        let bytes = std::fs::read(&path).map_err(|e| {
            (
                7u8,
                "malformed-input",
                format!("Failed to read context file: {e}"),
            )
        })?;
        if bytes.len() > crate::limits::NORMALIZED_CONTEXT_JSON_BYTES.max() {
            return Err((
                7u8,
                "oversized-input",
                "Normalized context exceeds 1 MiB limit".into(),
            ));
        }
        let bytes_len = bytes.len() as u64;
        let ctx = crate::context::parse_normalized_context(&bytes).map_err(|e| {
            (
                7u8,
                "malformed-input",
                format!("Invalid normalized context: {e}"),
            )
        })?;
        (ctx, bytes_len)
    } else if let (Some(transcript_str), Some(harness_str)) = (transcript_file, harness_opt) {
        let harness_id = crate::identity::HarnessId::new(harness_str)
            .map_err(|_| (2u8, "invalid-arguments", "Invalid harness ID".into()))?;
        if harness_id.as_str() != "claude_code" {
            return Err((
                2u8,
                "invalid-usage",
                "Unsupported harness: only claude_code is supported for native transcripts".into(),
            ));
        }
        let path = PathBuf::from(transcript_str);
        let transcript_path = if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        };
        let session = {
            let absolute = transcript_path.clone();
            let ws = workspace.clone();
            crate::blocking::run_blocking_leaf(
                &invocation,
                &cx,
                crate::blocking::BlockingLeafKind::Filesystem,
                false,
                move || crate::context::discovery::transcript_session(&absolute, &ws),
            )
            .ok()
            .and_then(|outcome| outcome.value)
        };
        // Resume where the previous observation pass stopped. Passing None here -- which is what
        // this call did -- meant the read always started at byte 0 and the window, bounded by
        // OBSERVATION_DELTA_BYTES or 2,000 records, never advanced: a session longer than that
        // window had everything past it silently unobserved forever, however many times `observe`
        // ran (sr-jgez).
        //
        // The ledger cannot persist the transcript's (dev, ino), so the identity below is the file
        // as it is right now and the reader proves the watermark by CONTENT instead: the record the
        // watermark names must still be inside the window, or the read falls back to the beginning.
        //
        // The branch is the explicit --branch, or "main". A native Claude transcript carries no
        // branch id of its own, and the cursor namespace needs a branch before the context has been
        // parsed, so this is the honest approximation available at this point. Guessing wrong costs
        // a resume, never correctness: an unmatched cursor row simply yields None and the read
        // starts at the beginning, exactly as it always did.
        let resume = session.as_ref().and_then(|session_id| {
            let branch = matches
                .get_one::<String>("branch")
                .cloned()
                .unwrap_or_else(|| "main".to_string());
            let key = format!(
                "native:{}:{}",
                harness_opt.map_or("", |h| h.as_str()),
                session_id.as_str()
            );
            let stored = crate::storage::get_session_cursor(
                &invocation,
                &cx,
                crate::storage::LedgerAccess::ExistingOnly,
                location.clone(),
                workspace.to_string_lossy().as_ref(),
                &key,
                &branch,
                crate::storage::CursorKind::Observation,
            )
            .ok()
            .flatten()?;
            let meta = std::fs::metadata(&transcript_path).ok()?;
            Some(crate::context::jsonl::JsonlCursor {
                kind: crate::context::jsonl::CursorKind::Observation,
                identity: crate::context::jsonl::FileIdentity::from_metadata(&meta),
                generation: stored.transcript_generation,
                byte_offset: stored.last_offset_bytes,
                last_event_id: crate::identity::EventId::new(stored.last_complete_event_id.clone())
                    .ok(),
                parser_version: crate::context::jsonl::PARSER_VERSION,
            })
        });
        let snapshot = crate::context::jsonl::snapshot_jsonl(
            &invocation,
            &cx,
            &transcript_path,
            resume.as_ref(),
            crate::context::jsonl::CursorKind::Observation,
        )
        .map_err(|e| {
            (
                7u8,
                "malformed-input",
                format!("Transcript snapshot failed: {e}"),
            )
        })?;
        // Partial coverage has to be visible. The reader computes this and nothing used to read
        // it, so a pass that stopped before the end of the file reported "ok" with no hint that
        // there was more (AGENTS.md: expose truncation and partial coverage).
        unread_backlog = snapshot.unread_backlog;
        let bytes_scanned = snapshot.cursor.byte_offset;
        let events = snapshot.events;
        let current = events.iter().rev().find(|e| {
            e.role == crate::context::Role::User && e.kind == crate::context::EventKind::Message
        });
        let current_req_text =
            current.map_or_else(|| crate::context::PrivateText::new(""), |e| e.text.clone());
        let current_event_id = current.and_then(|e| e.event_id.clone());
        let ctx = crate::context::NormalizedContext {
            schema_version: 1,
            harness: harness_id,
            producer_id: None,
            workspace_root: crate::context::PrivateText::new(workspace.to_string_lossy()),
            session_id: session,
            agent_id: None,
            branch_id: None,
            context_epoch: None,
            current_request: crate::context::CurrentRequest {
                event_id: current_event_id,
                text: current_req_text,
                attachments_omitted: false,
                essential_attachment_missing: false,
            },
            events,
            explicit_skill_references: Vec::new(),
            supplied_loads: Vec::new(),
        };
        (ctx, bytes_scanned)
    } else {
        return Err((
            3u8,
            "missing-session",
            "Cass sessions lack durable session identity; observation writes require durable identity".into(),
        ));
    };

    let session_id = match &normalized_context.session_id {
        Some(s) => s.as_str().to_string(),
        None => {
            return Err((
                3u8,
                "missing-session",
                "Durable session identity is absent; cannot record observations".into(),
            ));
        }
    };

    let branch_target = crate::context::branch::BranchResolutionTarget {
        target_event_id: None,
        target_branch_id: matches
            .get_one::<String>("branch")
            .and_then(|s| crate::identity::BranchId::new(s).ok())
            .or_else(|| normalized_context.branch_id.clone()),
        target_agent_id: normalized_context.agent_id.clone(),
    };
    let resolved_branch =
        crate::context::branch::resolve_active_branch(&normalized_context.events, &branch_target);
    let active_branch = resolved_branch.active_branch();
    let agent_branch = matches
        .get_one::<String>("branch")
        .cloned()
        .or_else(|| {
            active_branch
                .and_then(|b| b.branch_id.as_ref())
                .map(|b| b.as_str().to_string())
        })
        .unwrap_or_else(|| "main".to_string());

    // Resolve roster
    let home = std::env::var_os("HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    let plan = crate::roster::discovery::claude_code_plan_with_roots(
        &workspace,
        home.as_deref(),
        crate::roster::Visibility::Verified {
            contract_version: crate::pipeline::PROVISIONAL_CLAUDE_CONTRACT.into(),
        },
        &[],
    )
    .map_err(|_| {
        (
            5u8,
            "unusable-roster",
            "Failed to create roster source plan".into(),
        )
    })?;
    let overrides = std::collections::BTreeMap::new();
    let roster = match matches.get_one::<String>("roster") {
        Some(roster_path) => {
            let path = PathBuf::from(roster_path);
            let path = if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            };
            let bytes = crate::roster::import::read_roster_file(&path).map_err(|e| {
                (
                    7u8,
                    "malformed-input",
                    format!("Failed to read roster file: {e:?}"),
                )
            })?;
            crate::roster::import::import_authorized(&bytes, &plan, &overrides, &cx, clock)
                .map_err(|e| {
                    (
                        7u8,
                        "malformed-input",
                        format!("Failed to import roster: {e:?}"),
                    )
                })?
        }
        None => crate::roster::resolution::resolve_claude_plan(&plan, &overrides, &cx, clock)
            .map_err(|error| {
                (
                    5u8,
                    "unusable-roster",
                    format!("Failed to resolve roster: {error:?}"),
                )
            })?,
    };

    let evidence_resolver = crate::pipeline::skill_evidence_resolver_from_roster(&roster);

    let is_native = transcript_file.is_some();
    let cursor_session_id = if is_native {
        format!("native:{}:{}", harness_opt.unwrap(), session_id)
    } else if let Some(producer) = &normalized_context.producer_id {
        format!("producer:{}:{}", producer.as_str(), session_id)
    } else {
        session_id.clone()
    };

    let existing_cursor = crate::storage::get_session_cursor(
        &invocation,
        &cx,
        crate::storage::LedgerAccess::ExistingOnly,
        location.clone(),
        workspace.to_string_lossy().as_ref(),
        &cursor_session_id,
        &agent_branch,
        crate::storage::CursorKind::Observation,
    )
    .map_err(|err| match err {
        crate::storage::StoreError::Missing => (
            9u8,
            "storage-failure",
            "Ledger store is missing; run sr ledger init first".into(),
        ),
        crate::storage::StoreError::Permissions => (
            9u8,
            "storage-failure",
            "Ledger store is disabled or read-only".into(),
        ),
        e => (
            9u8,
            "storage-failure",
            format!("Failed to read session cursor: {e}"),
        ),
    })?;

    let (expected_cursor_gen, next_generation) = match &existing_cursor {
        Some(cur) => (
            Some(cur.transcript_generation),
            cur.transcript_generation + 1,
        ),
        None => (Some(0), 1),
    };

    let session_identity = if is_native {
        let adapter = crate::identity::AdapterId::new(crate::adapter::CLAUDE_CODE_ID).unwrap();
        let version =
            crate::identity::AdapterVersion::new(crate::adapter::CONTRACT_VERSION.to_string())
                .unwrap();
        crate::identity::SessionIdentity {
            source: crate::identity::SourceProvenance::Native { adapter, version },
            workspace: Some(workspace_id.clone()),
            session: normalized_context.session_id.clone(),
            agent: normalized_context.agent_id.clone(),
            branch: normalized_context.branch_id.clone(),
            epoch: normalized_context.context_epoch.clone(),
        }
    } else {
        normalized_context
            .session_identity(Some(workspace_id))
            .map_err(|e| {
                (
                    7u8,
                    "malformed-input",
                    format!("Invalid session identity: {e:?}"),
                )
            })?
    };

    let raw_observations = crate::context::tool::extract_load_observations(
        &normalized_context.events,
        &session_identity,
        &evidence_resolver,
        active_branch,
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let mut new_observations = Vec::with_capacity(raw_observations.len());
    for obs in &raw_observations {
        let state = match obs.state {
            crate::context::LoadState::ObservedLoaded => crate::storage::EvidenceState::Loaded,
            crate::context::LoadState::Attempted => crate::storage::EvidenceState::Attempted,
            crate::context::LoadState::Censored
            | crate::context::LoadState::NotObserved
            | crate::context::LoadState::Unobservable => crate::storage::EvidenceState::Censored,
        };
        let event_id_str = obs.event_id.as_ref().map_or("event-0", |e| e.as_str());
        let source_event_key = format!(
            "{}:{}:{}:{}:{}",
            if is_native { "native" } else { "normalized" },
            session_id,
            agent_branch,
            event_id_str,
            obs.skill_id.as_str()
        );
        // Derived from the uniqueness key rather than from a subset of it. `observation_id`
        // is the table's PRIMARY KEY and `source_event_key` is UNIQUE, so the two must carry
        // the same identity or they disagree: the old form omitted the producer namespace and
        // the agent branch, so observing one session under a second branch produced a new
        // source_event_key with an identical observation_id and the insert died on the primary
        // key. AGENTS.md requires those namespaces to stay distinct, not to collide
        // (sr-mdng).
        let observation_id = format!("obs-{source_event_key}");
        new_observations.push(crate::storage::NewObservation {
            observation_id,
            source_event_key,
            workspace_root: workspace.to_string_lossy().to_string(),
            session_id: session_id.clone(),
            agent_branch: agent_branch.clone(),
            attributed_event_id: None,
            skill_id: obs.skill_id.as_str().to_string(),
            evidence_state: state,
            observed_at_unix_ms: now_ms,
        });
    }

    let last_event_id = normalized_context
        .events
        .iter()
        .rev()
        .find_map(|e| e.event_id.as_ref().map(|id| id.as_str().to_string()))
        .unwrap_or_else(|| "event-0".to_string());

    let new_cursor = crate::storage::SessionCursor {
        workspace_root: workspace.to_string_lossy().to_string(),
        session_id: cursor_session_id.clone(),
        agent_branch: agent_branch.clone(),
        cursor_kind: crate::storage::CursorKind::Observation,
        transcript_generation: next_generation,
        last_complete_event_id: last_event_id.clone(),
        last_offset_bytes: bytes_scanned,
        updated_at_unix_ms: now_ms,
    };

    crate::storage::record_observations_with_cursor(
        &invocation,
        &cx,
        crate::storage::LedgerAccess::ExistingOnly,
        location,
        &new_observations,
        &new_cursor,
        expected_cursor_gen,
    )
    .map_err(|err| match err {
        crate::storage::StoreError::RecordConflict => (
            11u8,
            "revision-conflict",
            "Concurrent transcript observation or cursor conflict".into(),
        ),
        crate::storage::StoreError::Missing => (
            9u8,
            "storage-failure",
            "Ledger store is missing; run sr ledger init first".into(),
        ),
        crate::storage::StoreError::Permissions => (
            9u8,
            "storage-failure",
            "Ledger store is disabled or read-only".into(),
        ),
        e => (
            9u8,
            "storage-failure",
            format!("Failed to record observations: {e}"),
        ),
    })?;

    let out = if matches.get_flag("json") || !io::stdout().is_terminal() {
        let val = serde_json::json!({
            "status": "ok",
            "workspace_root": workspace.to_string_lossy(),
            "session_id": session_id,
            "agent_branch": agent_branch,
            "observations_recorded": new_observations.len(),
            "cursor_generation": next_generation,
            "last_event_id": last_event_id,
            "unread_backlog": unread_backlog,
        });
        format!("{}\n", val)
    } else {
        format!(
            "Recorded {} observation(s) for session {} on branch {} (generation {}){}\n",
            new_observations.len(),
            session_id,
            agent_branch,
            next_generation,
            if unread_backlog {
                "; input continues past this pass, run observe again to reach it"
            } else {
                ""
            },
        )
    };

    finish_invocation(invocation, Ok(out))
}

fn stats_command(clock: &EntryClock, matches: &clap::ArgMatches) -> Result<String, Failure> {
    timely(clock)?;
    let invocation = crate::runtime::ProcessInvocation::from_clock(*clock)
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;
    let cx = invocation
        .request_cx()
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;

    let location = if let Some(dir) = matches.get_one::<String>("dir") {
        crate::storage::LedgerLocation::Directory(PathBuf::from(dir))
    } else {
        crate::storage::LedgerLocation::Platform
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or_else(|| {
            (
                9u8,
                "storage-failure",
                "System clock cannot represent a Unix timestamp".into(),
            )
        })?;
    let since_ms = if let Some(s) = matches.get_one::<String>("since") {
        crate::storage::parse_cutoff_to_unix_ms(s, now_ms)
            .map_err(|err| (2u8, "invalid-arguments", err))?
    } else {
        0i64
    };

    let by_skill = matches.get_flag("by-skill");
    let wants_json =
        matches.get_flag("json") || (!matches.get_flag("table") && !io::stdout().is_terminal());

    let report = crate::storage::ledger_stats(&invocation, &cx, location, since_ms, by_skill)
        .map_err(|err| match err {
            crate::storage::StoreError::Missing => (
                9u8,
                "storage-failure",
                "Ledger store is missing; run sr ledger init first".into(),
            ),
            crate::storage::StoreError::Permissions => (
                9u8,
                "storage-failure",
                "Ledger store is disabled or read-only".into(),
            ),
            e => (
                9u8,
                "storage-failure",
                format!("Failed to compute stats report: {e}"),
            ),
        })?;

    let out = if wants_json {
        serde_json::to_string_pretty(&report)
            .map(|s| format!("{s}\n"))
            .map_err(|e| (9u8, "storage-failure", e.to_string()))?
    } else {
        format_stats_report(&report)
    };

    finish_invocation(invocation, Ok(out))
}

fn format_stats_report(report: &crate::storage::StatsValueReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "SkillRanker Value & Operational Report");
    let _ = writeln!(
        out,
        "Window: {} .. {}",
        crate::storage::format_unix_ms(report.since_unix_ms),
        crate::storage::format_unix_ms(report.as_of_unix_ms)
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "--- Evaluated Turns & Interruption ---");
    let _ = writeln!(
        out,
        "Total turns evaluated:    {}",
        report.turns.total_evaluated
    );
    let _ = writeln!(
        out,
        "Emitted suggestions:      {}",
        report.turns.emitted_suggestions
    );
    let _ = writeln!(
        out,
        "Valid abstentions:        {}",
        report.turns.valid_abstentions
    );
    let _ = writeln!(
        out,
        "Muted or suppressed:      {}",
        report.turns.muted_or_suppressed
    );
    let _ = writeln!(
        out,
        "Operational failures:     {}",
        report.turns.operational_failures
    );
    let _ = writeln!(
        out,
        "Explicit requirements:    {}",
        report.turns.explicit_requirements
    );
    let _ = writeln!(
        out,
        "Unfinished (in flight or killed): {}",
        report.turns.in_flight_or_killed
    );

    if !report.turns.failure_causes.is_empty() {
        let _ = writeln!(out, "\nOperational failures by cause:");
        for cause in &report.turns.failure_causes {
            let _ = writeln!(out, "  {}: {}", cause.reason, cause.count);
        }
    }

    if !report.turns.by_channel.is_empty() {
        let _ = writeln!(out, "\nBy Channel:");
        for c in &report.turns.by_channel {
            let _ = writeln!(
                out,
                "  [{}] evaluated: {}, emitted: {}, abstain: {}, muted: {}, unavailable: {}, unfinished: {}",
                c.channel,
                c.evaluated_turns,
                c.emitted,
                c.abstain,
                c.muted,
                c.unavailable,
                c.in_flight_or_killed
            );
        }
    }

    let _ = writeln!(out, "\n--- Latency ---");
    let _ = writeln!(
        out,
        "Mean: {} ms | Median: {} ms | P95: {} ms | Range: {} .. {} ms",
        report.latency.mean_ms,
        report.latency.median_ms,
        report.latency.p95_ms,
        report.latency.min_ms,
        report.latency.max_ms
    );
    if report.latency.excluded_unfinished > 0 {
        // Said out loud, because a duration summary that quietly covers only part of
        // the window reads exactly like one that covers all of it.
        let _ = writeln!(
            out,
            "Excluded {} unfinished turn(s) with no measured duration.",
            report.latency.excluded_unfinished
        );
    }

    let _ = writeln!(out, "\n--- Observations & Adoption ---");
    let _ = writeln!(
        out,
        "Total observations:       {}",
        report.observations.total_observations
    );
    let _ = writeln!(
        out,
        "Observed loads:           {}",
        report.observations.observed_loads
    );
    let _ = writeln!(
        out,
        "Attempted loads:          {}",
        report.observations.attempted_loads
    );
    let _ = writeln!(
        out,
        "Censored observations:    {}",
        report.observations.censored_observations
    );
    let _ = writeln!(
        out,
        "Attributed loads:         {}",
        report.observations.attributed_loads
    );
    let _ = writeln!(
        out,
        "Unattributed loads:       {}",
        report.observations.unattributed_loads
    );
    if let Some(rate) = report.observations.observation_coverage {
        let _ = writeln!(out, "Observation coverage:     {:.1}%", rate * 100.0);
    }
    if let Some(rate) = report.observations.suggestion_adoption_rate {
        let _ = writeln!(out, "Suggestion adoption rate: {:.1}%", rate * 100.0);
    }
    let _ = writeln!(out, "Note: {}", report.observations.caveat);

    let _ = writeln!(out, "\n--- Judgments (Judged Cohort) ---");
    let _ = writeln!(
        out,
        "Total judgments:          {}",
        report.judgments.total_judgments
    );
    let _ = writeln!(out, "Useful:                   {}", report.judgments.useful);
    let _ = writeln!(
        out,
        "Harmful:                  {}",
        report.judgments.harmful
    );
    let _ = writeln!(
        out,
        "Neutral:                  {}",
        report.judgments.neutral
    );
    let _ = writeln!(
        out,
        "Distinct judged events:   {}",
        report.judgments.distinct_judged_events
    );
    if let Some(cov) = report.judgments.label_coverage_rate {
        let _ = writeln!(out, "Label coverage rate:      {:.1}%", cov * 100.0);
    }
    if let Some(ratio) = report.judgments.useful_ratio_in_judged {
        let _ = writeln!(out, "Useful ratio in judged:   {:.1}%", ratio * 100.0);
    }

    let _ = writeln!(out, "\n--- Provider Usage & Cost ---");
    let _ = writeln!(
        out,
        "Total attempts:           {} (completed: {}, failed: {}, unknown: {})",
        report.provider.total_attempts,
        report.provider.completed_attempts,
        report.provider.failed_attempts,
        report.provider.unknown_attempts
    );
    let _ = writeln!(
        out,
        "Known tokens:             {} (input: {}, output: {})",
        report.provider.known_total_tokens,
        report.provider.known_input_tokens,
        report.provider.known_output_tokens
    );
    let _ = writeln!(
        out,
        "Unknown usage attempts:   {}",
        report.provider.unknown_usage_attempts
    );
    let _ = writeln!(
        out,
        "Cache-served events:      {}",
        report.provider.cache_served_events
    );
    if let Some(rate) = report.provider.cache_hit_rate {
        let _ = writeln!(out, "Cache hit rate:           {:.1}%", rate * 100.0);
    }
    let _ = writeln!(
        out,
        "Cost per useful suggestion: {}",
        report.provider.cost_per_useful_suggestion
    );
    if report.provider.judged_cohort_unknown_usage_attempts > 0 {
        // The token figure above is a floor, not a measurement, while this is above zero.
        let _ = writeln!(
            out,
            "  a lower bound: {} judged attempt(s) reported no usage",
            report.provider.judged_cohort_unknown_usage_attempts
        );
    }
    if report.provider.judged_turns_served_from_cache > 0 {
        // Said next to the figure, because a cost per useful suggestion computed over turns
        // that reused someone else's answer reads lower than the work actually cost.
        let _ = writeln!(
            out,
            "  of the judged turns, {} reused a response paid for elsewhere",
            report.provider.judged_turns_served_from_cache
        );
    }

    if let Some(skills) = &report.by_skill {
        let _ = writeln!(out, "\n--- Skill Breakdown ---");
        if skills.is_empty() {
            let _ = writeln!(out, "No skill activity recorded in window.");
        } else {
            let _ = writeln!(
                out,
                "{:<24} {:>6} {:>10} {:>10} {:>10} {:>7} {:>8} {:>8}",
                "Skill ID",
                "Top-1",
                "Shortlist",
                "Obs Loads",
                "Attr Loads",
                "Useful",
                "Harmful",
                "Neutral"
            );
            for s in skills {
                let _ = writeln!(
                    out,
                    "{:<24} {:>6} {:>10} {:>10} {:>10} {:>7} {:>8} {:>8}",
                    s.skill_id,
                    s.top1_recommendations,
                    s.shortlist_appearances,
                    s.observed_loads,
                    s.attributed_loads,
                    s.judged_useful,
                    s.judged_harmful,
                    s.judged_neutral
                );
            }
        }
    }

    out
}

fn ledger_command(
    clock: &EntryClock,
    ledger_matches: &clap::ArgMatches,
) -> Result<String, Failure> {
    timely(clock)?;
    let invocation = crate::runtime::ProcessInvocation::from_clock(*clock)
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;
    let cx = invocation
        .request_cx()
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;

    let (sub_name, sub_matches) = match ledger_matches.subcommand() {
        Some((name, m)) => (name, m),
        None => {
            return Err((
                2,
                "invalid-usage",
                "Missing ledger subcommand (init, migrate, status); see sr --help".into(),
            ));
        }
    };

    if sub_matches.get_flag("help") {
        return Ok(HELP.into());
    }

    let wants_json = sub_matches.get_flag("json") || ledger_matches.get_flag("json");
    let location = if let Some(dir) = sub_matches.get_one::<String>("dir") {
        crate::storage::LedgerLocation::Directory(PathBuf::from(dir))
    } else {
        crate::storage::LedgerLocation::Platform
    };

    let outcome: Result<String, Failure> = match sub_name {
        "init" => {
            let diagnosed = location.clone();
            let report =
                crate::storage::init_ledger(&invocation, &cx, location).map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        storage_failure("Failed to initialize ledger", &err, &diagnosed),
                    )
                })?;
            if wants_json {
                serde_json::to_string_pretty(&report)
                    .map(|s| format!("{s}\n"))
                    .map_err(|e| (9u8, "storage-failure", e.to_string()))
            } else {
                match report.status {
                    crate::storage::InitStatus::Created => Ok(format!(
                        "Initialized new ledger at {} (schema version {}, incarnation {})\n",
                        report.database_path.display(),
                        report.schema_version,
                        report.incarnation
                    )),
                    crate::storage::InitStatus::AlreadyCurrent => Ok(format!(
                        "Ledger at {} is already current (schema version {}, incarnation {})\n",
                        report.database_path.display(),
                        report.schema_version,
                        report.incarnation
                    )),
                }
            }
        }
        "migrate" => {
            let apply = sub_matches.get_flag("apply");
            if apply {
                let open_res = crate::storage::open_ledger(
                    &invocation,
                    &cx,
                    crate::storage::LedgerAccess::Migrate,
                    location,
                )
                .map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        format!("Failed to open ledger for migration: {err}"),
                    )
                })?;

                let mut store = match open_res {
                    crate::storage::LedgerOpen::Ready(store) => store,
                    crate::storage::LedgerOpen::Missing => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger database is missing; run `sr ledger init` first".into(),
                        ));
                    }
                    crate::storage::LedgerOpen::ReadOnly(_) => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger is opened read-only (newer schema version); migration cannot be applied".into(),
                        ));
                    }
                    crate::storage::LedgerOpen::Disabled => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger persistence is disabled".into(),
                        ));
                    }
                };

                let report = store
                    .migrate_apply(invocation.clock(), &cx)
                    .map_err(|err| (9u8, "storage-failure", format!("Migration failed: {err}")))?;

                if wants_json {
                    serde_json::to_string_pretty(&report)
                        .map(|s| format!("{s}\n"))
                        .map_err(|e| (9u8, "storage-failure", e.to_string()))
                } else {
                    Ok(format!(
                        "Successfully migrated ledger from version {} to {}.\nBackup saved to {} ({} bytes).\nApplied migrations: {}\n",
                        report.from_version,
                        report.to_version,
                        report.backup_path.display(),
                        report.backup_bytes,
                        report.applied_migrations.join(", ")
                    ))
                }
            } else {
                let open_res = crate::storage::open_ledger(
                    &invocation,
                    &cx,
                    crate::storage::LedgerAccess::ExistingOnly,
                    location,
                )
                .map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        format!("Failed to open ledger for preview: {err}"),
                    )
                })?;

                let store = match open_res {
                    crate::storage::LedgerOpen::Ready(store) => store,
                    crate::storage::LedgerOpen::ReadOnly(store) => store,
                    crate::storage::LedgerOpen::Missing => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger database is missing; run `sr ledger init` first".into(),
                        ));
                    }
                    crate::storage::LedgerOpen::Disabled => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger persistence is disabled".into(),
                        ));
                    }
                };

                let preview = store.migrate_preview().map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        format!("Migration preview failed: {err}"),
                    )
                })?;

                if wants_json {
                    serde_json::to_string_pretty(&preview)
                        .map(|s| format!("{s}\n"))
                        .map_err(|e| (9u8, "storage-failure", e.to_string()))
                } else {
                    let mut out = format!(
                        "Migration preview: current version {}, target version {}.\nRequired headroom: {} bytes.\n",
                        preview.current_version,
                        preview.target_version,
                        preview.required_headroom_bytes
                    );
                    if preview.pending_migrations.is_empty() {
                        out.push_str("Schema is already up to date. No pending migrations.\n");
                    } else {
                        out.push_str("Pending migrations:\n");
                        for m in &preview.pending_migrations {
                            out.push_str(&format!(
                                "  - v{}: {} (checksum: {})\n    {}\n",
                                m.version, m.name, m.checksum, m.description
                            ));
                        }
                    }
                    Ok(out)
                }
            }
        }
        "status" => {
            let status_report =
                crate::storage::ledger_status(&invocation, &cx, location).map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        format!("Failed to inspect ledger status: {err}"),
                    )
                })?;
            if wants_json {
                serde_json::to_string_pretty(&status_report)
                    .map(|s| format!("{s}\n"))
                    .map_err(|e| (9u8, "storage-failure", e.to_string()))
            } else {
                let mut out = format!(
                    "Ledger Status: {}\nPath: {}\nSchema Version: {}\nTarget Version: {}\nRead Only: {}\n",
                    status_report.status,
                    status_report.database_path.display(),
                    status_report
                        .schema_version
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "none".into()),
                    status_report.target_version,
                    status_report.is_read_only
                );
                if let Some(up) = status_report.upgrade_available {
                    out.push_str(&format!("Upgrade Available: {}\n", up));
                }
                Ok(out)
            }
        }
        "prune" => {
            let before_str = sub_matches.get_one::<String>("before").map(|s| s.as_str());
            // Wall-clock, not the monotonic entry clock. A retention cutoff is a point in
            // history, and `clock.now()` is milliseconds since this process started: deriving
            // the cutoff from it put every cutoff in 1969, so nothing was ever old enough to
            // prune and retention was never enforced through this command. `ledger_stats` had
            // the same defect and it was repaired in d10ca98; this is the same repair here.
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
                .ok_or((
                    9u8,
                    "storage-failure",
                    "The system clock is outside the representable range".to_string(),
                ))?;
            let cutoff_ms = match before_str {
                Some(s) => crate::storage::parse_cutoff_to_unix_ms(s, now_ms)
                    .map_err(|err| (2u8, "invalid-arguments", err))?,
                None => now_ms.saturating_sub(crate::storage::DEFAULT_RETENTION_MS),
            };
            let apply = sub_matches.get_flag("apply");
            if apply {
                let open_res = crate::storage::open_ledger(
                    &invocation,
                    &cx,
                    crate::storage::LedgerAccess::ExistingOnly,
                    location,
                )
                .map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        format!("Failed to open ledger for prune: {err}"),
                    )
                })?;

                let mut store = match open_res {
                    crate::storage::LedgerOpen::Ready(store) => store,
                    crate::storage::LedgerOpen::Missing => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger database is missing; run `sr ledger init` first".into(),
                        ));
                    }
                    crate::storage::LedgerOpen::ReadOnly(_) => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger is opened read-only; prune mutations are blocked".into(),
                        ));
                    }
                    crate::storage::LedgerOpen::Disabled => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger persistence is disabled".into(),
                        ));
                    }
                };

                let stamp = store.stamp();
                let report = store
                    .prune_apply(cutoff_ms, invocation.clock(), &cx, stamp)
                    .map_err(|err| {
                        (
                            9u8,
                            "storage-failure",
                            format!("Failed to apply prune: {err}"),
                        )
                    })?;

                if wants_json {
                    serde_json::to_string_pretty(&report)
                        .map(|s| format!("{s}\n"))
                        .map_err(|e| (9u8, "storage-failure", e.to_string()))
                } else {
                    Ok(format!(
                        "Pruned historical records before {} (cutoff {}):\n  Events pruned: {}\n  Candidates pruned: {}\n  Observations pruned: {}\n  Judgments pruned: {}\n  Provider attempts pruned: {}\n  Snapshots pruned: {}\n  Shared snapshots preserved: {}\n  New data generation: {}\n",
                        report.cutoff_iso,
                        report.cutoff_unix_ms,
                        report.events_pruned,
                        report.candidates_pruned,
                        report.observations_pruned,
                        report.judgments_pruned,
                        report.provider_attempts_pruned,
                        report.snapshots_pruned,
                        report.shared_snapshots_preserved,
                        report.stamp_after.data_generation
                    ))
                }
            } else {
                let open_res = crate::storage::open_ledger(
                    &invocation,
                    &cx,
                    crate::storage::LedgerAccess::ExistingOnly,
                    location,
                )
                .map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        format!("Failed to open ledger for prune preview: {err}"),
                    )
                })?;

                let store = match open_res {
                    crate::storage::LedgerOpen::Ready(store) => store,
                    crate::storage::LedgerOpen::ReadOnly(store) => store,
                    crate::storage::LedgerOpen::Missing => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger database is missing; run `sr ledger init` first".into(),
                        ));
                    }
                    crate::storage::LedgerOpen::Disabled => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger persistence is disabled".into(),
                        ));
                    }
                };

                let preview = store.prune_preview(cutoff_ms).map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        format!("Failed to preview prune: {err}"),
                    )
                })?;

                if wants_json {
                    serde_json::to_string_pretty(&preview)
                        .map(|s| format!("{s}\n"))
                        .map_err(|e| (9u8, "storage-failure", e.to_string()))
                } else {
                    Ok(format!(
                        "Prune preview for cutoff {} ({}):\n  Events to prune: {}\n  Candidates to prune: {}\n  Observations to prune: {}\n  Judgments to prune: {}\n  Provider attempts to prune: {}\n  Snapshots to prune: {}\n  Shared snapshots preserved: {}\nPass --apply to execute this retention cleanup.\n",
                        preview.cutoff_iso,
                        preview.cutoff_unix_ms,
                        preview.events_to_prune,
                        preview.candidates_to_prune,
                        preview.observations_to_prune,
                        preview.judgments_to_prune,
                        preview.provider_attempts_to_prune,
                        preview.snapshots_to_prune,
                        preview.shared_snapshots_preserved,
                    ))
                }
            }
        }
        "clear" => {
            let apply = sub_matches.get_flag("apply");
            if apply {
                let open_res = crate::storage::open_ledger(
                    &invocation,
                    &cx,
                    crate::storage::LedgerAccess::ExistingOnly,
                    location,
                )
                .map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        format!("Failed to open ledger for clear: {err}"),
                    )
                })?;

                let mut store = match open_res {
                    crate::storage::LedgerOpen::Ready(store) => store,
                    crate::storage::LedgerOpen::Missing => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger database is missing; run `sr ledger init` first".into(),
                        ));
                    }
                    crate::storage::LedgerOpen::ReadOnly(_) => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger is opened read-only; clear mutations are blocked".into(),
                        ));
                    }
                    crate::storage::LedgerOpen::Disabled => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger persistence is disabled".into(),
                        ));
                    }
                };

                let stamp = store.stamp();
                let report = store
                    .clear_apply(invocation.clock(), &cx, stamp)
                    .map_err(|err| {
                        (
                            9u8,
                            "storage-failure",
                            format!("Failed to clear ledger: {err}"),
                        )
                    })?;

                if wants_json {
                    serde_json::to_string_pretty(&report)
                        .map(|s| format!("{s}\n"))
                        .map_err(|e| (9u8, "storage-failure", e.to_string()))
                } else {
                    Ok(format!(
                        "Cleared all historical records ({} total records removed).\nNew data generation: {}\n",
                        report.records_cleared, report.stamp_after.data_generation
                    ))
                }
            } else {
                let open_res = crate::storage::open_ledger(
                    &invocation,
                    &cx,
                    crate::storage::LedgerAccess::ExistingOnly,
                    location,
                )
                .map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        format!("Failed to open ledger for clear preview: {err}"),
                    )
                })?;

                let store = match open_res {
                    crate::storage::LedgerOpen::Ready(store) => store,
                    crate::storage::LedgerOpen::ReadOnly(store) => store,
                    crate::storage::LedgerOpen::Missing => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger database is missing; run `sr ledger init` first".into(),
                        ));
                    }
                    crate::storage::LedgerOpen::Disabled => {
                        return Err((
                            9u8,
                            "storage-failure",
                            "Ledger persistence is disabled".into(),
                        ));
                    }
                };

                let preview = store.clear_preview().map_err(|err| {
                    (
                        9u8,
                        "storage-failure",
                        format!("Failed to preview clear: {err}"),
                    )
                })?;

                if wants_json {
                    serde_json::to_string_pretty(&preview)
                        .map(|s| format!("{s}\n"))
                        .map_err(|e| (9u8, "storage-failure", e.to_string()))
                } else {
                    Ok(format!(
                        "Clear preview (entire history):\n  Total records to clear: {}\n  Events: {}\n  Candidates: {}\n  Observations: {}\n  Judgments: {}\n  Provider attempts: {}\n  Snapshots: {}\n  Session cursors: {}\n  Feedback proposals: {}\n  Calibrations: {}\nPass --apply to execute clearing all history.\n",
                        preview.total_records,
                        preview.events_count,
                        preview.candidates_count,
                        preview.observations_count,
                        preview.judgments_count,
                        preview.provider_attempts_count,
                        preview.snapshots_count,
                        preview.session_cursors_count,
                        preview.feedback_proposals_count,
                        preview.calibrations_count,
                    ))
                }
            }
        }
        _ => Err((
            2,
            "invalid-usage",
            format!("Unknown ledger subcommand: {sub_name}"),
        )),
    };

    finish_invocation(invocation, outcome)
}

fn replay_command(
    clock: &EntryClock,
    replay_matches: &clap::ArgMatches,
) -> Result<String, Failure> {
    timely(clock)?;
    let file_str = replay_matches
        .get_one::<String>("file")
        .ok_or_else(|| (2, "invalid-usage", "Missing required case file".into()))?;
    let case_path = Path::new(file_str);

    let case = crate::replay::ReplayCase::load_from_file(case_path).map_err(|err| {
        let kind = err.kind();
        (kind.exit_code() as u8, kind.as_str(), err.to_string())
    })?;

    let policy = if let Some(p) = replay_matches.get_one::<String>("policy") {
        let pol = crate::replay::ReplayPolicy::load_from_file(Path::new(p)).map_err(|err| {
            let kind = err.kind();
            (kind.exit_code() as u8, kind.as_str(), err.to_string())
        })?;
        Some(pol)
    } else {
        None
    };

    let compare_policy = if let Some(p) = replay_matches.get_one::<String>("compare-policy") {
        let pol = crate::replay::ReplayPolicy::load_from_file(Path::new(p)).map_err(|err| {
            let kind = err.kind();
            (kind.exit_code() as u8, kind.as_str(), err.to_string())
        })?;
        Some(pol)
    } else {
        None
    };

    let outcome =
        crate::replay::execute_replay_comparison(&case, policy.as_ref(), compare_policy.as_ref())
            .map_err(|err| {
            let kind = err.kind();
            (kind.exit_code() as u8, kind.as_str(), err.to_string())
        })?;

    let json_output = replay_matches.get_flag("json")
        || (!replay_matches.get_flag("table") && !io::stdout().is_terminal());

    if json_output {
        let wire = outcome
            .document
            .to_json()
            .map_err(|e| (2, "output-error", e.to_string()))?;
        Ok(format!("{}\n", String::from_utf8_lossy(&wire)))
    } else {
        Ok(outcome.document.render_table())
    }
}

fn eval_command(clock: &EntryClock, eval_matches: &clap::ArgMatches) -> Result<String, Failure> {
    timely(clock)?;
    let dataset_str = eval_matches.get_one::<String>("dataset").ok_or_else(|| {
        (
            2,
            "invalid-usage",
            "Missing required argument --dataset".into(),
        )
    })?;
    // `--online` and `--max-requests` are planned flags refused before dispatch,
    // so this build only replays recorded cases.
    let allow_network = eval_matches.get_flag("allow-network");

    let max_runtime_ms = if let Some(s) = eval_matches.get_one::<String>("max-runtime-ms") {
        s.parse::<u64>().map_err(|_| {
            (
                2,
                "invalid-usage",
                "Invalid --max-runtime-ms: must be a positive integer".into(),
            )
        })?
    } else {
        crate::limits::DEFAULT_EVAL_BATCH_RUNTIME_MS
    };

    let per_case_timeout_ms = if let Some(s) = eval_matches.get_one::<String>("timeout-ms") {
        Some(s.parse::<u64>().map_err(|_| {
            (
                2,
                "invalid-usage",
                "Invalid --timeout-ms: must be a positive integer".into(),
            )
        })?)
    } else {
        None
    };

    let mut config = crate::evaluation::batch::BatchConfig {
        max_requests: None,
        max_runtime_ms,
        per_case_timeout_ms,
        online: false,
        allow_network,
        evidence_origin: crate::evaluation::batch::EvidenceOrigin::Recorded,
        policy: None,
        compare_policy: None,
    };

    crate::evaluation::batch::validate_batch_config(&config)
        .map_err(|err| (2, "invalid-usage", err.to_string()))?;
    timely(clock)?;

    let policy = if let Some(p) = eval_matches.get_one::<String>("policy") {
        let pol = crate::replay::ReplayPolicy::load_from_file(Path::new(p)).map_err(|err| {
            let kind = err.kind();
            (kind.exit_code() as u8, kind.as_str(), err.to_string())
        })?;
        Some(pol)
    } else {
        None
    };

    let compare_policy = if let Some(p) = eval_matches.get_one::<String>("compare-policy") {
        let pol = crate::replay::ReplayPolicy::load_from_file(Path::new(p)).map_err(|err| {
            let kind = err.kind();
            (kind.exit_code() as u8, kind.as_str(), err.to_string())
        })?;
        Some(pol)
    } else {
        None
    };

    config.policy = policy;
    config.compare_policy = compare_policy;
    timely(clock)?;
    // Pin the selected directory and use the descriptor that was checked as a
    // regular file. A FIFO (including a raced-in replacement) cannot block open.
    let input_error = || {
        (
            7,
            "malformed-input",
            "Evaluation dataset must be an accessible authorized regular file".into(),
        )
    };
    let absolute = std::path::absolute(dataset_str).map_err(|_| input_error())?;
    let parent = absolute.parent().ok_or_else(input_error)?;
    let parent = std::fs::canonicalize(parent).map_err(|_| input_error())?;
    let name = absolute.file_name().ok_or_else(input_error)?;
    let root = AuthorizedRoot::open_absolute(&parent).map_err(|_| input_error())?;
    let file = AuthorizedRoots::single(root)
        .open_absolute_file(&parent.join(name))
        .map_err(|_| input_error())?;
    timely(clock)?;
    let reader = std::io::BufReader::new(file);

    let report = match crate::evaluation::batch::execute_evaluation_batch(reader, &config, clock) {
        Ok(rep) => rep,
        Err(err) => {
            let kind = err.kind();
            return Err((kind.exit_code() as u8, kind.as_str(), err.to_string()));
        }
    };

    let doc = report
        .to_document()
        .map_err(|e| (2, "output-error", e.to_string()))?;

    let json_output = eval_matches.get_flag("json")
        || (!eval_matches.get_flag("table") && !io::stdout().is_terminal());

    if json_output {
        let wire = doc
            .to_json()
            .map_err(|e| (2, "output-error", e.to_string()))?;
        Ok(format!("{}\n", String::from_utf8_lossy(&wire)))
    } else {
        Ok(doc.render_table())
    }
}

fn demo_command(clock: &EntryClock, demo_matches: &clap::ArgMatches) -> Result<String, Failure> {
    timely(clock)?;
    let case_str = demo_matches
        .get_one::<String>("case")
        .map(String::as_str)
        .ok_or_else(|| {
            (
                2,
                "invalid-usage",
                "Missing required argument --case".into(),
            )
        })?;
    let case = crate::demo::DemoCase::parse(case_str)
        .ok_or_else(|| (2, "invalid-usage", format!("Unknown demo case: {case_str}")))?;

    let doc =
        crate::demo::generate_demo_doc(case).map_err(|e| (2, "output-error", e.to_string()))?;

    let json_output = demo_matches.get_flag("json")
        || (!demo_matches.get_flag("table") && !io::stdout().is_terminal());

    if json_output {
        let wire = doc
            .to_json()
            .map_err(|e| (2, "output-error", e.to_string()))?;
        Ok(format!("{}\n", String::from_utf8_lossy(&wire)))
    } else {
        Ok(doc.render_table())
    }
}

fn doctor_command(clock: &EntryClock, doctor: &clap::ArgMatches) -> Result<String, Failure> {
    let flags = crate::privacy::EffectFlags {
        offline: doctor.get_flag("offline"),
        allow_network: doctor.get_flag("allow-network"),
        dry_run: false,
        no_cache: false,
        no_ledger: false,
        no_persist: false,
        save_case: false,
    };
    // Doctor never sends a request; the gate reports what ranking would permit.
    let gate = crate::effects::EffectGate::new(flags, crate::effects::Scope::Rank).map_err(
        |conflicts| {
            let first = conflicts
                .first()
                .expect("from_flags reports at least one conflict on error");
            (2u8, "invalid-usage", first.to_string())
        },
    )?;
    let mut sources = ConfigSources::default();
    for key in SettingKey::ALL {
        let Some(flag) = key.spec().cli_flag else {
            continue;
        };
        let name = flag.trim_start_matches('-');
        let value = match name {
            "shadow" if doctor.get_flag(name) => Some(RawValue::String("shadow".into())),
            "no-tools" if doctor.get_flag(name) => Some(RawValue::Bool(true)),
            "shadow" | "no-tools" => None,
            _ => doctor
                .get_one::<String>(name)
                .map(|text| match key.spec().kind {
                    crate::config::ValueKind::Count { .. }
                    | crate::config::ValueKind::Millis { .. } => text
                        .parse()
                        .map(RawValue::Integer)
                        .map_err(|_| invalid("CLI count must be an integer")),
                    crate::config::ValueKind::Unit { .. } => text
                        .parse()
                        .map(RawValue::Float)
                        .map_err(|_| invalid("CLI threshold must be numeric")),
                    _ => Ok(RawValue::String(text.clone())),
                })
                .transpose()?,
        };
        if let Some(value) = value {
            sources.cli.push((key.path().into(), value));
        }
    }
    // Snapshot only recognized namespace candidates; the resolver rejects unknown SR_*.
    for (name, value) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"SR_")
            || name == "TYPESAFE_API_KEY"
            || name == "TYPESAFE_ENDPOINT"
        {
            if sources.environment.len() == MAX_LAYER_ENTRIES {
                return Err(invalid("Too many environment settings"));
            }
            sources.environment.push((name, value));
        }
    }
    timely(clock)?;
    // The current directory is the exact workspace. Never run Git or search ancestors.
    let workspace = std::env::current_dir().map_err(|_| invalid("Workspace is unavailable"))?;
    let files = ConfigFiles::new(workspace.clone(), user_config_root()?);
    let resolved = files.load(clock, sources)?;
    timely(clock)?;
    let json_output =
        doctor.get_flag("json") || (!doctor.get_flag("table") && !io::stdout().is_terminal());
    if !doctor.get_flag("config") {
        return readiness(clock, &workspace, &resolved, gate, json_output);
    }
    let report = config_report(&resolved);
    if json_output {
        Ok(format!("{report}\n"))
    } else {
        let mut output = String::from("SETTING\tVALUE\tSOURCE\n");
        for (key, entry) in report["settings"]
            .as_object()
            .expect("report settings object")
        {
            output.push_str(&format!(
                "{key}\t{}\t{}\n",
                entry["value"], entry["sources"]
            ));
        }
        Ok(output)
    }
}

fn rank_command(
    clock: &EntryClock,
    rank_matches: Option<&clap::ArgMatches>,
) -> Result<String, Failure> {
    timely(clock)?;
    let json_output = rank_matches
        .map(|m| m.get_flag("json") || (!m.get_flag("table") && !io::stdout().is_terminal()))
        .unwrap_or_else(|| !io::stdout().is_terminal());

    let offline = rank_matches.is_some_and(|m| m.get_flag("offline"));
    let allow_network = rank_matches.is_some_and(|m| m.get_flag("allow-network"));
    let dry_run = rank_matches.is_some_and(|m| m.get_flag("dry-run"));
    let no_cache = rank_matches.is_some_and(|m| m.get_flag("no-cache"));
    let no_ledger = rank_matches.is_some_and(|m| m.get_flag("no-ledger"));
    let no_persist = rank_matches.is_some_and(|m| m.get_flag("no-persist"));
    let save_case_path = rank_matches
        .and_then(|m| m.get_one::<String>("save-case"))
        .map(PathBuf::from);
    let save_case = save_case_path.is_some();

    let flags = crate::privacy::EffectFlags {
        offline,
        allow_network,
        dry_run,
        no_cache,
        no_ledger,
        no_persist,
        save_case,
    };
    let gate = crate::effects::EffectGate::new(flags, crate::effects::Scope::Rank).map_err(
        |conflicts| {
            let first = conflicts
                .first()
                .expect("from_flags reports at least one conflict on error");
            (2u8, "invalid-usage", first.to_string())
        },
    )?;

    let mut sources = ConfigSources::default();
    if let Some(m) = rank_matches {
        for key in SettingKey::ALL {
            let Some(flag) = key.spec().cli_flag else {
                continue;
            };
            let name = flag.trim_start_matches('-');
            let value = match name {
                "shadow" if m.get_flag(name) => Some(RawValue::String("shadow".into())),
                "no-tools" if m.get_flag(name) => Some(RawValue::Bool(true)),
                "shadow" | "no-tools" => None,
                _ => m
                    .get_one::<String>(name)
                    .map(|text| match key.spec().kind {
                        crate::config::ValueKind::Count { .. }
                        | crate::config::ValueKind::Millis { .. } => text
                            .parse()
                            .map(RawValue::Integer)
                            .map_err(|_| invalid("CLI count must be an integer")),
                        crate::config::ValueKind::Unit { .. } => text
                            .parse()
                            .map(RawValue::Float)
                            .map_err(|_| invalid("CLI threshold must be numeric")),
                        _ => Ok(RawValue::String(text.clone())),
                    })
                    .transpose()?,
            };
            if let Some(value) = value {
                sources.cli.push((key.path().into(), value));
            }
        }
    }

    // Snapshot only recognized namespace candidates; the resolver rejects unknown SR_*.
    for (name, value) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"SR_")
            || name == "TYPESAFE_API_KEY"
            || name == "TYPESAFE_ENDPOINT"
        {
            if sources.environment.len() == MAX_LAYER_ENTRIES {
                return Err(invalid("Too many environment settings"));
            }
            sources.environment.push((name, value));
        }
    }

    let workspace = std::env::current_dir().map_err(|_| invalid("Workspace is unavailable"))?;
    let user_root = user_config_root()?;
    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    // The persistent response cache lives in the platform cache directory
    // ($XDG_CACHE_HOME/sr or ~/.cache/sr); effect flags may still disable it.
    let cache_dir = user_cache_root()?;

    let stdin_supplied = is_stdin_supplied();
    let context = rank_matches
        .and_then(|m| m.get_one::<String>("context"))
        .map(|s| crate::roster::LocalPath::new(PathBuf::from(s)));
    let transcript = rank_matches
        .and_then(|m| m.get_one::<String>("transcript"))
        .map(|s| crate::roster::LocalPath::new(PathBuf::from(s)));
    let harness = rank_matches
        .and_then(|m| m.get_one::<String>("harness"))
        .map(|s| crate::identity::HarnessId::new(s).map_err(|_| invalid("Invalid harness ID")))
        .transpose()?;
    let cass_session = rank_matches
        .and_then(|m| m.get_one::<String>("session"))
        .map(|s| crate::roster::LocalPath::new(PathBuf::from(s)));

    let source_options = crate::context::source::SourceOptions {
        stdin_supplied,
        claude_hook: false,
        context,
        transcript,
        harness,
        cass_session,
        latest: rank_matches.is_some_and(|m| m.get_flag("latest")),
    };

    let roster_file = rank_matches
        .and_then(|m| m.get_one::<String>("roster"))
        .map(PathBuf::from);

    let mut require_skills = Vec::new();
    if let Some(m) = rank_matches
        && let Some(reqs) = m.get_many::<String>("require-skill")
    {
        for req in reqs {
            let id = crate::identity::SkillId::new(req).map_err(|_| {
                (
                    5u8,
                    "unresolved-explicit",
                    format!("Invalid skill ID: {req}"),
                )
            })?;
            require_skills.push(id);
        }
    }

    // Stage-2 evidence for a dry run; validated against the wide candidates.
    let mut shortlist_ids = Vec::new();
    for id in rank_matches
        .and_then(|m| m.get_many::<String>("shortlist-ids"))
        .into_iter()
        .flatten()
    {
        shortlist_ids.push(
            crate::identity::SkillId::new(id)
                .map_err(|_| invalid("Invalid skill ID for --shortlist-ids"))?,
        );
    }

    let explain = rank_matches.is_some_and(|m| m.get_flag("explain"));
    let why_not = rank_matches
        .and_then(|m| m.get_one::<String>("why-not"))
        .map(|s| {
            crate::identity::SkillId::new(s).map_err(|_| {
                (
                    2u8,
                    "invalid-usage",
                    format!("Invalid skill ID for --why-not: {s}"),
                )
            })
        })
        .transpose()?;

    let cursor = rank_matches
        .and_then(|m| m.get_one::<String>("cursor"))
        .map(|s| {
            crate::output::TraceCursor::from_token(s).map_err(|_| {
                (
                    2u8,
                    "invalid-usage",
                    format!("Invalid trace cursor token: {s}"),
                )
            })
        })
        .transpose()?;

    let args = crate::pipeline::RankArgs {
        workspace,
        user_config_root: user_root,
        home,
        cache_dir,
        ledger_dir: None,
        sources,
        gate,
        source_options,
        require_skills,
        shortlist_ids,
        roster_file,
        explain,
        why_not,
        cursor,
        output_json: json_output,
        output_table: !json_output,
        dry_run,
        save_case: save_case_path,
    };

    // The whole-invocation deadline is configurable (--timeout-ms,
    // SR_TIMEOUT_MS, trusted ranking.timeout_ms) but still ends relative to
    // process entry. Invalid configuration keeps the default here; the
    // pipeline's authoritative load reports it.
    let timeout_ms = ConfigFiles::new(args.workspace.clone(), args.user_config_root.clone())
        .load(clock, args.sources.clone())
        .map_or(clock.deadline().total().as_millis(), |resolved| {
            resolved.effective().timeout_ms()
        });
    let clock = &DurationMillis::new("timeout_ms", timeout_ms, crate::config::MAX_TIMEOUT_MS)
        .map_err(crate::runtime::RuntimeError::from)
        .and_then(|total| clock.with_total(total))
        .map_err(|_| invalid("Invalid ranking deadline"))?;
    timely(clock)?;
    let invocation = crate::runtime::ProcessInvocation::from_clock(*clock)
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;
    let outcome = invocation
        .request_cx()
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))
        .and_then(|cx| {
            invocation.runtime().block_on(async {
                crate::pipeline::execute_pipeline(&invocation, &cx, args, None).await
            })
        });
    // Completion must precede the work cutoff, but teardown may use the
    // reserved cleanup window. Do not reclassify timely work as late merely
    // because its successful cleanup entered that window.
    let completed_in_time = timely(clock);
    let output_doc = finish_invocation(invocation, outcome)?;
    validate_rank_completion(completed_in_time, &output_doc)?;

    // An unavailable decision, or a dry-run preview of one, exits with its
    // error category; the full document is still the JSON output.
    if output_doc.exit_code() != crate::output::CliExit::Success {
        let code = output_doc.exit_code() as u8;
        let whole = output_doc.as_value();
        let val = whole.get("local_decision").unwrap_or(whole);
        let kind = val["error"]["kind"]
            .as_str()
            .and_then(|s| {
                crate::output::ErrorKind::ALL
                    .iter()
                    .find(|k| k.as_str() == s)
            })
            .map(|k| k.as_str())
            .unwrap_or("unavailable");
        if json_output {
            let json_str = serde_json::to_string(output_doc.as_value()).unwrap();
            return Err((code, kind, json_str));
        } else {
            let msg = val["error"]["message"].as_str().unwrap_or("Unavailable");
            return Err((code, kind, msg.to_string()));
        }
    }

    if json_output {
        Ok(format!(
            "{}\n",
            serde_json::to_string(output_doc.as_value()).unwrap()
        ))
    } else {
        Ok(output_doc.render_table())
    }
}

fn is_stdin_supplied() -> bool {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return false;
    }
    #[cfg(unix)]
    {
        use nix::sys::stat::{SFlag, fstat};
        use std::os::fd::AsFd;
        if let Ok(stat) = fstat(std::io::stdin().as_fd()) {
            let flag = SFlag::from_bits_truncate(stat.st_mode);
            if flag.contains(SFlag::S_IFIFO)
                || flag.contains(SFlag::S_IFSOCK)
                || flag.contains(SFlag::S_IFREG)
            {
                return true;
            }
            return false;
        }
    }
    false
}

fn user_config_root() -> Result<Option<PathBuf>, Failure> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(invalid("User configuration directory must be absolute"));
        }
        return Ok(Some(path));
    }
    match std::env::var_os("HOME").filter(|p| !p.is_empty()) {
        Some(home) => {
            let home = PathBuf::from(home);
            if !home.is_absolute() {
                return Err(invalid("Home directory must be absolute"));
            }
            #[cfg(target_os = "macos")]
            let directory = home.join("Library/Application Support");
            #[cfg(not(target_os = "macos"))]
            let directory = home.join(".config");
            Ok(Some(directory))
        }
        None => Ok(None),
    }
}

fn user_cache_root() -> Result<Option<PathBuf>, Failure> {
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(invalid("User cache directory must be absolute"));
        }
        return Ok(Some(path.join("sr")));
    }
    match directories::BaseDirs::new() {
        Some(dirs) => Ok(Some(dirs.cache_dir().join("sr"))),
        None => Ok(None),
    }
}

fn hook_claude_command(clock: &EntryClock, m: &clap::ArgMatches) -> Result<String, Failure> {
    timely(clock)?;
    let offline = m.get_flag("offline");
    let allow_network = m.get_flag("allow-network");
    let no_cache = m.get_flag("no-cache");
    let no_ledger = m.get_flag("no-ledger");
    let no_persist = m.get_flag("no-persist");

    let flags = crate::privacy::EffectFlags {
        offline,
        allow_network,
        dry_run: false,
        no_cache,
        no_ledger,
        no_persist,
        save_case: false,
    };
    let gate = crate::effects::EffectGate::new(flags, crate::effects::Scope::Rank).map_err(
        |conflicts| {
            let first = conflicts
                .first()
                .expect("from_flags reports at least one conflict on error");
            (2u8, "invalid-usage", first.to_string())
        },
    )?;

    let mut sources = ConfigSources::default();
    for (name, value) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"SR_")
            || name == "TYPESAFE_API_KEY"
            || name == "TYPESAFE_ENDPOINT"
        {
            if sources.environment.len() == MAX_LAYER_ENTRIES {
                return Err(invalid("Too many environment settings"));
            }
            sources.environment.push((name, value));
        }
    }
    if m.get_flag("shadow") {
        sources
            .cli
            .push(("hook.mode".into(), RawValue::String("shadow".into())));
    }

    let workspace = if let Some(w) = m.get_one::<String>("workspace") {
        PathBuf::from(w)
    } else {
        std::env::current_dir().map_err(|_| invalid("Workspace is unavailable"))?
    };
    let user_root = user_config_root()?;
    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let cache_dir = user_cache_root()?;

    let source_options = crate::context::source::SourceOptions {
        stdin_supplied: true,
        claude_hook: true,
        context: None,
        transcript: None,
        harness: None,
        cass_session: None,
        latest: false,
    };

    let args = crate::pipeline::RankArgs {
        workspace: workspace.clone(),
        user_config_root: user_root.clone(),
        home,
        cache_dir,
        ledger_dir: m.get_one::<String>("dir").map(PathBuf::from),
        sources: sources.clone(),
        gate,
        source_options,
        require_skills: Vec::new(),
        shortlist_ids: Vec::new(),
        roster_file: None,
        explain: false,
        why_not: None,
        cursor: None,
        output_json: true,
        output_table: false,
        dry_run: false,
        save_case: None,
    };

    let timeout_ms = m
        .get_one::<String>("timeout-ms")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            ConfigFiles::new(args.workspace.clone(), args.user_config_root.clone())
                .load(clock, args.sources.clone())
                .map_or(clock.deadline().total().as_millis(), |resolved| {
                    resolved.effective().timeout_ms()
                })
        });
    // Clamp the internal deadline below the installed outer timeout: the
    // harness kills the process at the entry's timeout, and the internal
    // deadline must never exceed that installed budget. Derived from the
    // actual managed entry when readable; otherwise the conservative
    // default-entry budget applies.
    let installed_budget_ms =
        crate::installer::installed_hook_budget_ms(crate::installer::HookHarness::Claude, None)
            .unwrap_or(
                u64::from(
                    crate::installer::DEFAULT_HOOK_TIMEOUT_SECS
                        - crate::installer::HOOK_STARTUP_RESERVE_SECS,
                ) * 1_000,
            );
    let timeout_ms = if timeout_ms > installed_budget_ms {
        let _ = writeln!(
            io::stderr().lock(),
            "sr: configured timeout ({timeout_ms}ms) exceeds installed hook budget ({installed_budget_ms}ms); clamped to installed budget"
        );
        installed_budget_ms
    } else {
        timeout_ms
    };
    let clock = &DurationMillis::new("timeout_ms", timeout_ms, crate::config::MAX_TIMEOUT_MS)
        .map_err(crate::runtime::RuntimeError::from)
        .and_then(|total| clock.with_total(total))
        .map_err(|_| invalid("Invalid ranking deadline"))?;
    timely(clock)?;
    let invocation = crate::runtime::ProcessInvocation::from_clock(*clock)
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;
    let (reval_workspace, reval_user_root, reval_sources) = (
        args.workspace.clone(),
        args.user_config_root.clone(),
        args.sources.clone(),
    );
    let outcome = invocation
        .request_cx()
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))
        .and_then(|cx| {
            invocation.runtime().block_on(async {
                crate::pipeline::execute_pipeline(&invocation, &cx, args, None).await
            })
        });
    let completed_in_time = timely(clock);
    let output_doc = finish_invocation(invocation, outcome)?;
    completed_in_time?;

    // Policy revalidation seam before writing the first byte:
    // Check effective hook mode and configuration validity.
    let config_files = ConfigFiles::new(reval_workspace, reval_user_root);
    let refreshed_config = match config_files.load(clock, reval_sources) {
        Ok(c) => c,
        Err(_) => {
            let _ = writeln!(
                io::stderr().lock(),
                "sr: configuration became invalid before hook publication"
            );
            return Ok(String::new());
        }
    };
    let effective = refreshed_config.effective();
    let effective_mode = effective.hook_mode();

    // In shadow mode (the default in P6 or forced via --shadow), the hook emits zero stdout and returns 0.
    if m.get_flag("shadow") || effective_mode == crate::config::HookMode::Shadow {
        return Ok(String::new());
    }

    // In advisory mode, check adapter qualification before emitting native advice for ranked suggestions.
    // Explicit user directives are not blocked by harness qualification.
    if let crate::output::OutputKind::Decision(crate::output::Decision::Ranked) = output_doc.kind()
        && let Ok(foundation) = crate::adapter::foundation_capabilities()
        && let Some(record) = foundation
            .adapters
            .iter()
            .find(|a| a.adapter_id.as_str() == crate::adapter::CLAUDE_CODE_ID)
        && let crate::adapter::AdviceDisposition::Disabled(reason) = record.advice(
            crate::adapter::CompatibilityQuestion::EmitNativeAdvice,
            None,
        )
    {
        let _ = writeln!(
            io::stderr().lock(),
            "sr: native advice disabled for unverified harness ({reason:?})"
        );
        return Ok(String::new());
    }

    // In advisory mode, render one safe suggestion or explicit list
    let abstention_enabled = false;
    match output_doc.render_claude_hook(abstention_enabled) {
        Ok(Some(envelope)) => match envelope.to_json() {
            Ok(wire) => {
                let mut stdout = io::stdout().lock();
                match stdout
                    .write_all(wire.as_bytes())
                    .and_then(|()| stdout.write_all(b"\n"))
                    .and_then(|()| stdout.flush())
                {
                    Ok(()) => {
                        let total_written = wire.len() + 1;
                        if let Some(event_id) = output_doc
                            .as_value()
                            .get("event_id")
                            .and_then(|v| v.as_str())
                        {
                            let location = if let Some(dir) = m.get_one::<String>("dir") {
                                crate::storage::LedgerLocation::Directory(PathBuf::from(dir))
                            } else {
                                crate::storage::LedgerLocation::Platform
                            };
                            let _ =
                                try_record_cli_emission(clock, location, event_id, total_written);
                        }
                    }
                    Err(e) => {
                        // Short write, broken pipe: delivery remains unknown!
                        let _ = writeln!(io::stderr().lock(), "sr: stdout write failed: {e}");
                    }
                }
            }
            Err(e) => {
                let _ = writeln!(
                    io::stderr().lock(),
                    "sr: hook envelope serialization failed: {e}"
                );
            }
        },
        Ok(None) => {
            // Ordinary abstention or unavailable: zero stdout, exit 0
        }
        Err(e) => {
            // Output limit or unsafe text: quiet fallback with diagnostic to stderr
            let _ = writeln!(io::stderr().lock(), "sr: {e}");
        }
    }

    Ok(String::new())
}

fn install_hook_command(clock: &EntryClock, m: &clap::ArgMatches) -> Result<String, Failure> {
    timely(clock)?;
    let Some(("claude", claude_matches)) = m.subcommand() else {
        return Err((
            2,
            "invalid-usage",
            "Supported harness is 'claude'; e.g. sr install-hook claude [--apply]".into(),
        ));
    };

    if claude_matches.get_flag("help") {
        return Ok(HELP.into());
    }

    let apply = claude_matches.get_flag("apply");
    let settings_file = claude_matches
        .get_one::<String>("settings-file")
        .map(PathBuf::from);
    let binary_path = claude_matches
        .get_one::<String>("binary-path")
        .map(PathBuf::from);
    let timeout_secs = claude_matches
        .get_one::<String>("timeout-secs")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(crate::installer::DEFAULT_HOOK_TIMEOUT_SECS);

    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let user_root = user_config_root().unwrap_or(None);
    let mut sources = ConfigSources::default();
    for (name, value) in std::env::vars_os() {
        if (name.as_encoded_bytes().starts_with(b"SR_")
            || name == "TYPESAFE_API_KEY"
            || name == "TYPESAFE_ENDPOINT")
            && sources.environment.len() < MAX_LAYER_ENTRIES
        {
            sources.environment.push((name, value));
        }
    }
    let config_files = ConfigFiles::new(workspace, user_root);
    let resolved = config_files.load(clock, sources);
    let effective_mode = resolved
        .as_ref()
        .map(|c| c.effective().hook_mode())
        .unwrap_or(crate::config::HookMode::Shadow);
    let deadline_ms = resolved.map_or(crate::limits::DEFAULT_INVOCATION_DEADLINE_MS, |c| {
        c.effective().timeout_ms()
    });

    // The installed outer timeout must strictly cover the effective internal
    // deadline plus the startup reserve: the harness clock starts at spawn,
    // the internal clock at process entry, and an equal outer timeout can
    // kill sr during its output reserve. The effective deadline, not only the
    // product default, decides the minimum.
    if let Err(refusal) = crate::installer::check_hook_timeout(timeout_secs, deadline_ms) {
        return Err((2, "invalid-usage", refusal.to_string()));
    }

    let options = crate::installer::InstallOptions {
        apply,
        harness: crate::installer::HookHarness::Claude,
        settings_file,
        timeout_secs,
        binary_path,
        effective_mode,
    };

    match crate::installer::install_hook(&options) {
        Ok(crate::installer::InstallOutcome::Preview { diff, message }) => {
            Ok(format!("{diff}\n{message}\n"))
        }
        Ok(crate::installer::InstallOutcome::Applied {
            backup_path,
            message,
        }) => Ok(format!(
            "{message}\n(Backup created at {})\n",
            backup_path.display()
        )),
        Ok(crate::installer::InstallOutcome::AlreadyInstalled { message }) => {
            Ok(format!("{message}\n"))
        }
        Ok(crate::installer::InstallOutcome::Conflict { message }) => {
            Err((2, "invalid-usage", message))
        }
        Err(crate::installer::InstallerError::MalformedSettings(msg)) => {
            Err((7, "malformed-input", msg))
        }
        Err(crate::installer::InstallerError::EnterpriseRestricted(msg)) => {
            Err((2, "invalid-usage", msg))
        }
        Err(crate::installer::InstallerError::ExternalModificationDetected(msg)) => {
            Err((2, "invalid-usage", msg))
        }
        Err(crate::installer::InstallerError::LockBusy(msg)) => Err((2, "invalid-usage", msg)),
        Err(crate::installer::InstallerError::InvalidUsage(msg)) => Err((2, "invalid-usage", msg)),
        Err(crate::installer::InstallerError::Io(err)) => {
            Err((9, "storage-failure", err.to_string()))
        }
    }
}

fn uninstall_hook_command(clock: &EntryClock, m: &clap::ArgMatches) -> Result<String, Failure> {
    timely(clock)?;
    let Some(("claude", claude_matches)) = m.subcommand() else {
        return Err((
            2,
            "invalid-usage",
            "Supported harness is 'claude'; e.g. sr uninstall-hook claude [--apply]".into(),
        ));
    };

    if claude_matches.get_flag("help") {
        return Ok(HELP.into());
    }

    let apply = claude_matches.get_flag("apply");
    let settings_file = claude_matches
        .get_one::<String>("settings-file")
        .map(PathBuf::from);
    let binary_path = claude_matches
        .get_one::<String>("binary-path")
        .map(PathBuf::from);
    let timeout_secs = claude_matches
        .get_one::<String>("timeout-secs")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(crate::installer::DEFAULT_HOOK_TIMEOUT_SECS);

    match crate::installer::uninstall_hook(
        crate::installer::HookHarness::Claude,
        settings_file,
        binary_path,
        timeout_secs,
        apply,
    ) {
        Ok(crate::installer::UninstallOutcome::Preview { diff, message }) => {
            Ok(format!("{diff}\n{message}\n"))
        }
        Ok(crate::installer::UninstallOutcome::Applied {
            backup_path,
            message,
        }) => Ok(format!(
            "{message}\n(Backup created at {})\n",
            backup_path.display()
        )),
        Ok(crate::installer::UninstallOutcome::NotInstalled { message }) => {
            Ok(format!("{message}\n"))
        }
        Ok(crate::installer::UninstallOutcome::Conflict { message }) => {
            Err((2, "invalid-usage", message))
        }
        Err(crate::installer::InstallerError::MalformedSettings(msg)) => {
            Err((7, "malformed-input", msg))
        }
        Err(crate::installer::InstallerError::EnterpriseRestricted(msg)) => {
            Err((2, "invalid-usage", msg))
        }
        Err(crate::installer::InstallerError::ExternalModificationDetected(msg)) => {
            Err((2, "invalid-usage", msg))
        }
        Err(crate::installer::InstallerError::LockBusy(msg)) => Err((2, "invalid-usage", msg)),
        Err(crate::installer::InstallerError::InvalidUsage(msg)) => Err((2, "invalid-usage", msg)),
        Err(crate::installer::InstallerError::Io(err)) => {
            Err((9, "storage-failure", err.to_string()))
        }
    }
}

/// Fixed invocation paths for bounded initial reads and consequential rereads.
/// The caller supplies the independently selected workspace, never a path from
/// normalized session input. This resolver performs no discovery or state writes.
pub struct ConfigFiles {
    workspace: PathBuf,
    user_root: Option<PathBuf>,
}

impl ConfigFiles {
    pub fn new(workspace: PathBuf, user_root: Option<PathBuf>) -> Self {
        Self {
            workspace,
            user_root,
        }
    }

    pub fn load(
        &self,
        clock: &EntryClock,
        mut sources: ConfigSources,
    ) -> Result<ResolvedConfig, Failure> {
        sources.trusted_user = self.read_user(clock)?;
        sources.project = read_config(
            clock,
            &self.workspace,
            Path::new(".sr/config.toml"),
            "project",
        )?;
        let resolved =
            ResolvedConfig::resolve(sources, 0).map_err(|error| invalid(error.to_string()))?;
        timely(clock)?;
        Ok(resolved)
    }

    /// Refresh mutable file layers while retaining validated invocation CLI and
    /// environment. Read/parse/deadline errors cannot yield an authorizing receipt.
    pub fn refresh(
        &self,
        clock: &EntryClock,
        previous: &ResolvedConfig,
        receipt: &crate::config::PolicyReceipt,
        boundary: crate::config::PolicyBoundary,
    ) -> Result<(ResolvedConfig, crate::config::Revalidation), Failure> {
        let user = self.read_user(clock)?;
        let project = read_config(
            clock,
            &self.workspace,
            Path::new(".sr/config.toml"),
            "project",
        )?;
        let generation = receipt
            .generation()
            .checked_add(1)
            .ok_or_else(|| invalid("Configuration generation exhausted"))?;
        let current = previous
            .reresolve_files(user, project, generation)
            .map_err(|error| invalid(error.to_string()))?;
        timely(clock)?;
        let comparison = receipt.compare(&current.receipt(receipt.effects()), boundary);
        Ok((current, comparison))
    }

    fn read_user(&self, clock: &EntryClock) -> Result<Vec<(String, RawValue)>, Failure> {
        match &self.user_root {
            Some(root) => read_config(clock, root, Path::new("sr/config.toml"), "trusted-user"),
            None => Ok(Vec::new()),
        }
    }
}

fn read_config(
    clock: &EntryClock,
    root: &Path,
    path: &Path,
    layer: &str,
) -> Result<Vec<(String, RawValue)>, Failure> {
    timely(clock)?;
    let authorized = match AuthorizedRoot::open_absolute(root) {
        Ok(root) => root,
        Err(ReadError::NotFound) => return Ok(Vec::new()),
        Err(_) => return Err(invalid(format!("{layer}: configuration root unavailable"))),
    };
    let bytes = match AuthorizedRoots::single(authorized).read_bounded(0, path, CONFIG_FILE_BYTES) {
        Ok(bytes) => bytes,
        Err(ReadError::NotFound) => return Ok(Vec::new()),
        Err(_) => {
            return Err(invalid(format!(
                "{layer}: configuration must be a bounded authorized regular file"
            )));
        }
    };
    timely(clock)?;
    let text = std::str::from_utf8(bytes.bytes())
        .map_err(|_| invalid(format!("{layer}: invalid UTF-8 configuration")))?;
    let entries = decode_config(text)
        .map_err(|_| invalid(format!("{layer}: malformed or excessive configuration")))?;
    timely(clock)?;
    Ok(entries)
}

fn decode_config(text: &str) -> Result<Vec<(String, RawValue)>, ()> {
    if text.len() > CONFIG_FILE_BYTES.max() {
        return Err(());
    }
    // TOML's bounded parser rejects duplicate keys/table definitions before a map exists.
    let table: toml::Table = text.parse().map_err(|_| ())?;
    let mut entries = Vec::new();
    flatten_table(table, "", 0, &mut entries)?;
    Ok(entries)
}

fn flatten_table(
    table: toml::Table,
    prefix: &str,
    depth: usize,
    entries: &mut Vec<(String, RawValue)>,
) -> Result<(), ()> {
    if depth > 32 {
        return Err(());
    }
    for (name, value) in table {
        if name.contains('.') || name.len() + prefix.len() > crate::config::MAX_KEY_BYTES {
            return Err(());
        }
        let key = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}.{name}")
        };
        let raw = match value {
            toml::Value::Table(table) if !table.is_empty() => {
                flatten_table(table, &key, depth + 1, entries)?;
                continue;
            }
            toml::Value::Boolean(value) => RawValue::Bool(value),
            toml::Value::Integer(value) => RawValue::Integer(value),
            toml::Value::Float(value) => RawValue::Float(value),
            toml::Value::String(value) => RawValue::String(value),
            toml::Value::Array(values) => {
                if values.len() > crate::config::MAX_LIST_ITEMS {
                    return Err(());
                }
                RawValue::StringList(
                    values
                        .into_iter()
                        .map(|value| match value {
                            toml::Value::String(text) => Ok(text),
                            _ => Err(()),
                        })
                        .collect::<Result<_, _>>()?,
                )
            }
            _ => return Err(()),
        };
        if entries.len() == MAX_LAYER_ENTRIES {
            return Err(());
        }
        entries.push((key, raw));
    }
    Ok(())
}

/// `sr doctor`: independent local readiness checks. Configuration is already
/// valid here; an invalid policy fails before any discovery.
fn readiness(
    clock: &EntryClock,
    workspace: &Path,
    config: &ResolvedConfig,
    gate: crate::effects::EffectGate,
    json_output: bool,
) -> Result<String, Failure> {
    use crate::readiness::{Inputs, RosterCheck, TransportIdentity, assess_transport, report};
    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    // Readiness answers whether `sr rank` can work, so it resolves the roster
    // under the same provisional Claude contract rank uses. The report still
    // labels that visibility unverified.
    let resolved = resolve_workspace_roster(
        clock,
        workspace,
        home.as_deref(),
        config.effective().roster_roots(),
        crate::roster::Visibility::Verified {
            contract_version: crate::pipeline::PROVISIONAL_CLAUDE_CONTRACT.into(),
        },
    );
    let listing = resolved.as_ref().ok().map(crate::roster::inspect::listing);
    let roster = match (&resolved, &listing) {
        (Ok(_), Some(listing)) => RosterCheck::Resolved(listing.evidence()),
        (Err((6, _, _)), _) => RosterCheck::Timeout,
        _ => RosterCheck::Unusable,
    };
    timely(clock)?;
    let origin = match config.effective().endpoint() {
        Some(endpoint) => crate::jev::CanonicalOrigin::from_override(endpoint)
            .map_err(|_| invalid("The endpoint override is not a valid origin"))?,
        None => crate::jev::CanonicalOrigin::production(),
    };
    // No live check writes transport evidence in this build, so none is read.
    let transport = assess_transport(
        None,
        &TransportIdentity::current(config, origin.as_str()),
        0,
    );
    let ledger = {
        let invocation = crate::runtime::ProcessInvocation::from_clock(*clock);
        if let Ok(inv) = invocation {
            if let Ok(cx) = inv.request_cx() {
                match crate::storage::open_ledger(
                    &inv,
                    &cx,
                    crate::storage::LedgerAccess::ExistingOnly,
                    crate::storage::LedgerLocation::Platform,
                ) {
                    Ok(crate::storage::LedgerOpen::Ready(store)) => {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64;
                        let debt = store.cleanup_debt(now_ms).ok();
                        crate::readiness::LedgerCheck::Ready { cleanup_debt: debt }
                    }
                    Ok(crate::storage::LedgerOpen::ReadOnly(store)) => {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64;
                        let debt = store.cleanup_debt(now_ms).ok();
                        crate::readiness::LedgerCheck::ReadOnly { cleanup_debt: debt }
                    }
                    Ok(crate::storage::LedgerOpen::Missing) => {
                        crate::readiness::LedgerCheck::NotAvailable
                    }
                    Ok(crate::storage::LedgerOpen::Disabled) => {
                        crate::readiness::LedgerCheck::NotAvailable
                    }
                    Err(_) => crate::readiness::LedgerCheck::NotAvailable,
                }
            } else {
                crate::readiness::LedgerCheck::NotAvailable
            }
        } else {
            crate::readiness::LedgerCheck::NotAvailable
        }
    };
    let value = report(&Inputs {
        config,
        gate,
        roster,
        transport,
        ledger,
    });
    timely(clock)?;
    if json_output {
        return Ok(format!("{value}\n"));
    }
    let mut output = String::from("CHECK\tSTATE\tNEXT STEP\n");
    for (check, entry) in value["checks"].as_object().expect("report checks object") {
        let state = entry["state"]
            .as_str()
            .or_else(|| entry["mode"].as_str())
            .unwrap_or("-");
        let next = entry["next_step"].as_str().unwrap_or("-");
        output.push_str(&format!("{check}\t{state}\t{next}\n"));
    }
    Ok(output)
}

fn config_report(config: &ResolvedConfig) -> Value {
    let effective = config.effective();
    let mut settings = serde_json::Map::new();
    for key in SettingKey::ALL {
        use SettingKey::*;
        let value = match key {
            RankingTop => json!(effective.top()),
            RankingShortlist => json!(effective.shortlist()),
            RankingGate => json!(effective.gate()),
            RankingFits => json!(effective.fits()),
            RankingWFit => json!(effective.w_fit()),
            RankingWPrior => json!(effective.w_prior()),
            RankingWPhase => json!(effective.w_phase()),
            RankingTimeoutMs => json!(effective.timeout_ms()),
            ContextMessages => json!(effective.messages()),
            ContextBudgetChars => json!(effective.budget_chars()),
            ContextProfile => json!(effective.context_profile().as_str()),
            ContextNoTools => json!(effective.no_tools()),
            HookMode => json!(effective.hook_mode().as_str()),
            TypesafeApiKey => json!({"present":config.credential().is_some()}),
            TypesafeEndpoint => json!({"override_present":effective.endpoint().is_some()}),
            ProviderModel => json!(
                crate::privacy::redaction::Redactor::default()
                    .redact_field(effective.model().as_str())
                    .map(|v| v.as_str().to_owned())
                    .unwrap_or_else(|_| "[private]".into())
            ),
            ContextTranscriptRoots => json!({"count":effective.transcript_roots().len()}),
            RosterRoots => json!({"count":effective.roster_roots().len()}),
            RankingExcludeSkills => json!({"count":effective.exclude_skills().len()}),
            NetworkEnabled => json!(effective.trusted_user_network_enabled()),
            NetworkProxy | PrivacyRedaction | PrivacyRawRetention => continue,
        };
        let sources: Vec<_> = match config.source(*key) {
            ValueSource::Single(layer) => vec![layer.as_str()],
            ValueSource::Union(layers) => layers.iter().map(|layer| layer.as_str()).collect(),
        };
        settings.insert(key.path().into(), json!({"value":value,"sources":sources}));
    }
    json!({"schema_version":1,"command":"doctor-config","scope":"local-configuration-only",
        "policy_fingerprint":config.effective().policy_fingerprint().as_str(),"settings":settings})
}

/// `sr roster`: inspect, snapshot or compare the Claude roots of the current
/// workspace. Output is JSON; tables belong to the renderer.
fn roster_listing(clock: &EntryClock, matches: &clap::ArgMatches) -> Result<String, Failure> {
    use crate::roster::inspect::{MAX_PAGE, PageError, listing, page};
    let usage = |message: &str| (2u8, "invalid-usage", message.to_owned());
    let limit = match matches.get_one::<String>("limit") {
        Some(text) => text
            .parse::<usize>()
            .map_err(|_| usage("--limit must be a whole number from 1 to 128"))?,
        None => MAX_PAGE,
    };
    let cursor = matches.get_one::<String>("cursor").map(String::as_str);
    timely(clock)?;
    let workspace = std::env::current_dir().map_err(|_| {
        (
            5u8,
            "unusable-roster",
            "The workspace directory is unavailable".to_owned(),
        )
    })?;
    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let mut sources = ConfigSources::default();
    for (name, value) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"SR_")
            || name == "TYPESAFE_API_KEY"
            || name == "TYPESAFE_ENDPOINT"
        {
            if sources.environment.len() == MAX_LAYER_ENTRIES {
                return Err(invalid("Too many environment settings"));
            }
            sources.environment.push((name, value));
        }
    }
    let config = ConfigFiles::new(workspace.clone(), user_config_root()?).load(clock, sources)?;
    let roster = resolve_workspace_roster(
        clock,
        &workspace,
        home.as_deref(),
        config.effective().roster_roots(),
        crate::roster::Visibility::Unverified,
    )?;
    if let Some(target) = matches.get_one::<String>("snapshot") {
        let fresh = workspace_snapshot(&roster, &workspace, home.as_deref());
        timely(clock)?;
        return export_snapshot(&fresh, &workspace.join(target));
    }
    if let Some(saved) = matches.get_one::<String>("diff") {
        let saved = crate::roster::snapshot::read_snapshot_file(&workspace.join(saved))
            .map_err(snapshot_failure)?;
        let fresh = workspace_snapshot(&roster, &workspace, home.as_deref());
        let diff = crate::roster::snapshot::diff(&saved, &fresh).map_err(snapshot_failure)?;
        timely(clock)?;
        let value = serde_json::to_value(&diff).map_err(|_| {
            (
                5u8,
                "unusable-roster",
                "The comparison could not be rendered".to_owned(),
            )
        })?;
        return Ok(format!("{value}\n"));
    }
    let listing = listing(&roster);
    let page = page(&listing, cursor, limit).map_err(|error| match error {
        PageError::InvalidLimit => usage("--limit must be a whole number from 1 to 128"),
        PageError::InvalidCursor => usage("Unrecognized --cursor; restart without it"),
        PageError::RosterChanged => (
            5u8,
            "roster-changed",
            "The roster changed since this cursor was issued; restart without --cursor".to_owned(),
        ),
    })?;
    timely(clock)?;
    Ok(format!("{}\n", page.to_json()))
}

/// Discover and resolve the documented Claude roots of `workspace` under the
/// given visibility. `sr roster` claims no precedence (`Unverified`); doctor
/// uses rank's provisional contract so its readiness matches ranking.
fn resolve_workspace_roster(
    clock: &EntryClock,
    workspace: &Path,
    home: Option<&Path>,
    configured: &[crate::privacy::SkillRoot],
    visibility: crate::roster::Visibility,
) -> Result<crate::roster::resolution::ResolvedRoster, Failure> {
    let unusable = |message: &str| (5u8, "unusable-roster", message.to_owned());
    let plan = crate::roster::discovery::claude_code_plan_with_roots(
        workspace, home, visibility, configured,
    )
    .map_err(|_| unusable("The documented skill roots could not be planned"))?;
    let runtime_failure = |error| {
        if matches!(error, crate::runtime::RuntimeError::Deadline(_)) {
            (6u8, "timeout", "Local inspection deadline exceeded".into())
        } else {
            unusable("The local runtime is unavailable")
        }
    };
    let invocation =
        crate::runtime::ProcessInvocation::from_clock(*clock).map_err(runtime_failure)?;
    let outcome = invocation
        .request_cx()
        .map_err(runtime_failure)
        .and_then(|cx| {
            crate::roster::resolution::resolve_claude_plan(
                &plan,
                &std::collections::BTreeMap::new(),
                &cx,
                clock,
            )
            .map_err(|error| match error {
                crate::roster::resolution::ResolutionError::Deadline
                | crate::roster::resolution::ResolutionError::Cancelled => (
                    6u8,
                    "timeout",
                    "Local inspection deadline exceeded".to_owned(),
                ),
                _ => unusable("The roster could not be resolved"),
            })
        });
    finish_invocation(invocation, outcome)
}

/// A snapshot in this workspace's namespace; paths enter only as digests.
fn workspace_snapshot(
    roster: &crate::roster::resolution::ResolvedRoster,
    workspace: &Path,
    home: Option<&Path>,
) -> crate::roster::snapshot::Snapshot {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let home = home.map(canonical);
    let namespace = crate::roster::snapshot::Namespace::new(
        crate::adapter::CLAUDE_CODE_ID,
        &canonical(workspace),
        home.as_deref(),
    );
    crate::roster::snapshot::capture(roster, namespace)
}

fn snapshot_failure(error: crate::roster::snapshot::SnapshotError) -> Failure {
    use crate::roster::snapshot::SnapshotError;
    let kind = error.kind();
    let message = match error {
        SnapshotError::Malformed => "The saved snapshot is malformed or has unknown fields",
        SnapshotError::TooLarge => "The saved snapshot exceeds 32 MiB",
        SnapshotError::TooManyRecords => "The snapshot exceeds 10,000 records",
        SnapshotError::Unreadable => "The saved snapshot is not a readable regular file",
        SnapshotError::Incompatible => {
            "The saved snapshot belongs to a different schema, harness or workspace"
        }
    };
    (kind.exit_code() as u8, kind.as_str(), message.to_owned())
}

/// Export an owner-only snapshot without replacing any existing file.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn export_snapshot(
    snapshot: &crate::roster::snapshot::Snapshot,
    target: &Path,
) -> Result<String, Failure> {
    use crate::storage::export::{ExportConfig, ExportError, export_private_atomic};
    let bytes = snapshot.to_bytes().map_err(snapshot_failure)?;
    export_private_atomic(target, &bytes, ExportConfig::for_snapshot()).map_err(|error| {
        let message = match &error {
            ExportError::TargetAlreadyExists(_) => {
                "The snapshot target already exists; refusing to overwrite it"
            }
            ExportError::Oversized { .. } => "The snapshot exceeds 32 MiB",
            ExportError::InvalidDirectory(_) | ExportError::Permissions(_) => {
                "The snapshot directory is missing or not private enough"
            }
            ExportError::Io(_) => "The snapshot could not be written",
            ExportError::Durability(_) => {
                "The snapshot was published, but durability could not be confirmed; inspect the target before retrying"
            }
        };
        let kind = error.kind();
        (kind.exit_code() as u8, kind.as_str(), message.to_owned())
    })?;
    let receipt = json!({
        "schema": crate::roster::snapshot::SNAPSHOT_SCHEMA,
        "exported": true,
        "records": snapshot.records.len(),
        "snapshot": snapshot.snapshot,
        "partial": snapshot.partial,
    });
    Ok(format!("{receipt}\n"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn export_snapshot(
    _snapshot: &crate::roster::snapshot::Snapshot,
    _target: &Path,
) -> Result<String, Failure> {
    Err((
        9,
        "storage-failure",
        "Snapshot export is not qualified on this platform".to_owned(),
    ))
}

#[cfg(test)]
mod invocation_cleanup_tests {
    use super::*;
    use crate::runtime::ProcessInvocation;
    use std::time::Duration;

    #[test]
    fn work_cutoff_preserves_unavailable_but_never_late_advice_or_artifacts() {
        let timeout = (
            6,
            "timeout",
            "Local inspection deadline exceeded".to_owned(),
        );
        let unavailable = OutputDocument::failure(crate::output::ErrorKind::Timeout, false);
        assert!(validate_rank_completion(Err(timeout.clone()), &unavailable).is_ok());
        for fixture in [
            include_str!("../tests/fixtures/output-ranked.v1.json"),
            include_str!("../tests/fixtures/output-explicit.v1.json"),
            include_str!("../tests/fixtures/output-abstain.v1.json"),
            include_str!("../tests/fixtures/output-preview.v1.json"),
            include_str!("../tests/fixtures/output-report.v1.json"),
        ] {
            let document =
                OutputDocument::from_value(serde_json::from_str(fixture).unwrap()).unwrap();
            assert_eq!(
                validate_rank_completion(Err(timeout.clone()), &document),
                Err(timeout.clone())
            );
            assert!(validate_rank_completion(Ok(()), &document).is_ok());
        }
    }

    #[test]
    fn expired_roster_runtime_is_a_timeout_not_an_invalid_roster() {
        let clock = EntryClock::capture_with(
            DurationMillis::new("test_total", 2, 3_000).unwrap(),
            DurationMillis::new("test_cleanup", 1, 3_000).unwrap(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let workspace = std::env::current_dir().unwrap();
        let result = resolve_workspace_roster(
            &clock,
            &workspace,
            None,
            &[],
            crate::roster::Visibility::Unverified,
        );
        assert!(matches!(result, Err((6, "timeout", _))), "{result:?}");
    }

    #[test]
    fn unfinished_runtime_cannot_publish_success() {
        let clock = EntryClock::capture_with(
            DurationMillis::new("test_total", 1_000, 3_000).unwrap(),
            DurationMillis::new("test_cleanup", 200, 3_000).unwrap(),
        )
        .unwrap();
        let invocation = ProcessInvocation::from_clock(clock).unwrap();
        let retained = invocation.runtime().handle();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(
                clock.remaining_until_expiry().as_millis() + 200,
            ));
            drop(retained);
        });
        let outcome = finish_invocation(invocation, Ok("must not be emitted"));
        holder.join().unwrap();
        assert!(matches!(outcome, Err((6, "timeout", _))), "{outcome:?}");
    }

    #[test]
    fn completed_runtime_preserves_success_and_failure() {
        for outcome in [
            Ok("result"),
            Err((7, "malformed-input", "Invalid input".into())),
        ] {
            let invocation = ProcessInvocation::enter().unwrap();
            let expected = outcome.clone();
            assert_eq!(finish_invocation(invocation, outcome), expected);
        }
    }
}

#[cfg(test)]
mod documented_flag_tests {
    #[test]
    fn readme_flag_tables_and_prose_have_known_flag_inventory() {
        let app = super::command();
        let readme = include_str!("../README.md");
        let flag_pattern = regex::Regex::new(r"--[a-z][a-z0-9-]*").unwrap();
        let mut known = std::collections::BTreeSet::new();
        fn collect(command: &clap::Command, known: &mut std::collections::BTreeSet<String>) {
            known.extend(
                command
                    .get_arguments()
                    .filter_map(|arg| arg.get_long().map(str::to_owned)),
            );
            for child in command.get_subcommands() {
                collect(child, known);
            }
        }
        collect(&app, &mut known);
        // These belong to whole commands whose absence is already published.
        for (owner, flags) in [
            ("calibrate", &["evaluation", "rollback"][..]),
            ("budget", &["max-attempts", "window"][..]),
            ("snooze", &["all", "clear", "for"][..]),
        ] {
            if crate::capabilities::planned_command_phase(owner).is_some() {
                known.extend(flags.iter().map(|flag| (*flag).to_owned()));
            }
        }
        // Cargo and install.sh options in installation prose are not sr flags.
        let external = [
            "bin",
            "path",
            "release",
            "locked",
            "features",
            "source",
            "verify",
            "easy-mode",
            "no-configure",
        ];
        let mut checked = 0;
        for flag in flag_pattern.find_iter(readme) {
            let name = &flag.as_str()[2..];
            assert!(
                known.contains(name) || external.contains(&name),
                "README documents unclassified flag --{name}"
            );
            checked += 1;
        }
        assert!(
            checked > 50,
            "README flag inventory must not pass vacuously"
        );
        // Tables have an explicit command scope, unlike incidental prose mentions.
        let mut scope = "rank";
        let mut rows = 0;
        for line in readme.lines() {
            if line.starts_with('#') {
                scope = if line == "### Evaluation controls" {
                    "eval"
                } else {
                    "rank"
                };
            }
            if line.starts_with("| `--") {
                let flag = flag_pattern
                    .find(line)
                    .unwrap()
                    .as_str()
                    .trim_start_matches("--");
                assert!(
                    app.find_subcommand(scope)
                        .unwrap()
                        .get_arguments()
                        .any(|arg| arg.get_long() == Some(flag)),
                    "{scope} table contains unknown --{flag}"
                );
                rows += 1;
            }
        }
        assert!(rows > 20);
    }

    #[test]
    fn readme_command_examples_use_implemented_or_registered_planned_flags() {
        let app = super::command();
        let mut invocation = String::new();
        let mut checked = 0;
        for line in include_str!("../README.md").lines() {
            let line = line.trim();
            if invocation.is_empty() && !line.starts_with("sr ") {
                continue;
            }
            let line = line.split('#').next().unwrap().trim();
            invocation.push(' ');
            invocation.push_str(line.trim_end_matches('\\'));
            if line.ends_with('\\') {
                continue;
            }
            let words: Vec<_> = invocation.split_whitespace().collect();
            let name = words[1];
            if crate::capabilities::planned_command_phase(name).is_none() {
                let mut parser = if matches!(name, "--help" | "--version" | "-h" | "-V") {
                    &app
                } else if name.starts_with('-') {
                    app.find_subcommand("rank").unwrap()
                } else {
                    app.find_subcommand(name)
                        .expect("documented command must exist")
                };
                // install-hook claude and ledger init have nested parsers.
                for word in words.iter().skip(2) {
                    if let Some(child) = parser.find_subcommand(word) {
                        parser = child;
                    } else {
                        break;
                    }
                }
                for word in &words[1..] {
                    if let Some(flag) = word.strip_prefix("--") {
                        let flag = flag.split('=').next().unwrap();
                        assert!(
                            parser
                                .get_arguments()
                                .any(|arg| arg.get_long() == Some(flag)),
                            "README documents unregistered {name} --{flag}"
                        );
                        checked += 1;
                    }
                }
            }
            invocation.clear();
        }
        assert!(
            checked > 30,
            "README invocation scan must not pass vacuously"
        );
        assert!(
            invocation.is_empty(),
            "unfinished README shell continuation"
        );
    }
}
