//! Qualified local cache storage. Ledger and allowance stores are separate.
//!
//! Disk work runs on the invocation's bounded blocking pool. SQLite progress
//! cancellation and busy limits bound cooperative work; they cannot interrupt
//! an uninterruptible kernel filesystem wait. Late results are not published.

pub mod export;
mod filesystem;
pub mod ledger;
mod platform;
pub(crate) use platform::storage_path;

pub use export::{
    DEFAULT_MAX_CASE_BYTES, DEFAULT_MAX_SNAPSHOT_BYTES, ExportConfig, ExportError,
    export_private_atomic,
};
pub use ledger::{
    CandidateStage, CleanupDebt, ClearPreview, ClearReport, CursorKind, DEFAULT_RETENTION_MS,
    DecisionKind, EvidenceState, ExposureState, FeedbackError, FeedbackOutcome, FeedbackRequest,
    ChannelStats, InitReport, InitStatus, JudgmentLabel, JudgmentMetrics, LatencySummary,
    LEDGER_APPLICATION_ID, LEDGER_FILE, LEDGER_MAINTENANCE_RESERVE_BYTES,
    LEDGER_MUTATION_RESERVE_BYTES, LEDGER_QUOTA_BYTES, LEDGER_SCHEMA_ID, LEDGER_SCHEMA_VERSION,
    LEDGER_TARGET_SCHEMA_VERSION, LedgerAccess, LedgerCapacityReport, LedgerLocation, LedgerOpen,
    LedgerStamp, LedgerStatusReport, LedgerStore, MaintenanceError, MaintenanceKind,
    MaintenancePreflight, MembershipCoverage, MigrationError, MigrationPreview, MigrationReport,
    MigrationStep, NewCalibration, NewFeedbackProposal, NewJudgment, NewObservation,
    NewProviderAttempt, NewRankingCandidate, NewRankingEvent, NewRosterSnapshot,
    ObservationMetrics, PairedCorrectionRequest, PendingMigration, ProposalStatus, ProviderMetrics,
    PrunePreview, PruneReport, RetainedStats, SessionCursor, SingleFeedbackRequest,
    SkillStatSummary, SnapshotMember, StatsValueReport, TurnMetrics,
    find_latest_preceding_emission, format_unix_ms, get_session_cursor, get_session_observations,
    init_ledger, ledger_stats, ledger_status, open_ledger, parse_cutoff_to_unix_ms, record_acknowledgment,
    record_emission, record_observations_with_cursor, record_ranking, record_ranking_with_attempts,
    submit_feedback,
};

use crate::blocking::{BlockingLeafKind, remaining_busy_wait, run_blocking_leaf};
use crate::cache::{CacheKey, CachedResponseEntry, RequestFingerprint, RequestStage};
use crate::jev::codec::{MAX_RESPONSE_BYTES, Usage};
use crate::runtime::{EntryClock, ProcessInvocation, RuntimeError};
use crate::sqlite_engine::EngineQualificationError;
pub use crate::sqlite_engine::{
    EngineIdentity, QUALIFIED_SQLITE_SOURCE_ID, QUALIFIED_SQLITE_VERSION, RUSQLITE_VERSION,
};
use asupersync::Cx;
use filesystem::PrivateDirectory;
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, config::DbConfig,
    limits::Limit, params,
};
use std::{fmt, fs::File, path::PathBuf, time::Duration};

pub const CACHE_FILE: &str = "cache.sqlite3";
pub const CACHE_SCHEMA_VERSION: u32 = 3;
pub const CACHE_QUOTA_BYTES: u64 = 64 * 1024 * 1024;
pub const MAINTENANCE_RESERVE_BYTES: u64 = 4 * 1024 * 1024;
// Metadata and key writes are single rows. A response row is bounded by the
// decoded-response cap; SQLite's page quota, not this margin, stops recording.
pub const MUTATION_RESERVE_BYTES: u64 = 1024 * 1024;
pub const CACHE_RECORDING_CEILING_BYTES: u64 =
    CACHE_QUOTA_BYTES - MAINTENANCE_RESERVE_BYTES - MUTATION_RESERVE_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheCapacityReport {
    pub total_quota_bytes: u64,
    pub maintenance_reserve_bytes: u64,
    pub mutation_reserve_bytes: u64,
    pub recording_ceiling_bytes: u64,
    pub occupied_bytes: u64,
    pub usable_recording_bytes: u64,
    pub available_disk_bytes: u64,
    pub is_recording_admitted: bool,
}
pub const MAX_BUSY_WAIT_MS: u64 = 25;
const APPLICATION_ID: i64 = 0x53524348; // SRCH: cache, never ledger/accounting.
const SCHEMA_ID: &str = "sr-cache-responses-v3";
/// Maximum age of a stored response; the ten-minute TTL is a ceiling.
pub const MAX_RESPONSE_TTL_SECONDS: u32 = 600;
const METADATA_DDL: &str = "CREATE TABLE sr_cache_meta (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        incarnation BLOB NOT NULL CHECK(length(incarnation)=16),
        generation INTEGER NOT NULL CHECK(generation>=0),
        schema_id TEXT NOT NULL
    ) STRICT";
