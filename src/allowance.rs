//! Trusted, optional shared HTTP-attempt allowance (I03).
//!
//! A user opts in with `sr budget --max-attempts N --window 1h --apply`. The
//! guard then charges every provider attempt, retries and live evaluation
//! included, to a fixed one-hour UTC window per canonical endpoint origin,
//! across every local `sr` process of this user. It is a request allowance,
//! not a monetary or cross-machine billing cap: the count measures admissions,
//! not the instant bytes reach the provider or its billing timestamp.
//!
//! Two stores hold it, and no transaction spans both:
//!
//! - the guard, `sr/allowance.toml` in the trusted user configuration root,
//!   which only `sr budget --apply` writes: `state` is `intent` or `ready`,
//!   with a guard `generation`;
//! - the accounting database, `allowance.sqlite3` in the private cache
//!   directory, written with WAL and `synchronous=FULL`, both read back.
//!
//! Setup therefore runs under the guard's lock in three durable steps: publish
//! an intent with a new generation, bring the accounting to that generation
//! while keeping every charge, then publish the matching ready generation. A
//! crash between steps leaves an intent (or a generation mismatch) that admits
//! no provider attempt until a retried `--apply` resumes the same intent.
//! Every admission takes the same lock, re-reads the guard, checks the
//! generation and commits its debit before the request is sent. A debit is
//! never refunded, whatever happens to the request afterwards.

use crate::jev::{CanonicalOrigin, RankingStage};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Relative to the trusted user configuration root.
pub const GUARD_FILE: &str = "sr/allowance.toml";
/// Inside the private cache directory, beside the response cache.
pub const ACCOUNTING_FILE: &str = "allowance.sqlite3";
pub const MAX_ATTEMPTS_LIMIT: u32 = 10_000;
/// Fixed UTC windows; the only length this build accepts.
pub const WINDOW_MS: u64 = 3_600_000;
pub const WINDOW_TEXT: &str = "1h";
const GUARD_SCHEMA_VERSION: u32 = 1;
const ACCOUNTING_SCHEMA_VERSION: i64 = 1;
/// Charges are enforcement state within the 64 MiB cache allocation; this
/// store never needs more than a few pages per window.
const ACCOUNTING_MAX_BYTES: i64 = 4 * 1024 * 1024;
/// Attempt rows older than this many closed windows no longer apply.
const RETAINED_WINDOWS: u64 = 24;
const GUARD_FILE_MAX_BYTES: u64 = 4096;

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AllowanceError {
    /// Guard or accounting state cannot support enforcement: incomplete setup,
    /// a generation mismatch, a malformed guard, busy or failing storage, a
    /// clock behind recorded charges, or persistence disabled.
    State(String),
    /// The current window's allowance is spent.
    Exhausted {
        max_attempts: u32,
        window_end_unix_ms: u64,
    },
    InvalidRequest(String),
}

impl fmt::Display for AllowanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State(why) => write!(f, "request allowance unavailable: {why}"),
            Self::Exhausted {
                max_attempts,
                window_end_unix_ms,
            } => write!(
                f,
                "request allowance of {max_attempts} attempts is spent until {window_end_unix_ms} \
                 (Unix ms, UTC window end)"
            ),
            Self::InvalidRequest(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for AllowanceError {}

fn state(why: impl Into<String>) -> AllowanceError {
    AllowanceError::State(why.into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardPhase {
    Intent,
    Ready,
}

impl GuardPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Ready => "ready",
        }
    }
}

/// The trusted guard file. Absent means no allowance is configured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Guard {
    pub phase: GuardPhase,
    pub generation: u64,
    pub max_attempts: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardRecord {
    schema_version: u32,
    state: String,
    generation: u64,
    max_attempts: u32,
    window: String,
}

