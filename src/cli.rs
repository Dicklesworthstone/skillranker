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

const HELP: &str = "SkillRanker — powered by TypeSafe.ai Jev\n\nUsage: sr doctor --config [--json | --table] [--top N] [--shortlist N]\n       sr roster [--json] [--limit N] [--cursor TOKEN]\n       sr --help | --version\n\nLocal configuration and roster inspection only. Ranking requires your own TypeSafe\nAPI key and trusted network consent; ranking is not available here.\n";

fn command() -> Command {
    let mut doctor = Command::new("doctor")
        .disable_help_flag(true)
        .arg(
            Arg::new("help")
                .long("help")
                .short('h')
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("config")
                .long("config")
                .required_unless_present("help")
                .action(ArgAction::SetTrue),
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
                .arg(Arg::new("cursor").long("cursor").action(ArgAction::Set)),
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
                let error = json!({"schema_version":1,"decision":"unavailable","error":{
                    "code":code,"kind":kind,"message":message,"hint":"Use sr --help; inspect trusted-user and project configuration.","retryable":false}});
                let _ = writeln!(io::stdout().lock(), "{error}");
            } else {
                let _ = writeln!(io::stderr().lock(), "sr: {message}");
            }
            code
        }
    }
}

type Failure = (u8, &'static str, String);
fn invalid(message: impl Into<String>) -> Failure {
    (2, "invalid-configuration", message.into())
}
fn timely(clock: &EntryClock) -> Result<(), Failure> {
    clock
        .admit_new_work()
        .map(|_| ())
        .map_err(|_| (6, "timeout", "Local inspection deadline exceeded".into()))
}

fn execute(clock: &EntryClock, args: Vec<OsString>) -> Result<String, Failure> {
    timely(clock)?;
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
    let Some(("doctor", doctor)) = matches.subcommand() else {
        return Err((
            2,
            "invalid-usage",
            "Select an implemented command; use --help".into(),
        ));
    };
    if doctor.get_flag("help") {
        return Ok(HELP.into());
    }
    let flags = crate::privacy::EffectFlags {
        offline: doctor.get_flag("offline"),
        allow_network: doctor.get_flag("allow-network"),
        dry_run: false,
        no_cache: false,
        no_ledger: false,
        no_persist: false,
        save_case: false,
    };
    let _effects = crate::privacy::EffectPolicy::from_flags(flags).map_err(|conflicts| {
        let first = conflicts
            .first()
            .expect("from_flags reports at least one conflict on error");
        (2u8, "invalid-usage", first.to_string())
    })?;
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
    let files = ConfigFiles::new(workspace, user_config_root()?);
    let resolved = files.load(clock, sources)?;
    timely(clock)?;
    let report = config_report(&resolved);
    if doctor.get_flag("json") || (!doctor.get_flag("table") && !io::stdout().is_terminal()) {
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
    json!({"schema_version":1,"command":"doctor-config","scope":"local-configuration-only","settings":settings})
}

/// `sr roster`: inspect the Claude roots of the current workspace. The Claude
/// adapter's visibility is unverified, so no precedence is claimed. Output is
/// JSON; tables belong to the renderer.
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
    let unusable = |message: &str| (5u8, "unusable-roster", message.to_owned());
    let workspace =
        std::env::current_dir().map_err(|_| unusable("The workspace directory is unavailable"))?;
    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let plan = crate::roster::discovery::claude_code_plan(
        &workspace,
        home.as_deref(),
        crate::roster::Visibility::Unverified,
    )
    .map_err(|_| unusable("The documented skill roots could not be planned"))?;
    let invocation = crate::runtime::ProcessInvocation::from_clock(*clock)
        .map_err(|_| unusable("The local runtime is unavailable"))?;
    let cx = invocation
        .request_cx()
        .map_err(|_| unusable("The local runtime is unavailable"))?;
    let roster = crate::roster::resolution::resolve_claude_plan(
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
    })?;
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