/// The owner-only random key for keyed request and namespace fingerprints.
const KEY_DDL: &str = "CREATE TABLE sr_cache_key (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        key BLOB NOT NULL CHECK(length(key)=32)
    ) STRICT";
/// Validated provider responses, bound to the store generation that wrote them.
const RESPONSE_DDL: &str = "CREATE TABLE sr_cache_response (
        generation INTEGER NOT NULL CHECK(generation>=0),
        namespace BLOB NOT NULL CHECK(length(namespace)=32),
        stage TEXT NOT NULL CHECK(stage IN ('wide','rerank')),
        fingerprint BLOB NOT NULL CHECK(length(fingerprint)=32),
        response BLOB NOT NULL CHECK(length(response)<=2097152),
        received_at_unix_ms INTEGER NOT NULL CHECK(received_at_unix_ms>=0),
        ttl_seconds INTEGER NOT NULL CHECK(ttl_seconds BETWEEN 1 AND 600),
        model TEXT NOT NULL CHECK(length(model) BETWEEN 1 AND 256),
        model_revision TEXT CHECK(model_revision IS NULL OR length(model_revision)<=256),
        input_tokens INTEGER NOT NULL CHECK(input_tokens>=0),
        output_tokens INTEGER NOT NULL CHECK(output_tokens>=0),
        PRIMARY KEY (generation, namespace, stage, fingerprint)
    ) STRICT";
const LEASE_DDL: &str = "CREATE TABLE sr_coordination_leases (
        coordination_key BLOB PRIMARY KEY CHECK(length(coordination_key)=32),
        owner_token BLOB NOT NULL CHECK(length(owner_token)=16),
        fencing_generation INTEGER NOT NULL CHECK(fencing_generation>=1),
        acquired_at_unix_ms INTEGER NOT NULL CHECK(acquired_at_unix_ms>=0),
        expires_at_unix_ms INTEGER NOT NULL CHECK(expires_at_unix_ms>=acquired_at_unix_ms),
        attempt_id TEXT NOT NULL CHECK(length(attempt_id)<=128),
        is_completed INTEGER NOT NULL CHECK(is_completed IN (0,1))
    ) STRICT";