impl Guard {
    pub fn decode(text: &str) -> Result<Self, AllowanceError> {
        let record: GuardRecord =
            toml::from_str(text).map_err(|_| state("the guard file is not strict TOML"))?;
        let phase = match record.state.as_str() {
            "intent" => GuardPhase::Intent,
            "ready" => GuardPhase::Ready,
            _ => return Err(state("the guard file names an unknown state")),
        };
        if record.schema_version != GUARD_SCHEMA_VERSION
            || record.window != WINDOW_TEXT
            || record.generation == 0
            || !(1..=MAX_ATTEMPTS_LIMIT).contains(&record.max_attempts)
        {
            return Err(state("the guard file is outside its supported schema"));
        }
        Ok(Self {
            phase,
            generation: record.generation,
            max_attempts: record.max_attempts,
        })
    }

    pub fn encode(&self) -> String {
        format!(
            "# Managed by `sr budget --apply`: a trusted request allowance guard.\n\
             schema_version = {GUARD_SCHEMA_VERSION}\nstate = \"{}\"\ngeneration = {}\n\
             max_attempts = {}\nwindow = \"{WINDOW_TEXT}\"\n",
            self.phase.as_str(),
            self.generation,
            self.max_attempts
        )
    }
}

/// `1h` is the only window this build accepts.
pub fn parse_window(text: &str) -> Result<u64, AllowanceError> {
    if text == WINDOW_TEXT {
        Ok(WINDOW_MS)
    } else {
        Err(AllowanceError::InvalidRequest(format!(
            "--window '{text}' is unsupported; only fixed one-hour UTC windows (1h) exist"
        )))
    }
}

pub fn parse_max_attempts(text: &str) -> Result<u32, AllowanceError> {
    text.parse::<u32>()
        .ok()
        .filter(|n| (1..=MAX_ATTEMPTS_LIMIT).contains(n))
        .ok_or_else(|| {
            AllowanceError::InvalidRequest(format!(
                "--max-attempts must be an integer from 1 to {MAX_ATTEMPTS_LIMIT}"
            ))
        })
}

/// The fixed UTC window containing `now`: `[start, end)`.
pub const fn window_bounds(now_unix_ms: u64) -> (u64, u64) {
    let start = now_unix_ms - now_unix_ms % WINDOW_MS;
    (start, start + WINDOW_MS)
}

pub fn wall_clock_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}

/// Where the guard and its accounting live.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllowancePaths {
    pub guard: PathBuf,
    pub accounting: PathBuf,
}

impl AllowancePaths {
    pub fn new(user_config_root: &Path, cache_dir: &Path) -> Self {
        Self {
            guard: user_config_root.join(GUARD_FILE),
            accounting: cache_dir.join(ACCOUNTING_FILE),
        }
    }

    /// The standard locations: the guard under `user_config_root`, the
    /// accounting in the private `sr` cache directory (`$XDG_CACHE_HOME/sr`
    /// or the platform cache directory), else beside the guard.
    pub fn for_user(user_config_root: &Path) -> Self {
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.cache_dir().to_path_buf()))
            .map_or_else(|| user_config_root.join("sr"), |root| root.join("sr"));
        Self::new(user_config_root, &cache)
    }

    fn lock_path(&self) -> PathBuf {
        self.guard.with_extension("toml.sr-lock")
    }
}

/// Reads the guard without the lock, for inspection and the stateless check.
pub fn read_guard(paths: &AllowancePaths) -> Result<Option<Guard>, AllowanceError> {
    match fs::symlink_metadata(&paths.guard) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(state("the guard file cannot be inspected")),
        Ok(meta) if !meta.file_type().is_file() || meta.len() > GUARD_FILE_MAX_BYTES => {
            Err(state("the guard path is not a bounded regular file"))
        }
        Ok(_) => {
            let text = fs::read_to_string(&paths.guard)
                .map_err(|_| state("the guard file cannot be read"))?;
            Guard::decode(&text).map(Some)
        }
    }
}

