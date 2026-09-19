//! Local configuration and roster inspection. Roster discovery reads only the
//! documented skill roots; there are no child, network, or state writes.
use crate::authorized_read::{AuthorizedRoot, AuthorizedRoots, ReadError};
use crate::config::{
    ConfigSources, MAX_LAYER_ENTRIES, RawValue, ResolvedConfig, SettingKey, ValueSource,
};
use crate::limits::CONFIG_FILE_BYTES;
use crate::runtime::EntryClock;
use clap::{Arg, ArgAction, Command};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

const HELP: &str = "SkillRanker — powered by TypeSafe.ai Jev\n\nUsage: sr [rank] [--context FILE | --transcript FILE --harness NAME | --session PATH]\n                 [--roster FILE] [--require-skill ID] [--dry-run]\n                 [--offline | --allow-network] [--json | --table]\n                 [--top N] [--shortlist M] [--gate FLOAT] [--fits FLOAT]\n                 [--explain] [--why-not ID] [--no-tools] [--no-cache]\n       sr doctor [--json | --table] [--offline | --allow-network]\n       sr doctor --config [--json | --table] [--top N] [--shortlist N]\n       sr roster [--json] [--limit N] [--cursor TOKEN]\n       sr roster --snapshot FILE | --diff FILE\n       sr capabilities [--json]\n       sr --help | --version\n\nRank the next step of an agent session using TypeSafe Jev.\nRequires your own TypeSafe API key (TYPESAFE_API_KEY) and network consent (--allow-network).\n";

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

    Command::new("sr")
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
}

/// Exit and streams are deliberately separate; diagnostics never echo clap/TOML input.
pub fn run(clock: EntryClock) -> u8 {
    let args: Vec<OsString> = std::env::args_os().collect();
    let wants_json = args.iter().any(|arg| arg == "--json") || !io::stdout().is_terminal();
    match execute(&clock, args) {
        Ok(output) => match io::stdout().lock().write_all(output.as_bytes()) {
            Ok(()) => 0,
            Err(_) => 1,
        },
        Err((code, kind, message)) => {
            if wants_json {
                if message.trim_start().starts_with('{')
                    && crate::output::OutputDocument::from_json(message.as_bytes()).is_ok()
                {
                    let _ = writeln!(io::stdout().lock(), "{message}");
                } else {
                    let error_kind = crate::output::ErrorKind::ALL
                        .iter()
                        .copied()
                        .find(|k| k.as_str() == kind)
                        .unwrap_or_else(|| match code {
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

pub type Failure = (u8, &'static str, String);

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
    let matches = command().try_get_matches_from(args).map_err(|_| {
        (
            2,
            "invalid-usage",
            "Unsupported or conflicting arguments; use --help".into(),
        )
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
    // Bare `sr` ranks once, as documented.
    rank_command(clock, None)
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
    let save_case = rank_matches
        .is_some_and(|m| m.contains_id("save-case") && m.get_one::<String>("save-case").is_some());

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
    // Flag conflicts are reported above; the capture itself ships in P5.
    if save_case {
        return Err((
            2,
            "invalid-usage",
            "--save-case (case capture) is planned for P5 and is not available in this build"
                .into(),
        ));
    }

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
        latest: false,
    };

    let roster_file = rank_matches
        .and_then(|m| m.get_one::<String>("roster"))
        .map(PathBuf::from);

    let mut require_skills = Vec::new();
    if let Some(m) = rank_matches {
        if let Some(reqs) = m.get_many::<String>("require-skill") {
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

    let args = crate::pipeline::RankArgs {
        workspace,
        user_config_root: user_root,
        sources,
        gate,
        source_options,
        require_skills,
        roster_file,
        explain,
        why_not,
        output_json: json_output,
        output_table: !json_output,
        dry_run,
    };

    timely(clock)?;
    let invocation = crate::runtime::ProcessInvocation::from_clock(*clock)
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;
    let cx = invocation
        .request_cx()
        .map_err(|_| (6u8, "timeout", "Local runtime unavailable".into()))?;

    let output_doc = invocation
        .runtime()
        .block_on(async { crate::pipeline::execute_pipeline(clock, &cx, args, None).await })?;

    timely(clock)?;

    if output_doc.kind()
        == crate::output::OutputKind::Decision(crate::output::Decision::Unavailable)
    {
        let code = output_doc.exit_code() as u8;
        let val = output_doc.as_value();
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
    let resolved = resolve_workspace_roster(clock, workspace, home.as_deref());
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
    let value = report(&Inputs {
        config,
        gate,
        roster,
        transport,
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
    let roster = resolve_workspace_roster(clock, &workspace, home.as_deref())?;
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

/// Discover and resolve the documented Claude roots of `workspace`. The Claude
/// adapter's visibility is unverified, so no precedence is claimed.
fn resolve_workspace_roster(
    clock: &EntryClock,
    workspace: &Path,
    home: Option<&Path>,
) -> Result<crate::roster::resolution::ResolvedRoster, Failure> {
    let unusable = |message: &str| (5u8, "unusable-roster", message.to_owned());
    let plan = crate::roster::discovery::claude_code_plan(
        workspace,
        home,
        crate::roster::Visibility::Unverified,
    )
    .map_err(|_| unusable("The documented skill roots could not be planned"))?;
    let invocation = crate::runtime::ProcessInvocation::from_clock(*clock)
        .map_err(|_| unusable("The local runtime is unavailable"))?;
    let cx = invocation
        .request_cx()
        .map_err(|_| unusable("The local runtime is unavailable"))?;
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
#[cfg(target_os = "linux")]
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

#[cfg(not(target_os = "linux"))]
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