const TABLES: [(&str, &str); 4] = [
    ("sr_cache_meta", METADATA_DDL),
    ("sr_cache_key", KEY_DDL),
    ("sr_cache_response", RESPONSE_DDL),
    ("sr_coordination_leases", LEASE_DDL),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheAccess {
    Disabled,
    ExistingOnly,
    Initialize,
}

/// Supplied only by trusted host configuration; never by transcript/model text.
pub enum CacheLocation {
    Platform,
    Directory(PathBuf),
}
impl fmt::Debug for CacheLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CacheLocation(<private>)")
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct CacheStamp {
    incarnation: [u8; 16],
    generation: u64,
}
impl CacheStamp {
    pub const fn generation(self) -> u64 {
        self.generation
    }
}
impl fmt::Debug for CacheStamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CacheStamp")
            .field("schema", &CACHE_SCHEMA_VERSION)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreError {
    Missing,
    Uninitialized,
    UnsafePath,
    Permissions,
    UnsupportedFilesystem,
    Io,
    Busy,
    Corrupt,
    InvalidRecord,
    RecordConflict,
    WrongStore,
    IncompatibleSchema,
    NewerSchema { version: i64 },
    UnqualifiedEngine,
    StoreReplaced,
    StaleGeneration,
    LeaseSuperseded,
    LeaseUnavailable,
    GenerationExhausted,
    Quota,
    InsufficientSpace,
    Cancelled,
    Runtime(RuntimeError),
}
impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "cache store is missing",
            Self::Uninitialized => "cache store is not initialized",
            Self::UnsafePath => "cache path is not a safe regular-file location",
            Self::Permissions => "cache path does not satisfy owner-only permissions",
            Self::UnsupportedFilesystem => "cache requires a qualified local filesystem",
            Self::Io => "cache filesystem operation failed",
            Self::Busy => "cache lock wait exhausted its bounded allowance",
            Self::Corrupt => "cache database could not be read safely",
            Self::InvalidRecord => "storage record does not satisfy its bounded metadata contract",
            Self::RecordConflict => "stored identity is already bound to different metadata",
            Self::WrongStore => "database is not a SkillRanker cache",
            Self::IncompatibleSchema => "cache schema is incompatible",
            Self::NewerSchema { .. } => {
                "cache schema is newer than this build; no repair performed"
            }
            Self::UnqualifiedEngine => {
                "linked SQLite engine is not the qualified version and source"
            }
            Self::StoreReplaced => "cache directory, file or incarnation changed",
            Self::StaleGeneration => "cache generation changed before mutation",
            Self::LeaseSuperseded => "response publisher no longer owns its lease",
            Self::LeaseUnavailable => "response publication could not lock its lease",
            Self::GenerationExhausted => "cache generation cannot be advanced",
            Self::Quota => "cache recording capacity is exhausted; maintenance reserve retained",
            Self::InsufficientSpace => {
                "cache requires 5 MiB free for mutation and maintenance headroom"
            }
            Self::Cancelled => "cache operation cancelled",
            Self::Runtime(_) => "cache operation exceeded runtime admission or publication bounds",
        })
    }
}
impl std::error::Error for StoreError {}
impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        match error.sqlite_error_code() {
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => Self::Busy,
            Some(ErrorCode::OperationInterrupted) => Self::Cancelled,
            Some(ErrorCode::DiskFull) => Self::InsufficientSpace,
            Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly) => Self::Permissions,
            Some(
                ErrorCode::SystemIoFailure
                | ErrorCode::CannotOpen
                | ErrorCode::FileLockingProtocolFailed,
            ) => Self::Io,
            Some(ErrorCode::SchemaChanged) => Self::IncompatibleSchema,
            _ => Self::Corrupt,
        }
    }
}
impl From<EngineQualificationError> for StoreError {
    fn from(error: EngineQualificationError) -> Self {
        match error {
            EngineQualificationError::Unqualified => Self::UnqualifiedEngine,
            EngineQualificationError::Sqlite(error) => Self::from(error),
        }
    }
}

pub enum CacheOpen {
    Disabled,
    Missing,
    Ready(Box<CacheStore>),
}
impl fmt::Debug for CacheOpen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("CacheOpen::Disabled"),
            Self::Missing => f.write_str("CacheOpen::Missing"),
            Self::Ready(store) => f.debug_tuple("CacheOpen::Ready").field(store).finish(),
        }
    }
}

pub struct CacheStore {
    // Connection drops before the descriptors that bind its admitted location.
    connection: Connection,
    directory: PrivateDirectory,
    file: File,
    engine: EngineIdentity,
    stamp: CacheStamp,
    key: [u8; 32],
}
impl fmt::Debug for CacheStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CacheStore")
            .field("engine", &self.engine)
            .field("stamp", &self.stamp)
            .finish_non_exhaustive()
    }
}

pub(super) fn check_work(clock: EntryClock, cx: &Cx) -> Result<(), StoreError> {
    cx.checkpoint().map_err(|_| StoreError::Cancelled)?;
    clock.admit_new_work().map_err(StoreError::Runtime)?;
    Ok(())
}

/// Compatibility adapter: engine inspection is disk-free and platform-neutral.
pub fn linked_engine() -> Result<EngineIdentity, StoreError> {
    crate::sqlite_engine::linked_engine().map_err(StoreError::from)
}

/// Disabled persistence is checked before resolving platform paths or engine I/O.
pub fn open_cache(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: CacheAccess,
    location: CacheLocation,
) -> Result<CacheOpen, StoreError> {
    if access == CacheAccess::Disabled {
        return Ok(CacheOpen::Disabled);
    }
    let clock = invocation.clock();
    let child = cx.clone();
    run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || open_blocking(clock, &child, access, location),
    )
    .map_err(StoreError::Runtime)?
    .value
}

fn refresh_busy_limit(
    connection: &Connection,
    clock: EntryClock,
    cx: &Cx,
) -> Result<(), StoreError> {
    check_work(clock, cx)?;
    connection.busy_timeout(
        remaining_busy_wait(&clock, Duration::from_millis(MAX_BUSY_WAIT_MS))
            .map_err(StoreError::Runtime)?,
    )?;
    Ok(())
}