/// The advisory lock that setup and every persistent admission share.
struct GuardLock {
    _flock: nix::fcntl::Flock<File>,
}

impl GuardLock {
    fn acquire(paths: &AllowancePaths, budget: Duration) -> Result<Self, AllowanceError> {
        let parent = paths
            .guard
            .parent()
            .ok_or_else(|| state("the guard has no directory"))?;
        create_private_dir(parent)?;
        let started = Instant::now();
        loop {
            let handle = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(paths.lock_path())
                .map_err(|_| state("the guard lock cannot be opened"))?;
            match nix::fcntl::Flock::lock(handle, nix::fcntl::FlockArg::LockExclusiveNonblock) {
                Ok(flock) => return Ok(Self { _flock: flock }),
                Err(_) if started.elapsed() < budget => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return Err(state("the guard lock is busy")),
            }
        }
    }
}

fn create_private_dir(dir: &Path) -> Result<(), AllowanceError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|_| state("a private directory cannot be created"))
}

/// Replaces the guard durably: temporary file, fsync, rename, directory fsync.
fn publish_guard(paths: &AllowancePaths, guard: &Guard) -> Result<(), AllowanceError> {
    let parent = paths
        .guard
        .parent()
        .ok_or_else(|| state("the guard has no directory"))?;
    let tmp = parent.join(format!(
        "allowance.toml.sr-tmp.{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let write = || -> std::io::Result<()> {
        let mut handle = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        handle.write_all(guard.encode().as_bytes())?;
        handle.sync_all()?;
        fs::rename(&tmp, &paths.guard)?;
        File::open(parent)?.sync_all()
    };
    write().map_err(|_| {
        // Our own unpublished temporary file, never user data.
        let _ = fs::remove_file(&tmp);
        state("the guard could not be published durably")
    })
}

/// Opens the accounting database with the durability enforcement requires:
/// WAL and `synchronous=FULL`, each read back, a page quota and defensive mode.
fn open_accounting(path: &Path, create: bool) -> Result<Connection, AllowanceError> {
    let fail = |_| state("the accounting database is unavailable");
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    if !create && !path.exists() {
        return Err(state("the accounting database is missing"));
    }
    if fs::symlink_metadata(path).is_ok_and(|meta| !meta.file_type().is_file()) {
        return Err(state("the accounting path is not a regular file"));
    }
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    if create {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let connection = Connection::open_with_flags(path, flags).map_err(fail)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| state("the accounting database permissions cannot be restricted"))?;
    }
    connection
        .busy_timeout(Duration::from_millis(25))
        .map_err(fail)?;
    connection
        .set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .map_err(fail)?;
    let mode: String = connection
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(fail)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(fail)?;
    let synchronous: i64 = connection
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .map_err(fail)?;
    // FULL is 2. NORMAL can lose a committed debit on power loss.
    if mode != "wal" || synchronous != 2 {
        return Err(state(
            "the accounting database cannot guarantee durable debits",
        ));
    }
    let page_size: i64 = connection
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .map_err(fail)?;
    if page_size <= 0 {
        return Err(state("the accounting database reports no page size"));
    }
    connection
        .pragma_update(None, "max_page_count", ACCOUNTING_MAX_BYTES / page_size)
        .map_err(fail)?;
    Ok(connection)
}

