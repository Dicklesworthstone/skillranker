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
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use std::fmt;
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod accounting;
mod admission_budget;
mod guard_file;
use admission_budget::AdmissionBudget;
use guard_file::GuardLock;

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
            || record.generation > i64::MAX as u64
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
    guard_file::read(&paths.guard)
}

#[cfg(test)]
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
    guard_file::publish(paths, guard)
}

/// Opens the accounting database with the durability enforcement requires:
/// WAL and `synchronous=FULL`, each read back, a page quota and defensive mode.
fn open_accounting(path: &Path, create: bool) -> Result<accounting::Store, AllowanceError> {
    accounting::open_writer(path, create)
}

/// Inspect one read-only snapshot without writer configuration or path creation.
/// SQLite may still maintain WAL shared-memory bookkeeping on a read-only open.
fn read_accounting<T>(
    path: &Path,
    inspect: impl FnOnce(&Connection) -> Result<T, AllowanceError>,
) -> Result<Option<T>, AllowanceError> {
    accounting::read(path, inspect)
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
/// Only a genuinely empty database is uninitialized. A partial schema or a
/// missing singleton may represent lost enforcement history, never a new budget.
fn read_meta(connection: &Connection) -> Result<Option<(i64, u64, u64)>, AllowanceError> {
    let objects: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| state("the accounting database cannot be read"))?;
    // This schema stores its version in allowance_meta; it never assigns either
    // header identifier. A foreign/newer database is not initialization input.
    for pragma in ["application_id", "user_version"] {
        let value: i64 = connection
            .pragma_query_value(None, pragma, |row| row.get(0))
            .map_err(|_| state("the accounting database cannot be inspected"))?;
        if value != 0 {
            return Err(state("the accounting database has an unsupported identity"));
        }
    }
    if objects == 0 {
        return Ok(None);
    }
    if objects != 3 {
        return Err(state("the accounting schema is incomplete or incompatible"));
    }
    // Compare against the very DDL used by setup, not a second schema copy.
    // SQLite strips IF NOT EXISTS and the terminal semicolon when storing DDL.
    for (name, ddl) in ["allowance_meta", "allowance_charges", "allowance_attempts"]
        .into_iter()
        .zip(ACCOUNTING_DDL.split(';'))
    {
        let expected = ddl
            .trim()
            .replacen("CREATE TABLE IF NOT EXISTS", "CREATE TABLE", 1);
        let actual: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| state("the accounting schema cannot be read"))?;
        if actual.as_deref() != Some(expected.as_str()) {
            return Err(state("the accounting schema is incomplete or incompatible"));
        }
    }
    let count: i64 = connection
        .query_row("SELECT count(*) FROM allowance_meta", [], |row| row.get(0))
        .map_err(|_| state("the accounting metadata cannot be read"))?;
    if count != 1 {
        return Err(state(
            "the accounting metadata singleton is missing or duplicated",
        ));
    }
    let (schema, generation, last): (i64, i64, i64) = connection
        .query_row(
            "SELECT schema_version, ready_generation, last_charge_unix_ms FROM allowance_meta \
             WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| state("the accounting metadata cannot be read"))?;
    if schema != ACCOUNTING_SCHEMA_VERSION || generation <= 0 || last < 0 {
        return Err(state(
            "the accounting metadata is outside its supported schema",
        ));
    }
    Ok(Some((schema, generation as u64, last as u64)))
}

fn charged(
    connection: &Connection,
    origin: &str,
    window_start: u64,
) -> Result<u32, AllowanceError> {
    let window_start = i64::try_from(window_start)
        .map_err(|_| state("the accounting window is outside its supported range"))?;
    let attempts: i64 = connection
        .query_row(
            "SELECT attempts FROM allowance_charges WHERE origin = ?1 AND window_start_unix_ms = ?2",
            params![origin, window_start],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| state("the accounting database cannot be read"))?
        .unwrap_or(0);
    // Invalid enforcement state is not free allowance. A real missing row is
    // still a fresh origin/window; stored negative or oversized counts are not.
    u32::try_from(attempts).map_err(|_| state("the stored attempt count is invalid"))
}