fn configure(connection: &Connection, clock: EntryClock, cx: &Cx) -> Result<(), StoreError> {
    refresh_busy_limit(connection, clock, cx)?;
    let child = cx.clone();
    connection.progress_handler(
        100,
        Some(move || child.is_cancel_requested() || clock.admit_new_work().is_err()),
    )?;
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, 2 * 1024 * 1024)?;
    connection.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024)?;
    connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    connection.set_limit(Limit::SQLITE_LIMIT_WORKER_THREADS, 0)?;
    for (setting, expected) in [
        (DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true),
        (DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false),
        (DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false),
        (DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false),
        (DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY, true),
        (DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true),
    ] {
        if connection.set_db_config(setting, expected)? != expected {
            return Err(StoreError::IncompatibleSchema);
        }
    }
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

fn schema_version(connection: &Connection) -> Result<i64, StoreError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > i64::from(CACHE_SCHEMA_VERSION) {
        return Err(StoreError::NewerSchema { version });
    }
    Ok(version)
}

fn sql_integer(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Quota)
}

/// Refuse to serve or record under a replaced store or a changed generation.
fn check_stamp(connection: &Connection, expected: CacheStamp) -> Result<(), StoreError> {
    let actual = read_stamp(connection)?;
    if actual.incarnation != expected.incarnation {
        return Err(StoreError::StoreReplaced);
    }
    if actual.generation != expected.generation {
        return Err(StoreError::StaleGeneration);
    }
    Ok(())
}

fn read_stamp(connection: &Connection) -> Result<CacheStamp, StoreError> {
    if schema_version(connection)? != i64::from(CACHE_SCHEMA_VERSION) {
        return Err(StoreError::IncompatibleSchema);
    }
    let app: i64 = connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if app != APPLICATION_ID {
        return Err(StoreError::WrongStore);
    }
    let objects: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
        [],
        |row| row.get(0),
    )?;
    if objects != TABLES.len() as i64 {
        return Err(StoreError::IncompatibleSchema);
    }
    for (name, expected) in TABLES {
        let ddl: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        if ddl.as_deref() != Some(expected) {
            return Err(StoreError::IncompatibleSchema);
        }
    }
    for singleton in ["sr_cache_meta", "sr_cache_key"] {
        let rows: i64 =
            connection.query_row(&format!("SELECT count(*) FROM {singleton}"), [], |row| {
                row.get(0)
            })?;
        if rows != 1 {
            return Err(StoreError::IncompatibleSchema);
        }
    }
    let (incarnation, generation, schema): (Vec<u8>, i64, String) = connection.query_row(
        "SELECT incarnation, generation, schema_id FROM sr_cache_meta WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if generation < 0 || schema != SCHEMA_ID {
        return Err(StoreError::IncompatibleSchema);
    }
    let incarnation = incarnation
        .try_into()
        .map_err(|_| StoreError::IncompatibleSchema)?;
    Ok(CacheStamp {
        incarnation,
        generation: generation as u64,
    })
}

fn read_key(connection: &Connection) -> Result<[u8; 32], StoreError> {
    let key: Vec<u8> = connection.query_row(
        "SELECT key FROM sr_cache_key WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    key.try_into().map_err(|_| StoreError::IncompatibleSchema)
}

fn initialize(connection: &mut Connection, clock: EntryClock, cx: &Cx) -> Result<(), StoreError> {
    configure(connection, clock, cx)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_work(clock, cx)?;
    if schema_version(&tx)? == i64::from(CACHE_SCHEMA_VERSION) {
        read_stamp(&tx)?;
        return Ok(());
    }
    let app: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let objects: i64 = tx.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
        [],
        |row| row.get(0),
    )?;
    if app != 0 || objects != 0 {
        return Err(StoreError::WrongStore);
    }
    for (_, ddl) in TABLES {
        tx.execute_batch(ddl)?;
    }
    tx.execute(
        "INSERT INTO sr_cache_meta VALUES (1, randomblob(16), 0, ?1)",
        [SCHEMA_ID],
    )?;
    // The fingerprint key comes from the operating system CSPRNG and never
    // leaves this owner-only store except as keyed-hash input.
    let key = CacheKey::generate().map_err(|_| StoreError::Io)?;
    tx.execute(
        "INSERT INTO sr_cache_key VALUES (1, ?1)",
        [&key.as_raw_bytes()[..]],
    )?;
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    tx.pragma_update(None, "user_version", CACHE_SCHEMA_VERSION)?;
    refresh_busy_limit(&tx, clock, cx)?;
    tx.commit()?;
    Ok(())
}