const ACCOUNTING_DDL: &str = "
CREATE TABLE IF NOT EXISTS allowance_meta (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    schema_version INTEGER NOT NULL,
    ready_generation INTEGER NOT NULL CHECK(ready_generation >= 0),
    last_charge_unix_ms INTEGER NOT NULL CHECK(last_charge_unix_ms >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS allowance_charges (
    origin TEXT NOT NULL,
    window_start_unix_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL CHECK(attempts >= 0),
    PRIMARY KEY (origin, window_start_unix_ms)
) STRICT;
CREATE TABLE IF NOT EXISTS allowance_attempts (
    attempt_id TEXT PRIMARY KEY,
    origin TEXT NOT NULL,
    window_start_unix_ms INTEGER NOT NULL,
    guard_generation INTEGER NOT NULL,
    stage TEXT NOT NULL,
    charged_at_unix_ms INTEGER NOT NULL
) STRICT;
";

/// `(schema_version, ready_generation, last_charge_unix_ms)`, if initialized.
fn read_meta(connection: &Connection) -> Result<Option<(i64, u64, u64)>, AllowanceError> {
    let exists: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'allowance_meta'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| state("the accounting database cannot be read"))?;
    if exists == 0 {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT schema_version, ready_generation, last_charge_unix_ms FROM allowance_meta \
             WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            },
        )
        .optional()
        .map_err(|_| state("the accounting database cannot be read"))
}

fn charged(
    connection: &Connection,
    origin: &str,
    window_start: u64,
) -> Result<u32, AllowanceError> {
    connection
        .query_row(
            "SELECT attempts FROM allowance_charges WHERE origin = ?1 AND window_start_unix_ms = ?2",
            params![origin, window_start as i64],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map(|n| n.unwrap_or(0).clamp(0, i64::from(u32::MAX)) as u32)
        .map_err(|_| state("the accounting database cannot be read"))
}

/// Setup steps, in order, for crash tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupStep {
    IntentPublished,
    AccountingReady,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupPlan {
    pub previous: Option<Guard>,
    pub target: Guard,
    /// Nothing to do: this limit is already ready at a matching generation.
    pub unchanged: bool,
    /// A retried setup continues an interrupted intent's generation.
    pub resumes_intent: bool,
}

fn plan_setup(previous: Option<Guard>, accounting_generation: u64, max_attempts: u32) -> SetupPlan {
    let next = previous
        .map_or(0, |guard| guard.generation)
        .max(accounting_generation)
        + 1;
    let (generation, unchanged, resumes_intent) = match previous {
        Some(guard) if guard.max_attempts == max_attempts => match guard.phase {
            GuardPhase::Ready if accounting_generation == guard.generation => {
                (guard.generation, true, false)
            }
            GuardPhase::Intent => (guard.generation.max(accounting_generation), false, true),
            GuardPhase::Ready => (next, false, false),
        },
        _ => (next, false, false),
    };
    SetupPlan {
        previous,
        target: Guard {
            phase: GuardPhase::Ready,
            generation,
            max_attempts,
        },
        unchanged,
        resumes_intent,
    }
}

/// What `sr budget` reports: guard, accounting health and the current window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inspection {
    pub guard: Option<Guard>,
    /// `ready`, `disabled`, `setup-incomplete`, `generation-mismatch`,
    /// `accounting-missing`, `invalid` or `clock-behind-charges`.
    pub health: &'static str,
    pub detail: Option<String>,
    pub accounting_generation: Option<u64>,
    pub window_start_unix_ms: u64,
    pub window_end_unix_ms: u64,
    pub charged_attempts: Option<u32>,
}

impl Inspection {
    pub fn remaining(&self) -> Option<u32> {
        let guard = self.guard?;
        Some(guard.max_attempts.saturating_sub(self.charged_attempts?))
    }
}