/// Both counters are written and pruned in the same debit transaction. Missing
/// or altered totals are not an empty budget while receipt evidence remains.
/// Call only inside the enclosing inspection snapshot or debit transaction so
/// a peer commit cannot make the two reads disagree spuriously.
fn verified_charged(
    connection: &Connection,
    origin: &str,
    window_start: u64,
) -> Result<u32, AllowanceError> {
    if connection.is_autocommit() {
        return Err(state("charge verification requires an accounting snapshot"));
    }
    let attempts = charged(connection, origin, window_start)?;
    let window_start = i64::try_from(window_start)
        .map_err(|_| state("the accounting window is outside its supported range"))?;
    // One excess receipt suffices to reject an undercount. Do not collect IDs,
    // expose receipt contents, or add an index to the existing exact schema.
    // Normal scans remain bounded by the accounting store's existing size cap.
    let receipts: i64 = connection
        .query_row(
            "SELECT count(*) FROM (SELECT 1 FROM allowance_attempts \
             WHERE origin=?1 AND window_start_unix_ms=?2 LIMIT ?3)",
            params![origin, window_start, i64::from(attempts) + 1],
            |row| row.get(0),
        )
        .map_err(|_| state("the accounting attempt receipts cannot be read"))?;
    if receipts != i64::from(attempts) {
        return Err(state(
            "the accounting charge total disagrees with its attempt receipts; restore consistent history",
        ));
    }
    Ok(attempts)
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

fn validate_setup_limit(max_attempts: u32) -> Result<(), AllowanceError> {
    if !(1..=MAX_ATTEMPTS_LIMIT).contains(&max_attempts) {
        return Err(AllowanceError::InvalidRequest(format!(
            "--max-attempts must be an integer from 1 to {MAX_ATTEMPTS_LIMIT}"
        )));
    }
    Ok(())
}

fn plan_setup(
    previous: Option<Guard>,
    accounting_generation: u64,
    max_attempts: u32,
) -> Result<SetupPlan, AllowanceError> {
    validate_setup_limit(max_attempts)?;
    if accounting_generation > i64::MAX as u64
        || previous.is_some_and(|guard| guard.generation == 0 || guard.generation > i64::MAX as u64)
    {
        return Err(state(
            "the allowance generation is outside its supported range",
        ));
    }
    if accounting_generation == 0
        && let Some(guard) = previous
        && !(guard.phase == GuardPhase::Intent
            && guard.generation == 1
            && guard.max_attempts == max_attempts)
    {
        return Err(state(
            "existing allowance accounting is missing; restore its history instead of resetting charges",
        ));
    }
    // Compute a successor only when one is needed. An unchanged ready guard,
    // or the exact interrupted intent, remains usable at the last generation.
    let next = || {
        previous
            .map_or(0, |guard| guard.generation)
            .max(accounting_generation)
            .checked_add(1)
            .filter(|generation| *generation <= i64::MAX as u64)
            .ok_or_else(|| state("the allowance generation cannot be advanced"))
    };
    let (generation, unchanged, resumes_intent) = match previous {
        Some(guard) if guard.max_attempts == max_attempts => match guard.phase {
            GuardPhase::Ready if accounting_generation == guard.generation => {
                (guard.generation, true, false)
            }
            GuardPhase::Intent => (guard.generation.max(accounting_generation), false, true),
            GuardPhase::Ready => (next()?, false, false),
        },
        _ => (next()?, false, false),
    };
    Ok(SetupPlan {
        previous,
        target: Guard {
            phase: GuardPhase::Ready,
            generation,
            max_attempts,
        },
        unchanged,
        resumes_intent,
    })
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
    let accounting = read_accounting(&paths.accounting, |connection| {
        let meta = read_meta(connection)?;
        let charged = match meta {
            Some(_) => Some(verified_charged(
                connection,
                origin.as_str(),
                window_start_unix_ms,
            )?),
            None => None,
        };
        Ok((meta, charged))
    })
    .map(|found| found.unwrap_or((None, None)));
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
    validate_setup_limit(max_attempts)?;
    let previous = read_guard(paths)?;
    let accounting_generation = read_accounting(&paths.accounting, read_meta)?
        .flatten()
        .map_or(0, |(_, generation, _)| generation);
    plan_setup(previous, accounting_generation, max_attempts)
}

/// Applies setup in its three durable steps under the guard lock. `interrupt`
/// stops after a step, as a crash there would, for recovery tests.
pub fn apply_setup(
    paths: &AllowancePaths,
    max_attempts: u32,
    lock_budget: Duration,
    interrupt: Option<SetupStep>,
) -> Result<SetupPlan, AllowanceError> {
    validate_setup_limit(max_attempts)?;
    let _lock = GuardLock::acquire(paths, lock_budget)?;
    let previous = read_guard(paths)?;
    // A ready guard proves accounting has existed. Do not turn a missing file
    // into fresh allowance. Only first activation (including its exact initial
    // intent retry, before any debit was possible) can create accounting.
    let create = previous.is_none_or(|guard| {
        guard.phase == GuardPhase::Intent
            && guard.generation == 1
            && guard.max_attempts == max_attempts
    });
    let mut accounting = open_accounting(&paths.accounting, create)?;
    let accounting_generation = read_meta(&accounting.connection)?.map_or(0, |(_, g, _)| g);
    let plan = plan_setup(previous, accounting_generation, max_attempts)?;
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
    accounting.files.verify()?;
    let tx = accounting
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| state("the accounting database is busy"))?;
    // Recheck inside the writer transaction before any IF NOT EXISTS could
    // reconstruct missing history after the preflight read.
    if read_meta(&tx)?.map_or(0, |(_, generation, _)| generation) != accounting_generation {
        return Err(state("the accounting generation changed during setup"));
    }
    accounting.files.verify()?;
    tx.execute_batch(ACCOUNTING_DDL)
        .map_err(|_| state("the accounting schema cannot be created"))?;
    tx.execute(
        "INSERT INTO allowance_meta (id, schema_version, ready_generation, last_charge_unix_ms)
         VALUES (1, ?1, ?2, 0)
         ON CONFLICT (id) DO UPDATE SET ready_generation = excluded.ready_generation",
        params![ACCOUNTING_SCHEMA_VERSION, plan.target.generation as i64],
    )
    .map_err(|_| state("the accounting generation cannot be recorded"))?;
    accounting.files.verify()?;
    tx.commit()
        .map_err(|_| state("the accounting generation cannot be committed"))?;
    accounting.files.verify()?;
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
///
/// For an enabled guard, `lock_budget` bounds the whole admission, not a fresh
/// allowance for each lock or connection. Guard reads, path admission, SQLite
/// scans, lock waits and commit verification share one monotonic start time.
/// Expiry before commit rolls back; expiry after a possible commit withholds
/// admission without refund. Unconfigured/stateless behavior is unchanged.
/// These are cooperative checks, not interruption of a kernel filesystem wait.
pub fn admit(
    paths: &AllowancePaths,
    origin: &CanonicalOrigin,
    stage: RankingStage,
    persistent: bool,
    lock_budget: Duration,
) -> Result<Admission, AllowanceError> {
    let budget = AdmissionBudget::start(lock_budget);
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
    let _lock = GuardLock::acquire(paths, budget.remaining()?)?;
    budget.check()?;
    let guard = read_guard(paths)?;
    budget.check()?;
    let Some(guard) = guard else {
        drop(_lock);
        budget.check()?;
        return Ok(Admission::Unguarded);
    };
    if guard.phase == GuardPhase::Intent {
        return Err(state(
            "allowance setup is incomplete; rerun sr budget --apply to finish it",
        ));
    }
    let mut accounting =
        accounting::open_writer_with_budget(&paths.accounting, false, Some(budget))?;
    accounting.files.verify()?;
    budget.refresh_busy_wait(&accounting.connection)?;
    let tx = accounting
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| state("the accounting database is busy"))?;
    let (_, generation, last_charge) =
        read_meta(&tx)?.ok_or_else(|| state("the accounting database is not initialized"))?;
    if generation != guard.generation {
        return Err(state(
            "the accounting generation does not match the guard; rerun sr budget --apply",
        ));
    }
    budget.check()?;
    let now = wall_clock_ms().ok_or_else(|| state("the wall clock reads before 1970"))?;
    if now < last_charge {
        return Err(state(
            "the clock is behind recorded charges; charges are kept until it passes them",
        ));
    }
    let (window_start, window_end) = window_bounds(now);
    let used = verified_charged(&tx, origin.as_str(), window_start)?;
    budget.check()?;
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
    accounting.files.verify()?;
    write().map_err(|_| state("the debit cannot be recorded; storage may be full"))?;
    accounting.files.verify()?;
    budget.refresh_busy_wait(&tx)?;
    tx.commit()
        .map_err(|_| state("the debit cannot be committed durably"))?;
    // If post-commit identity verification fails, withhold the send. A charge
    // that may already be durable is never refunded.
    accounting.files.verify()?;
    // Closing the final SQLite connection can checkpoint the WAL. Include
    // connection cleanup and guard unlock before the final admission check,
    // not in implicit destructors after constructing a successful result.
    drop(accounting);
    drop(_lock);
    // A completed debit is never refunded, even when it returned too late.
    budget.check()?;
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
        let root = std::fs::canonicalize("/tmp")
            .expect("resolve trusted system temporary directory")
            .join(format!(
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

#[cfg(test)]
#[path = "allowance/integrity_tests.rs"]
mod integrity_tests;

#[cfg(all(test, unix))]
#[path = "allowance/inspection_tests.rs"]
mod inspection_tests;

#[cfg(test)]
#[path = "allowance/charge_consistency_tests.rs"]
mod charge_consistency_tests;
