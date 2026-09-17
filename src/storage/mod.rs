//! Qualified Linux cache storage. Ledger and allowance stores are separate.
//!
//! Disk work runs on the invocation's bounded blocking pool. SQLite progress
//! cancellation and busy limits bound cooperative work; they cannot interrupt
//! an uninterruptible kernel filesystem wait. Late results are not published.

mod filesystem;

use crate::blocking::{BlockingLeafKind, remaining_busy_wait, run_blocking_leaf};
use crate::runtime::{EntryClock, ProcessInvocation, RuntimeError};
use asupersync::Cx;
use filesystem::PrivateDirectory;
use rusqlite::{
    Connection, ErrorCode, OpenFlags, TransactionBehavior, config::DbConfig, limits::Limit,
};
use std::{fmt, fs::File, path::PathBuf, time::Duration};

pub const CACHE_FILE: &str = "cache.sqlite3";
pub const CACHE_SCHEMA_VERSION: u32 = 1;
pub const CACHE_QUOTA_BYTES: u64 = 64 * 1024 * 1024;
pub const MAINTENANCE_RESERVE_BYTES: u64 = 4 * 1024 * 1024;
// The only write is one metadata row. Even at SQLite's largest page size,
// initialization and a generation update fit well below this admission margin.
pub const MUTATION_RESERVE_BYTES: u64 = 1024 * 1024;
pub const MAX_BUSY_WAIT_MS: u64 = 25;
pub const RUSQLITE_VERSION: &str = "0.40.2";
pub const QUALIFIED_SQLITE_VERSION: &str = "3.53.2";
pub const QUALIFIED_SQLITE_SOURCE_ID: &str =
    "2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24";
const MIN_SQLITE_VERSION: i32 = 3_051_003;
const APPLICATION_ID: i64 = 0x53524348; // SRCH: cache, never ledger/accounting.
const SCHEMA_ID: &str = "sr-cache-foundation-v1";
const METADATA_DDL: &str = "CREATE TABLE sr_cache_meta (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        incarnation BLOB NOT NULL CHECK(length(incarnation)=16),
        generation INTEGER NOT NULL CHECK(generation>=0),
        schema_id TEXT NOT NULL
    ) STRICT";

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineIdentity {
    pub version: String,
    pub version_number: i32,
    pub source_id: String,
    pub rust_dependency: &'static str,
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
    WrongStore,
    IncompatibleSchema,
    NewerSchema { version: i64 },
    UnqualifiedEngine,
    StoreReplaced,
    StaleGeneration,
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

fn validate_engine(version: i32, name: &str, source: &str) -> Result<(), StoreError> {
    if version < MIN_SQLITE_VERSION
        || version != 3_053_002
        || name != QUALIFIED_SQLITE_VERSION
        || source != QUALIFIED_SQLITE_SOURCE_ID
    {
        return Err(StoreError::UnqualifiedEngine);
    }
    Ok(())
}

/// Qualify the actual linked engine without touching any disk store.
pub fn linked_engine() -> Result<EngineIdentity, StoreError> {
    let version_number = rusqlite::version_number();
    if version_number < MIN_SQLITE_VERSION {
        return Err(StoreError::UnqualifiedEngine);
    }
    let connection = Connection::open_in_memory()?;
    let (version, source_id): (String, String) =
        connection.query_row("SELECT sqlite_version(), sqlite_source_id()", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    validate_engine(version_number, &version, &source_id)?;
    Ok(EngineIdentity {
        version,
        version_number,
        source_id,
        rust_dependency: RUSQLITE_VERSION,
    })
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
    let ddl: String = connection.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name='sr_cache_meta'",
        [],
        |row| row.get(0),
    )?;
    if objects != 1 || ddl != METADATA_DDL {
        return Err(StoreError::IncompatibleSchema);
    }
    let rows: i64 =
        connection.query_row("SELECT count(*) FROM sr_cache_meta", [], |row| row.get(0))?;
    if rows != 1 {
        return Err(StoreError::IncompatibleSchema);
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
    tx.execute_batch(METADATA_DDL)?;
    tx.execute(
        "INSERT INTO sr_cache_meta VALUES (1, randomblob(16), 0, ?1)",
        [SCHEMA_ID],
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
    })))
}

impl CacheStore {
    pub fn engine(&self) -> &EngineIdentity {
        &self.engine
    }
    pub const fn stamp(&self) -> CacheStamp {
        self.stamp
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sqlite_operational_failures_are_private_and_not_reported_as_corruption() {
        for (code, expected) in [
            (ErrorCode::DatabaseBusy, StoreError::Busy),
            (ErrorCode::DatabaseLocked, StoreError::Busy),
            (ErrorCode::PermissionDenied, StoreError::Permissions),
            (ErrorCode::ReadOnly, StoreError::Permissions),
            (ErrorCode::SystemIoFailure, StoreError::Io),
            (ErrorCode::CannotOpen, StoreError::Io),
            (ErrorCode::FileLockingProtocolFailed, StoreError::Io),
            (ErrorCode::SchemaChanged, StoreError::IncompatibleSchema),
            (ErrorCode::DiskFull, StoreError::InsufficientSpace),
            (ErrorCode::DatabaseCorrupt, StoreError::Corrupt),
        ] {
            let error = rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code,
                    extended_code: 0,
                },
                Some("synthetic-private-sqlite-detail".to_owned()),
            );
            let safe = StoreError::from(error);
            assert_eq!(safe, expected);
            assert!(!format!("{safe:?}: {safe}").contains("synthetic-private"));
        }
    }
    #[test]
    fn engine_guard_requires_minimum_and_the_exact_qualified_source() {
        assert_eq!(
            validate_engine(
                3_053_002,
                QUALIFIED_SQLITE_VERSION,
                QUALIFIED_SQLITE_SOURCE_ID
            ),
            Ok(())
        );
        for version in [3_050_004, 3_051_002, 3_051_003, 3_053_003] {
            assert_eq!(
                validate_engine(
                    version,
                    QUALIFIED_SQLITE_VERSION,
                    QUALIFIED_SQLITE_SOURCE_ID
                ),
                Err(StoreError::UnqualifiedEngine)
            );
        }
        assert_eq!(
            validate_engine(3_053_002, QUALIFIED_SQLITE_VERSION, "unexpected-source"),
            Err(StoreError::UnqualifiedEngine)
        );
    }
}