/// Reads guard and accounting without writing anything; no network.
pub fn inspect(paths: &AllowancePaths, origin: &CanonicalOrigin, now_unix_ms: u64) -> Inspection {
    let (window_start_unix_ms, window_end_unix_ms) = window_bounds(now_unix_ms);
    let mut report = Inspection {
        guard: None,
        health: "disabled",
        detail: None,
        accounting_generation: None,
        window_start_unix_ms,
        window_end_unix_ms,
        charged_attempts: None,
    };
    let guard = match read_guard(paths) {
        Ok(guard) => guard,
        Err(error) => {
            report.health = "invalid";
            report.detail = Some(error.to_string());
            return report;
        }
    };
    report.guard = guard;
    let accounting = if paths.accounting.exists() {
        open_accounting(&paths.accounting, false).and_then(|connection| {
            let meta = read_meta(&connection)?;
            let charged = match meta {
                Some(_) => Some(charged(&connection, origin.as_str(), window_start_unix_ms)?),
                None => None,
            };
            Ok((meta, charged))
        })
    } else {
        Ok((None, None))
    };
    let (meta, charged) = match accounting {
        Ok(found) => found,
        Err(error) => {
            report.health = if guard.is_some() {
                "invalid"
            } else {
                "disabled"
            };
            report.detail = Some(error.to_string());
            return report;
        }
    };
    report.accounting_generation = meta.map(|(_, generation, _)| generation);
    report.charged_attempts = charged;
    let Some(guard) = guard else {
        return report;
    };
    report.health = match (guard.phase, meta) {
        (GuardPhase::Intent, _) => "setup-incomplete",
        (GuardPhase::Ready, None) => "accounting-missing",
        (GuardPhase::Ready, Some((_, generation, _))) if generation != guard.generation => {
            "generation-mismatch"
        }
        (GuardPhase::Ready, Some((_, _, last))) if now_unix_ms < last => "clock-behind-charges",
        (GuardPhase::Ready, Some(_)) => "ready",
    };
    report
}

/// Previews `--max-attempts N --window 1h` without writing.
pub fn preview_setup(
    paths: &AllowancePaths,
    max_attempts: u32,
) -> Result<SetupPlan, AllowanceError> {
    let previous = read_guard(paths)?;
    let accounting_generation = if paths.accounting.exists() {
        read_meta(&open_accounting(&paths.accounting, false)?)?.map_or(0, |(_, g, _)| g)
    } else {
        0
    };
    Ok(plan_setup(previous, accounting_generation, max_attempts))
}

/// Applies setup in its three durable steps under the guard lock. `interrupt`
/// stops after a step, as a crash there would, for recovery tests.
pub fn apply_setup(
    paths: &AllowancePaths,
    max_attempts: u32,
    lock_budget: Duration,
    interrupt: Option<SetupStep>,
) -> Result<SetupPlan, AllowanceError> {
    let _lock = GuardLock::acquire(paths, lock_budget)?;
    let previous = read_guard(paths)?;
    let mut connection = open_accounting(&paths.accounting, true)?;
    let accounting_generation = read_meta(&connection)?.map_or(0, |(_, g, _)| g);
    let plan = plan_setup(previous, accounting_generation, max_attempts);
    if plan.unchanged {
        return Ok(plan);
    }
    // 1. The intent: from here, no admission succeeds until step 3.
    publish_guard(
        paths,
        &Guard {
            phase: GuardPhase::Intent,
            ..plan.target
        },
    )?;
    if interrupt == Some(SetupStep::IntentPublished) {
        return Ok(plan);
    }
    // 2. Accounting at the intent's generation, keeping every charge.
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| state("the accounting database is busy"))?;
    tx.execute_batch(ACCOUNTING_DDL)
        .map_err(|_| state("the accounting schema cannot be created"))?;
    tx.execute(
        "INSERT INTO allowance_meta (id, schema_version, ready_generation, last_charge_unix_ms)
         VALUES (1, ?1, ?2, 0)
         ON CONFLICT (id) DO UPDATE SET ready_generation = excluded.ready_generation",
        params![ACCOUNTING_SCHEMA_VERSION, plan.target.generation as i64],
    )
    .map_err(|_| state("the accounting generation cannot be recorded"))?;
    tx.commit()
        .map_err(|_| state("the accounting generation cannot be committed"))?;
    if interrupt == Some(SetupStep::AccountingReady) {
        return Ok(plan);
    }
    // 3. The matching ready generation.
    publish_guard(paths, &plan.target)?;
    Ok(plan)
}