fn open_blocking(
    clock: EntryClock,
    cx: &Cx,
    access: CacheAccess,
    location: CacheLocation,
) -> Result<CacheOpen, StoreError> {
    check_work(clock, cx)?;
    let engine = linked_engine()?;
    check_work(clock, cx)?;
    let path = match location {
        CacheLocation::Platform => directories::BaseDirs::new()
            .ok_or(StoreError::UnsafePath)?
            .cache_dir()
            .join("sr"),
        CacheLocation::Directory(path) => path,
    };
    let create = access == CacheAccess::Initialize;
    let directory = match PrivateDirectory::open(path, create, clock, cx) {
        Err(StoreError::Missing) if !create => return Ok(CacheOpen::Missing),
        result => result?,
    };
    let file = match directory.open_database_file(create, clock, cx) {
        Err(StoreError::Missing) if !create => return Ok(CacheOpen::Missing),
        result => result?,
    };
    check_work(clock, cx)?;
    let flags = OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
    // Inspect an existing/newer schema through a read-only SQLite connection.
    // WAL shared-memory bookkeeping may occur, but schema/data is never repaired.
    let probe = Connection::open_with_flags(
        directory.database_path(),
        flags | OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    configure(&probe, clock, cx)?;
    let version = schema_version(&probe)?;
    if version == 0 && !create {
        return Err(StoreError::Uninitialized);
    }
    if version != 0 {
        read_stamp(&probe)?;
    }
    drop(probe);
    check_work(clock, cx)?;
    directory.verify_database_file(&file, clock, cx)?;
    let mut connection = Connection::open_with_flags(
        directory.database_path(),
        flags | OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    configure(&connection, clock, cx)?;
    // Recheck after reopen; another initializer may have won between connections.
    let version = schema_version(&connection)?;
    if version == 0 {
        if !create {
            return Err(StoreError::Uninitialized);
        }
        initialize(&mut connection, clock, cx)?;
    }
    let stamp = read_stamp(&connection)?;
    let key = read_key(&connection)?;
    check_work(clock, cx)?;
    let mode: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if mode != "wal" {
        refresh_busy_limit(&connection, clock, cx)?;
        let selected: String =
            connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if selected != "wal" {
            return Err(StoreError::IncompatibleSchema);
        }
    }
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "wal_autocheckpoint", 64)?;
    connection.pragma_update(None, "journal_size_limit", 1024 * 1024)?;
    let page_size: i64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    if !(512..=65536).contains(&page_size) {
        return Err(StoreError::IncompatibleSchema);
    }
    let max_pages = i64::try_from(
        (CACHE_QUOTA_BYTES - MAINTENANCE_RESERVE_BYTES - MUTATION_RESERVE_BYTES)
            / u64::try_from(page_size).map_err(|_| StoreError::IncompatibleSchema)?,
    )
    .map_err(|_| StoreError::Quota)?;
    connection.pragma_update(None, "max_page_count", max_pages)?;
    let applied: i64 = connection.pragma_query_value(None, "max_page_count", |row| row.get(0))?;
    if applied != max_pages {
        return Err(StoreError::Quota);
    }
    let foreign_keys: i64 =
        connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(StoreError::IncompatibleSchema);
    }
    directory.verify_database_file(&file, clock, cx)?;
    directory.admit_space()?;
    check_work(clock, cx)?;
    Ok(CacheOpen::Ready(Box::new(CacheStore {
        connection,
        directory,
        file,
        engine,
        stamp,
        key,
    })))
}

impl CacheStore {
    pub fn engine(&self) -> &EngineIdentity {
        &self.engine
    }
    pub const fn stamp(&self) -> CacheStamp {
        self.stamp
    }

    /// The store's fingerprint key. Together with the stamp generation it
    /// scopes every keyed request and namespace fingerprint to this store.
    pub const fn fingerprint_key(&self) -> CacheKey {
        CacheKey::from_bytes(self.key)
    }

    /// Exposes usable recording capacity separately from the total quota cap.
    pub fn capacity_report(&self) -> Result<CacheCapacityReport, StoreError> {
        self.directory.capacity_report()
    }

    /// Checkpoints and truncates the WAL file to bound WAL growth and reclaim space.
    pub fn checkpoint_truncate(&mut self, clock: EntryClock, cx: &Cx) -> Result<(), StoreError> {
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        refresh_busy_limit(&self.connection, clock, cx)?;
        self.connection
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        Ok(())
    }