/// One durable debit, made before its request is sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Debit {
    pub attempt_id: String,
    pub guard_generation: u64,
    pub window_start_unix_ms: u64,
    pub charged_attempts: u32,
    pub max_attempts: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Admission {
    /// No allowance is configured.
    Unguarded,
    Debited(Debit),
}

/// Checks the guard and, when it is ready, debits one attempt durably.
///
/// Under the setup lock: re-read the guard, match its generation to the
/// accounting, then charge the current window in one `synchronous=FULL`
/// transaction. The lock is released before the caller sends anything.
/// `persistent` is false when persistence is disabled: then no shared state
/// may be created or used, so a configured guard refuses the attempt.
pub fn admit(
    paths: &AllowancePaths,
    origin: &CanonicalOrigin,
    stage: RankingStage,
    persistent: bool,
    lock_budget: Duration,
) -> Result<Admission, AllowanceError> {
    if !persistent {
        // The final trusted read admits only when no guard is configured.
        return match read_guard(paths)? {
            None => Ok(Admission::Unguarded),
            Some(_) => Err(state(
                "an enforced allowance cannot be checked with persistence disabled",
            )),
        };
    }
    // The unlocked read only skips the lock when no guard exists; any guard
    // is read again under the lock before its generation is trusted.
    if read_guard(paths)?.is_none() {
        return Ok(Admission::Unguarded);
    }
    let _lock = GuardLock::acquire(paths, lock_budget)?;
    let Some(guard) = read_guard(paths)? else {
        return Ok(Admission::Unguarded);
    };
    if guard.phase == GuardPhase::Intent {
        return Err(state(
            "allowance setup is incomplete; rerun sr budget --apply to finish it",
        ));
    }
    let mut connection = open_accounting(&paths.accounting, false)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| state("the accounting database is busy"))?;
    let (_, generation, last_charge) =
        read_meta(&tx)?.ok_or_else(|| state("the accounting database is not initialized"))?;
    if generation != guard.generation {
        return Err(state(
            "the accounting generation does not match the guard; rerun sr budget --apply",
        ));
    }
    let now = wall_clock_ms().ok_or_else(|| state("the wall clock reads before 1970"))?;
    if now < last_charge {
        return Err(state(
            "the clock is behind recorded charges; charges are kept until it passes them",
        ));
    }
    let (window_start, window_end) = window_bounds(now);
    let used = charged(&tx, origin.as_str(), window_start)?;
    if used >= guard.max_attempts {
        return Err(AllowanceError::Exhausted {
            max_attempts: guard.max_attempts,
            window_end_unix_ms: window_end,
        });
    }
    let attempt_id = format!(
        "{:x}-{:x}-{:x}",
        now,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let write = || -> rusqlite::Result<()> {
        tx.execute(
            "INSERT INTO allowance_charges (origin, window_start_unix_ms, attempts)
             VALUES (?1, ?2, 1)
             ON CONFLICT (origin, window_start_unix_ms) DO UPDATE SET attempts = attempts + 1",
            params![origin.as_str(), window_start as i64],
        )?;
        tx.execute(
            "INSERT INTO allowance_attempts
                 (attempt_id, origin, window_start_unix_ms, guard_generation, stage,
                  charged_at_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                attempt_id,
                origin.as_str(),
                window_start as i64,
                guard.generation as i64,
                stage.as_str(),
                now as i64
            ],
        )?;
        tx.execute(
            "UPDATE allowance_meta SET last_charge_unix_ms = max(last_charge_unix_ms, ?1)
             WHERE id = 1",
            params![now as i64],
        )?;
        // Windows long closed no longer bound anything.
        let horizon = window_start.saturating_sub(RETAINED_WINDOWS * WINDOW_MS) as i64;
        tx.execute(
            "DELETE FROM allowance_attempts WHERE window_start_unix_ms < ?1",
            params![horizon],
        )?;
        tx.execute(
            "DELETE FROM allowance_charges WHERE window_start_unix_ms < ?1",
            params![horizon],
        )?;
        Ok(())
    };
    write().map_err(|_| state("the debit cannot be recorded; storage may be full"))?;
    tx.commit()
        .map_err(|_| state("the debit cannot be committed durably"))?;
    Ok(Admission::Debited(Debit {
        attempt_id,
        guard_generation: guard.generation,
        window_start_unix_ms: window_start,
        charged_attempts: used + 1,
        max_attempts: guard.max_attempts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths() -> (PathBuf, AllowancePaths) {
        let root = std::env::temp_dir().join(format!(
            "sr-allowance-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let paths = AllowancePaths::new(&root.join("config"), &root.join("cache"));
        (root, paths)
    }

    fn origin() -> CanonicalOrigin {
        CanonicalOrigin::production()
    }

    const LOCK: Duration = Duration::from_millis(200);

    #[test]
    fn the_accounting_writer_is_wal_with_synchronous_full() {
        let (_root, paths) = temp_paths();
        let connection = open_accounting(&paths.accounting, true).unwrap();
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let synchronous: i64 = connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!((mode.as_str(), synchronous), ("wal", 2));
    }

    #[test]
    fn windows_are_fixed_utc_hours() {
        assert_eq!(window_bounds(3_600_000), (3_600_000, 7_200_000));
        assert_eq!(window_bounds(7_199_999), (3_600_000, 7_200_000));
        assert!(parse_window("1h").is_ok());
        for bad in ["60m", "2h", "", "1H"] {
            assert!(parse_window(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_max_attempts("10000"), Ok(10_000));
        for bad in ["0", "10001", "-1", "x"] {
            assert!(parse_max_attempts(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn no_guard_admits_without_creating_state() {
        let (_root, paths) = temp_paths();
        for persistent in [true, false] {
            assert_eq!(
                admit(&paths, &origin(), RankingStage::Wide, persistent, LOCK),
                Ok(Admission::Unguarded)
            );
        }
        assert!(!paths.accounting.exists() && !paths.guard.exists());
    }

    #[test]
    fn a_ready_guard_debits_until_the_window_is_spent() {
        let (_root, paths) = temp_paths();
        let plan = apply_setup(&paths, 2, LOCK, None).unwrap();
        assert_eq!(plan.target.generation, 1);
        for n in 1..=2 {
            match admit(&paths, &origin(), RankingStage::Wide, true, LOCK).unwrap() {
                Admission::Debited(debit) => assert_eq!(debit.charged_attempts, n),
                other => panic!("{other:?}"),
            }
        }
        assert!(matches!(
            admit(&paths, &origin(), RankingStage::Rerank, true, LOCK),
            Err(AllowanceError::Exhausted {
                max_attempts: 2,
                ..
            })
        ));
        // Another origin has its own count.
        let other = CanonicalOrigin::parse("https://jev.example").unwrap();
        assert!(admit(&paths, &other, RankingStage::Wide, true, LOCK).is_ok());
        // Persistence disabled: a configured guard refuses.
        assert!(matches!(
            admit(&paths, &origin(), RankingStage::Wide, false, LOCK),
            Err(AllowanceError::State(_))
        ));
    }

    #[test]
    fn a_crash_at_each_setup_step_blocks_until_apply_resumes_it() {
        for step in [SetupStep::IntentPublished, SetupStep::AccountingReady] {
            let (_root, paths) = temp_paths();
            apply_setup(&paths, 5, LOCK, None).unwrap();
            assert!(admit(&paths, &origin(), RankingStage::Wide, true, LOCK).is_ok());
            // Raising the limit crashes part-way through.
            let crashed = apply_setup(&paths, 9, LOCK, Some(step)).unwrap();
            assert_eq!(crashed.target.generation, 2);
            assert!(
                matches!(
                    admit(&paths, &origin(), RankingStage::Wide, true, LOCK),
                    Err(AllowanceError::State(_))
                ),
                "{step:?}: an interrupted setup admitted a request"
            );
            let health = inspect(&paths, &origin(), wall_clock_ms().unwrap()).health;
            assert_eq!(health, "setup-incomplete", "{step:?}");
            // Retrying resumes the same generation and keeps the charge.
            let resumed = apply_setup(&paths, 9, LOCK, None).unwrap();
            assert!(resumed.resumes_intent);
            assert_eq!(resumed.target.generation, 2);
            let report = inspect(&paths, &origin(), wall_clock_ms().unwrap());
            assert_eq!(report.health, "ready");
            assert_eq!(
                report.charged_attempts,
                Some(1),
                "a policy edit erased a charge"
            );
            assert_eq!(report.remaining(), Some(8));
        }
    }

    #[test]
    fn a_mismatched_or_missing_accounting_blocks_admission() {
        let (_root, paths) = temp_paths();
        apply_setup(&paths, 3, LOCK, None).unwrap();
        // The guard advanced without its accounting (an external edit).
        publish_guard(
            &paths,
            &Guard {
                phase: GuardPhase::Ready,
                generation: 7,
                max_attempts: 3,
            },
        )
        .unwrap();
        assert!(matches!(
            admit(&paths, &origin(), RankingStage::Wide, true, LOCK),
            Err(AllowanceError::State(_))
        ));
        assert_eq!(
            inspect(&paths, &origin(), wall_clock_ms().unwrap()).health,
            "generation-mismatch"
        );
        let (_root, fresh) = temp_paths();
        publish_guard_for_test(&fresh);
        assert!(matches!(
            admit(&fresh, &origin(), RankingStage::Wide, true, LOCK),
            Err(AllowanceError::State(_))
        ));
    }

    fn publish_guard_for_test(paths: &AllowancePaths) {
        create_private_dir(paths.guard.parent().unwrap()).unwrap();
        publish_guard(
            paths,
            &Guard {
                phase: GuardPhase::Ready,
                generation: 1,
                max_attempts: 3,
            },
        )
        .unwrap();
    }

    #[test]
    fn equivalent_origins_share_one_count() {
        let (_root, paths) = temp_paths();
        apply_setup(&paths, 1, LOCK, None).unwrap();
        let a = CanonicalOrigin::parse("https://JEV.example:443").unwrap();
        let b = CanonicalOrigin::parse("https://jev.example/").unwrap();
        assert_eq!(a, b);
        assert!(admit(&paths, &a, RankingStage::Wide, true, LOCK).is_ok());
        assert!(matches!(
            admit(&paths, &b, RankingStage::Wide, true, LOCK),
            Err(AllowanceError::Exhausted { .. })
        ));
    }

    #[test]
    fn repeating_apply_is_a_no_op_and_changing_the_limit_advances_the_generation() {
        let (_root, paths) = temp_paths();
        let first = apply_setup(&paths, 4, LOCK, None).unwrap();
        let again = apply_setup(&paths, 4, LOCK, None).unwrap();
        assert!(again.unchanged);
        assert_eq!(again.target.generation, first.target.generation);
        let changed = apply_setup(&paths, 6, LOCK, None).unwrap();
        assert_eq!(changed.target.generation, first.target.generation + 1);
        assert_eq!(read_guard(&paths).unwrap(), Some(changed.target));
    }

    #[test]
    fn guard_decoding_is_strict() {
        let good = Guard {
            phase: GuardPhase::Ready,
            generation: 3,
            max_attempts: 50,
        };
        assert_eq!(Guard::decode(&good.encode()), Ok(good));
        for bad in [
            good.encode().replace("\"1h\"", "\"2h\""),
            good.encode().replace("50", "0"),
            good.encode().replace("\"ready\"", "\"armed\""),
            good.encode().replace("generation = 3", "generation = 0"),
            format!("{}color = 1\n", good.encode()),
        ] {
            assert!(Guard::decode(&bad).is_err(), "{bad}");
        }
    }
}