    /// Read one stored response of this generation by exact namespace hash,
    /// stage and request fingerprint. Freshness is the caller's decision;
    /// nothing is renewed, repaired or pruned on read. A changed generation or
    /// incarnation is refused rather than served.
    pub fn response(
        mut self,
        invocation: &ProcessInvocation,
        cx: &Cx,
        namespace: [u8; 32],
        stage: RequestStage,
        fingerprint: RequestFingerprint,
    ) -> Result<(Self, Option<CachedResponseEntry>), StoreError> {
        let clock = invocation.clock();
        let child = cx.clone();
        run_blocking_leaf(
            invocation,
            cx,
            BlockingLeafKind::Database,
            false,
            move || {
                configure(&self.connection, clock, &child)?;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                refresh_busy_limit(&self.connection, clock, &child)?;
                let expected = self.stamp;
                let generation = sql_integer(expected.generation)?;
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Deferred)?;
                check_stamp(&tx, expected)?;
                let row = tx
                    .query_row(
                        "SELECT response, received_at_unix_ms, ttl_seconds, model, \
                         model_revision, input_tokens, output_tokens FROM sr_cache_response \
                         WHERE generation=?1 AND namespace=?2 AND stage=?3 AND fingerprint=?4",
                        params![
                            generation,
                            &namespace[..],
                            stage.as_str(),
                            &fingerprint.as_bytes()[..]
                        ],
                        |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, Option<String>>(4)?,
                                row.get::<_, i64>(5)?,
                                row.get::<_, i64>(6)?,
                            ))
                        },
                    )
                    .optional()?;
                tx.finish()?;
                check_work(clock, &child)?;
                let entry = row
                    .map(
                        |(response, received, ttl, model, revision, input, output)| {
                            let unsigned =
                                |v: i64| u64::try_from(v).map_err(|_| StoreError::Corrupt);
                            Ok::<_, StoreError>(CachedResponseEntry {
                                stage,
                                request_fingerprint: fingerprint,
                                response_bytes: response,
                                received_at_unix_ms: unsigned(received)?,
                                ttl_seconds: u32::try_from(ttl)
                                    .ok()
                                    .filter(|t| (1..=MAX_RESPONSE_TTL_SECONDS).contains(t))
                                    .ok_or(StoreError::Corrupt)?,
                                model,
                                model_revision: revision,
                                original_usage: Usage {
                                    input_tokens: unsigned(input)?,
                                    output_tokens: unsigned(output)?,
                                },
                                attempt_id: None,
                            })
                        },
                    )
                    .transpose()?;
                Ok((self, entry))
            },
        )
        .map_err(StoreError::Runtime)?
        .value
    }

    /// Record one validated provider response for this generation, fenced by
    /// the store incarnation and generation inside `BEGIN IMMEDIATE`. Rows of
    /// other generations and rows past their TTL (or dated in the future) are
    /// pruned first; they could never be served. At quota, recording stops
    /// with a typed error and the maintenance reserve is kept.
    pub fn record_response(
        self,
        invocation: &ProcessInvocation,
        cx: &Cx,
        namespace: [u8; 32],
        entry: CachedResponseEntry,
        now_unix_ms: u64,
    ) -> Result<Self, StoreError> {
        self.record_response_inner(invocation, cx, namespace, entry, now_unix_ms, None)
    }

    /// Validate ownership and write the response inside one cache transaction.
    /// The supplied path must identify this cache, never a separate lease store.
    pub fn record_response_fenced(
        self,
        invocation: &ProcessInvocation,
        cx: &Cx,
        namespace: [u8; 32],
        entry: CachedResponseEntry,
        fence: (PathBuf, crate::cache::LeaderContext),
    ) -> Result<Self, StoreError> {
        let now = cache_wall_clock_ms();
        self.record_response_inner(invocation, cx, namespace, entry, now, Some(fence))
    }

    #[allow(clippy::too_many_arguments)]
    fn record_response_inner(
        mut self,
        invocation: &ProcessInvocation,
        cx: &Cx,
        namespace: [u8; 32],
        entry: CachedResponseEntry,
        now_unix_ms: u64,
        fence: Option<(PathBuf, crate::cache::LeaderContext)>,
    ) -> Result<Self, StoreError> {
        if entry.response_bytes.len() > MAX_RESPONSE_BYTES
            || !(1..=MAX_RESPONSE_TTL_SECONDS).contains(&entry.ttl_seconds)
            || entry.model.is_empty()
            || entry.model.len() > 256
            || entry.model_revision.as_ref().is_some_and(|r| r.len() > 256)
        {
            return Err(StoreError::Quota);
        }
        let clock = invocation.clock();
        let child = cx.clone();
        run_blocking_leaf(
            invocation,
            cx,
            BlockingLeafKind::Database,
            false,
            move || {
                let expires = fence
                    .as_ref()
                    .map(|(_, leader)| leader.lease_expires_at_unix_ms);
                if fence.as_ref().is_some_and(|(path, _)| {
                    storage_path(path.clone()) != self.directory.database_path()
                }) {
                    return Err(StoreError::LeaseUnavailable);
                }
                configure(&self.connection, clock, &child)?;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                refresh_busy_limit(&self.connection, clock, &child)?;
                let expected = self.stamp;
                let generation = sql_integer(expected.generation)?;
                let now = sql_integer(now_unix_ms)?;
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                check_stamp(&tx, expected)?;
                if let Some((_, leader)) = &fence
                    && !crate::cache::SqliteLeaseCoordinator::active_lease_in_transaction(
                        &tx,
                        leader,
                        cache_wall_clock_ms(),
                    )
                    .map_err(coordination_error)?
                {
                    return Err(StoreError::LeaseSuperseded);
                }
                self.directory.admit_space()?;
                tx.execute(
                    "DELETE FROM sr_cache_response WHERE generation<>?1 \
                 OR received_at_unix_ms>?2 OR received_at_unix_ms+ttl_seconds*1000<=?2",
                    params![generation, now],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO sr_cache_response VALUES \
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        generation,
                        &namespace[..],
                        entry.stage.as_str(),
                        &entry.request_fingerprint.as_bytes()[..],
                        entry.response_bytes,
                        sql_integer(entry.received_at_unix_ms)?,
                        entry.ttl_seconds,
                        entry.model,
                        entry.model_revision,
                        sql_integer(entry.original_usage.input_tokens)?,
                        sql_integer(entry.original_usage.output_tokens)?,
                    ],
                )?;
                refresh_busy_limit(&tx, clock, &child)?;
                if expires.is_some_and(|expires| cache_wall_clock_ms() >= expires) {
                    return Err(StoreError::LeaseSuperseded);
                }
                tx.commit()?;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                check_work(clock, &child)?;
                Ok(self)
            },
        )
        .map_err(StoreError::Runtime)?
        .value
    }

    fn lease_transaction<T: Send + 'static>(
        mut self,
        invocation: &ProcessInvocation,
        cx: &Cx,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<(Self, T), StoreError> {
        let clock = invocation.clock();
        let child = cx.clone();
        run_blocking_leaf(
            invocation,
            cx,
            BlockingLeafKind::Database,
            false,
            move || {
                configure(&self.connection, clock, &child)?;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                refresh_busy_limit(&self.connection, clock, &child)?;
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                check_stamp(&tx, self.stamp)?;
                self.directory.admit_space()?;
                let result = operation(&tx)?;
                refresh_busy_limit(&tx, clock, &child)?;
                tx.commit()?;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                check_work(clock, &child)?;
                Ok((self, result))
            },
        )
        .map_err(StoreError::Runtime)?
        .value
    }

    /// Acquire or refresh leadership in the same qualified store as responses.
    pub fn acquire_lease(
        self,
        invocation: &ProcessInvocation,
        cx: &Cx,
        key: crate::cache::CoordinationKey,
        force_refresh: bool,
    ) -> Result<(Self, crate::cache::LeaseAcquisition), StoreError> {
        self.lease_transaction(invocation, cx, move |tx| {
            let policy = crate::cache::CoordinationPolicy::default();
            if force_refresh {
                crate::cache::SqliteLeaseCoordinator::force_reacquire_in_transaction(
                    tx,
                    key,
                    cache_wall_clock_ms(),
                    &policy,
                )
            } else {
                crate::cache::SqliteLeaseCoordinator::acquire_in_transaction(
                    tx,
                    key,
                    cache_wall_clock_ms(),
                    &policy,
                )
            }
            .map_err(coordination_error)
        })
    }

    /// Mark completion without hiding response bodies in coordination state.
    pub fn complete_lease(
        self,
        invocation: &ProcessInvocation,
        cx: &Cx,
        leader: crate::cache::LeaderContext,
    ) -> Result<(Self, crate::cache::PublishOutcome), StoreError> {
        self.lease_transaction(invocation, cx, move |tx| {
            crate::cache::SqliteLeaseCoordinator::complete_in_transaction(
                tx,
                leader.key,
                leader.owner_token,
                leader.fencing_generation,
                cache_wall_clock_ms(),
                None,
            )
            .map_err(coordination_error)
        })
    }

    /// Read settlement without obtaining a writer lock or advancing state.
    pub fn lease(
        self,
        invocation: &ProcessInvocation,
        cx: &Cx,
        key: crate::cache::CoordinationKey,
    ) -> Result<(Self, Option<crate::cache::LeaseRecord>), StoreError> {
        let clock = invocation.clock();
        let child = cx.clone();
        run_blocking_leaf(
            invocation,
            cx,
            BlockingLeafKind::Database,
            false,
            move || {
                configure(&self.connection, clock, &child)?;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                check_stamp(&self.connection, self.stamp)?;
                let result = crate::cache::SqliteLeaseCoordinator::check_lease_on_connection(
                    &self.connection,
                    key,
                )
                .map_err(coordination_error)?;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                check_work(clock, &child)?;
                Ok((self, result))
            },
        )
        .map_err(StoreError::Runtime)?
        .value
    }

    /// Advance only this disposable cache's generation. No ledger/allowance is
    /// opened. Future entries must carry this stamp and reject stale consumers.
    /// The connection moves into the owned blocking leaf and back on success.
    pub fn advance_generation(
        mut self,
        invocation: &ProcessInvocation,
        cx: &Cx,
        expected: CacheStamp,
    ) -> Result<Self, StoreError> {
        let clock = invocation.clock();
        let child = cx.clone();
        run_blocking_leaf(
            invocation,
            cx,
            BlockingLeafKind::Database,
            false,
            move || {
                configure(&self.connection, clock, &child)?;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                refresh_busy_limit(&self.connection, clock, &child)?;
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                let actual = read_stamp(&tx)?;
                if actual.incarnation != expected.incarnation {
                    return Err(StoreError::StoreReplaced);
                }
                if actual.generation != expected.generation {
                    return Err(StoreError::StaleGeneration);
                }
                self.directory.admit_space()?;
                let next = actual
                    .generation
                    .checked_add(1)
                    .filter(|n| *n <= i64::MAX as u64)
                    .ok_or(StoreError::GenerationExhausted)?;
                let sql_next = i64::try_from(next).map_err(|_| StoreError::GenerationExhausted)?;
                if tx.execute(
                    "UPDATE sr_cache_meta SET generation=?1 WHERE singleton=1",
                    [sql_next],
                )? != 1
                {
                    return Err(StoreError::IncompatibleSchema);
                }
                refresh_busy_limit(&tx, clock, &child)?;
                tx.commit()?;
                self.stamp.generation = next;
                self.directory
                    .verify_database_file(&self.file, clock, &child)?;
                check_work(clock, &child)?;
                Ok(self)
            },
        )
        .map_err(StoreError::Runtime)?
        .value
    }
}

fn coordination_error(error: crate::cache::CoordinationError) -> StoreError {
    match error {
        crate::cache::CoordinationError::StorageBusy => StoreError::Busy,
        _ => StoreError::LeaseUnavailable,
    }
}

fn cache_wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sqlite_operational_failures_are_private_and_not_reported_as_corruption() {
        for (code, expected) in [
            (ErrorCode::DatabaseBusy, StoreError::Busy),
            (ErrorCode::DatabaseLocked, StoreError::Busy),
            (ErrorCode::OperationInterrupted, StoreError::Cancelled),
            (ErrorCode::PermissionDenied, StoreError::Permissions),
            (ErrorCode::ReadOnly, StoreError::Permissions),
            (ErrorCode::SystemIoFailure, StoreError::Io),
            (ErrorCode::CannotOpen, StoreError::Io),
            (ErrorCode::FileLockingProtocolFailed, StoreError::Io),
            (ErrorCode::SchemaChanged, StoreError::IncompatibleSchema),
            (ErrorCode::DiskFull, StoreError::InsufficientSpace),
            (ErrorCode::DatabaseCorrupt, StoreError::Corrupt),
        ] {
            let sqlite_error = || {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code,
                        extended_code: 0,
                    },
                    Some("synthetic-private-sqlite-detail".to_owned()),
                )
            };
            for safe in [
                StoreError::from(sqlite_error()),
                StoreError::from(EngineQualificationError::Sqlite(sqlite_error())),
            ] {
                assert_eq!(safe, expected);
                assert!(!format!("{safe:?}: {safe}").contains("synthetic-private"));
            }
        }
    }

    #[test]
    fn unqualified_identity_keeps_the_storage_error_contract() {
        assert_eq!(
            StoreError::from(EngineQualificationError::Unqualified),
            StoreError::UnqualifiedEngine
        );
    }

    #[test]
    fn storage_facade_returns_the_shared_qualified_identity() {
        let identity: crate::sqlite_engine::EngineIdentity = linked_engine().unwrap();
        assert_eq!(identity, crate::sqlite_engine::linked_engine().unwrap());
        assert_eq!(identity.version, QUALIFIED_SQLITE_VERSION);
        assert_eq!(identity.source_id, QUALIFIED_SQLITE_SOURCE_ID);
        assert_eq!(identity.rust_dependency, RUSQLITE_VERSION);
    }
}
