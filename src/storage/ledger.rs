//! Qualified local observation ledger storage.
//!
//! Stores execution events, session cursors, roster snapshots, candidate scoring,
//! provider attempts, observations, judgments, feedback proposals, and calibrations
//! under `$XDG_DATA_HOME/sr/ledger.sqlite3` with strict owner-only permissions (0o700 dir, 0o600 file).
//!
//! Every mutation is fenced by store incarnation, schema generation, and data generation.

use super::platform::{DirectoryIdentity, local_filesystem, storage_path};
use crate::blocking::{BlockingLeafKind, remaining_busy_wait, run_blocking_leaf};
use crate::jev::admission::{AttemptFailure, AttemptOutcome, AttemptProvenance, RankingStage};
use crate::limits::MonotonicMillis;
use crate::runtime::{EntryClock, ProcessInvocation};
use crate::storage::{EngineIdentity, StoreError, check_work, linked_engine};
use asupersync::Cx;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat, mkdirat};
use nix::sys::statfs::fstatfs;
use nix::sys::statvfs::fstatvfs;
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, config::DbConfig,
    limits::Limit, params,
};
use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const LEDGER_FILE: &str = "ledger.sqlite3";
pub const LEDGER_APPLICATION_ID: i64 = 0x53524C47; // SRLG: SkillRanker Ledger
pub const LEDGER_SCHEMA_VERSION: u32 = 1;
pub const LEDGER_SCHEMA_ID: &str = "sr-ledger-v1";
pub const LEDGER_QUOTA_BYTES: u64 = 256 * 1024 * 1024;
pub const LEDGER_MAINTENANCE_RESERVE_BYTES: u64 = 16 * 1024 * 1024;
pub const LEDGER_MUTATION_RESERVE_BYTES: u64 = 4 * 1024 * 1024;
pub const LEDGER_RECORDING_CEILING_BYTES: u64 =
    LEDGER_QUOTA_BYTES - LEDGER_MAINTENANCE_RESERVE_BYTES - LEDGER_MUTATION_RESERVE_BYTES;
pub const MAX_BUSY_WAIT_MS: u64 = 25;
pub const DEFAULT_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000; // 30 days in milliseconds: 2_592_000_000

/// The `reason` recorded on a ranking event written before its first provider send.
///
/// An invocation records itself with this reason, `DecisionKind::Unavailable`,
/// `ExposureState::Generated` and `elapsed_ms = 0` before it sends anything, so that
/// a process killed while waiting on the provider leaves evidence that it ran and may
/// have paid. Completion rewrites all four fields in place, so a row that still
/// carries this reason belongs to an invocation that never came back: either one still
/// running, or one that was killed.
///
/// Statistics must therefore not read such a row as a finished outcome. Its decision
/// is not a failure of the provider, its `elapsed_ms` is a placeholder rather than a
/// measurement, and its lack of provider attempts is not evidence of cache reuse. The
/// producer in the ranking pipeline and every reader share this constant so that the
/// two cannot drift apart silently.
pub const IN_FLIGHT_REASON: &str = "in-flight";

/// SQL predicate matching a `ranking_events` row whose invocation never came back.
///
/// The reason is bound as `?3` rather than written into the SQL, so [`IN_FLIGHT_REASON`]
/// stays the single spelling shared by the pipeline that writes these rows and every
/// statistic that has to exclude them. Any query using this fragment must therefore
/// bind the window as `?1`/`?2` and the reason as `?3`.
const UNFINISHED_ROW_SQL: &str =
    "(decision = 'unavailable' AND exposure_state = 'generated' AND reason = ?3)";

/// The primary key of one attempt's row.
///
/// The in-process attempt id is `<invocation id>-att-<n>`, and the invocation id is a
/// fixed word for a given entry point, so it repeats on every run. The owner alone is not
/// enough either: a duplicate delivery is deliberately the *same* event, so two deliveries
/// that each paid would collide on an owner-scoped key and the second would be dropped.
/// The invocation token distinguishes them, so a repeat appends its own rows under the one
/// event it belongs to (sr-qqlk).
///
/// One attempt is written more than once — when admitted, when its request reaches the
/// wire, and when it settles — and every one of those writers has to land on the same row.
/// They share this function so that they cannot compose the key differently and leave
/// several rows behind for a single attempt.
pub fn attempt_row_key(owner_event_id: &str, invocation_token: &str, attempt_id: &str) -> String {
    format!("{owner_event_id}:{invocation_token}:{attempt_id}")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LedgerCapacityReport {
    pub total_quota_bytes: u64,
    pub maintenance_reserve_bytes: u64,
    pub mutation_reserve_bytes: u64,
    pub recording_ceiling_bytes: u64,
    pub occupied_bytes: u64,
    pub usable_recording_bytes: u64,
    pub available_disk_bytes: u64,
    pub is_recording_admitted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaintenanceKind {
    Vacuum,
    Backup,
    Migration,
    Prune,
    Clear,
}

impl MaintenanceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vacuum => "vacuum",
            Self::Backup => "backup",
            Self::Migration => "migration",
            Self::Prune => "prune",
            Self::Clear => "clear",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaintenancePreflight {
    pub kind: MaintenanceKind,
    pub current_occupied_bytes: u64,
    pub required_additional_bytes: u64,
    pub projected_total_bytes: u64,
    pub available_disk_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaintenanceError {
    QuotaExceeded {
        occupied_bytes: u64,
        required_additional_bytes: u64,
        quota_bytes: u64,
        recovery_step: &'static str,
    },
    InsufficientDiskSpace {
        available_bytes: u64,
        required_additional_bytes: u64,
        recovery_step: &'static str,
    },
    Store(StoreError),
    Sqlite(String),
}

impl fmt::Display for MaintenanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuotaExceeded {
                occupied_bytes,
                required_additional_bytes,
                quota_bytes,
                recovery_step,
            } => write!(
                f,
                "ledger maintenance requires {required_additional_bytes} additional bytes, which would exceed the {quota_bytes} byte quota (currently {occupied_bytes} bytes occupied); recovery step: {recovery_step}"
            ),
            Self::InsufficientDiskSpace {
                available_bytes,
                required_additional_bytes,
                recovery_step,
            } => write!(
                f,
                "ledger maintenance requires {required_additional_bytes} bytes but only {available_bytes} bytes are available on disk; recovery step: {recovery_step}"
            ),
            Self::Store(err) => write!(f, "ledger store error during maintenance: {err}"),
            Self::Sqlite(msg) => write!(f, "ledger SQLite error during maintenance: {msg}"),
        }
    }
}

impl std::error::Error for MaintenanceError {}

impl From<StoreError> for MaintenanceError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

impl From<rusqlite::Error> for MaintenanceError {
    fn from(err: rusqlite::Error) -> Self {
        match err.sqlite_error_code() {
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
                Self::Store(StoreError::Busy)
            }
            Some(ErrorCode::OperationInterrupted) => Self::Store(StoreError::Cancelled),
            Some(ErrorCode::DiskFull) => Self::Store(StoreError::InsufficientSpace),
            Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly) => {
                Self::Store(StoreError::Permissions)
            }
            _ => Self::Sqlite(err.to_string()),
        }
    }
}

const SCHEMA_MIGRATIONS_DDL: &str = "CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY CHECK(version >= 1),
    checksum TEXT NOT NULL CHECK(length(checksum) > 0),
    applied_at_unix_ms INTEGER NOT NULL CHECK(applied_at_unix_ms >= 0)
) STRICT";

const STORE_META_DDL: &str = "CREATE TABLE store_meta (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    incarnation BLOB NOT NULL CHECK(length(incarnation) = 16),
    schema_generation INTEGER NOT NULL CHECK(schema_generation >= 1),
    data_generation INTEGER NOT NULL CHECK(data_generation >= 1),
    schema_id TEXT NOT NULL
) STRICT";

const SESSION_CURSORS_DDL: &str = "CREATE TABLE session_cursors (
    workspace_root TEXT NOT NULL CHECK(length(workspace_root) > 0),
    session_id TEXT NOT NULL CHECK(length(session_id) > 0),
    agent_branch TEXT NOT NULL,
    cursor_kind TEXT NOT NULL CHECK(cursor_kind IN ('ranking', 'observation')),
    transcript_generation INTEGER NOT NULL CHECK(transcript_generation >= 0),
    last_complete_event_id TEXT NOT NULL,
    last_offset_bytes INTEGER NOT NULL CHECK(last_offset_bytes >= 0),
    updated_at_unix_ms INTEGER NOT NULL CHECK(updated_at_unix_ms >= 0),
    PRIMARY KEY (workspace_root, session_id, agent_branch, cursor_kind)
) STRICT";

const ROSTER_SNAPSHOTS_DDL: &str = "CREATE TABLE roster_snapshots (
    snapshot_id TEXT PRIMARY KEY CHECK(length(snapshot_id) > 0),
    workspace_root TEXT NOT NULL,
    adapter TEXT NOT NULL,
    total_candidates INTEGER NOT NULL CHECK(total_candidates >= 0),
    eligible_candidates INTEGER NOT NULL CHECK(eligible_candidates >= 0),
    membership_coverage TEXT NOT NULL CHECK(membership_coverage IN ('complete', 'unknown', 'partial')),
    members_json TEXT NOT NULL,
    created_at_unix_ms INTEGER NOT NULL CHECK(created_at_unix_ms >= 0)
) STRICT";

const RANKING_EVENTS_DDL: &str = "CREATE TABLE ranking_events (
    event_id TEXT PRIMARY KEY CHECK(length(event_id) > 0),
    verified_delivery_key TEXT UNIQUE,
    workspace_root TEXT NOT NULL,
    session_id TEXT NOT NULL,
    agent_branch TEXT NOT NULL,
    mode_channel TEXT NOT NULL,
    policy_version TEXT NOT NULL,
    schema_version INTEGER NOT NULL CHECK(schema_version >= 1),
    decision TEXT NOT NULL CHECK(decision IN ('ranked', 'explicit', 'abstain', 'unavailable')),
    reason TEXT NOT NULL,
    exposure_state TEXT NOT NULL CHECK(exposure_state IN ('generated', 'prepared', 'emitted', 'acknowledged', 'unknown')),
    elapsed_ms INTEGER NOT NULL CHECK(elapsed_ms >= 0),
    created_at_unix_ms INTEGER NOT NULL CHECK(created_at_unix_ms >= 0),
    input_tokens INTEGER CHECK(input_tokens IS NULL OR input_tokens >= 0),
    output_tokens INTEGER CHECK(output_tokens IS NULL OR output_tokens >= 0),
    snapshot_id TEXT REFERENCES roster_snapshots(snapshot_id) ON DELETE RESTRICT
) STRICT";

const RANKING_CANDIDATES_DDL: &str = "CREATE TABLE ranking_candidates (
    event_id TEXT NOT NULL REFERENCES ranking_events(event_id) ON DELETE CASCADE,
    stage TEXT NOT NULL CHECK(stage IN ('wide', 'rerank')),
    skill_id TEXT NOT NULL CHECK(length(skill_id) > 0),
    skill_version TEXT NOT NULL,
    raw_probability REAL CHECK(raw_probability IS NULL OR (raw_probability >= 0.0 AND raw_probability <= 1.0)),
    normalized_probability REAL CHECK(normalized_probability IS NULL OR (normalized_probability >= 0.0 AND normalized_probability <= 1.0)),
    fit_score REAL CHECK(fit_score IS NULL OR (fit_score >= 0.0 AND fit_score <= 1.0)),
    rank_score REAL,
    rank_position INTEGER CHECK(rank_position IS NULL OR rank_position >= 1),
    excluded INTEGER NOT NULL CHECK(excluded IN (0, 1)),
    exclusion_reason TEXT,
    PRIMARY KEY (event_id, stage, skill_id)
) STRICT";

const PROVIDER_ATTEMPTS_DDL: &str = "CREATE TABLE provider_attempts (
    attempt_id TEXT PRIMARY KEY CHECK(length(attempt_id) > 0),
    owner_event_id TEXT NOT NULL REFERENCES ranking_events(event_id) ON DELETE RESTRICT,
    stage TEXT NOT NULL CHECK(stage IN ('wide', 'rerank')),
    request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint) > 0),
    admitted_at_unix_ms INTEGER NOT NULL CHECK(admitted_at_unix_ms >= 0),
    sent_at_unix_ms INTEGER CHECK(sent_at_unix_ms IS NULL OR sent_at_unix_ms >= 0),
    completed_at_unix_ms INTEGER CHECK(completed_at_unix_ms IS NULL OR completed_at_unix_ms >= 0),
    status TEXT NOT NULL CHECK(status IN ('admitted', 'sent', 'completed', 'failed', 'unknown')),
    input_tokens INTEGER CHECK(input_tokens IS NULL OR input_tokens >= 0),
    output_tokens INTEGER CHECK(output_tokens IS NULL OR output_tokens >= 0),
    http_status INTEGER CHECK(http_status IS NULL OR (http_status >= 100 AND http_status <= 599)),
    error_kind TEXT
) STRICT";

const OBSERVATIONS_DDL: &str = "CREATE TABLE observations (
    observation_id TEXT PRIMARY KEY CHECK(length(observation_id) > 0),
    source_event_key TEXT UNIQUE NOT NULL CHECK(length(source_event_key) > 0),
    workspace_root TEXT NOT NULL,
    session_id TEXT NOT NULL,
    agent_branch TEXT NOT NULL,
    attributed_event_id TEXT REFERENCES ranking_events(event_id) ON DELETE SET NULL,
    skill_id TEXT NOT NULL CHECK(length(skill_id) > 0),
    evidence_state TEXT NOT NULL CHECK(evidence_state IN ('attempted', 'loaded', 'censored')),
    observed_at_unix_ms INTEGER NOT NULL CHECK(observed_at_unix_ms >= 0)
) STRICT";

const JUDGMENTS_DDL: &str = "CREATE TABLE judgments (
    judgment_id TEXT PRIMARY KEY CHECK(length(judgment_id) > 0),
    attributed_event_id TEXT NOT NULL REFERENCES ranking_events(event_id) ON DELETE RESTRICT,
    skill_id TEXT NOT NULL CHECK(length(skill_id) > 0),
    label TEXT NOT NULL CHECK(label IN ('useful', 'harmful', 'neutral')),
    label_version INTEGER NOT NULL DEFAULT 1 CHECK(label_version >= 1),
    provenance TEXT NOT NULL CHECK(length(provenance) > 0),
    created_at_unix_ms INTEGER NOT NULL CHECK(created_at_unix_ms >= 0)
) STRICT";

const FEEDBACK_PROPOSALS_DDL: &str = "CREATE TABLE feedback_proposals (
    proposal_id TEXT PRIMARY KEY CHECK(length(proposal_id) > 0),
    workspace_root TEXT NOT NULL,
    session_id TEXT NOT NULL,
    suggested_skill_reference TEXT NOT NULL CHECK(length(suggested_skill_reference) > 0),
    status TEXT NOT NULL CHECK(status IN ('unresolved', 'historically_absent', 'rejected', 'adopted')),
    notes TEXT,
    created_at_unix_ms INTEGER NOT NULL CHECK(created_at_unix_ms >= 0)
) STRICT";

const CALIBRATIONS_DDL: &str = "CREATE TABLE calibrations (
    calibration_id TEXT PRIMARY KEY CHECK(length(calibration_id) > 0),
    dataset_fingerprint TEXT NOT NULL CHECK(length(dataset_fingerprint) > 0),
    split TEXT NOT NULL CHECK(split IN ('train', 'validation', 'holdout')),
    objective TEXT NOT NULL,
    coefficients_json TEXT NOT NULL,
    evaluation_report_id TEXT NOT NULL,
    created_at_unix_ms INTEGER NOT NULL CHECK(created_at_unix_ms >= 0)
) STRICT";

const TABLES: [(&str, &str); 11] = [
    ("schema_migrations", SCHEMA_MIGRATIONS_DDL),
    ("store_meta", STORE_META_DDL),
    ("session_cursors", SESSION_CURSORS_DDL),
    ("roster_snapshots", ROSTER_SNAPSHOTS_DDL),
    ("ranking_events", RANKING_EVENTS_DDL),
    ("ranking_candidates", RANKING_CANDIDATES_DDL),
    ("provider_attempts", PROVIDER_ATTEMPTS_DDL),
    ("observations", OBSERVATIONS_DDL),
    ("judgments", JUDGMENTS_DDL),
    ("feedback_proposals", FEEDBACK_PROPOSALS_DDL),
    ("calibrations", CALIBRATIONS_DDL),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerAccess {
    Disabled,
    ExistingOnly,
    Initialize,
    Migrate,
}

#[derive(Clone, Eq, PartialEq)]
pub enum LedgerLocation {
    Platform,
    Directory(PathBuf),
}

impl fmt::Debug for LedgerLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform => f.write_str("LedgerLocation::Platform"),
            Self::Directory(_) => f.write_str("LedgerLocation::Directory(<private>)"),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LedgerStamp {
    pub incarnation: [u8; 16],
    pub schema_generation: u64,
    pub data_generation: u64,
}

impl fmt::Debug for LedgerStamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LedgerStamp")
            .field("schema_generation", &self.schema_generation)
            .field("data_generation", &self.data_generation)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum LedgerOpen {
    Disabled,
    Missing,
    ReadOnly(Box<LedgerStore>),
    Ready(Box<LedgerStore>),
}

pub struct LedgerStore {
    connection: Connection,
    directory: PrivateLedgerDirectory,
    file: File,
    stamp: LedgerStamp,
    engine: EngineIdentity,
    read_only: bool,
}

impl fmt::Debug for LedgerStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LedgerStore")
            .field("engine", &self.engine)
            .field("stamp", &self.stamp)
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

pub const LEDGER_TARGET_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InitStatus {
    Created,
    AlreadyCurrent,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct InitReport {
    pub status: InitStatus,
    pub schema_version: u32,
    pub database_path: PathBuf,
    pub incarnation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct MigrationPreview {
    pub current_version: u32,
    pub target_version: u32,
    pub pending_migrations: Vec<PendingMigration>,
    pub required_headroom_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PendingMigration {
    pub version: u32,
    pub name: String,
    pub description: String,
    pub checksum: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct MigrationReport {
    pub from_version: u32,
    pub to_version: u32,
    pub applied_migrations: Vec<String>,
    pub backup_path: PathBuf,
    pub backup_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationError {
    AlreadyUpToDate,
    UnsupportedNewerVersion {
        current_version: u32,
        target_version: u32,
    },
    Preflight(MaintenanceError),
    Store(StoreError),
    Sqlite(String),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyUpToDate => write!(f, "database schema is already up to date"),
            Self::UnsupportedNewerVersion {
                current_version,
                target_version,
            } => {
                write!(
                    f,
                    "database schema version {current_version} is newer than supported version {target_version}; auto-downgrade is disabled"
                )
            }
            Self::Preflight(err) => write!(f, "preflight check failed: {err}"),
            Self::Store(err) => write!(f, "store error: {err}"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
        }
    }
}

impl std::error::Error for MigrationError {}

impl From<MaintenanceError> for MigrationError {
    fn from(err: MaintenanceError) -> Self {
        Self::Preflight(err)
    }
}

impl From<StoreError> for MigrationError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

impl From<rusqlite::Error> for MigrationError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err.to_string())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CleanupDebt {
    pub expired_events: u64,
    pub unreferenced_snapshots: u64,
    pub expired_observations: u64,
    pub expired_judgments: u64,
    pub freelist_bytes: u64,
    pub has_debt: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PrunePreview {
    pub cutoff_unix_ms: i64,
    pub cutoff_iso: String,
    pub events_to_prune: u64,
    pub candidates_to_prune: u64,
    pub observations_to_prune: u64,
    pub judgments_to_prune: u64,
    pub provider_attempts_to_prune: u64,
    pub snapshots_to_prune: u64,
    pub shared_snapshots_preserved: u64,
    pub requires_apply: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PruneReport {
    pub cutoff_unix_ms: i64,
    pub cutoff_iso: String,
    pub events_pruned: u64,
    pub candidates_pruned: u64,
    pub observations_pruned: u64,
    pub judgments_pruned: u64,
    pub provider_attempts_pruned: u64,
    pub snapshots_pruned: u64,
    pub shared_snapshots_preserved: u64,
    pub stamp_before: LedgerStamp,
    pub stamp_after: LedgerStamp,
    pub affected_provenance: &'static str,
    pub preflight_headroom_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClearPreview {
    pub events_count: u64,
    pub candidates_count: u64,
    pub observations_count: u64,
    pub judgments_count: u64,
    pub provider_attempts_count: u64,
    pub snapshots_count: u64,
    pub session_cursors_count: u64,
    pub feedback_proposals_count: u64,
    pub calibrations_count: u64,
    pub total_records: u64,
    pub requires_apply: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClearReport {
    pub records_cleared: u64,
    pub stamp_before: LedgerStamp,
    pub stamp_after: LedgerStamp,
    pub affected_provenance: &'static str,
    pub preflight_headroom_bytes: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RetainedStats {
    pub as_of_unix_ms: i64,
    pub cutoff_unix_ms: i64,
    pub total_events: u64,
    pub active_events: u64,
    pub expired_events: u64,
    pub active_judgments: u64,
    pub active_observations: u64,
    pub active_snapshots: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChannelStats {
    pub channel: String,
    pub evaluated_turns: u64,
    pub emitted: u64,
    pub abstain: u64,
    pub muted: u64,
    pub unavailable: u64,
    /// Turns in this channel that recorded themselves and never came back: still
    /// running, or killed. Counted apart from `unavailable`, which is about a
    /// provider outcome this turn never reached. See [`IN_FLIGHT_REASON`].
    pub in_flight_or_killed: u64,
}

/// Finished `unavailable` turns sharing one recorded failure kind.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FailureCause {
    pub reason: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LatencySummary {
    pub mean_ms: u64,
    pub median_ms: u64,
    pub p95_ms: u64,
    pub min_ms: u64,
    pub max_ms: u64,
    /// Turns left out of the five figures above because they never finished, so their
    /// recorded duration is a placeholder rather than a measurement. Reported so that
    /// a summary drawn from a subset says which subset, rather than implying it
    /// covered every turn in the window.
    pub excluded_unfinished: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnMetrics {
    pub total_evaluated: u64,
    pub emitted_suggestions: u64,
    pub valid_abstentions: u64,
    pub muted_or_suppressed: u64,
    pub operational_failures: u64,
    pub explicit_requirements: u64,
    /// Turns that recorded themselves before sending and never recorded an outcome.
    /// An invocation still in flight and one that was killed are indistinguishable
    /// from the ledger alone, so they share one honest count rather than being split
    /// on a guess. They are neither failures nor deliveries: see [`IN_FLIGHT_REASON`].
    pub in_flight_or_killed: u64,
    pub by_channel: Vec<ChannelStats>,
    /// `operational_failures` split by recorded kind, most frequent first. An
    /// absent credential (`credential-absent`) is an environment gap, not a
    /// provider outage or a rejected key (`authentication`), so the availability
    /// cohort must be able to tell them apart.
    pub failure_causes: Vec<FailureCause>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ObservationMetrics {
    pub total_observations: u64,
    pub observed_loads: u64,
    pub attempted_loads: u64,
    pub censored_observations: u64,
    pub attributed_loads: u64,
    pub unattributed_loads: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_coverage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion_adoption_rate: Option<f64>,
    pub caveat: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct JudgmentMetrics {
    pub total_judgments: u64,
    pub useful: u64,
    pub harmful: u64,
    pub neutral: u64,
    pub distinct_judged_events: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_coverage_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub useful_ratio_in_judged: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProviderMetrics {
    pub total_attempts: u64,
    pub completed_attempts: u64,
    pub failed_attempts: u64,
    pub unknown_attempts: u64,
    pub known_input_tokens: u64,
    pub known_output_tokens: u64,
    pub known_total_tokens: u64,
    pub unknown_usage_attempts: u64,
    pub cache_served_events: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit_rate: Option<f64>,
    pub cost_per_useful_suggestion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_useful_suggestion: Option<f64>,
    /// Judged turns that owned no provider attempt, because they were served from a response
    /// another turn paid for. Their usefulness is real and their cost is not theirs, so they
    /// are counted apart rather than folded into a ratio that would read as free.
    pub judged_turns_served_from_cache: u64,
    /// Judged-cohort attempts whose usage the provider never reported, counted by the same
    /// predicate as `unknown_usage_attempts`. While this is above zero the cohort's token total
    /// is a lower bound: those attempts were paid for and contribute nothing to the sum.
    pub judged_cohort_unknown_usage_attempts: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SkillStatSummary {
    pub skill_id: String,
    pub top1_recommendations: u64,
    pub shortlist_appearances: u64,
    pub observed_loads: u64,
    pub attributed_loads: u64,
    pub judged_useful: u64,
    pub judged_harmful: u64,
    pub judged_neutral: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatsValueReport {
    pub as_of_unix_ms: i64,
    pub since_unix_ms: i64,
    pub turns: TurnMetrics,
    pub latency: LatencySummary,
    pub observations: ObservationMetrics,
    pub judgments: JudgmentMetrics,
    pub provider: ProviderMetrics,
    pub retained: RetainedStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_skill: Option<Vec<SkillStatSummary>>,
}

/// Formats a unix millisecond timestamp as ISO-8601 UTC string (e.g. `2026-09-01T00:00:00Z`).
pub fn format_unix_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let rem_ms = ms.rem_euclid(1000);
    let days = secs.div_euclid(86400);
    let rem_secs = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let hour = rem_secs / 3600;
    let min = (rem_secs % 3600) / 60;
    let sec = rem_secs % 60;
    if rem_ms == 0 {
        format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
    } else {
        format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}.{rem_ms:03}Z")
    }
}

/// Howard Hinnant's algorithm for days since unix epoch (1970-01-01) from civil date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Howard Hinnant's algorithm for civil date from days since unix epoch (1970-01-01).
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1024 + doe / 1460 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Parses a date or cutoff string into a unix timestamp in milliseconds.
/// Supports:
/// - Relative durations: "30d", "7d", "24h"
/// - ISO date: "YYYY-MM-DD" (UTC midnight)
/// - ISO timestamp: "YYYY-MM-DDTHH:MM:SSZ" or "YYYY-MM-DDTHH:MM:SS"
/// - Unix timestamp (ms or sec)
pub fn parse_cutoff_to_unix_ms(input: &str, now_unix_ms: i64) -> Result<i64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Cutoff date cannot be empty".into());
    }
    if let Some(days_str) = trimmed.strip_suffix('d') {
        let days = days_str
            .parse::<i64>()
            .map_err(|_| format!("Invalid days duration: '{trimmed}'"))?;
        let ms = days
            .checked_mul(86_400_000)
            .ok_or_else(|| "Duration overflow".to_string())?;
        return Ok(now_unix_ms.saturating_sub(ms));
    }
    if let Some(hours_str) = trimmed.strip_suffix('h') {
        let hours = hours_str
            .parse::<i64>()
            .map_err(|_| format!("Invalid hours duration: '{trimmed}'"))?;
        let ms = hours
            .checked_mul(3_600_000)
            .ok_or_else(|| "Duration overflow".to_string())?;
        return Ok(now_unix_ms.saturating_sub(ms));
    }
    if let Some(min_str) = trimmed.strip_suffix('m') {
        let mins = min_str
            .parse::<i64>()
            .map_err(|_| format!("Invalid minutes duration: '{trimmed}'"))?;
        let ms = mins
            .checked_mul(60_000)
            .ok_or_else(|| "Duration overflow".to_string())?;
        return Ok(now_unix_ms.saturating_sub(ms));
    }
    if let Ok(num) = trimmed.parse::<i64>() {
        if num > 0 && num < 10_000_000_000 {
            return Ok(num * 1000);
        }
        return Ok(num);
    }
    let date_part = if let Some((d, _)) = trimmed.split_once('T') {
        d
    } else if let Some((d, _)) = trimmed.split_once(' ') {
        d
    } else {
        trimmed
    };
    let parts: Vec<&str> = date_part.split('-').collect();
    if parts.len() != 3 {
        return Err(format!(
            "Invalid date format: '{trimmed}'. Expected YYYY-MM-DD, ISO timestamp, or duration like '30d'"
        ));
    }
    let y = parts[0]
        .parse::<i64>()
        .map_err(|_| format!("Invalid year in '{trimmed}'"))?;
    let m = parts[1]
        .parse::<i64>()
        .map_err(|_| format!("Invalid month in '{trimmed}'"))?;
    let d = parts[2]
        .parse::<i64>()
        .map_err(|_| format!("Invalid day in '{trimmed}'"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(format!("Date out of range in '{trimmed}'"));
    }
    let days = days_from_civil(y, m, d);
    let mut ms = days
        .checked_mul(86_400_000)
        .ok_or_else(|| "Date overflow".to_string())?;

    let time_str = if let Some((_, t)) = trimmed.split_once('T') {
        Some(t.trim_end_matches('Z'))
    } else if let Some((_, t)) = trimmed.split_once(' ') {
        Some(t.trim_end_matches('Z'))
    } else {
        None
    };

    if let Some(t) = time_str {
        let t_parts: Vec<&str> = t.split(':').collect();
        if t_parts.len() >= 2 {
            let hour = t_parts[0]
                .parse::<i64>()
                .map_err(|_| format!("Invalid hour in '{trimmed}'"))?;
            let min = t_parts[1]
                .parse::<i64>()
                .map_err(|_| format!("Invalid minute in '{trimmed}'"))?;
            let sec = if t_parts.len() >= 3 {
                t_parts[2]
                    .parse::<f64>()
                    .map_err(|_| format!("Invalid second in '{trimmed}'"))?
            } else {
                0.0
            };
            let time_ms = (hour * 3600 + min * 60) * 1000 + (sec * 1000.0) as i64;
            ms += time_ms;
        }
    }

    Ok(ms)
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LedgerStatusReport {
    pub status: &'static str,
    pub schema_version: Option<u32>,
    pub target_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_available: Option<u32>,
    pub schema_generation: Option<u64>,
    pub data_generation: Option<u64>,
    pub is_read_only: bool,
    pub database_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_debt: Option<CleanupDebt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationStep {
    pub from_version: u32,
    pub to_version: u32,
    pub name: &'static str,
    pub checksum: &'static str,
    pub description: &'static str,
    pub ddl: &'static str,
}

pub const MIGRATION_V1_TO_V2: MigrationStep = MigrationStep {
    from_version: 1,
    to_version: 2,
    name: "v2-add-audit-log",
    checksum: "b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c",
    description: "Add ledger audit log table for provenance tracking",
    ddl: "CREATE TABLE ledger_audit_log (
    entry_id TEXT PRIMARY KEY CHECK(length(entry_id) > 0),
    action TEXT NOT NULL CHECK(length(action) > 0),
    occurred_at_unix_ms INTEGER NOT NULL CHECK(occurred_at_unix_ms >= 0),
    detail TEXT
) STRICT;",
};

pub const AVAILABLE_MIGRATIONS: &[MigrationStep] = &[MIGRATION_V1_TO_V2];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorKind {
    Ranking,
    Observation,
}

impl CursorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ranking => "ranking",
            Self::Observation => "observation",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for CursorKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ranking" => Ok(Self::Ranking),
            "observation" => Ok(Self::Observation),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCursor {
    pub workspace_root: String,
    pub session_id: String,
    pub agent_branch: String,
    pub cursor_kind: CursorKind,
    pub transcript_generation: u64,
    pub last_complete_event_id: String,
    pub last_offset_bytes: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MembershipCoverage {
    Complete,
    Unknown,
    Partial,
}

impl MembershipCoverage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Unknown => "unknown",
            Self::Partial => "partial",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for MembershipCoverage {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "complete" => Ok(Self::Complete),
            "unknown" => Ok(Self::Unknown),
            "partial" => Ok(Self::Partial),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewRosterSnapshot {
    pub snapshot_id: String,
    pub workspace_root: String,
    pub adapter: String,
    pub total_candidates: u64,
    pub eligible_candidates: u64,
    pub membership_coverage: MembershipCoverage,
    pub members_json: String,
    pub created_at_unix_ms: u64,
}

/// Bound before JSON allocation; SQLite additionally bounds the complete row.
pub const MAX_SNAPSHOT_METADATA_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SNAPSHOT_MEMBERS: usize = 10_000;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotMember {
    pub skill_id: String,
    pub invocation_name: Option<String>,
    pub content_hash: Option<String>,
    pub source: String,
    pub eligible: bool,
    pub exclusion_reason: Option<String>,
}

fn bounded_metadata(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn validated_snapshot(snapshot: &NewRosterSnapshot) -> Result<String, StoreError> {
    if !bounded_metadata(&snapshot.snapshot_id, 256)
        || !bounded_metadata(&snapshot.workspace_root, 4096)
        || !bounded_metadata(&snapshot.adapter, 256)
        || snapshot.total_candidates > MAX_SNAPSHOT_MEMBERS as u64
        || snapshot.eligible_candidates > snapshot.total_candidates
        || i64::try_from(snapshot.created_at_unix_ms).is_err()
    {
        return Err(StoreError::InvalidRecord);
    }
    let value = crate::adapter::decode_json(
        snapshot.members_json.as_bytes(),
        MAX_SNAPSHOT_METADATA_BYTES,
    )
    .map_err(|_| StoreError::InvalidRecord)?;
    if value
        .as_array()
        .is_none_or(|members| members.len() > MAX_SNAPSHOT_MEMBERS)
    {
        return Err(StoreError::InvalidRecord);
    }
    let mut members: Vec<SnapshotMember> =
        serde_json::from_value(value).map_err(|_| StoreError::InvalidRecord)?;
    let mut ids = BTreeSet::new();
    for member in &members {
        if !bounded_metadata(&member.skill_id, 256)
            || member
                .invocation_name
                .as_ref()
                .is_some_and(|name| !bounded_metadata(name, 1024))
            || member
                .content_hash
                .as_ref()
                .is_some_and(|hash| !bounded_metadata(hash, 256))
            || (member.eligible
                && (member.invocation_name.is_none() || member.content_hash.is_none()))
            || !bounded_metadata(&member.source, 256)
            || member.exclusion_reason.as_ref().is_some_and(|reason| {
                !bounded_metadata(reason, 256)
                    || !reason
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            })
            || !ids.insert(&member.skill_id)
        {
            return Err(StoreError::InvalidRecord);
        }
    }
    let count = members.len() as u64;
    let eligible = members.iter().filter(|member| member.eligible).count() as u64;
    if count > snapshot.total_candidates
        || eligible > snapshot.eligible_candidates
        || count - eligible > snapshot.total_candidates - snapshot.eligible_candidates
        || (snapshot.membership_coverage == MembershipCoverage::Complete
            && (count != snapshot.total_candidates || eligible != snapshot.eligible_candidates))
    {
        return Err(StoreError::InvalidRecord);
    }
    // Membership is a set. Normalize formatting and order for exact reuse.
    members.sort_unstable_by(|a, b| a.skill_id.cmp(&b.skill_id));
    let encoded = serde_json::to_string(&members).map_err(|_| StoreError::InvalidRecord)?;
    if encoded.len() > MAX_SNAPSHOT_METADATA_BYTES {
        return Err(StoreError::InvalidRecord);
    }
    Ok(encoded)
}

/// Insert one attempt row inside a caller-owned transaction, so an attempt can be
/// committed together with the ranking event that owns it.
const PROVIDER_ATTEMPT_COLUMNS: &str = "INSERT INTO provider_attempts (
            attempt_id, owner_event_id, stage, request_fingerprint,
            admitted_at_unix_ms, sent_at_unix_ms, completed_at_unix_ms,
            status, input_tokens, output_tokens, http_status, error_kind
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)";

/// Writes one attempt row, and refuses to write over one that already exists.
///
/// The refusal is the point. An attempt id that already owns a row owns a record of what
/// that attempt cost, and a second unrelated write under the same id would replace it —
/// silently, with whatever the second caller happened to know. Recorded spend is not
/// overwritable, so this stays a plain insert and a duplicate is an error the caller has
/// to deal with. Advancing an attempt through its own lifecycle is a different operation
/// with its own function: see [`advance_provider_attempt`].
fn insert_provider_attempt(
    tx: &rusqlite::Transaction<'_>,
    attempt: &NewProviderAttempt,
) -> Result<(), StoreError> {
    tx.execute(
        PROVIDER_ATTEMPT_COLUMNS,
        params![
            attempt.attempt_id,
            attempt.owner_event_id,
            attempt.stage.as_str(),
            attempt.request_fingerprint,
            attempt.admitted_at_unix_ms as i64,
            attempt.sent_at_unix_ms.map(|t| t as i64),
            attempt.completed_at_unix_ms.map(|t| t as i64),
            attempt.status.as_str(),
            attempt.input_tokens.map(|t| t as i64),
            attempt.output_tokens.map(|t| t as i64),
            attempt.http_status.map(|s| s as i64),
            attempt.error_kind,
        ],
    )?;
    Ok(())
}

/// Writes one attempt row, or advances the row this same attempt already owns.
///
/// Distinct from [`insert_provider_attempt`], and the distinction is about who is writing.
/// A duplicate write under an existing attempt id is refused there, because it would
/// replace a record of real spend with a stranger's guess at it. This function is for the
/// one writer entitled to update a row: the invocation that created it, carrying the same
/// attempt through its own lifecycle. An attempt is recorded when it is admitted, again
/// when its request reaches the wire, and again when it settles, so that a process killed
/// in between still leaves behind what was true at the time — and those three writes have
/// to land on one row rather than collide.
///
/// Each of those writes knows strictly more than the one before it, which is why `status`
/// comes from the newest. Every other column is coalesced, so a write that happens not to
/// carry a fact — a settlement that observed no usage, say — cannot erase one already
/// recorded. Callers derive the key from an invocation token, but the storage boundary
/// also checks the immutable owner, stage, request and admission time before updating.
/// A conflicting key must roll back the enclosing event transaction, not alter another
/// attempt's recorded cost.
fn advance_provider_attempt(
    tx: &rusqlite::Transaction<'_>,
    attempt: &NewProviderAttempt,
) -> Result<(), StoreError> {
    let rows = tx.execute(
        &format!(
            "{PROVIDER_ATTEMPT_COLUMNS}
        ON CONFLICT(attempt_id) DO UPDATE SET
            status = excluded.status,
            sent_at_unix_ms = coalesce(excluded.sent_at_unix_ms, sent_at_unix_ms),
            completed_at_unix_ms = coalesce(excluded.completed_at_unix_ms, completed_at_unix_ms),
            input_tokens = coalesce(excluded.input_tokens, input_tokens),
            output_tokens = coalesce(excluded.output_tokens, output_tokens),
            http_status = coalesce(excluded.http_status, http_status),
            error_kind = coalesce(excluded.error_kind, error_kind)
        WHERE provider_attempts.owner_event_id = excluded.owner_event_id
          AND provider_attempts.stage = excluded.stage
          AND provider_attempts.request_fingerprint = excluded.request_fingerprint
          AND provider_attempts.admitted_at_unix_ms = excluded.admitted_at_unix_ms"
        ),
        params![
            attempt.attempt_id,
            attempt.owner_event_id,
            attempt.stage.as_str(),
            attempt.request_fingerprint,
            attempt.admitted_at_unix_ms as i64,
            attempt.sent_at_unix_ms.map(|t| t as i64),
            attempt.completed_at_unix_ms.map(|t| t as i64),
            attempt.status.as_str(),
            attempt.input_tokens.map(|t| t as i64),
            attempt.output_tokens.map(|t| t as i64),
            attempt.http_status.map(|s| s as i64),
            attempt.error_kind,
        ],
    )?;
    if rows == 0 {
        return Err(StoreError::RecordConflict);
    }
    Ok(())
}

fn insert_snapshot(
    tx: &rusqlite::Transaction<'_>,
    snapshot: &NewRosterSnapshot,
    members_json: &str,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO roster_snapshots (
            snapshot_id, workspace_root, adapter, total_candidates,
            eligible_candidates, membership_coverage, members_json, created_at_unix_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ON CONFLICT(snapshot_id) DO NOTHING",
        params![
            snapshot.snapshot_id,
            snapshot.workspace_root,
            snapshot.adapter,
            snapshot.total_candidates as i64,
            snapshot.eligible_candidates as i64,
            snapshot.membership_coverage.as_str(),
            members_json,
            snapshot.created_at_unix_ms as i64
        ],
    )?;
    let stored = read_snapshot(tx, &snapshot.snapshot_id)?.ok_or(StoreError::Corrupt)?;
    if stored.workspace_root != snapshot.workspace_root
        || stored.adapter != snapshot.adapter
        || stored.total_candidates != snapshot.total_candidates
        || stored.eligible_candidates != snapshot.eligible_candidates
        || stored.membership_coverage != snapshot.membership_coverage
        || validated_snapshot(&stored)? != members_json
    {
        return Err(StoreError::RecordConflict);
    }
    Ok(())
}

fn read_snapshot(
    tx: &rusqlite::Transaction<'_>,
    snapshot_id: &str,
) -> Result<Option<NewRosterSnapshot>, StoreError> {
    Ok(tx
        .query_row(
            "SELECT workspace_root, adapter, total_candidates, eligible_candidates,
                membership_coverage, members_json, created_at_unix_ms
         FROM roster_snapshots WHERE snapshot_id = ?1",
            [snapshot_id],
            |row| {
                Ok(NewRosterSnapshot {
                    snapshot_id: snapshot_id.to_owned(),
                    workspace_root: row.get(0)?,
                    adapter: row.get(1)?,
                    total_candidates: u64::try_from(row.get::<_, i64>(2)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    eligible_candidates: u64::try_from(row.get::<_, i64>(3)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    membership_coverage: MembershipCoverage::parse_str(&row.get::<_, String>(4)?)
                        .ok_or(rusqlite::Error::InvalidQuery)?,
                    members_json: row.get(5)?,
                    created_at_unix_ms: u64::try_from(row.get::<_, i64>(6)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            },
        )
        .optional()?)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum DecisionKind {
    Ranked,
    Explicit,
    Abstain,
    Unavailable,
}

impl DecisionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ranked => "ranked",
            Self::Explicit => "explicit",
            Self::Abstain => "abstain",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for DecisionKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ranked" => Ok(Self::Ranked),
            "explicit" => Ok(Self::Explicit),
            "abstain" => Ok(Self::Abstain),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err(()),
        }
    }
}

/// How far a recommendation got toward the agent that asked for it.
///
/// Only two of these have a producer in v1, and a reader that counts the others
/// is counting nothing (sr-ect5):
///
/// - `Prepared` is what `try_record_ledger` writes for every recorded ranking.
/// - `Emitted` is what `record_emission` advances to once bytes were written.
/// - `Generated` is what an invocation writes about itself before it sends anything,
///   so that a process killed while waiting on the provider is not simply absent
///   from the ledger. A completing invocation updates the row to `Prepared`; a row
///   left in `Generated` past the invocation deadline died in flight, and its cost
///   is unknown rather than zero. Attribution ignores it: only `emitted` and
///   `acknowledged` rows can be attributed an observed load.
/// - `Acknowledged` has no writer either, because nothing available establishes
///   delivery. `record_acknowledgment` exists and is tested, and it requires a
///   `verified_delivery_key`: evidence that the harness received the advisory.
///   Claude's `UserPromptSubmit` protocol returns no such confirmation.
///
/// **An observed load must not be used to synthesize `Acknowledged`.** It is
/// tempting, because load attribution already links an observation to an emission,
/// but a load is consistent with delivery and also with the agent reaching for that
/// skill on its own; treating it as confirmation would turn correlational evidence
/// into a delivery claim. A reporting consumer should render the two producerless
/// states as not recorded rather than as a measured zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ExposureState {
    Generated,
    Prepared,
    Emitted,
    Acknowledged,
    Unknown,
}

impl ExposureState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::Prepared => "prepared",
            Self::Emitted => "emitted",
            Self::Acknowledged => "acknowledged",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for ExposureState {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "generated" => Ok(Self::Generated),
            "prepared" => Ok(Self::Prepared),
            "emitted" => Ok(Self::Emitted),
            "acknowledged" => Ok(Self::Acknowledged),
            "unknown" => Ok(Self::Unknown),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NewRankingEvent {
    pub event_id: String,
    pub verified_delivery_key: Option<String>,
    pub workspace_root: String,
    pub session_id: String,
    pub agent_branch: String,
    pub mode_channel: String,
    pub policy_version: String,
    pub schema_version: u32,
    pub decision: DecisionKind,
    pub reason: String,
    pub exposure_state: ExposureState,
    pub elapsed_ms: u64,
    pub created_at_unix_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub snapshot_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CandidateStage {
    Wide,
    Rerank,
}

impl CandidateStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wide => "wide",
            Self::Rerank => "rerank",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for CandidateStage {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "wide" => Ok(Self::Wide),
            "rerank" => Ok(Self::Rerank),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NewRankingCandidate {
    pub event_id: String,
    pub stage: CandidateStage,
    pub skill_id: String,
    pub skill_version: String,
    pub raw_probability: Option<f64>,
    pub normalized_probability: Option<f64>,
    pub fit_score: Option<f64>,
    pub rank_score: Option<f64>,
    pub rank_position: Option<u32>,
    pub excluded: bool,
    pub exclusion_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AttemptStatus {
    #[default]
    Admitted,
    Sent,
    Completed,
    Failed,
    Unknown,
}

impl AttemptStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Sent => "sent",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for AttemptStatus {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "admitted" => Ok(Self::Admitted),
            "sent" => Ok(Self::Sent),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "unknown" => Ok(Self::Unknown),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewProviderAttempt {
    pub attempt_id: String,
    pub owner_event_id: String,
    pub stage: CandidateStage,
    pub request_fingerprint: String,
    pub admitted_at_unix_ms: u64,
    pub sent_at_unix_ms: Option<u64>,
    pub completed_at_unix_ms: Option<u64>,
    pub status: AttemptStatus,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub http_status: Option<u16>,
    pub error_kind: Option<String>,
}

impl NewProviderAttempt {
    /// The row for one attempt this invocation owned.
    ///
    /// `entry_wall_clock_unix_ms` is the wall-clock reading that corresponds to
    /// process entry, taken once by the caller. Attempt times are monotonic
    /// milliseconds since entry, so converting them against a single reading keeps
    /// one attempt's admitted, sent and settled times on the same timeline; a
    /// separate wall-clock sample per transition would not.
    ///
    /// A follower that reuses an owner's cached response admits no attempt and so
    /// has no row to write here: only the owner incurs a uniquely identified
    /// attempt, and ledger absence cannot invent free historical service.
    ///
    /// Returns `None` for a breaker probe or an evaluation batch attempt. This
    /// table's stage domain is the two ranking stages, and filing a probe as a
    /// wide request would attribute its cost to a ranking the user never made.
    /// Those attempts are accounted for in the invocation's cost receipt; giving
    /// them a durable home is separate work.
    pub fn from_provenance(
        owner_event_id: impl Into<String>,
        invocation_token: &str,
        request_fingerprint: impl Into<String>,
        entry_wall_clock_unix_ms: u64,
        provenance: &AttemptProvenance,
    ) -> Option<Self> {
        let stage = match provenance.stage {
            RankingStage::Wide => CandidateStage::Wide,
            RankingStage::Rerank => CandidateStage::Rerank,
            RankingStage::Probe | RankingStage::Evaluation => return None,
        };
        let wall = |at: MonotonicMillis| entry_wall_clock_unix_ms.saturating_add(at.as_millis());
        let failure = provenance.failure;
        let status = match provenance.outcome {
            AttemptOutcome::Admitted => AttemptStatus::Admitted,
            AttemptOutcome::Sent => AttemptStatus::Sent,
            AttemptOutcome::Completed => AttemptStatus::Completed,
            // A discard never reached the wire and a determinate transport failure
            // is established, so both are failures. An indeterminate ending is not:
            // recording it as failed would assert that the provider did no work on
            // this request's behalf, which is exactly what is unknown.
            AttemptOutcome::Failed | AttemptOutcome::Discarded => {
                if failure.is_none_or(AttemptFailure::is_determinate) {
                    AttemptStatus::Failed
                } else {
                    AttemptStatus::Unknown
                }
            }
        };
        let owner_event_id = owner_event_id.into();
        Some(Self {
            attempt_id: attempt_row_key(&owner_event_id, invocation_token, &provenance.attempt_id),
            owner_event_id,
            stage,
            request_fingerprint: request_fingerprint.into(),
            admitted_at_unix_ms: wall(provenance.admitted_at),
            sent_at_unix_ms: provenance.sent_at.map(wall),
            completed_at_unix_ms: provenance.settled_at.map(wall),
            status,
            // Absent usage stays absent. An attempt that returned no validated
            // usage did not thereby cost zero tokens.
            input_tokens: provenance.usage.map(|usage| usage.input_tokens),
            output_tokens: provenance.usage.map(|usage| usage.output_tokens),
            // The column admits 100..=599 only, so an out-of-range status keeps the
            // kind and drops the number instead of failing the whole write.
            http_status: failure
                .and_then(AttemptFailure::http_status)
                .filter(|status| (100..=599).contains(status)),
            error_kind: failure.map(|failure| failure.kind().to_owned()),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceState {
    Attempted,
    Loaded,
    Censored,
}

impl EvidenceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Attempted => "attempted",
            Self::Loaded => "loaded",
            Self::Censored => "censored",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for EvidenceState {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "attempted" => Ok(Self::Attempted),
            "loaded" => Ok(Self::Loaded),
            "censored" => Ok(Self::Censored),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewObservation {
    pub observation_id: String,
    pub source_event_key: String,
    pub workspace_root: String,
    pub session_id: String,
    pub agent_branch: String,
    pub attributed_event_id: Option<String>,
    pub skill_id: String,
    pub evidence_state: EvidenceState,
    pub observed_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JudgmentLabel {
    Useful,
    Harmful,
    Neutral,
}

impl JudgmentLabel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Useful => "useful",
            Self::Harmful => "harmful",
            Self::Neutral => "neutral",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for JudgmentLabel {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "useful" => Ok(Self::Useful),
            "harmful" => Ok(Self::Harmful),
            "neutral" => Ok(Self::Neutral),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NewJudgment {
    pub judgment_id: String,
    pub attributed_event_id: String,
    pub skill_id: String,
    pub label: JudgmentLabel,
    pub label_version: u32,
    pub provenance: String,
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ProposalStatus {
    Unresolved,
    HistoricallyAbsent,
    Rejected,
    Adopted,
}

impl ProposalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unresolved => "unresolved",
            Self::HistoricallyAbsent => "historically_absent",
            Self::Rejected => "rejected",
            Self::Adopted => "adopted",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for ProposalStatus {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unresolved" => Ok(Self::Unresolved),
            "historically_absent" => Ok(Self::HistoricallyAbsent),
            "rejected" => Ok(Self::Rejected),
            "adopted" => Ok(Self::Adopted),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NewFeedbackProposal {
    pub proposal_id: String,
    pub workspace_root: String,
    pub session_id: String,
    pub suggested_skill_reference: String,
    pub status: ProposalStatus,
    pub notes: Option<String>,
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatasetSplit {
    Train,
    Validation,
    Holdout,
}

impl DatasetSplit {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Train => "train",
            Self::Validation => "validation",
            Self::Holdout => "holdout",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for DatasetSplit {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "train" => Ok(Self::Train),
            "validation" => Ok(Self::Validation),
            "holdout" => Ok(Self::Holdout),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderAttemptOutcome<'a> {
    pub status: AttemptStatus,
    pub tokens: Option<(u64, u64)>,
    pub http_status: Option<u16>,
    pub error_kind: Option<&'a str>,
    pub completed_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewCalibration {
    pub calibration_id: String,
    pub dataset_fingerprint: String,
    pub split: DatasetSplit,
    pub objective: String,
    pub coefficients_json: String,
    pub evaluation_report_id: String,
    pub created_at_unix_ms: u64,
}

// -----------------------------------------------------------------------------
// Feedback & Paired Correction Models
// -----------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PairedCorrectionRequest {
    pub event_id: String,
    pub original_skill_id: String,
    pub alternative_skill_id: String,
    pub reason_code: Option<String>,
    pub provenance: Option<String>,
    pub expected_version: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SingleFeedbackRequest {
    pub event_id: String,
    pub skill_id: String,
    pub verdict: JudgmentLabel,
    pub reason_code: Option<String>,
    pub provenance: Option<String>,
    pub expected_version: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FeedbackRequest {
    Paired(PairedCorrectionRequest),
    Single(SingleFeedbackRequest),
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum FeedbackOutcome {
    PairedCorrection {
        event_id: String,
        original_skill_id: String,
        alternative_skill_id: String,
        group_id: String,
        original_judgment_id: String,
        alternative_judgment_id: String,
        data_generation: u64,
    },
    ProspectiveProposal {
        event_id: String,
        original_skill_id: String,
        alternative_skill_id: String,
        proposal_id: String,
        reason: String,
    },
    SingleJudgment {
        event_id: String,
        skill_id: String,
        verdict: JudgmentLabel,
        judgment_id: String,
        data_generation: u64,
    },
}

#[derive(Debug, Eq, PartialEq)]
pub enum FeedbackError {
    Store(StoreError),
    EventNotFound(String),
    MissingSnapshot,
    IneligibleAlternative {
        skill_id: String,
        reason: Option<String>,
    },
    OriginalSkillNotFound(String),
    IdenticalSkills,
    InvalidSkillId(String),
    RevisionConflict {
        expected: u32,
        actual: u32,
    },
    StaleStamp,
}

impl fmt::Display for FeedbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(err) => write!(f, "store error: {err}"),
            Self::EventNotFound(id) => write!(f, "ranking event '{id}' not found in ledger"),
            Self::MissingSnapshot => write!(
                f,
                "historical roster snapshot is missing or incomplete; cannot verify historical alternative eligibility"
            ),
            Self::IneligibleAlternative { skill_id, reason } => {
                if let Some(r) = reason {
                    write!(
                        f,
                        "alternative skill '{skill_id}' was historically ineligible ({r})"
                    )
                } else {
                    write!(
                        f,
                        "alternative skill '{skill_id}' was historically ineligible"
                    )
                }
            }
            Self::OriginalSkillNotFound(id) => {
                write!(
                    f,
                    "original skill '{id}' not found in historical event context"
                )
            }
            Self::IdenticalSkills => {
                write!(f, "original and alternative skill IDs must be distinct")
            }
            Self::InvalidSkillId(id) => write!(f, "invalid skill ID '{id}'"),
            Self::RevisionConflict { expected, actual } => {
                write!(
                    f,
                    "judgment revision conflict: expected version {expected}, found {actual}"
                )
            }
            Self::StaleStamp => write!(
                f,
                "ledger store stamp generation is stale; concurrent modification detected"
            ),
        }
    }
}

impl std::error::Error for FeedbackError {}

impl From<StoreError> for FeedbackError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

fn generate_random_hex(byte_count: usize) -> String {
    use std::io::Read;
    let mut bytes = vec![0u8; byte_count];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    } else {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let hash = blake3::hash(format!("{nanos}:{pid}").as_bytes());
        bytes.copy_from_slice(&hash.as_bytes()[..byte_count.min(32)]);
    }
    let mut s = String::with_capacity(byte_count * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

// -----------------------------------------------------------------------------
// Directory Handling
// -----------------------------------------------------------------------------

fn io_error(error: Errno) -> StoreError {
    match error {
        Errno::ENOENT => StoreError::Missing,
        Errno::ELOOP | Errno::ENOTDIR => StoreError::UnsafePath,
        Errno::EACCES | Errno::EPERM => StoreError::Permissions,
        _ => StoreError::Io,
    }
}

fn owned_regular(stat: &FileStat, uid: u32) -> Result<(), StoreError> {
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFREG || stat.st_nlink != 1 {
        return Err(StoreError::UnsafePath);
    }
    if stat.st_uid != uid || stat.st_mode & 0o7777 != 0o600 {
        return Err(StoreError::Permissions);
    }
    Ok(())
}

fn trusted_ancestor(stat: &FileStat, uid: u32, leaf: bool) -> Result<(), StoreError> {
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFDIR {
        return Err(StoreError::UnsafePath);
    }
    if leaf {
        if stat.st_uid != uid || stat.st_mode & 0o7777 != 0o700 {
            return Err(StoreError::Permissions);
        }
    } else if (stat.st_uid != 0 && stat.st_uid != uid)
        || (stat.st_mode & 0o022 != 0 && !(stat.st_uid == 0 && stat.st_mode & 0o1000 != 0))
    {
        return Err(StoreError::Permissions);
    }
    Ok(())
}

fn recording_capacity(bytes: u64, available: u128) -> Result<(), StoreError> {
    if bytes > LEDGER_QUOTA_BYTES - LEDGER_MAINTENANCE_RESERVE_BYTES - LEDGER_MUTATION_RESERVE_BYTES
    {
        return Err(StoreError::Quota);
    }
    if available < u128::from(LEDGER_MAINTENANCE_RESERVE_BYTES + LEDGER_MUTATION_RESERVE_BYTES) {
        return Err(StoreError::InsufficientSpace);
    }
    Ok(())
}

pub(crate) struct PrivateLedgerDirectory {
    pub path: PathBuf,
    handle: File,
    identity: DirectoryIdentity,
    simulated_available_disk_bytes: Option<u64>,
}

impl PrivateLedgerDirectory {
    pub fn open(
        path: PathBuf,
        create: bool,
        clock: EntryClock,
        cx: &Cx,
    ) -> Result<Self, StoreError> {
        let path = storage_path(path);
        if !path.is_absolute() || path.as_os_str().len() > 4096 {
            return Err(StoreError::UnsafePath);
        }
        let components: Vec<_> = path.components().collect();
        if components.len() < 2
            || components.len() > 128
            || components
                .iter()
                .skip(1)
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(StoreError::UnsafePath);
        }
        let uid = nix::unistd::geteuid().as_raw();
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let mut handle = open(Path::new("/"), flags, Mode::empty()).map_err(io_error)?;
        trusted_ancestor(&fstat(&handle).map_err(io_error)?, uid, false)?;
        for (i, component) in components.iter().enumerate().skip(1) {
            check_work(clock, cx)?;
            let name = component.as_os_str();
            let opened = match openat(&handle, name, flags, Mode::empty()) {
                Err(Errno::ENOENT) if create => {
                    if !local_filesystem(&fstatfs(&handle).map_err(io_error)?) {
                        return Err(StoreError::UnsupportedFilesystem);
                    }
                    match mkdirat(&handle, name, Mode::from_bits_truncate(0o700)) {
                        Ok(()) | Err(Errno::EEXIST) => {}
                        Err(error) => return Err(io_error(error)),
                    }
                    openat(&handle, name, flags, Mode::empty()).map_err(io_error)?
                }
                result => result.map_err(io_error)?,
            };
            trusted_ancestor(
                &fstat(&opened).map_err(io_error)?,
                uid,
                i + 1 == components.len(),
            )?;
            handle = opened;
        }
        let stat = fstat(&handle).map_err(io_error)?;
        let directory = Self {
            path,
            handle: handle.into(),
            identity: (stat.st_dev, stat.st_ino),
            simulated_available_disk_bytes: None,
        };
        if create {
            directory.admit_space()?;
        } else {
            directory.inspect_files()?;
        }
        Ok(directory)
    }

    pub fn set_simulated_available_disk_bytes(&mut self, bytes: Option<u64>) {
        self.simulated_available_disk_bytes = bytes;
    }

    pub fn available_disk_bytes(&self) -> Result<u64, StoreError> {
        if let Some(simulated) = self.simulated_available_disk_bytes {
            return Ok(simulated);
        }
        let stat = fstatvfs(&self.handle).map_err(io_error)?;
        let available = u128::from(stat.blocks_available()) * u128::from(stat.fragment_size());
        Ok(u64::try_from(available).unwrap_or(u64::MAX))
    }

    pub fn capacity_report(&self) -> Result<LedgerCapacityReport, StoreError> {
        let occupied_bytes = self.inspect_files()?;
        let available_disk_bytes = self.available_disk_bytes()?;

        let recording_ceiling_bytes = LEDGER_QUOTA_BYTES
            .saturating_sub(LEDGER_MAINTENANCE_RESERVE_BYTES)
            .saturating_sub(LEDGER_MUTATION_RESERVE_BYTES);

        let usable_recording_bytes = recording_ceiling_bytes.saturating_sub(occupied_bytes);

        let is_recording_admitted = occupied_bytes <= recording_ceiling_bytes
            && (available_disk_bytes as u128)
                >= u128::from(LEDGER_MAINTENANCE_RESERVE_BYTES + LEDGER_MUTATION_RESERVE_BYTES);

        Ok(LedgerCapacityReport {
            total_quota_bytes: LEDGER_QUOTA_BYTES,
            maintenance_reserve_bytes: LEDGER_MAINTENANCE_RESERVE_BYTES,
            mutation_reserve_bytes: LEDGER_MUTATION_RESERVE_BYTES,
            recording_ceiling_bytes,
            occupied_bytes,
            usable_recording_bytes,
            available_disk_bytes,
            is_recording_admitted,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.path.join(LEDGER_FILE)
    }

    fn verify_database_file(
        &self,
        file: &File,
        clock: EntryClock,
        cx: &Cx,
    ) -> Result<(), StoreError> {
        self.revalidate(clock, cx)?;
        let held = fstat(file).map_err(io_error)?;
        let current = match fstatat(&self.handle, LEDGER_FILE, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::ENOENT) => return Err(StoreError::StoreReplaced),
            Err(error) => return Err(io_error(error)),
        };
        if (held.st_dev, held.st_ino) != (current.st_dev, current.st_ino) {
            return Err(StoreError::StoreReplaced);
        }
        let uid = nix::unistd::geteuid().as_raw();
        owned_regular(&held, uid)?;
        owned_regular(&current, uid)
    }

    pub fn revalidate(&self, clock: EntryClock, cx: &Cx) -> Result<(), StoreError> {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let mut fd = open(Path::new("/"), flags, Mode::empty()).map_err(io_error)?;
        let uid = nix::unistd::geteuid().as_raw();
        let components: Vec<_> = self.path.components().collect();
        for (i, component) in components.iter().enumerate().skip(1) {
            check_work(clock, cx)?;
            fd = openat(&fd, component.as_os_str(), flags, Mode::empty()).map_err(io_error)?;
            trusted_ancestor(
                &fstat(&fd).map_err(io_error)?,
                uid,
                i + 1 == components.len(),
            )?;
        }
        let stat = fstat(&fd).map_err(io_error)?;
        if (stat.st_dev, stat.st_ino) != self.identity {
            return Err(StoreError::StoreReplaced);
        }
        Ok(())
    }

    pub fn inspect_files(&self) -> Result<u64, StoreError> {
        let uid = nix::unistd::geteuid().as_raw();
        let mut bytes = 0_u64;
        let mut checked_names = BTreeSet::new();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let name = format!("{LEDGER_FILE}{suffix}");
            match fstatat(&self.handle, name.as_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => {
                    owned_regular(&stat, uid)?;
                    bytes = bytes
                        .checked_add(u64::try_from(stat.st_size).map_err(|_| StoreError::Quota)?)
                        .ok_or(StoreError::Quota)?;
                    checked_names.insert(name);
                }
                Err(Errno::ENOENT) => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        if let Ok(entries) = std::fs::read_dir(&self.path) {
            for entry in entries {
                let entry = entry.map_err(|_| StoreError::Io)?;
                let file_name = entry.file_name();
                let name_str = file_name.to_string_lossy();
                if checked_names.contains(name_str.as_ref()) {
                    continue;
                }
                match fstatat(
                    &self.handle,
                    name_str.as_ref(),
                    AtFlags::AT_SYMLINK_NOFOLLOW,
                ) {
                    Ok(stat) => {
                        owned_regular(&stat, uid)?;
                        bytes = bytes
                            .checked_add(
                                u64::try_from(stat.st_size).map_err(|_| StoreError::Quota)?,
                            )
                            .ok_or(StoreError::Quota)?;
                    }
                    Err(Errno::ENOENT) => {}
                    Err(error) => return Err(io_error(error)),
                }
            }
        }
        if bytes > LEDGER_QUOTA_BYTES {
            return Err(StoreError::Quota);
        }
        Ok(bytes)
    }

    pub fn admit_space(&self) -> Result<(), StoreError> {
        if !local_filesystem(&fstatfs(&self.handle).map_err(io_error)?) {
            return Err(StoreError::UnsupportedFilesystem);
        }
        let bytes = self.inspect_files()?;
        let available = if let Some(simulated) = self.simulated_available_disk_bytes {
            u128::from(simulated)
        } else {
            let stat = fstatvfs(&self.handle).map_err(io_error)?;
            u128::from(stat.blocks_available()) * u128::from(stat.fragment_size())
        };
        recording_capacity(bytes, available)
    }

    pub fn open_database_file(
        &self,
        create: bool,
        clock: EntryClock,
        cx: &Cx,
    ) -> Result<File, StoreError> {
        self.revalidate(clock, cx)?;
        if create {
            self.admit_space()?;
        } else {
            self.inspect_files()?;
        }
        let flags = OFlag::O_RDWR | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
        let fd = match openat(&self.handle, LEDGER_FILE, flags, Mode::empty()) {
            Err(Errno::ENOENT) if create => match openat(
                &self.handle,
                LEDGER_FILE,
                flags | OFlag::O_CREAT | OFlag::O_EXCL,
                Mode::from_bits_truncate(0o600),
            ) {
                Err(Errno::EEXIST) => {
                    openat(&self.handle, LEDGER_FILE, flags, Mode::empty()).map_err(io_error)?
                }
                result => result.map_err(io_error)?,
            },
            result => result.map_err(io_error)?,
        };
        let file: File = fd.into();
        let stat = fstat(&file).map_err(io_error)?;
        owned_regular(&stat, nix::unistd::geteuid().as_raw())?;
        Ok(file)
    }
}

// -----------------------------------------------------------------------------
// Database Connection Configuration & Verification
// -----------------------------------------------------------------------------

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
    // A repository handle may outlive the invocation that opened it. Every
    // operation must replace both the busy limit and the progress context.
    let child = cx.clone();
    connection.progress_handler(
        100,
        Some(move || child.is_cancel_requested() || clock.admit_new_work().is_err()),
    )?;
    Ok(())
}

fn configure(connection: &Connection, clock: EntryClock, cx: &Cx) -> Result<(), StoreError> {
    refresh_busy_limit(connection, clock, cx)?;
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
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "wal_autocheckpoint", 1000)?;
    connection.pragma_update(None, "journal_size_limit", 4 * 1024 * 1024)?;
    let page_size: i64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    if (512..=65536).contains(&page_size) {
        let max_pages =
            i64::try_from(LEDGER_RECORDING_CEILING_BYTES / (page_size as u64)).unwrap_or(i64::MAX);
        connection.pragma_update(None, "max_page_count", max_pages)?;
    }
    Ok(())
}

fn configure_read_only(
    connection: &Connection,
    clock: EntryClock,
    cx: &Cx,
) -> Result<(), StoreError> {
    check_work(clock, cx)?;
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, 2 * 1024 * 1024)?;
    connection.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024)?;
    connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    connection.set_limit(Limit::SQLITE_LIMIT_WORKER_THREADS, 0)?;
    refresh_busy_limit(connection, clock, cx)?;
    Ok(())
}

fn read_stamp(connection: &Connection) -> Result<LedgerStamp, StoreError> {
    let app: i64 = connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if app != LEDGER_APPLICATION_ID {
        return Err(StoreError::WrongStore);
    }
    let objects: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*'",
        [],
        |row| row.get(0),
    )?;
    if objects < TABLES.len() as i64 {
        return Err(StoreError::IncompatibleSchema);
    }
    for (name, _) in TABLES {
        let exists: bool = connection
            .query_row(
                "SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1",
                [name],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !exists {
            return Err(StoreError::IncompatibleSchema);
        }
    }
    let (incarnation, schema_gen, data_gen, schema): (Vec<u8>, i64, i64, String) = connection.query_row(
        "SELECT incarnation, schema_generation, data_generation, schema_id FROM store_meta WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if schema_gen < 1 || data_gen < 1 || !schema.starts_with("sr-ledger-") {
        return Err(StoreError::IncompatibleSchema);
    }
    let incarnation: [u8; 16] = incarnation
        .try_into()
        .map_err(|_| StoreError::IncompatibleSchema)?;
    Ok(LedgerStamp {
        incarnation,
        schema_generation: schema_gen as u64,
        data_generation: data_gen as u64,
    })
}

fn check_stamp(connection: &Connection, expected: LedgerStamp) -> Result<(), StoreError> {
    let actual = read_stamp(connection)?;
    if actual.incarnation != expected.incarnation {
        return Err(StoreError::StoreReplaced);
    }
    if actual.schema_generation != expected.schema_generation {
        return Err(StoreError::IncompatibleSchema);
    }
    if actual.data_generation != expected.data_generation {
        return Err(StoreError::StaleGeneration);
    }
    Ok(())
}

fn initialize(
    connection: &mut Connection,
    clock: EntryClock,
    cx: &Cx,
) -> Result<InitStatus, StoreError> {
    configure(connection, clock, cx)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_work(clock, cx)?;
    let current_ver: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current_ver >= i64::from(LEDGER_SCHEMA_VERSION) {
        read_stamp(&tx)?;
        return Ok(InitStatus::AlreadyCurrent);
    }
    let app: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let objects: i64 = tx.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*'",
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
        "INSERT INTO store_meta VALUES (1, randomblob(16), 1, 1, ?1)",
        [LEDGER_SCHEMA_ID],
    )?;
    tx.execute(
        "INSERT INTO schema_migrations VALUES (1, ?1, unixepoch('subsec') * 1000)",
        ["v1-initial-schema"],
    )?;
    tx.pragma_update(None, "application_id", LEDGER_APPLICATION_ID)?;
    tx.pragma_update(None, "user_version", LEDGER_SCHEMA_VERSION)?;
    refresh_busy_limit(&tx, clock, cx)?;
    tx.commit()?;
    Ok(InitStatus::Created)
}

pub fn default_ledger_directory() -> Result<PathBuf, StoreError> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            return Ok(p.join("sr"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home);
        if p.is_absolute() {
            return Ok(p.join(".local").join("share").join("sr"));
        }
    }
    Err(StoreError::UnsafePath)
}

fn open_blocking(
    clock: EntryClock,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
) -> Result<LedgerOpen, StoreError> {
    check_work(clock, cx)?;
    let engine = linked_engine()?;
    let path = match location {
        LedgerLocation::Platform => default_ledger_directory()?,
        LedgerLocation::Directory(dir) => dir,
    };
    let create = access == LedgerAccess::Initialize;
    let directory = match PrivateLedgerDirectory::open(path, create, clock, cx) {
        Err(StoreError::Missing) if !create => return Ok(LedgerOpen::Missing),
        result => result?,
    };
    let file = match directory.open_database_file(create, clock, cx) {
        Err(StoreError::Missing) if !create => return Ok(LedgerOpen::Missing),
        result => result?,
    };
    let database_path = directory.database_path();
    directory.verify_database_file(&file, clock, cx)?;

    if access == LedgerAccess::Initialize {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_CREATE;
        let mut connection = Connection::open_with_flags(&database_path, flags)?;
        directory.verify_database_file(&file, clock, cx)?;
        initialize(&mut connection, clock, cx)?;
        let stamp = read_stamp(&connection)?;
        directory.verify_database_file(&file, clock, cx)?;
        return Ok(LedgerOpen::Ready(Box::new(LedgerStore {
            connection,
            directory,
            file,
            stamp,
            engine,
            read_only: false,
        })));
    }

    // ExistingOnly or Migrate: probe first with SQLITE_OPEN_READ_ONLY
    let probe_flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let probe = match Connection::open_with_flags(&database_path, probe_flags) {
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.extended_code == rusqlite::ffi::SQLITE_CANTOPEN =>
        {
            return Ok(LedgerOpen::Missing);
        }
        result => result?,
    };
    let app: i64 = probe.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if app != LEDGER_APPLICATION_ID {
        return Err(StoreError::WrongStore);
    }
    let user_ver: i64 = probe.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let stamp = read_stamp(&probe)?;
    drop(probe);

    check_work(clock, cx)?;
    directory.verify_database_file(&file, clock, cx)?;

    if user_ver > i64::from(LEDGER_TARGET_SCHEMA_VERSION) {
        // Unsupported newer schema: open read-only where safe
        let read_flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = Connection::open_with_flags(&database_path, read_flags)?;
        configure_read_only(&connection, clock, cx)?;
        directory.verify_database_file(&file, clock, cx)?;
        return Ok(LedgerOpen::ReadOnly(Box::new(LedgerStore {
            connection,
            directory,
            file,
            stamp,
            engine,
            read_only: true,
        })));
    }

    if user_ver < i64::from(LEDGER_SCHEMA_VERSION) && access != LedgerAccess::Migrate {
        return Err(StoreError::IncompatibleSchema);
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(&database_path, flags)?;
    configure(&connection, clock, cx)?;
    let stamp = read_stamp(&connection)?;
    directory.verify_database_file(&file, clock, cx)?;
    Ok(LedgerOpen::Ready(Box::new(LedgerStore {
        connection,
        directory,
        file,
        stamp,
        engine,
        read_only: false,
    })))
}

pub fn open_ledger(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
) -> Result<LedgerOpen, StoreError> {
    if access == LedgerAccess::Disabled {
        return Ok(LedgerOpen::Disabled);
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

pub fn init_ledger(
    invocation: &ProcessInvocation,
    cx: &Cx,
    location: LedgerLocation,
) -> Result<InitReport, StoreError> {
    let clock = invocation.clock();
    let child = cx.clone();
    run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let path = match location {
                LedgerLocation::Platform => default_ledger_directory()?,
                LedgerLocation::Directory(dir) => dir,
            };
            let directory = PrivateLedgerDirectory::open(path, true, clock, &child)?;
            let file = directory.open_database_file(true, clock, &child)?;
            let database_path = directory.database_path();
            let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_CREATE;
            let mut connection = Connection::open_with_flags(&database_path, flags)?;
            directory.verify_database_file(&file, clock, &child)?;
            let status = initialize(&mut connection, clock, &child)?;
            let stamp = read_stamp(&connection)?;
            let user_ver: i64 =
                connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
            directory.verify_database_file(&file, clock, &child)?;
            Ok(InitReport {
                status,
                schema_version: user_ver as u32,
                database_path,
                incarnation: stamp
                    .incarnation
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            })
        },
    )
    .map_err(StoreError::Runtime)?
    .value
}

pub fn ledger_status(
    invocation: &ProcessInvocation,
    cx: &Cx,
    location: LedgerLocation,
) -> Result<LedgerStatusReport, StoreError> {
    let clock = invocation.clock();
    let child = cx.clone();
    run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let path = match location {
                LedgerLocation::Platform => default_ledger_directory()?,
                LedgerLocation::Directory(dir) => dir,
            };
            let database_path = path.join(LEDGER_FILE);
            match open_blocking(
                clock,
                &child,
                LedgerAccess::ExistingOnly,
                LedgerLocation::Directory(path),
            ) {
                Ok(LedgerOpen::Missing) => Ok(LedgerStatusReport {
                    status: "missing",
                    schema_version: None,
                    target_version: LEDGER_TARGET_SCHEMA_VERSION,
                    upgrade_available: None,
                    schema_generation: None,
                    data_generation: None,
                    is_read_only: false,
                    database_path,
                    cleanup_debt: None,
                }),
                Ok(LedgerOpen::ReadOnly(store)) => {
                    let stamp = store.stamp();
                    let ver = store.schema_version().ok();
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    let cleanup_debt = store.cleanup_debt(now_ms).ok();
                    Ok(LedgerStatusReport {
                        status: "read_only",
                        schema_version: ver,
                        target_version: LEDGER_TARGET_SCHEMA_VERSION,
                        upgrade_available: None,
                        schema_generation: Some(stamp.schema_generation),
                        data_generation: Some(stamp.data_generation),
                        is_read_only: true,
                        database_path,
                        cleanup_debt,
                    })
                }
                Ok(LedgerOpen::Ready(store)) => {
                    let stamp = store.stamp();
                    let ver = store.schema_version().ok();
                    let upgrade_available = match ver {
                        Some(v) if v < LEDGER_TARGET_SCHEMA_VERSION => {
                            Some(LEDGER_TARGET_SCHEMA_VERSION)
                        }
                        _ => None,
                    };
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    let cleanup_debt = store.cleanup_debt(now_ms).ok();
                    Ok(LedgerStatusReport {
                        status: "ready",
                        schema_version: ver,
                        target_version: LEDGER_TARGET_SCHEMA_VERSION,
                        upgrade_available,
                        schema_generation: Some(stamp.schema_generation),
                        data_generation: Some(stamp.data_generation),
                        is_read_only: false,
                        database_path,
                        cleanup_debt,
                    })
                }
                Ok(LedgerOpen::Disabled) => Ok(LedgerStatusReport {
                    status: "disabled",
                    schema_version: None,
                    target_version: LEDGER_TARGET_SCHEMA_VERSION,
                    upgrade_available: None,
                    schema_generation: None,
                    data_generation: None,
                    is_read_only: false,
                    database_path,
                    cleanup_debt: None,
                }),
                Err(StoreError::WrongStore) => Ok(LedgerStatusReport {
                    status: "wrong_store",
                    schema_version: None,
                    target_version: LEDGER_TARGET_SCHEMA_VERSION,
                    upgrade_available: None,
                    schema_generation: None,
                    data_generation: None,
                    is_read_only: false,
                    database_path,
                    cleanup_debt: None,
                }),
                Err(StoreError::IncompatibleSchema) => {
                    let (ver, stamp) = Connection::open_with_flags(
                        &database_path,
                        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )
                    .ok()
                    .map(|conn| {
                        let v: Option<u32> = conn
                            .pragma_query_value(None, "user_version", |r| r.get(0))
                            .ok()
                            .map(|x: i64| x as u32);
                        let s = read_stamp(&conn).ok();
                        (v, s)
                    })
                    .unwrap_or((None, None));
                    Ok(LedgerStatusReport {
                        status: "needs_migration",
                        schema_version: ver,
                        target_version: LEDGER_TARGET_SCHEMA_VERSION,
                        upgrade_available: Some(LEDGER_TARGET_SCHEMA_VERSION),
                        schema_generation: stamp.as_ref().map(|s| s.schema_generation),
                        data_generation: stamp.as_ref().map(|s| s.data_generation),
                        is_read_only: false,
                        database_path,
                        cleanup_debt: None,
                    })
                }
                Err(e) => Err(e),
            }
        },
    )
    .map_err(StoreError::Runtime)?
    .value
}

pub fn ledger_stats(
    invocation: &ProcessInvocation,
    cx: &Cx,
    location: LedgerLocation,
    since_unix_ms: i64,
    by_skill: bool,
) -> Result<StatsValueReport, StoreError> {
    let clock = invocation.clock();
    let child = cx.clone();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or(StoreError::InvalidRecord)?;
    run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let open = open_blocking(clock, &child, LedgerAccess::ExistingOnly, location)?;
            let store = match open {
                LedgerOpen::Ready(s) => s,
                LedgerOpen::ReadOnly(s) => s,
                LedgerOpen::Missing => return Err(StoreError::Missing),
                LedgerOpen::Disabled => return Err(StoreError::Uninitialized),
            };
            store.query_value_stats(since_unix_ms, now_ms, by_skill)
        },
    )
    .map_err(StoreError::Runtime)?
    .value
}

// -----------------------------------------------------------------------------
// LedgerStore Implementations
// -----------------------------------------------------------------------------

impl LedgerStore {
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        let v: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        Ok(v as u32)
    }

    pub fn migrate_preview(&self) -> Result<MigrationPreview, MigrationError> {
        let current = self.schema_version()?;
        let target = LEDGER_TARGET_SCHEMA_VERSION;
        if current > target {
            return Err(MigrationError::UnsupportedNewerVersion {
                current_version: current,
                target_version: target,
            });
        }
        let pending: Vec<PendingMigration> = AVAILABLE_MIGRATIONS
            .iter()
            .filter(|m| m.from_version >= current && m.to_version <= target)
            .map(|m| PendingMigration {
                version: m.to_version,
                name: m.name.to_string(),
                description: m.description.to_string(),
                checksum: m.checksum.to_string(),
            })
            .collect();

        let required_headroom_bytes =
            self.worst_case_maintenance_bytes(MaintenanceKind::Migration)?;

        Ok(MigrationPreview {
            current_version: current,
            target_version: target,
            pending_migrations: pending,
            required_headroom_bytes,
        })
    }

    pub fn migrate_apply(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
    ) -> Result<MigrationReport, MigrationError> {
        if self.read_only {
            return Err(MigrationError::Store(StoreError::Permissions));
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let current = self.schema_version()?;
        let target = LEDGER_TARGET_SCHEMA_VERSION;
        if current > target {
            return Err(MigrationError::UnsupportedNewerVersion {
                current_version: current,
                target_version: target,
            });
        }
        if current == target {
            return Err(MigrationError::AlreadyUpToDate);
        }

        // 1. Preflight maintenance space before any modification
        let _preflight = self.preflight_maintenance(MaintenanceKind::Migration)?;

        // 2. Create atomic, recoverable WAL-inclusive backup using VACUUM INTO
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let backup_file_name = format!(
            "{}.pre_migration_v{}_to_v{}_{}.bak",
            LEDGER_FILE, current, target, now_ms
        );
        let backup_path = self.directory.path.join(&backup_file_name);
        let backup_bytes = self.backup_to(&backup_path, clock, cx)?;

        // 3. Begin transaction
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_work(clock, cx)?;

        let current_stamp = read_stamp(&tx)?;
        if current_stamp.schema_generation != self.stamp.schema_generation {
            return Err(MigrationError::Store(StoreError::StaleGeneration));
        }

        let mut applied = Vec::new();
        let pending: Vec<&MigrationStep> = AVAILABLE_MIGRATIONS
            .iter()
            .filter(|m| m.from_version >= current && m.to_version <= target)
            .collect();

        for step in pending {
            check_work(clock, cx)?;
            tx.execute_batch(step.ddl)?;
            tx.execute(
                "INSERT INTO schema_migrations (version, checksum, applied_at_unix_ms) VALUES (?1, ?2, ?3)",
                rusqlite::params![step.to_version, step.checksum, now_ms as i64],
            )?;
            applied.push(step.name.to_string());
        }

        tx.execute(
            "UPDATE store_meta SET schema_generation = schema_generation + 1 WHERE singleton = 1",
            [],
        )?;
        tx.pragma_update(None, "user_version", target)?;
        refresh_busy_limit(&tx, clock, cx)?;
        tx.commit()?;

        self.stamp = read_stamp(&self.connection)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;

        Ok(MigrationReport {
            from_version: current,
            to_version: target,
            applied_migrations: applied,
            backup_path,
            backup_bytes,
        })
    }

    pub fn stamp(&self) -> LedgerStamp {
        self.stamp
    }

    pub fn engine(&self) -> &EngineIdentity {
        &self.engine
    }

    pub fn database_path(&self) -> PathBuf {
        self.directory.database_path()
    }

    pub fn set_simulated_available_disk_bytes(&mut self, bytes: Option<u64>) {
        self.directory.set_simulated_available_disk_bytes(bytes);
    }

    /// Exposes usable recording capacity separately from the total quota cap.
    pub fn capacity_report(&self) -> Result<LedgerCapacityReport, StoreError> {
        self.directory.capacity_report()
    }

    /// Estimates worst-case additional bytes needed on disk for the given maintenance operation.
    pub fn worst_case_maintenance_bytes(&self, kind: MaintenanceKind) -> Result<u64, StoreError> {
        let uid = nix::unistd::geteuid().as_raw();
        let mut db_and_wal_bytes = 0_u64;
        for suffix in ["", "-wal", "-journal"] {
            let name = format!("{LEDGER_FILE}{suffix}");
            match fstatat(
                &self.directory.handle,
                name.as_str(),
                AtFlags::AT_SYMLINK_NOFOLLOW,
            ) {
                Ok(stat) => {
                    owned_regular(&stat, uid)?;
                    db_and_wal_bytes = db_and_wal_bytes
                        .checked_add(u64::try_from(stat.st_size).map_err(|_| StoreError::Quota)?)
                        .ok_or(StoreError::Quota)?;
                }
                Err(Errno::ENOENT) => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        let min_temp_bytes = 1024 * 1024; // 1 MiB
        let estimated = match kind {
            MaintenanceKind::Vacuum => db_and_wal_bytes
                .checked_add(min_temp_bytes)
                .ok_or(StoreError::Quota)?,
            MaintenanceKind::Backup => db_and_wal_bytes
                .checked_add(64 * 1024)
                .ok_or(StoreError::Quota)?,
            MaintenanceKind::Migration => db_and_wal_bytes
                .checked_add(2 * 1024 * 1024)
                .ok_or(StoreError::Quota)?,
            MaintenanceKind::Prune | MaintenanceKind::Clear => 2 * 1024 * 1024,
        };
        Ok(estimated)
    }

    /// Preflights maintenance operation worst-case additional bytes against both quota and free disk space.
    pub fn preflight_maintenance(
        &self,
        kind: MaintenanceKind,
    ) -> Result<MaintenancePreflight, MaintenanceError> {
        let required_bytes = self.worst_case_maintenance_bytes(kind)?;
        self.preflight_maintenance_with_bytes(kind, required_bytes)
    }

    /// Preflights maintenance with explicit required additional bytes against quota and free disk space.
    pub fn preflight_maintenance_with_bytes(
        &self,
        kind: MaintenanceKind,
        required_additional_bytes: u64,
    ) -> Result<MaintenancePreflight, MaintenanceError> {
        let occupied_bytes = self.directory.inspect_files()?;
        let available_disk_bytes = self.directory.available_disk_bytes()?;

        let projected_total_bytes = occupied_bytes
            .checked_add(required_additional_bytes)
            .ok_or(MaintenanceError::QuotaExceeded {
                occupied_bytes,
                required_additional_bytes,
                quota_bytes: LEDGER_QUOTA_BYTES,
                recovery_step: "projected storage size overflows integer bounds; prune historical records",
            })?;

        if projected_total_bytes > LEDGER_QUOTA_BYTES {
            let recovery_step = match kind {
                MaintenanceKind::Vacuum => {
                    "database size is too large to vacuum within the 256 MiB quota; prune expired events before compacting"
                }
                MaintenanceKind::Backup => {
                    "backup would exceed the 256 MiB directory quota; target a separate external backup destination or prune events"
                }
                MaintenanceKind::Migration => {
                    "database size leaves insufficient headroom for pre-migration backup within quota; prune old records before migrating"
                }
                MaintenanceKind::Prune => {
                    "prune operation exceeds remaining quota headroom; run checkpoint_truncate to reclaim WAL space"
                }
                MaintenanceKind::Clear => {
                    "clear operation exceeds remaining quota headroom; run checkpoint_truncate to reclaim WAL space"
                }
            };
            return Err(MaintenanceError::QuotaExceeded {
                occupied_bytes,
                required_additional_bytes,
                quota_bytes: LEDGER_QUOTA_BYTES,
                recovery_step,
            });
        }

        if (available_disk_bytes as u128) < (required_additional_bytes as u128) {
            return Err(MaintenanceError::InsufficientDiskSpace {
                available_bytes: available_disk_bytes,
                required_additional_bytes,
                recovery_step: "free disk space on the filesystem hosting the ledger directory before retrying maintenance",
            });
        }

        Ok(MaintenancePreflight {
            kind,
            current_occupied_bytes: occupied_bytes,
            required_additional_bytes,
            projected_total_bytes,
            available_disk_bytes,
        })
    }

    /// Checkpoints and truncates the WAL file to bound WAL growth and reclaim space.
    pub fn checkpoint_truncate(&mut self, clock: EntryClock, cx: &Cx) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        refresh_busy_limit(&self.connection, clock, cx)?;
        self.connection
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        Ok(())
    }

    /// Runs VACUUM after preflighting headroom against quota and free disk space.
    pub fn vacuum(&mut self, clock: EntryClock, cx: &Cx) -> Result<(), MaintenanceError> {
        if self.read_only {
            return Err(MaintenanceError::Store(StoreError::Permissions));
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        let _preflight = self.preflight_maintenance(MaintenanceKind::Vacuum)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        self.connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1)?;
        let vacuum_res = self.connection.execute_batch("VACUUM;");
        let _ = self.connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
        vacuum_res?;

        self.file = self.directory.open_database_file(false, clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        Ok(())
    }

    /// Creates an atomic, recoverable, WAL-inclusive backup using VACUUM INTO after preflighting headroom.
    pub fn backup_to(
        &mut self,
        dest_path: &Path,
        clock: EntryClock,
        cx: &Cx,
    ) -> Result<u64, MaintenanceError> {
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        let _preflight = self.preflight_maintenance(MaintenanceKind::Backup)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let path_str = dest_path.to_str().ok_or(StoreError::UnsafePath)?;
        self.connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1)?;
        let backup_res = self.connection.execute("VACUUM INTO ?1", [path_str]);
        let _ = self.connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
        backup_res?;

        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dest_path, std::fs::Permissions::from_mode(0o600));

        self.directory.verify_database_file(&self.file, clock, cx)?;
        let meta = std::fs::metadata(dest_path).map_err(|_| StoreError::Io)?;
        Ok(meta.len())
    }

    /// Queries cleanup debt for events and records older than the logical retention policy (30 days).
    pub fn cleanup_debt(&self, as_of_unix_ms: i64) -> Result<CleanupDebt, StoreError> {
        let cutoff = as_of_unix_ms.saturating_sub(DEFAULT_RETENTION_MS);
        let expired_events: i64 = self.connection.query_row(
            "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms < ?1",
            [cutoff],
            |r| r.get(0),
        )?;
        let unreferenced_snapshots: i64 = self.connection.query_row(
            "SELECT count(*) FROM roster_snapshots
             WHERE snapshot_id NOT IN (SELECT snapshot_id FROM ranking_events WHERE snapshot_id IS NOT NULL)",
            [],
            |r| r.get(0),
        )?;
        let expired_observations: i64 = self.connection.query_row(
            "SELECT count(*) FROM observations
             WHERE attributed_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)
                OR (attributed_event_id IS NULL AND observed_at_unix_ms < ?1)",
            [cutoff],
            |r| r.get(0),
        )?;
        let expired_judgments: i64 = self.connection.query_row(
            "SELECT count(*) FROM judgments
             WHERE attributed_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)
                OR created_at_unix_ms < ?1",
            [cutoff],
            |r| r.get(0),
        )?;

        let freelist_count: i64 = self
            .connection
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let page_size: i64 = self
            .connection
            .query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let freelist_bytes = (freelist_count.max(0) as u64).saturating_mul(page_size.max(0) as u64);

        let has_debt = expired_events > 0
            || unreferenced_snapshots > 0
            || expired_observations > 0
            || expired_judgments > 0
            || freelist_bytes > 1024 * 1024;

        Ok(CleanupDebt {
            expired_events: expired_events as u64,
            unreferenced_snapshots: unreferenced_snapshots as u64,
            expired_observations: expired_observations as u64,
            expired_judgments: expired_judgments as u64,
            freelist_bytes,
            has_debt,
        })
    }

    /// Queries statistics over active records, excluding records older than 30 days relative to versioned as_of timestamp.
    pub fn query_retained_stats(&self, as_of_unix_ms: i64) -> Result<RetainedStats, StoreError> {
        let cutoff = as_of_unix_ms.saturating_sub(DEFAULT_RETENTION_MS);
        let total_events: i64 =
            self.connection
                .query_row("SELECT count(*) FROM ranking_events", [], |row| row.get(0))?;
        let active_events: i64 = self.connection.query_row(
            "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms >= ?1",
            [cutoff],
            |row| row.get(0),
        )?;
        let expired_events = total_events.saturating_sub(active_events);
        let active_judgments: i64 = self.connection.query_row(
            "SELECT count(*) FROM judgments j JOIN ranking_events e ON j.attributed_event_id = e.event_id WHERE e.created_at_unix_ms >= ?1",
            [cutoff],
            |row| row.get(0),
        )?;
        let active_observations: i64 = self.connection.query_row(
            "SELECT count(*) FROM observations WHERE (attributed_event_id IS NULL AND observed_at_unix_ms >= ?1) OR attributed_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms >= ?1)",
            [cutoff],
            |row| row.get(0),
        )?;
        let active_snapshots: i64 = self.connection.query_row(
            "SELECT count(DISTINCT snapshot_id) FROM ranking_events WHERE created_at_unix_ms >= ?1 AND snapshot_id IS NOT NULL",
            [cutoff],
            |row| row.get(0),
        )?;
        Ok(RetainedStats {
            as_of_unix_ms,
            cutoff_unix_ms: cutoff,
            total_events: total_events as u64,
            active_events: active_events as u64,
            expired_events: expired_events as u64,
            active_judgments: active_judgments as u64,
            active_observations: active_observations as u64,
            active_snapshots: active_snapshots as u64,
        })
    }

    /// Queries compact value report metrics over ranking events, observations, judgments, and provider attempts.
    pub fn query_value_stats(
        &self,
        since_unix_ms: i64,
        as_of_unix_ms: i64,
        by_skill: bool,
    ) -> Result<StatsValueReport, StoreError> {
        // One snapshot for the whole report. This builds every figure from more than thirty
        // separate statements, and without a read transaction each one sees whatever the database
        // held at the moment it ran. A hook firing once per turn while somebody runs `sr stats` is
        // the case this product is built for, not an edge case, and under that load the parts stop
        // agreeing: measured across 130 concurrent runs, one report printed a channel breakdown
        // summing to 495 against a total of 494, and a cache hit rate of 100.2% — a number whose
        // existence is proof that its numerator and denominator came from different instants.
        //
        // A deferred read transaction on this connection gives every subsequent read the same
        // snapshot until it ends. It takes no write lock, so it cannot block the hook that is
        // writing; it only stops this reader from straddling that writer's commits. Dropping it
        // rolls back, which for a read-only report is the same as committing.
        let snapshot = self
            .connection
            .unchecked_transaction()
            .map_err(StoreError::from)?;
        let retained = self.query_retained_stats(as_of_unix_ms)?;

        // 1. Turns
        let total_evaluated: i64 = self.connection.query_row(
            "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms >= ?1 AND created_at_unix_ms <= ?2",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;
        let emitted_suggestions: i64 = self.connection.query_row(
            "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms >= ?1 AND created_at_unix_ms <= ?2 AND decision IN ('ranked', 'explicit') AND exposure_state IN ('emitted', 'acknowledged')",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;
        let valid_abstentions: i64 = self.connection.query_row(
            "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms >= ?1 AND created_at_unix_ms <= ?2 AND decision = 'abstain'",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;
        let muted_or_suppressed: i64 = self.connection.query_row(
            "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms >= ?1 AND created_at_unix_ms <= ?2 AND decision = 'ranked' AND exposure_state IN ('generated', 'prepared')",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;
        // A row that never came back also carries `decision = 'unavailable'`, because
        // at the moment it was written nothing had been decided. Counting it here
        // would report an invocation that is still running, or one that was killed
        // outright, as a failure of the provider.
        let operational_failures: i64 = self.connection.query_row(
            &format!(
                "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms >= ?1 \
                 AND created_at_unix_ms <= ?2 AND decision = 'unavailable' AND NOT {UNFINISHED_ROW_SQL}"
            ),
            params![since_unix_ms, as_of_unix_ms, IN_FLIGHT_REASON],
            |r| r.get(0),
        )?;
        let in_flight_or_killed: i64 = self.connection.query_row(
            &format!(
                "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms >= ?1 \
                 AND created_at_unix_ms <= ?2 AND {UNFINISHED_ROW_SQL}"
            ),
            params![since_unix_ms, as_of_unix_ms, IN_FLIGHT_REASON],
            |r| r.get(0),
        )?;
        let explicit_requirements: i64 = self.connection.query_row(
            "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms >= ?1 AND created_at_unix_ms <= ?2 AND decision = 'explicit'",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;

        // Channels
        let mut channel_stmt = self.connection.prepare(&format!(
            "SELECT mode_channel, count(*), \
             sum(CASE WHEN decision IN ('ranked', 'explicit') AND exposure_state IN ('emitted', 'acknowledged') THEN 1 ELSE 0 END), \
             sum(CASE WHEN decision = 'abstain' THEN 1 ELSE 0 END), \
             sum(CASE WHEN decision = 'ranked' AND exposure_state IN ('generated', 'prepared') THEN 1 ELSE 0 END), \
             sum(CASE WHEN decision = 'unavailable' AND NOT {UNFINISHED_ROW_SQL} THEN 1 ELSE 0 END), \
             sum(CASE WHEN {UNFINISHED_ROW_SQL} THEN 1 ELSE 0 END) \
             FROM ranking_events WHERE created_at_unix_ms >= ?1 AND created_at_unix_ms <= ?2 \
             GROUP BY mode_channel ORDER BY mode_channel"
        ))?;
        let channel_rows = channel_stmt.query_map(
            params![since_unix_ms, as_of_unix_ms, IN_FLIGHT_REASON],
            |row| {
                Ok(ChannelStats {
                    channel: row.get(0)?,
                    evaluated_turns: row.get::<_, i64>(1)? as u64,
                    emitted: row.get::<_, i64>(2)? as u64,
                    abstain: row.get::<_, i64>(3)? as u64,
                    muted: row.get::<_, i64>(4)? as u64,
                    unavailable: row.get::<_, i64>(5)? as u64,
                    in_flight_or_killed: row.get::<_, i64>(6)? as u64,
                })
            },
        )?;
        let mut by_channel = Vec::new();
        for ch in channel_rows {
            by_channel.push(ch?);
        }

        let mut cause_stmt = self.connection.prepare(&format!(
            "SELECT reason, count(*) FROM ranking_events WHERE created_at_unix_ms >= ?1 \
             AND created_at_unix_ms <= ?2 AND decision = 'unavailable' AND NOT {UNFINISHED_ROW_SQL} \
             GROUP BY reason ORDER BY count(*) DESC, reason"
        ))?;
        let cause_rows = cause_stmt.query_map(
            params![since_unix_ms, as_of_unix_ms, IN_FLIGHT_REASON],
            |row| {
                Ok(FailureCause {
                    reason: row.get(0)?,
                    count: row.get::<_, i64>(1)? as u64,
                })
            },
        )?;
        let mut failure_causes = Vec::new();
        for cause in cause_rows {
            failure_causes.push(cause?);
        }

        let turns = TurnMetrics {
            total_evaluated: total_evaluated as u64,
            emitted_suggestions: emitted_suggestions as u64,
            valid_abstentions: valid_abstentions as u64,
            muted_or_suppressed: muted_or_suppressed as u64,
            operational_failures: operational_failures as u64,
            explicit_requirements: explicit_requirements as u64,
            in_flight_or_killed: in_flight_or_killed as u64,
            by_channel,
            failure_causes,
        };

        // 2. Latency. An unfinished row carries `elapsed_ms = 0` as a placeholder, not
        // as a measurement of a very fast turn. Admitting it would pull the mean, the
        // median and p95 toward zero and make every killed invocation look like the
        // fastest thing the product ever did, so the sample is drawn from turns that
        // finished and the summary reports how many it left out.
        let mut lat_stmt = self.connection.prepare(&format!(
            "SELECT elapsed_ms FROM ranking_events WHERE created_at_unix_ms >= ?1 \
             AND created_at_unix_ms <= ?2 AND NOT {UNFINISHED_ROW_SQL} ORDER BY elapsed_ms ASC"
        ))?;
        let lat_rows = lat_stmt.query_map(
            params![since_unix_ms, as_of_unix_ms, IN_FLIGHT_REASON],
            |row| row.get::<_, i64>(0),
        )?;
        let mut latencies: Vec<u64> = Vec::new();
        let mut sum_lat: u64 = 0;
        for l in lat_rows {
            let val = l? as u64;
            sum_lat = sum_lat.saturating_add(val);
            latencies.push(val);
        }
        let excluded_unfinished = in_flight_or_killed as u64;
        let latency = if latencies.is_empty() {
            LatencySummary {
                mean_ms: 0,
                median_ms: 0,
                p95_ms: 0,
                min_ms: 0,
                max_ms: 0,
                excluded_unfinished,
            }
        } else {
            let len = latencies.len();
            let p95_idx = ((len as f64 * 0.95).ceil() as usize)
                .saturating_sub(1)
                .min(len - 1);
            LatencySummary {
                mean_ms: sum_lat / (len as u64),
                median_ms: latencies[len / 2],
                p95_ms: latencies[p95_idx],
                min_ms: latencies[0],
                max_ms: latencies[len - 1],
                excluded_unfinished,
            }
        };

        // 3. Observations
        let total_obs: i64 = self.connection.query_row(
            "SELECT count(*) FROM observations WHERE observed_at_unix_ms >= ?1 AND observed_at_unix_ms <= ?2",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;
        let observed_loads: i64 = self.connection.query_row(
            "SELECT count(*) FROM observations WHERE observed_at_unix_ms >= ?1 AND observed_at_unix_ms <= ?2 AND evidence_state = 'loaded'",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;
        let attempted_loads: i64 = self.connection.query_row(
            "SELECT count(*) FROM observations WHERE observed_at_unix_ms >= ?1 AND observed_at_unix_ms <= ?2 AND evidence_state = 'attempted'",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;
        let censored_obs: i64 = self.connection.query_row(
            "SELECT count(*) FROM observations WHERE observed_at_unix_ms >= ?1 AND observed_at_unix_ms <= ?2 AND evidence_state = 'censored'",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;
        let attributed_loads: i64 = self.connection.query_row(
            "SELECT count(*) FROM observations WHERE observed_at_unix_ms >= ?1 AND observed_at_unix_ms <= ?2 AND evidence_state = 'loaded' AND attributed_event_id IS NOT NULL",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;
        let unattributed_loads = (observed_loads as u64).saturating_sub(attributed_loads as u64);
        let observation_coverage = if observed_loads > 0 {
            Some(attributed_loads as f64 / observed_loads as f64)
        } else {
            None
        };
        let suggestion_adoption_rate = if emitted_suggestions > 0 {
            Some(attributed_loads as f64 / emitted_suggestions as f64)
        } else {
            None
        };
        let observations = ObservationMetrics {
            total_observations: total_obs as u64,
            observed_loads: observed_loads as u64,
            attempted_loads: attempted_loads as u64,
            censored_observations: censored_obs as u64,
            attributed_loads: attributed_loads as u64,
            unattributed_loads,
            observation_coverage,
            suggestion_adoption_rate,
            caveat: "Observed adoption is not task success; a recommendation can cause its own observed load without proving counterfactual benefit.".into(),
        };

        // 4. Judgments
        let (total_judgments, useful, harmful, neutral, distinct_judged): (
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = self.connection.query_row(
            "SELECT count(*), \
             coalesce(sum(CASE WHEN j.label = 'useful' THEN 1 ELSE 0 END), 0), \
             coalesce(sum(CASE WHEN j.label = 'harmful' THEN 1 ELSE 0 END), 0), \
             coalesce(sum(CASE WHEN j.label = 'neutral' THEN 1 ELSE 0 END), 0), \
             count(DISTINCT j.attributed_event_id) \
             FROM judgments j JOIN ranking_events e ON j.attributed_event_id = e.event_id \
             WHERE e.created_at_unix_ms >= ?1 AND e.created_at_unix_ms <= ?2",
            [since_unix_ms, as_of_unix_ms],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let label_coverage_rate = if emitted_suggestions > 0 {
            Some(distinct_judged as f64 / emitted_suggestions as f64)
        } else {
            None
        };
        let useful_ratio_in_judged = if total_judgments > 0 {
            Some(useful as f64 / total_judgments as f64)
        } else {
            None
        };
        let judgments = JudgmentMetrics {
            total_judgments: total_judgments as u64,
            useful: useful as u64,
            harmful: harmful as u64,
            neutral: neutral as u64,
            distinct_judged_events: distinct_judged as u64,
            label_coverage_rate,
            useful_ratio_in_judged,
        };

        // 5. Provider Attempts & Cost
        let (tot_attempts, comp_attempts, fail_attempts, unk_attempts, in_tokens, out_tokens, unk_usage_attempts): (i64, i64, i64, i64, i64, i64, i64) = self.connection.query_row(
            "SELECT count(*), \
             coalesce(sum(CASE WHEN a.status = 'completed' THEN 1 ELSE 0 END), 0), \
             coalesce(sum(CASE WHEN a.status = 'failed' THEN 1 ELSE 0 END), 0), \
             coalesce(sum(CASE WHEN a.status NOT IN ('completed', 'failed') THEN 1 ELSE 0 END), 0), \
             coalesce(sum(coalesce(a.input_tokens, 0)), 0), \
             coalesce(sum(coalesce(a.output_tokens, 0)), 0), \
             coalesce(sum(CASE WHEN a.input_tokens IS NULL OR a.output_tokens IS NULL OR a.status != 'completed' THEN 1 ELSE 0 END), 0) \
             FROM provider_attempts a JOIN ranking_events e ON a.owner_event_id = e.event_id \
             WHERE e.created_at_unix_ms >= ?1 AND e.created_at_unix_ms <= ?2",
            [since_unix_ms, as_of_unix_ms],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )?;
        // "No attempt rows" alone is not evidence of cache reuse, and two different kinds of turn
        // satisfy it without the cache having served anything. A turn killed before it admitted an
        // attempt has none because it died; a turn that abstained, or resolved an explicit
        // requirement, before the provider was ever consulted has none because it never needed one.
        // Counting either as a hit invents reuse: three turns, one paid and two abstaining, used to
        // report a 67% hit rate against a cache that served nothing.
        //
        // So a served turn is one that produced a ranking without paying for it, and the rate is
        // over rankings rather than over all turns — otherwise abstentions dilute a figure they
        // cannot contribute to. This deliberately undercounts one case it cannot see: an abstention
        // reached by reading a reused response is real cache reuse and is not counted here, because
        // the ledger records no marker distinguishing it from one decided locally. Under-reporting
        // reuse is the safe direction; inventing it is not. Counting it properly needs the
        // pipeline's own cache flags persisted on the event row, which is a schema change.
        let cache_served_events: i64 = self.connection.query_row(
            &format!(
                "SELECT count(*) FROM ranking_events e WHERE e.created_at_unix_ms >= ?1 \
                 AND e.created_at_unix_ms <= ?2 AND e.decision = 'ranked' AND NOT {UNFINISHED_ROW_SQL} \
                 AND NOT EXISTS (SELECT 1 FROM provider_attempts a WHERE a.owner_event_id = e.event_id)"
            ),
            params![since_unix_ms, as_of_unix_ms, IN_FLIGHT_REASON],
            |r| r.get(0),
        )?;
        let finished_rankings: i64 = self.connection.query_row(
            &format!(
                "SELECT count(*) FROM ranking_events e WHERE e.created_at_unix_ms >= ?1 \
                 AND e.created_at_unix_ms <= ?2 AND e.decision = 'ranked' AND NOT {UNFINISHED_ROW_SQL}"
            ),
            params![since_unix_ms, as_of_unix_ms, IN_FLIGHT_REASON],
            |r| r.get(0),
        )?;
        let cache_hit_rate = if finished_rankings > 0 {
            Some(cache_served_events as f64 / finished_rankings as f64)
        } else {
            None
        };

        // Judged cohort attempts & tokens
        // Each attempt is counted once, against the set of judged events rather than
        // against the judgments themselves. Joining attempts to judgments directly
        // repeats every attempt once per label on its turn, so a turn labelled twice —
        // two reviewers, or a revised label — would report twice the tokens it spent
        // without a single extra token having been spent. What a turn cost is a
        // property of the turn, not of how many times anyone judged it.
        // The third column uses the same predicate as `unknown_usage_attempts` above, so the
        // cohort figure and the window figure mean the same thing by construction.
        let (judged_cohort_tokens, judged_cohort_attempts, judged_cohort_unknown_usage): (
            i64,
            i64,
            i64,
        ) = self.connection.query_row(
            "SELECT coalesce(sum(coalesce(a.input_tokens, 0) + coalesce(a.output_tokens, 0)), 0), count(*), \
             coalesce(sum(CASE WHEN a.input_tokens IS NULL OR a.output_tokens IS NULL OR a.status != 'completed' THEN 1 ELSE 0 END), 0) \
             FROM provider_attempts a WHERE a.owner_event_id IN ( \
               SELECT DISTINCT j.attributed_event_id FROM judgments j \
               JOIN ranking_events e ON j.attributed_event_id = e.event_id \
               WHERE e.created_at_unix_ms >= ?1 AND e.created_at_unix_ms <= ?2 \
             )",
            [since_unix_ms, as_of_unix_ms],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        // Judged turns that owned no attempt of their own: served from a response another turn
        // paid for. Counted because otherwise their effect on the ratio below is invisible.
        let judged_turns_served_from_cache: i64 = self.connection.query_row(
            "SELECT count(*) FROM ( \
               SELECT DISTINCT j.attributed_event_id AS event_id FROM judgments j \
               JOIN ranking_events e ON j.attributed_event_id = e.event_id \
               WHERE e.created_at_unix_ms >= ?1 AND e.created_at_unix_ms <= ?2 \
             ) judged WHERE NOT EXISTS ( \
               SELECT 1 FROM provider_attempts a WHERE a.owner_event_id = judged.event_id \
             )",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;

        // A useful *suggestion* is a turn somebody found useful, not a label. Three skills judged
        // useful on one emission are three labels and one suggestion, and the numerator already
        // counts a turn's attempts once per turn rather than once per label — that was the point of
        // aggregating against distinct judged events. Dividing turn-scoped attempts by a label count
        // measured two different things against each other, so one turn's single attempt divided by
        // three labels rounded to "0 attempts" printed beside a nonzero token figure. The rest of
        // the report already treats a suggestion as a turn: `label_coverage_rate` is distinct judged
        // events over emitted suggestions.
        let useful_suggestions: i64 = self.connection.query_row(
            "SELECT count(*) FROM ( \
               SELECT DISTINCT j.attributed_event_id FROM judgments j \
               JOIN ranking_events e ON j.attributed_event_id = e.event_id \
               WHERE e.created_at_unix_ms >= ?1 AND e.created_at_unix_ms <= ?2 AND j.label = 'useful' \
             )",
            [since_unix_ms, as_of_unix_ms],
            |r| r.get(0),
        )?;

        let cost_per_useful_suggestion = if useful_suggestions == 0 {
            "not estimable (0 useful labels in judged cohort)".to_string()
        } else if judged_cohort_attempts == 0 {
            // Every judged turn reused a response it did not pay for. Dividing by `useful`
            // here yields zero, and zero is a claim: it says a useful suggestion cost nothing,
            // when what happened is that the spend belongs to turns outside this cohort.
            // Attributing the owner's tokens to the follower instead would report the same
            // spend twice as soon as the owner is judged too, so the honest answer is to
            // decline the ratio and say why.
            format!(
                "not estimable (judged cohort owns no provider attempt; {judged_turns_served_from_cache} judged turn(s) reused a response paid for elsewhere)"
            )
        } else {
            // An attempt whose usage the provider never reported contributes nothing to the token
            // sum because nothing is known, not because nothing was spent. The figure is then a
            // lower bound, and a lower bound presented as a measurement is the same mistake as a
            // zero presented as one — so it says which it is.
            format!(
                "pricing configuration absent; {} attempts / {} tokens per useful suggestion across judged cohort{}{}",
                (judged_cohort_attempts as f64 / useful_suggestions as f64).round() as u64,
                (judged_cohort_tokens as f64 / useful_suggestions as f64).round() as u64,
                if judged_cohort_unknown_usage > 0 {
                    format!(
                        "; a lower bound, because {judged_cohort_unknown_usage} of those attempts reported no usage"
                    )
                } else {
                    String::new()
                },
                if judged_turns_served_from_cache > 0 {
                    format!(
                        "; excludes {judged_turns_served_from_cache} judged turn(s) that reused a response paid for elsewhere"
                    )
                } else {
                    String::new()
                }
            )
        };
        let tokens_per_useful_suggestion = if useful_suggestions > 0 && judged_cohort_attempts > 0 {
            Some(judged_cohort_tokens as f64 / useful_suggestions as f64)
        } else {
            None
        };

        let provider = ProviderMetrics {
            total_attempts: tot_attempts as u64,
            completed_attempts: comp_attempts as u64,
            failed_attempts: fail_attempts as u64,
            unknown_attempts: unk_attempts as u64,
            known_input_tokens: in_tokens as u64,
            known_output_tokens: out_tokens as u64,
            known_total_tokens: (in_tokens + out_tokens) as u64,
            unknown_usage_attempts: unk_usage_attempts as u64,
            cache_served_events: cache_served_events as u64,
            cache_hit_rate,
            cost_per_useful_suggestion,
            tokens_per_useful_suggestion,
            judged_turns_served_from_cache: judged_turns_served_from_cache as u64,
            judged_cohort_unknown_usage_attempts: judged_cohort_unknown_usage as u64,
        };

        // 6. By Skill (optional)
        let by_skill_data = if by_skill {
            let mut skill_set = std::collections::BTreeSet::new();
            {
                let mut stmt = self.connection.prepare(
                    "SELECT DISTINCT c.skill_id FROM ranking_candidates c JOIN ranking_events e ON c.event_id = e.event_id WHERE e.created_at_unix_ms >= ?1 AND e.created_at_unix_ms <= ?2",
                )?;
                let rows =
                    stmt.query_map([since_unix_ms, as_of_unix_ms], |r| r.get::<_, String>(0))?;
                for s in rows {
                    skill_set.insert(s?);
                }
            }
            {
                let mut stmt = self.connection.prepare(
                    "SELECT DISTINCT skill_id FROM observations WHERE observed_at_unix_ms >= ?1 AND observed_at_unix_ms <= ?2",
                )?;
                let rows =
                    stmt.query_map([since_unix_ms, as_of_unix_ms], |r| r.get::<_, String>(0))?;
                for s in rows {
                    skill_set.insert(s?);
                }
            }
            {
                let mut stmt = self.connection.prepare(
                    "SELECT DISTINCT j.skill_id FROM judgments j JOIN ranking_events e ON j.attributed_event_id = e.event_id WHERE e.created_at_unix_ms >= ?1 AND e.created_at_unix_ms <= ?2",
                )?;
                let rows =
                    stmt.query_map([since_unix_ms, as_of_unix_ms], |r| r.get::<_, String>(0))?;
                for s in rows {
                    skill_set.insert(s?);
                }
            }

            let mut skills = Vec::new();
            for skill_id in skill_set {
                let top1: i64 = self.connection.query_row(
                    "SELECT count(*) FROM ranking_candidates c JOIN ranking_events e ON c.event_id = e.event_id \
                     WHERE e.created_at_unix_ms >= ?1 AND e.created_at_unix_ms <= ?2 AND c.skill_id = ?3 \
                     AND c.stage = 'rerank' AND c.rank_position = 1 AND e.decision = 'ranked' AND e.exposure_state IN ('emitted', 'acknowledged')",
                    rusqlite::params![since_unix_ms, as_of_unix_ms, &skill_id],
                    |r| r.get(0),
                )?;
                let shortlist: i64 = self.connection.query_row(
                    "SELECT count(*) FROM ranking_candidates c JOIN ranking_events e ON c.event_id = e.event_id \
                     WHERE e.created_at_unix_ms >= ?1 AND e.created_at_unix_ms <= ?2 AND c.skill_id = ?3 AND c.stage = 'rerank'",
                    rusqlite::params![since_unix_ms, as_of_unix_ms, &skill_id],
                    |r| r.get(0),
                )?;
                let obs_loads: i64 = self.connection.query_row(
                    "SELECT count(*) FROM observations WHERE observed_at_unix_ms >= ?1 AND observed_at_unix_ms <= ?2 AND skill_id = ?3 AND evidence_state = 'loaded'",
                    rusqlite::params![since_unix_ms, as_of_unix_ms, &skill_id],
                    |r| r.get(0),
                )?;
                let attr_loads: i64 = self.connection.query_row(
                    "SELECT count(*) FROM observations WHERE observed_at_unix_ms >= ?1 AND observed_at_unix_ms <= ?2 AND skill_id = ?3 AND evidence_state = 'loaded' AND attributed_event_id IS NOT NULL",
                    rusqlite::params![since_unix_ms, as_of_unix_ms, &skill_id],
                    |r| r.get(0),
                )?;
                let (useful, harmful, neutral): (i64, i64, i64) = self.connection.query_row(
                    "SELECT coalesce(sum(CASE WHEN j.label = 'useful' THEN 1 ELSE 0 END), 0), \
                     coalesce(sum(CASE WHEN j.label = 'harmful' THEN 1 ELSE 0 END), 0), \
                     coalesce(sum(CASE WHEN j.label = 'neutral' THEN 1 ELSE 0 END), 0) \
                     FROM judgments j JOIN ranking_events e ON j.attributed_event_id = e.event_id \
                     WHERE e.created_at_unix_ms >= ?1 AND e.created_at_unix_ms <= ?2 AND j.skill_id = ?3",
                    rusqlite::params![since_unix_ms, as_of_unix_ms, &skill_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;

                skills.push(SkillStatSummary {
                    skill_id,
                    top1_recommendations: top1 as u64,
                    shortlist_appearances: shortlist as u64,
                    observed_loads: obs_loads as u64,
                    attributed_loads: attr_loads as u64,
                    judged_useful: useful as u64,
                    judged_harmful: harmful as u64,
                    judged_neutral: neutral as u64,
                });
            }
            skills.sort_by(|a, b| {
                b.top1_recommendations
                    .cmp(&a.top1_recommendations)
                    .then_with(|| b.shortlist_appearances.cmp(&a.shortlist_appearances))
                    .then_with(|| a.skill_id.cmp(&b.skill_id))
            });
            Some(skills)
        } else {
            None
        };

        // Released explicitly rather than by drop, so that a future edit moving work below this
        // point cannot silently read outside the snapshot the figures above were built from.
        snapshot.finish().map_err(StoreError::from)?;

        Ok(StatsValueReport {
            as_of_unix_ms,
            since_unix_ms,
            turns,
            latency,
            observations,
            judgments,
            provider,
            retained,
            by_skill: by_skill_data,
        })
    }

    /// Previews retention cleanup for records created before cutoff timestamp without mutating storage.
    pub fn prune_preview(&self, cutoff_unix_ms: i64) -> Result<PrunePreview, StoreError> {
        let events_to_prune: i64 = self.connection.query_row(
            "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms < ?1",
            [cutoff_unix_ms],
            |row| row.get(0),
        )?;
        let candidates_to_prune: i64 = self.connection.query_row(
            "SELECT count(*) FROM ranking_candidates WHERE event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
            [cutoff_unix_ms],
            |row| row.get(0),
        )?;
        let judgments_to_prune: i64 = self.connection.query_row(
            "SELECT count(*) FROM judgments WHERE attributed_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
            [cutoff_unix_ms],
            |row| row.get(0),
        )?;
        let observations_to_prune: i64 = self.connection.query_row(
            "SELECT count(*) FROM observations WHERE attributed_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1) OR (attributed_event_id IS NULL AND observed_at_unix_ms < ?1)",
            [cutoff_unix_ms],
            |row| row.get(0),
        )?;
        let provider_attempts_to_prune: i64 = self.connection.query_row(
            "SELECT count(*) FROM provider_attempts WHERE owner_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
            [cutoff_unix_ms],
            |row| row.get(0),
        )?;

        let shared_snapshots_preserved: i64 = self.connection.query_row(
            "SELECT count(DISTINCT snapshot_id) FROM ranking_events
             WHERE snapshot_id IS NOT NULL
               AND created_at_unix_ms >= ?1
               AND snapshot_id IN (SELECT snapshot_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
            [cutoff_unix_ms],
            |row| row.get(0),
        )?;

        let snapshots_to_prune: i64 = self.connection.query_row(
            "SELECT count(*) FROM roster_snapshots
             WHERE created_at_unix_ms < ?1
               AND snapshot_id NOT IN (
                   SELECT snapshot_id FROM ranking_events WHERE snapshot_id IS NOT NULL AND created_at_unix_ms >= ?1
               )",
            [cutoff_unix_ms],
            |row| row.get(0),
        )?;

        Ok(PrunePreview {
            cutoff_unix_ms,
            cutoff_iso: format_unix_ms(cutoff_unix_ms),
            events_to_prune: events_to_prune as u64,
            candidates_to_prune: candidates_to_prune as u64,
            observations_to_prune: observations_to_prune as u64,
            judgments_to_prune: judgments_to_prune as u64,
            provider_attempts_to_prune: provider_attempts_to_prune as u64,
            snapshots_to_prune: snapshots_to_prune as u64,
            shared_snapshots_preserved: shared_snapshots_preserved as u64,
            requires_apply: true,
        })
    }

    /// Prunes events created before cutoff timestamp and their dependent records after preflighting headroom.
    /// Advances data generation to invalidate derived priors and dependent decisions.
    pub fn prune_apply(
        &mut self,
        cutoff_unix_ms: i64,
        clock: EntryClock,
        cx: &Cx,
        expected_stamp: LedgerStamp,
    ) -> Result<PruneReport, MaintenanceError> {
        if self.read_only {
            return Err(MaintenanceError::Store(StoreError::Permissions));
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        let preflight = self.preflight_maintenance(MaintenanceKind::Prune)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        let preview = {
            let events_to_prune: i64 = tx.query_row(
                "SELECT count(*) FROM ranking_events WHERE created_at_unix_ms < ?1",
                [cutoff_unix_ms],
                |row| row.get(0),
            )?;
            let candidates_to_prune: i64 = tx.query_row(
                "SELECT count(*) FROM ranking_candidates WHERE event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
                [cutoff_unix_ms],
                |row| row.get(0),
            )?;
            let judgments_to_prune: i64 = tx.query_row(
                "SELECT count(*) FROM judgments WHERE attributed_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
                [cutoff_unix_ms],
                |row| row.get(0),
            )?;
            let observations_to_prune: i64 = tx.query_row(
                "SELECT count(*) FROM observations WHERE attributed_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1) OR (attributed_event_id IS NULL AND observed_at_unix_ms < ?1)",
                [cutoff_unix_ms],
                |row| row.get(0),
            )?;
            let provider_attempts_to_prune: i64 = tx.query_row(
                "SELECT count(*) FROM provider_attempts WHERE owner_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
                [cutoff_unix_ms],
                |row| row.get(0),
            )?;
            let shared_snapshots_preserved: i64 = tx.query_row(
                "SELECT count(DISTINCT snapshot_id) FROM ranking_events
                 WHERE snapshot_id IS NOT NULL
                   AND created_at_unix_ms >= ?1
                   AND snapshot_id IN (SELECT snapshot_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
                [cutoff_unix_ms],
                |row| row.get(0),
            )?;
            let snapshots_to_prune: i64 = tx.query_row(
                "SELECT count(*) FROM roster_snapshots
                 WHERE created_at_unix_ms < ?1
                   AND snapshot_id NOT IN (
                       SELECT snapshot_id FROM ranking_events WHERE snapshot_id IS NOT NULL AND created_at_unix_ms >= ?1
                   )",
                [cutoff_unix_ms],
                |row| row.get(0),
            )?;
            (
                events_to_prune as u64,
                candidates_to_prune as u64,
                observations_to_prune as u64,
                judgments_to_prune as u64,
                provider_attempts_to_prune as u64,
                snapshots_to_prune as u64,
                shared_snapshots_preserved as u64,
            )
        };

        let (
            events_pruned,
            candidates_pruned,
            observations_pruned,
            judgments_pruned,
            provider_attempts_pruned,
            snapshots_pruned,
            shared_snapshots_preserved,
        ) = preview;

        if events_pruned > 0 || observations_pruned > 0 || snapshots_pruned > 0 {
            tx.execute(
                "DELETE FROM judgments WHERE attributed_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
                [cutoff_unix_ms],
            )?;
            tx.execute(
                "DELETE FROM observations WHERE attributed_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1) OR (attributed_event_id IS NULL AND observed_at_unix_ms < ?1)",
                [cutoff_unix_ms],
            )?;
            tx.execute(
                "DELETE FROM provider_attempts WHERE owner_event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
                [cutoff_unix_ms],
            )?;
            tx.execute(
                "DELETE FROM ranking_candidates WHERE event_id IN (SELECT event_id FROM ranking_events WHERE created_at_unix_ms < ?1)",
                [cutoff_unix_ms],
            )?;
            tx.execute(
                "DELETE FROM ranking_events WHERE created_at_unix_ms < ?1",
                [cutoff_unix_ms],
            )?;
            tx.execute(
                "DELETE FROM roster_snapshots
                 WHERE created_at_unix_ms < ?1
                   AND snapshot_id NOT IN (SELECT snapshot_id FROM ranking_events WHERE snapshot_id IS NOT NULL)",
                [cutoff_unix_ms],
            )?;
        }

        let new_data_gen = expected_stamp
            .data_generation
            .checked_add(1)
            .ok_or(StoreError::GenerationExhausted)?;

        tx.execute(
            "UPDATE store_meta SET data_generation = ?1 WHERE singleton = 1",
            [new_data_gen as i64],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;

        let stamp_before = self.stamp;
        self.stamp.data_generation = new_data_gen;
        let stamp_after = self.stamp;

        Ok(PruneReport {
            cutoff_unix_ms,
            cutoff_iso: format_unix_ms(cutoff_unix_ms),
            events_pruned,
            candidates_pruned,
            observations_pruned,
            judgments_pruned,
            provider_attempts_pruned,
            snapshots_pruned,
            shared_snapshots_preserved,
            stamp_before,
            stamp_after,
            affected_provenance: "ordinary-retention-prune",
            preflight_headroom_bytes: preflight.current_occupied_bytes,
        })
    }

    /// Prunes events created before cutoff timestamp and their dependent records after preflighting headroom.
    pub fn prune_events_before(
        &mut self,
        cutoff_unix_ms: i64,
        clock: EntryClock,
        cx: &Cx,
        expected_stamp: LedgerStamp,
    ) -> Result<u64, MaintenanceError> {
        let report = self.prune_apply(cutoff_unix_ms, clock, cx, expected_stamp)?;
        Ok(report.events_pruned)
    }

    /// Previews clearing all mutable history without modifying storage.
    pub fn clear_preview(&self) -> Result<ClearPreview, StoreError> {
        let events_count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM ranking_events", [], |r| r.get(0))?;
        let candidates_count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM ranking_candidates", [], |r| r.get(0))?;
        let observations_count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM observations", [], |r| r.get(0))?;
        let judgments_count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM judgments", [], |r| r.get(0))?;
        let provider_attempts_count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM provider_attempts", [], |r| r.get(0))?;
        let snapshots_count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM roster_snapshots", [], |r| r.get(0))?;
        let session_cursors_count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM session_cursors", [], |r| r.get(0))?;
        let feedback_proposals_count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM feedback_proposals", [], |r| r.get(0))?;
        let calibrations_count: i64 =
            self.connection
                .query_row("SELECT count(*) FROM calibrations", [], |r| r.get(0))?;

        let total = events_count
            + candidates_count
            + observations_count
            + judgments_count
            + provider_attempts_count
            + snapshots_count
            + session_cursors_count
            + feedback_proposals_count
            + calibrations_count;

        Ok(ClearPreview {
            events_count: events_count as u64,
            candidates_count: candidates_count as u64,
            observations_count: observations_count as u64,
            judgments_count: judgments_count as u64,
            provider_attempts_count: provider_attempts_count as u64,
            snapshots_count: snapshots_count as u64,
            session_cursors_count: session_cursors_count as u64,
            feedback_proposals_count: feedback_proposals_count as u64,
            calibrations_count: calibrations_count as u64,
            total_records: total as u64,
            requires_apply: true,
        })
    }

    /// Clears all mutable history after preflighting headroom, advancing data generation.
    pub fn clear_apply(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        expected_stamp: LedgerStamp,
    ) -> Result<ClearReport, MaintenanceError> {
        if self.read_only {
            return Err(MaintenanceError::Store(StoreError::Permissions));
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        let preflight = self.preflight_maintenance(MaintenanceKind::Clear)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let preview = self.clear_preview()?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        let new_data_gen = expected_stamp
            .data_generation
            .checked_add(1)
            .ok_or(StoreError::GenerationExhausted)?;

        tx.execute(
            "UPDATE store_meta SET data_generation = ?1 WHERE singleton = 1",
            [new_data_gen as i64],
        )?;

        // Purge data tables in dependency order
        tx.execute("DELETE FROM judgments", [])?;
        tx.execute("DELETE FROM observations", [])?;
        tx.execute("DELETE FROM provider_attempts", [])?;
        tx.execute("DELETE FROM ranking_candidates", [])?;
        tx.execute("DELETE FROM ranking_events", [])?;
        tx.execute("DELETE FROM roster_snapshots", [])?;
        tx.execute("DELETE FROM session_cursors", [])?;
        tx.execute("DELETE FROM feedback_proposals", [])?;
        tx.execute("DELETE FROM calibrations", [])?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;

        let stamp_before = self.stamp;
        self.stamp.data_generation = new_data_gen;
        let stamp_after = self.stamp;

        Ok(ClearReport {
            records_cleared: preview.total_records,
            stamp_before,
            stamp_after,
            affected_provenance: "explicit-ledger-clear",
            preflight_headroom_bytes: preflight.current_occupied_bytes,
        })
    }

    /// Advances the data generation and deletes mutable history, fencing stale writers.
    pub fn clear(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        expected_stamp: LedgerStamp,
    ) -> Result<LedgerStamp, StoreError> {
        match self.clear_apply(clock, cx, expected_stamp) {
            Ok(report) => Ok(report.stamp_after),
            Err(MaintenanceError::Store(e)) => Err(e),
            Err(MaintenanceError::QuotaExceeded { .. }) => Err(StoreError::Quota),
            Err(MaintenanceError::InsufficientDiskSpace { .. }) => {
                Err(StoreError::InsufficientSpace)
            }
            Err(MaintenanceError::Sqlite(_)) => Err(StoreError::Io),
        }
    }

    /// Revises an existing judgment label, incrementing label_version and advancing data generation.
    /// Fences concurrent stale writers and invalidates derived priors.
    #[allow(clippy::too_many_arguments)]
    pub fn revise_judgment(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        judgment_id: &str,
        new_label: JudgmentLabel,
        provenance: &str,
        updated_at_unix_ms: u64,
        expected_stamp: LedgerStamp,
    ) -> Result<LedgerStamp, StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        let current_version: i64 = tx
            .query_row(
                "SELECT label_version FROM judgments WHERE judgment_id = ?1",
                [judgment_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::InvalidRecord,
                other => StoreError::from(other),
            })?;

        let new_version = current_version
            .checked_add(1)
            .ok_or(StoreError::GenerationExhausted)?;
        let new_data_gen = expected_stamp
            .data_generation
            .checked_add(1)
            .ok_or(StoreError::GenerationExhausted)?;

        tx.execute(
            "UPDATE judgments SET label = ?1, label_version = ?2, provenance = ?3, created_at_unix_ms = ?4 WHERE judgment_id = ?5",
            params![
                new_label.as_str(),
                new_version,
                provenance,
                updated_at_unix_ms as i64,
                judgment_id,
            ],
        )?;

        tx.execute(
            "UPDATE store_meta SET data_generation = ?1 WHERE singleton = 1",
            [new_data_gen as i64],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;

        self.stamp.data_generation = new_data_gen;
        Ok(self.stamp)
    }

    /// Removes a judgment label, advancing data generation to invalidate derived priors.
    pub fn remove_judgment(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        judgment_id: &str,
        expected_stamp: LedgerStamp,
    ) -> Result<LedgerStamp, StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        let rows_affected = tx.execute(
            "DELETE FROM judgments WHERE judgment_id = ?1",
            [judgment_id],
        )?;

        if rows_affected == 0 {
            return Err(StoreError::InvalidRecord);
        }

        let new_data_gen = expected_stamp
            .data_generation
            .checked_add(1)
            .ok_or(StoreError::GenerationExhausted)?;

        tx.execute(
            "UPDATE store_meta SET data_generation = ?1 WHERE singleton = 1",
            [new_data_gen as i64],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;

        self.stamp.data_generation = new_data_gen;
        Ok(self.stamp)
    }

    pub fn record_roster_snapshot(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        snapshot: &NewRosterSnapshot,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        let members_json = validated_snapshot(snapshot)?;
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        insert_snapshot(&tx, snapshot, &members_json)?;
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_ranking_event(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        event: &NewRankingEvent,
        candidates: &[NewRankingCandidate],
        snapshot: Option<&NewRosterSnapshot>,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        self.record_ranking_event_with_attempts(
            clock,
            cx,
            event,
            candidates,
            snapshot,
            &[],
            expected_stamp,
        )
    }

    /// Record a ranking event together with the provider attempts it owns.
    ///
    /// One transaction, because cost evidence and the event it is attributed to are
    /// not independently meaningful: an attempt row without its owner would be cost
    /// attributed to nothing, and an event whose attempts were dropped would look
    /// like it was served for free.
    #[allow(clippy::too_many_arguments)]
    pub fn record_ranking_event_with_attempts(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        event: &NewRankingEvent,
        candidates: &[NewRankingCandidate],
        snapshot: Option<&NewRosterSnapshot>,
        attempts: &[NewProviderAttempt],
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        if attempts
            .iter()
            .any(|attempt| attempt.owner_event_id != event.event_id)
        {
            return Err(StoreError::InvalidRecord);
        }
        if candidates
            .iter()
            .any(|candidate| candidate.event_id != event.event_id)
            || snapshot.is_some_and(|snapshot| {
                event.snapshot_id.as_deref() != Some(snapshot.snapshot_id.as_str())
                    || snapshot.workspace_root != event.workspace_root
            })
        {
            return Err(StoreError::InvalidRecord);
        }
        let members_json = snapshot.map(validated_snapshot).transpose()?;
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        if let (Some(snap), Some(members)) = (snapshot, members_json.as_deref()) {
            insert_snapshot(&tx, snap, members)?;
        }
        if let Some(snapshot_id) = &event.snapshot_id {
            let stored = read_snapshot(&tx, snapshot_id)?.ok_or(StoreError::InvalidRecord)?;
            if stored.workspace_root != event.workspace_root {
                return Err(StoreError::InvalidRecord);
            }
            // Pre-validation ledgers may contain opaque JSON. Referencing an
            // existing ID must not turn that unknown evidence into a claim.
            validated_snapshot(&stored)?;
        }

        let inserted = tx.execute(
            "INSERT INTO ranking_events (
                event_id, verified_delivery_key, workspace_root, session_id,
                agent_branch, mode_channel, policy_version, schema_version,
                decision, reason, exposure_state, elapsed_ms, created_at_unix_ms,
                input_tokens, output_tokens, snapshot_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ON CONFLICT(event_id) DO NOTHING",
            params![
                event.event_id,
                event.verified_delivery_key,
                event.workspace_root,
                event.session_id,
                event.agent_branch,
                event.mode_channel,
                event.policy_version,
                event.schema_version as i64,
                event.decision.as_str(),
                event.reason,
                event.exposure_state.as_str(),
                event.elapsed_ms as i64,
                event.created_at_unix_ms as i64,
                event.input_tokens.map(|t| t as i64),
                event.output_tokens.map(|t| t as i64),
                event.snapshot_id,
            ],
        )?;
        if inserted == 0 {
            // Either a duplicate delivery of the same event, or this invocation's own
            // in-flight row being finalised. Both are the same turn, which the identity
            // derivation is supposed to produce; failing here would roll the whole
            // transaction back and take this delivery's attempt rows with it, which is
            // how a repeat that really paid ended up recorded nowhere (sr-qqlk).
            let stored: (String, String, String, String, String) = tx.query_row(
                "SELECT workspace_root, session_id, agent_branch, exposure_state, decision
                 FROM ranking_events WHERE event_id = ?1",
                params![event.event_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
            // An id reused for a different session or workspace is a collision, not a
            // repeat, and is still refused.
            if (&stored.0, &stored.1, &stored.2)
                != (
                    &event.workspace_root,
                    &event.session_id,
                    &event.agent_branch,
                )
            {
                return Err(StoreError::RecordConflict);
            }
            // `generated` means an invocation wrote itself down before sending and has
            // not finished: this write is that invocation finishing. A finished
            // failure (`unavailable`, `prepared`) delivered nothing, so a later
            // delivery of the same turn that is running or got further is the
            // turn's outcome; keeping the failure would label that delivery's paid
            // attempts a failed turn (sr-e8vm). Anything else belongs to a delivery
            // that already produced output, and the first delivery's record stands.
            let running = stored.3 == ExposureState::Generated.as_str();
            let failed = stored.3 == ExposureState::Prepared.as_str()
                && stored.4 == DecisionKind::Unavailable.as_str();
            if running || failed {
                tx.execute(
                    "UPDATE ranking_events SET decision = ?1, reason = ?2, exposure_state = ?3,
                            elapsed_ms = ?4, input_tokens = ?5, output_tokens = ?6,
                            snapshot_id = ?7
                     WHERE event_id = ?8 AND exposure_state = ?9 AND decision = ?10",
                    params![
                        event.decision.as_str(),
                        event.reason,
                        event.exposure_state.as_str(),
                        event.elapsed_ms as i64,
                        event.input_tokens.map(|t| t as i64),
                        event.output_tokens.map(|t| t as i64),
                        event.snapshot_id,
                        event.event_id,
                        stored.3,
                        stored.4,
                    ],
                )?;
            }
        }

        for cand in candidates {
            tx.execute(
                "INSERT INTO ranking_candidates (
                    event_id, stage, skill_id, skill_version, raw_probability,
                    normalized_probability, fit_score, rank_score, rank_position,
                    excluded, exclusion_reason
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT(event_id, stage, skill_id) DO NOTHING",
                params![
                    cand.event_id,
                    cand.stage.as_str(),
                    cand.skill_id,
                    cand.skill_version,
                    cand.raw_probability,
                    cand.normalized_probability,
                    cand.fit_score,
                    cand.rank_score,
                    cand.rank_position.map(|p| p as i64),
                    if cand.excluded { 1 } else { 0 },
                    cand.exclusion_reason,
                ],
            )?;
        }
        for attempt in attempts {
            // This invocation settling its own attempts, which the sending path may
            // already have recorded as admitted or sent. Advancing those rows is the
            // point; a stranger overwriting them is what `insert_provider_attempt`
            // refuses.
            advance_provider_attempt(&tx, attempt)?;
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_session_cursor(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        cursor: &SessionCursor,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        tx.execute(
            "INSERT INTO session_cursors (
                workspace_root, session_id, agent_branch, cursor_kind,
                transcript_generation, last_complete_event_id, last_offset_bytes, updated_at_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT (workspace_root, session_id, agent_branch, cursor_kind) DO UPDATE SET
                transcript_generation = excluded.transcript_generation,
                last_complete_event_id = excluded.last_complete_event_id,
                last_offset_bytes = excluded.last_offset_bytes,
                updated_at_unix_ms = excluded.updated_at_unix_ms",
            params![
                cursor.workspace_root,
                cursor.session_id,
                cursor.agent_branch,
                cursor.cursor_kind.as_str(),
                cursor.transcript_generation as i64,
                cursor.last_complete_event_id,
                cursor.last_offset_bytes as i64,
                cursor.updated_at_unix_ms as i64,
            ],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_session_cursor(
        &self,
        clock: EntryClock,
        cx: &Cx,
        workspace_root: &str,
        session_id: &str,
        agent_branch: &str,
        kind: CursorKind,
    ) -> Result<Option<SessionCursor>, StoreError> {
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let cursor = self
            .connection
            .query_row(
                "SELECT workspace_root, session_id, agent_branch, cursor_kind,
                        transcript_generation, last_complete_event_id, last_offset_bytes, updated_at_unix_ms
                 FROM session_cursors
                 WHERE workspace_root = ?1 AND session_id = ?2 AND agent_branch = ?3 AND cursor_kind = ?4",
                params![workspace_root, session_id, agent_branch, kind.as_str()],
                |row| {
                    let k_str: String = row.get(3)?;
                    let k = CursorKind::parse_str(&k_str)
                        .ok_or_else(|| rusqlite::Error::InvalidColumnType(3, "cursor_kind".into(), rusqlite::types::Type::Text))?;
                    let t_gen: i64 = row.get(4)?;
                    let off: i64 = row.get(6)?;
                    let upd: i64 = row.get(7)?;
                    Ok(SessionCursor {
                        workspace_root: row.get(0)?,
                        session_id: row.get(1)?,
                        agent_branch: row.get(2)?,
                        cursor_kind: k,
                        transcript_generation: t_gen as u64,
                        last_complete_event_id: row.get(5)?,
                        last_offset_bytes: off as u64,
                        updated_at_unix_ms: upd as u64,
                    })
                },
            )
            .optional()?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        Ok(cursor)
    }

    pub fn record_provider_attempt(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        attempt: &NewProviderAttempt,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        insert_provider_attempt(&tx, attempt)?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;
        Ok(())
    }

    /// Records that this attempt's request reached the wire, before its response is
    /// awaited.
    ///
    /// This is the one transition that cannot be written after the fact. Everything
    /// else about an attempt is known once it settles, but whether a request was
    /// actually sent is only knowable *before* the wait that a kill interrupts — and it
    /// is the fact that decides whether the provider may already have charged for this
    /// attempt. It deliberately does not set a completion time: an attempt that reached
    /// the wire has not finished, and dating its completion would claim otherwise.
    ///
    /// Only an `admitted` row is advanced, so this can never drag a settled attempt
    /// backwards. A missing row is reported rather than ignored, because it means the
    /// admission write did not land; callers on the sending path are expected to carry
    /// on regardless, since losing the record must never cost the caller its answer.
    pub fn mark_provider_attempt_sent(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        attempt_id: &str,
        sent_at_unix_ms: u64,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        let rows = tx.execute(
            "UPDATE provider_attempts SET status = ?1, sent_at_unix_ms = ?2 \
             WHERE attempt_id = ?3 AND status = ?4",
            params![
                AttemptStatus::Sent.as_str(),
                sent_at_unix_ms as i64,
                attempt_id,
                AttemptStatus::Admitted.as_str(),
            ],
        )?;
        if rows == 0 {
            return Err(StoreError::Missing);
        }

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_provider_attempt_outcome(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        attempt_id: &str,
        outcome: &ProviderAttemptOutcome<'_>,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        let in_tok = outcome.tokens.map(|(i, _)| i as i64);
        let out_tok = outcome.tokens.map(|(_, o)| o as i64);

        let rows = tx.execute(
            "UPDATE provider_attempts SET
                status = ?1,
                input_tokens = coalesce(?2, input_tokens),
                output_tokens = coalesce(?3, output_tokens),
                http_status = coalesce(?4, http_status),
                error_kind = coalesce(?5, error_kind),
                completed_at_unix_ms = ?6
             WHERE attempt_id = ?7",
            params![
                outcome.status.as_str(),
                in_tok,
                out_tok,
                outcome.http_status.map(|s| s as i64),
                outcome.error_kind,
                outcome.completed_at_unix_ms as i64,
                attempt_id,
            ],
        )?;

        if rows == 0 {
            return Err(StoreError::Missing);
        }

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_observation(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        obs: &NewObservation,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        tx.execute(
            "INSERT INTO observations (
                observation_id, source_event_key, workspace_root, session_id,
                agent_branch, attributed_event_id, skill_id, evidence_state, observed_at_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                obs.observation_id,
                obs.source_event_key,
                obs.workspace_root,
                obs.session_id,
                obs.agent_branch,
                obs.attributed_event_id,
                obs.skill_id,
                obs.evidence_state.as_str(),
                obs.observed_at_unix_ms as i64,
            ],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_observations_with_cursor(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        observations: &[NewObservation],
        cursor: &SessionCursor,
        expected_cursor_gen: Option<u64>,
        expected_stamp: LedgerStamp,
    ) -> Result<LedgerStamp, StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        // Compare-and-swap on cursor generation if specified
        if let Some(expected_gen) = expected_cursor_gen {
            let current_gen: Option<i64> = tx
                .query_row(
                    "SELECT transcript_generation FROM session_cursors
                     WHERE workspace_root = ?1 AND session_id = ?2 AND agent_branch = ?3 AND cursor_kind = ?4",
                    params![
                        cursor.workspace_root,
                        cursor.session_id,
                        cursor.agent_branch,
                        cursor.cursor_kind.as_str(),
                    ],
                    |r| r.get(0),
                )
                .optional()?;

            match current_gen {
                Some(current_g) if current_g as u64 != expected_gen => {
                    return Err(StoreError::RecordConflict);
                }
                None if expected_gen != 0 => {
                    return Err(StoreError::RecordConflict);
                }
                _ => {}
            }
        }

        // Replayed bounded input can resolve a previously attempted load. Upgrade
        // only that same observation, without moving its original attribution.
        // Never let stale attempts or censored input weaken confirmed evidence.
        for obs in observations {
            let attributed_event_id = match &obs.attributed_event_id {
                Some(id) => Some(id.clone()),
                None => {
                    let min_time = obs.observed_at_unix_ms.saturating_sub(1_800_000);
                    tx.query_row(
                        "SELECT event_id FROM ranking_events
                         WHERE workspace_root = ?1 AND session_id = ?2 AND agent_branch = ?3
                           AND exposure_state IN ('emitted', 'acknowledged')
                           AND created_at_unix_ms <= ?4 AND created_at_unix_ms >= ?5
                         ORDER BY created_at_unix_ms DESC LIMIT 1",
                        params![
                            obs.workspace_root,
                            obs.session_id,
                            obs.agent_branch,
                            obs.observed_at_unix_ms as i64,
                            min_time as i64,
                        ],
                        |r| r.get(0),
                    )
                    .optional()?
                }
            };
            tx.execute(
                "INSERT INTO observations (
                    observation_id, source_event_key, workspace_root, session_id,
                    agent_branch, attributed_event_id, skill_id, evidence_state, observed_at_unix_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT (source_event_key) DO UPDATE SET
                    evidence_state = excluded.evidence_state,
                    observed_at_unix_ms = excluded.observed_at_unix_ms
                WHERE observations.evidence_state = 'attempted'
                  AND excluded.evidence_state = 'loaded'
                  AND observations.observation_id = excluded.observation_id
                  AND observations.workspace_root = excluded.workspace_root
                  AND observations.session_id = excluded.session_id
                  AND observations.agent_branch = excluded.agent_branch
                  AND observations.skill_id = excluded.skill_id
                  AND excluded.observed_at_unix_ms >= observations.observed_at_unix_ms",
                params![
                    obs.observation_id,
                    obs.source_event_key,
                    obs.workspace_root,
                    obs.session_id,
                    obs.agent_branch,
                    attributed_event_id,
                    obs.skill_id,
                    obs.evidence_state.as_str(),
                    obs.observed_at_unix_ms as i64,
                ],
            )?;
        }

        // Upsert the cursor watermark
        tx.execute(
            "INSERT INTO session_cursors (
                workspace_root, session_id, agent_branch, cursor_kind,
                transcript_generation, last_complete_event_id, last_offset_bytes, updated_at_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT (workspace_root, session_id, agent_branch, cursor_kind) DO UPDATE SET
                transcript_generation = excluded.transcript_generation,
                last_complete_event_id = excluded.last_complete_event_id,
                last_offset_bytes = excluded.last_offset_bytes,
                updated_at_unix_ms = excluded.updated_at_unix_ms",
            params![
                cursor.workspace_root,
                cursor.session_id,
                cursor.agent_branch,
                cursor.cursor_kind.as_str(),
                cursor.transcript_generation as i64,
                cursor.last_complete_event_id,
                cursor.last_offset_bytes as i64,
                cursor.updated_at_unix_ms as i64,
            ],
        )?;

        let new_data_gen = expected_stamp
            .data_generation
            .checked_add(1)
            .ok_or(StoreError::GenerationExhausted)?;

        tx.execute(
            "UPDATE store_meta SET data_generation = ?1 WHERE singleton = 1",
            [new_data_gen as i64],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;

        self.stamp.data_generation = new_data_gen;
        Ok(self.stamp)
    }

    pub fn get_session_observations(
        &self,
        clock: EntryClock,
        cx: &Cx,
        workspace_root: &str,
        session_id: &str,
    ) -> Result<Vec<NewObservation>, StoreError> {
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let mut stmt = self.connection.prepare(
            "SELECT observation_id, source_event_key, workspace_root, session_id,
                    agent_branch, attributed_event_id, skill_id, evidence_state, observed_at_unix_ms
             FROM observations
             WHERE workspace_root = ?1 AND session_id = ?2
             ORDER BY observed_at_unix_ms ASC",
        )?;

        let rows = stmt.query_map(params![workspace_root, session_id], |row| {
            let state_str: String = row.get(7)?;
            let state = EvidenceState::parse_str(&state_str).unwrap_or(EvidenceState::Attempted);
            let time: i64 = row.get(8)?;
            Ok(NewObservation {
                observation_id: row.get(0)?,
                source_event_key: row.get(1)?,
                workspace_root: row.get(2)?,
                session_id: row.get(3)?,
                agent_branch: row.get(4)?,
                attributed_event_id: row.get(5)?,
                skill_id: row.get(6)?,
                evidence_state: state,
                observed_at_unix_ms: time as u64,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn find_latest_preceding_emission(
        &self,
        clock: EntryClock,
        cx: &Cx,
        workspace_root: &str,
        session_id: &str,
        agent_branch: &str,
        observed_at_unix_ms: u64,
        max_window_ms: u64,
    ) -> Result<Option<String>, StoreError> {
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let min_time = observed_at_unix_ms.saturating_sub(max_window_ms);
        let event_id: Option<String> = self
            .connection
            .query_row(
                "SELECT event_id FROM ranking_events
                 WHERE workspace_root = ?1 AND session_id = ?2 AND agent_branch = ?3
                   AND exposure_state IN ('emitted', 'acknowledged')
                   AND created_at_unix_ms <= ?4 AND created_at_unix_ms >= ?5
                 ORDER BY created_at_unix_ms DESC LIMIT 1",
                params![
                    workspace_root,
                    session_id,
                    agent_branch,
                    observed_at_unix_ms as i64,
                    min_time as i64,
                ],
                |r| r.get(0),
            )
            .optional()?;

        Ok(event_id)
    }

    pub fn record_judgment(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        judgment: &NewJudgment,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        tx.execute(
            "INSERT INTO judgments (
                judgment_id, attributed_event_id, skill_id, label, label_version, provenance, created_at_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                judgment.judgment_id,
                judgment.attributed_event_id,
                judgment.skill_id,
                judgment.label.as_str(),
                judgment.label_version as i64,
                judgment.provenance,
                judgment.created_at_unix_ms as i64,
            ],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_feedback_proposal(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        proposal: &NewFeedbackProposal,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        tx.execute(
            "INSERT INTO feedback_proposals (
                proposal_id, workspace_root, session_id, suggested_skill_reference, status, notes, created_at_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                proposal.proposal_id,
                proposal.workspace_root,
                proposal.session_id,
                proposal.suggested_skill_reference,
                proposal.status.as_str(),
                proposal.notes,
                proposal.created_at_unix_ms as i64,
            ],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_calibration(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        cal: &NewCalibration,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        tx.execute(
            "INSERT INTO calibrations (
                calibration_id, dataset_fingerprint, split, objective, coefficients_json, evaluation_report_id, created_at_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                cal.calibration_id,
                cal.dataset_fingerprint,
                cal.split.as_str(),
                cal.objective,
                cal.coefficients_json,
                cal.evaluation_report_id,
                cal.created_at_unix_ms as i64,
            ],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_ranking_event(
        &self,
        clock: EntryClock,
        cx: &Cx,
        event_id: &str,
    ) -> Result<Option<NewRankingEvent>, StoreError> {
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let event = self
            .connection
            .query_row(
                "SELECT event_id, verified_delivery_key, workspace_root, session_id,
                        agent_branch, mode_channel, policy_version, schema_version,
                        decision, reason, exposure_state, elapsed_ms, created_at_unix_ms,
                        input_tokens, output_tokens, snapshot_id
                 FROM ranking_events WHERE event_id = ?1",
                [event_id],
                |row| {
                    let d_str: String = row.get(8)?;
                    let e_str: String = row.get(10)?;
                    let s_ver: i64 = row.get(7)?;
                    let elapsed: i64 = row.get(11)?;
                    let created: i64 = row.get(12)?;
                    let in_tok: Option<i64> = row.get(13)?;
                    let out_tok: Option<i64> = row.get(14)?;
                    Ok(NewRankingEvent {
                        event_id: row.get(0)?,
                        verified_delivery_key: row.get(1)?,
                        workspace_root: row.get(2)?,
                        session_id: row.get(3)?,
                        agent_branch: row.get(4)?,
                        mode_channel: row.get(5)?,
                        policy_version: row.get(6)?,
                        schema_version: s_ver as u32,
                        decision: DecisionKind::parse_str(&d_str)
                            .unwrap_or(DecisionKind::Unavailable),
                        reason: row.get(9)?,
                        exposure_state: ExposureState::parse_str(&e_str)
                            .unwrap_or(ExposureState::Unknown),
                        elapsed_ms: elapsed as u64,
                        created_at_unix_ms: created as u64,
                        input_tokens: in_tok.map(|t| t as u64),
                        output_tokens: out_tok.map(|t| t as u64),
                        snapshot_id: row.get(15)?,
                    })
                },
            )
            .optional()?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        Ok(event)
    }

    pub fn get_roster_snapshot(
        &self,
        clock: EntryClock,
        cx: &Cx,
        snapshot_id: &str,
    ) -> Result<Option<NewRosterSnapshot>, StoreError> {
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let snapshot = self
            .connection
            .query_row(
                "SELECT workspace_root, adapter, total_candidates, eligible_candidates,
                        membership_coverage, members_json, created_at_unix_ms
                 FROM roster_snapshots WHERE snapshot_id = ?1",
                [snapshot_id],
                |row| {
                    let total: i64 = row.get(2)?;
                    let eligible: i64 = row.get(3)?;
                    let cov_str: String = row.get(4)?;
                    let created: i64 = row.get(6)?;
                    Ok(NewRosterSnapshot {
                        snapshot_id: snapshot_id.to_string(),
                        workspace_root: row.get(0)?,
                        adapter: row.get(1)?,
                        total_candidates: total as u64,
                        eligible_candidates: eligible as u64,
                        membership_coverage: MembershipCoverage::parse_str(&cov_str)
                            .unwrap_or(MembershipCoverage::Unknown),
                        members_json: row.get(5)?,
                        created_at_unix_ms: created as u64,
                    })
                },
            )
            .optional()?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        Ok(snapshot)
    }

    /// Matches one supplied reference against retained snapshot members.
    ///
    /// `Ok(None)` means the snapshot simply does not contain it, which the callers
    /// distinguish from ambiguity: a name matching several skills is refused rather than
    /// guessed, because invocation names are not unique across sources and the harness may
    /// keep same-name skills distinct. Attaching a label to the wrong skill is worse than
    /// asking the assessor for the stable id.
    fn match_reference_in_members(
        members: &[SnapshotMember],
        supplied: &str,
    ) -> Result<Option<String>, FeedbackError> {
        if members.iter().any(|member| member.skill_id == supplied) {
            return Ok(Some(supplied.to_string()));
        }
        let mut matched = members
            .iter()
            .filter(|member| member.invocation_name.as_deref() == Some(supplied))
            .map(|member| member.skill_id.clone());
        let Some(first) = matched.next() else {
            return Ok(None);
        };
        if matched.next().is_some() {
            return Err(FeedbackError::InvalidSkillId(format!(
                "'{supplied}' matches more than one skill in this event's roster snapshot; \
                 supply the stable skill id"
            )));
        }
        Ok(Some(first))
    }

    /// Resolves a user-supplied skill reference to the stable skill id, against evidence
    /// bound to this event.
    ///
    /// A person reads an invocation name in the ranking output — `rust-code-review` — and
    /// types that into `sr feedback --skill`. The ledger joins on the stable opaque id.
    /// Accepting only the id would make the command unusable from the output it exists to
    /// follow; storing the name would key the judgment differently from the candidates and
    /// observations for the same skill, so every per-skill figure would split in two. So the
    /// reference is resolved here, once, before anything is written.
    ///
    /// Order matters. A value that already names a candidate of this event is taken as the
    /// stable id and needs no snapshot, which keeps feedback working for events whose roster
    /// membership was only partially observed. Otherwise the event's retained snapshot is
    /// consulted, because it is the only record of what the invocation names meant at the
    /// time; today's roster cannot establish what a past name referred to.
    ///
    /// The supplied name is not lost by resolving it: `SnapshotMember` retains `skill_id`
    /// alongside `invocation_name`, so the mapping stays recoverable from the same snapshot
    /// that resolved it.
    fn resolve_event_skill_reference(
        tx: &rusqlite::Transaction<'_>,
        event_id: &str,
        snapshot_id: Option<&str>,
        supplied: &str,
    ) -> Result<String, FeedbackError> {
        let is_candidate: i64 = tx
            .query_row(
                "SELECT count(*) FROM ranking_candidates WHERE event_id = ?1 AND skill_id = ?2",
                [event_id, supplied],
                |row| row.get(0),
            )
            .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;
        if is_candidate > 0 {
            return Ok(supplied.to_string());
        }

        let Some(snapshot_id) = snapshot_id else {
            return Err(FeedbackError::InvalidSkillId(format!(
                "'{supplied}' is not a candidate of this event and it has no roster snapshot to \
                 resolve a name against; supply the stable skill id"
            )));
        };
        let snapshot = read_snapshot(tx, snapshot_id)
            .map_err(FeedbackError::Store)?
            .ok_or(FeedbackError::MissingSnapshot)?;
        let members = Self::parse_snapshot_members(&snapshot).map_err(FeedbackError::Store)?;
        // A retained stable id proves membership even in a partial snapshot.
        // A name needs complete membership: an omitted skill may share it.
        if snapshot.membership_coverage != MembershipCoverage::Complete
            && !members.iter().any(|member| member.skill_id == supplied)
        {
            return Err(FeedbackError::MissingSnapshot);
        }
        Self::match_reference_in_members(&members, supplied)?.ok_or_else(|| {
            FeedbackError::InvalidSkillId(format!(
                "'{supplied}' matches no skill id or invocation name in this event's roster \
                 snapshot"
            ))
        })
    }

    pub fn parse_snapshot_members(
        snapshot: &NewRosterSnapshot,
    ) -> Result<Vec<SnapshotMember>, StoreError> {
        let value = crate::adapter::decode_json(
            snapshot.members_json.as_bytes(),
            MAX_SNAPSHOT_METADATA_BYTES,
        )
        .map_err(|_| StoreError::InvalidRecord)?;
        serde_json::from_value(value).map_err(|_| StoreError::InvalidRecord)
    }

    pub fn get_judgment(
        &self,
        clock: EntryClock,
        cx: &Cx,
        attributed_event_id: &str,
        skill_id: &str,
    ) -> Result<Option<NewJudgment>, StoreError> {
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let jdg = self
            .connection
            .query_row(
                "SELECT judgment_id, attributed_event_id, skill_id, label, label_version, provenance, created_at_unix_ms
                 FROM judgments WHERE attributed_event_id = ?1 AND skill_id = ?2",
                [attributed_event_id, skill_id],
                |row| {
                    let l_str: String = row.get(3)?;
                    let ver: i64 = row.get(4)?;
                    let created: i64 = row.get(6)?;
                    Ok(NewJudgment {
                        judgment_id: row.get(0)?,
                        attributed_event_id: row.get(1)?,
                        skill_id: row.get(2)?,
                        label: JudgmentLabel::parse_str(&l_str).unwrap_or(JudgmentLabel::Neutral),
                        label_version: ver as u32,
                        provenance: row.get(5)?,
                        created_at_unix_ms: created as u64,
                    })
                },
            )
            .optional()?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        Ok(jdg)
    }

    pub fn record_paired_correction(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        req: &PairedCorrectionRequest,
        expected_stamp: LedgerStamp,
    ) -> Result<(FeedbackOutcome, LedgerStamp), FeedbackError> {
        if self.read_only {
            return Err(FeedbackError::Store(StoreError::Permissions));
        }
        check_work(clock, cx).map_err(FeedbackError::Store)?;
        self.directory
            .verify_database_file(&self.file, clock, cx)
            .map_err(FeedbackError::Store)?;
        self.directory.admit_space().map_err(FeedbackError::Store)?;
        refresh_busy_limit(&self.connection, clock, cx).map_err(FeedbackError::Store)?;

        if req.original_skill_id == req.alternative_skill_id {
            return Err(FeedbackError::IdenticalSkills);
        }
        if !bounded_metadata(&req.original_skill_id, 256) {
            return Err(FeedbackError::InvalidSkillId(req.original_skill_id.clone()));
        }
        if !bounded_metadata(&req.alternative_skill_id, 256) {
            return Err(FeedbackError::InvalidSkillId(
                req.alternative_skill_id.clone(),
            ));
        }

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;
        check_stamp(&tx, expected_stamp).map_err(|_| FeedbackError::StaleStamp)?;

        // 1. Fetch ranking_event
        let event_row: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT workspace_root, session_id, snapshot_id FROM ranking_events WHERE event_id = ?1",
                [&req.event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

        let (workspace_root, session_id, snapshot_id_opt) = match event_row {
            Some(row) => row,
            None => return Err(FeedbackError::EventNotFound(req.event_id.clone())),
        };

        let snapshot_id = match snapshot_id_opt {
            Some(id) if !id.is_empty() => id,
            _ => return Err(FeedbackError::MissingSnapshot),
        };

        // 2. Fetch snapshot
        let snap = read_snapshot(&tx, &snapshot_id)
            .map_err(FeedbackError::Store)?
            .ok_or(FeedbackError::MissingSnapshot)?;

        if snap.membership_coverage != MembershipCoverage::Complete {
            return Err(FeedbackError::MissingSnapshot);
        }

        let members = Self::parse_snapshot_members(&snap).map_err(FeedbackError::Store)?;

        // Resolve BOTH references before anything is checked or written. A correction commits
        // two labels in one transaction, so a resolvable original paired with an unresolvable
        // alternative must write neither: AGENTS.md requires that a failed alternate lookup
        // cannot leave an unintended standalone negative label. Resolving up front is what
        // makes that automatic rather than a thing to remember.
        let original_skill_id = Self::match_reference_in_members(&members, &req.original_skill_id)?
            .unwrap_or_else(|| req.original_skill_id.clone());
        let alternative_skill_id =
            Self::match_reference_in_members(&members, &req.alternative_skill_id)?
                .unwrap_or_else(|| req.alternative_skill_id.clone());
        if original_skill_id == alternative_skill_id {
            // Two different spellings of one skill, for instance its id and its invocation
            // name, are still one skill and cannot be a correction.
            return Err(FeedbackError::IdenticalSkills);
        }

        // 3. Resolve original skill
        let orig_in_snap = members.iter().any(|m| m.skill_id == original_skill_id);
        let orig_in_cands: bool = if !orig_in_snap {
            tx.query_row(
                "SELECT count(*) FROM ranking_candidates WHERE event_id = ?1 AND skill_id = ?2",
                [&req.event_id, &original_skill_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false)
        } else {
            true
        };
        if !orig_in_cands {
            return Err(FeedbackError::OriginalSkillNotFound(
                original_skill_id.clone(),
            ));
        }

        let alt_member = members.iter().find(|m| m.skill_id == alternative_skill_id);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        match alt_member {
            None => {
                // Alternative is historically absent: record prospective proposal, ZERO judgments committed!
                let proposal_id = format!("prop-{}", generate_random_hex(12));
                let notes = req
                    .reason_code
                    .clone()
                    .unwrap_or_else(|| "historically absent alternative".to_string());
                tx.execute(
                    "INSERT INTO feedback_proposals (
                        proposal_id, workspace_root, session_id, suggested_skill_reference, status, notes, created_at_unix_ms
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        proposal_id,
                        workspace_root,
                        session_id,
                        alternative_skill_id,
                        ProposalStatus::HistoricallyAbsent.as_str(),
                        notes,
                        now_ms as i64,
                    ],
                )
                .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

                self.directory
                    .verify_database_file(&self.file, clock, cx)
                    .map_err(FeedbackError::Store)?;
                check_work(clock, cx).map_err(FeedbackError::Store)?;
                tx.commit()
                    .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

                Ok((
                    FeedbackOutcome::ProspectiveProposal {
                        event_id: req.event_id.clone(),
                        original_skill_id: original_skill_id.clone(),
                        alternative_skill_id: alternative_skill_id.clone(),
                        proposal_id,
                        reason: "alternative skill was not present in the historical roster snapshot; recorded as prospective proposal".to_string(),
                    },
                    self.stamp,
                ))
            }
            Some(alt) => {
                // Alternative was present: must be eligible!
                if !alt.eligible || alt.exclusion_reason.is_some() {
                    return Err(FeedbackError::IneligibleAlternative {
                        skill_id: alt.skill_id.clone(),
                        reason: alt.exclusion_reason.clone(),
                    });
                }

                // Check expected_version if specified
                let orig_existing: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT judgment_id, label_version FROM judgments WHERE attributed_event_id = ?1 AND skill_id = ?2",
                        [&req.event_id, &original_skill_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

                let alt_existing: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT judgment_id, label_version FROM judgments WHERE attributed_event_id = ?1 AND skill_id = ?2",
                        [&req.event_id, &alternative_skill_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

                if let Some(expected) = req.expected_version {
                    if let Some((_, actual)) = &orig_existing
                        && *actual as u32 != expected
                    {
                        return Err(FeedbackError::RevisionConflict {
                            expected,
                            actual: *actual as u32,
                        });
                    }
                    if let Some((_, actual)) = &alt_existing
                        && *actual as u32 != expected
                    {
                        return Err(FeedbackError::RevisionConflict {
                            expected,
                            actual: *actual as u32,
                        });
                    }
                }

                let group_id = format!("grp-{}", generate_random_hex(12));
                let user_prov = req.provenance.as_deref().unwrap_or("user");
                let provenance = format!("paired:{group_id}:{user_prov}");

                let orig_jdg_id = match orig_existing {
                    Some((id, ver)) => {
                        let new_ver = ver
                            .checked_add(1)
                            .ok_or(FeedbackError::Store(StoreError::GenerationExhausted))?;
                        tx.execute(
                            "UPDATE judgments SET label = ?1, label_version = ?2, provenance = ?3, created_at_unix_ms = ?4 WHERE judgment_id = ?5",
                            params![
                                JudgmentLabel::Harmful.as_str(),
                                new_ver,
                                provenance,
                                now_ms as i64,
                                id,
                            ],
                        )
                        .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;
                        id
                    }
                    None => {
                        let id = format!("jdg-{}", generate_random_hex(12));
                        tx.execute(
                            "INSERT INTO judgments (
                                judgment_id, attributed_event_id, skill_id, label, label_version, provenance, created_at_unix_ms
                            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            params![
                                id,
                                req.event_id,
                                original_skill_id,
                                JudgmentLabel::Harmful.as_str(),
                                1,
                                provenance,
                                now_ms as i64,
                            ],
                        )
                        .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;
                        id
                    }
                };

                let alt_jdg_id = match alt_existing {
                    Some((id, ver)) => {
                        let new_ver = ver
                            .checked_add(1)
                            .ok_or(FeedbackError::Store(StoreError::GenerationExhausted))?;
                        tx.execute(
                            "UPDATE judgments SET label = ?1, label_version = ?2, provenance = ?3, created_at_unix_ms = ?4 WHERE judgment_id = ?5",
                            params![
                                JudgmentLabel::Useful.as_str(),
                                new_ver,
                                provenance,
                                now_ms as i64,
                                id,
                            ],
                        )
                        .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;
                        id
                    }
                    None => {
                        let id = format!("jdg-{}", generate_random_hex(12));
                        tx.execute(
                            "INSERT INTO judgments (
                                judgment_id, attributed_event_id, skill_id, label, label_version, provenance, created_at_unix_ms
                            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            params![
                                id,
                                req.event_id,
                                alternative_skill_id,
                                JudgmentLabel::Useful.as_str(),
                                1,
                                provenance,
                                now_ms as i64,
                            ],
                        )
                        .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;
                        id
                    }
                };

                let new_data_gen = expected_stamp
                    .data_generation
                    .checked_add(1)
                    .ok_or(FeedbackError::Store(StoreError::GenerationExhausted))?;

                tx.execute(
                    "UPDATE store_meta SET data_generation = ?1 WHERE singleton = 1",
                    [new_data_gen as i64],
                )
                .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

                self.directory
                    .verify_database_file(&self.file, clock, cx)
                    .map_err(FeedbackError::Store)?;
                check_work(clock, cx).map_err(FeedbackError::Store)?;
                tx.commit()
                    .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

                self.stamp.data_generation = new_data_gen;

                Ok((
                    FeedbackOutcome::PairedCorrection {
                        event_id: req.event_id.clone(),
                        original_skill_id: original_skill_id.clone(),
                        alternative_skill_id: alternative_skill_id.clone(),
                        group_id,
                        original_judgment_id: orig_jdg_id,
                        alternative_judgment_id: alt_jdg_id,
                        data_generation: new_data_gen,
                    },
                    self.stamp,
                ))
            }
        }
    }

    pub fn record_single_feedback(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        req: &SingleFeedbackRequest,
        expected_stamp: LedgerStamp,
    ) -> Result<(FeedbackOutcome, LedgerStamp), FeedbackError> {
        if self.read_only {
            return Err(FeedbackError::Store(StoreError::Permissions));
        }
        check_work(clock, cx).map_err(FeedbackError::Store)?;
        self.directory
            .verify_database_file(&self.file, clock, cx)
            .map_err(FeedbackError::Store)?;
        self.directory.admit_space().map_err(FeedbackError::Store)?;
        refresh_busy_limit(&self.connection, clock, cx).map_err(FeedbackError::Store)?;

        if !bounded_metadata(&req.skill_id, 256) {
            return Err(FeedbackError::InvalidSkillId(req.skill_id.clone()));
        }

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;
        check_stamp(&tx, expected_stamp).map_err(|_| FeedbackError::StaleStamp)?;

        let event_row: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT workspace_root, session_id, snapshot_id FROM ranking_events WHERE event_id = ?1",
                [&req.event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

        let Some((_, _, snapshot_id)) = event_row else {
            return Err(FeedbackError::EventNotFound(req.event_id.clone()));
        };

        // Resolve before any lookup keyed on the skill, so the existing-judgment check, the
        // revision check and the write all agree on one identity.
        let skill_id = Self::resolve_event_skill_reference(
            &tx,
            &req.event_id,
            snapshot_id.as_deref(),
            &req.skill_id,
        )?;

        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT judgment_id, label_version FROM judgments WHERE attributed_event_id = ?1 AND skill_id = ?2",
                [&req.event_id, &skill_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

        if let Some(expected) = req.expected_version
            && let Some((_, actual)) = &existing
            && *actual as u32 != expected
        {
            return Err(FeedbackError::RevisionConflict {
                expected,
                actual: *actual as u32,
            });
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let provenance = req.provenance.clone().unwrap_or_else(|| "user".to_string());
        let judgment_id = match existing {
            Some((id, ver)) => {
                let new_ver = ver
                    .checked_add(1)
                    .ok_or(FeedbackError::Store(StoreError::GenerationExhausted))?;
                tx.execute(
                    "UPDATE judgments SET label = ?1, label_version = ?2, provenance = ?3, created_at_unix_ms = ?4 WHERE judgment_id = ?5",
                    params![
                        req.verdict.as_str(),
                        new_ver,
                        provenance,
                        now_ms as i64,
                        id,
                    ],
                )
                .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;
                id
            }
            None => {
                let new_id = format!("jdg-{}", generate_random_hex(12));
                tx.execute(
                    "INSERT INTO judgments (
                        judgment_id, attributed_event_id, skill_id, label, label_version, provenance, created_at_unix_ms
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        new_id,
                        req.event_id,
                        skill_id,
                        req.verdict.as_str(),
                        1,
                        provenance,
                        now_ms as i64,
                    ],
                )
                .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;
                new_id
            }
        };

        let new_data_gen = expected_stamp
            .data_generation
            .checked_add(1)
            .ok_or(FeedbackError::Store(StoreError::GenerationExhausted))?;

        tx.execute(
            "UPDATE store_meta SET data_generation = ?1 WHERE singleton = 1",
            [new_data_gen as i64],
        )
        .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

        self.directory
            .verify_database_file(&self.file, clock, cx)
            .map_err(FeedbackError::Store)?;
        check_work(clock, cx).map_err(FeedbackError::Store)?;
        tx.commit()
            .map_err(|e| FeedbackError::Store(StoreError::from(e)))?;

        self.stamp.data_generation = new_data_gen;

        Ok((
            FeedbackOutcome::SingleJudgment {
                event_id: req.event_id.clone(),
                skill_id,
                verdict: req.verdict,
                judgment_id,
                data_generation: new_data_gen,
            },
            self.stamp,
        ))
    }

    pub fn record_emission(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        event_id: &str,
        bytes_written: usize,
        expected_stamp: LedgerStamp,
    ) -> Result<(bool, LedgerStamp), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT mode_channel, exposure_state FROM ranking_events WHERE event_id = ?1",
                [event_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        let (mode_channel, current_state_str) = match row {
            Some(r) => r,
            None => return Err(StoreError::InvalidRecord),
        };

        // Shadow evaluations with zero bytes written are not exposure:
        // never transition to emitted.
        if mode_channel == "shadow" && bytes_written == 0 {
            return Ok((false, self.stamp));
        }

        // If zero bytes written for any channel, no emission occurred.
        if bytes_written == 0 {
            return Ok((false, self.stamp));
        }

        let current_state =
            ExposureState::parse_str(&current_state_str).unwrap_or(ExposureState::Unknown);
        if current_state == ExposureState::Emitted || current_state == ExposureState::Acknowledged {
            return Ok((true, self.stamp));
        }

        tx.execute(
            "UPDATE ranking_events SET exposure_state = ?1 WHERE event_id = ?2",
            params![ExposureState::Emitted.as_str(), event_id],
        )?;

        let new_data_gen = expected_stamp
            .data_generation
            .checked_add(1)
            .ok_or(StoreError::GenerationExhausted)?;

        tx.execute(
            "UPDATE store_meta SET data_generation = ?1 WHERE singleton = 1",
            [new_data_gen as i64],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;

        self.stamp.data_generation = new_data_gen;
        Ok((true, self.stamp))
    }

    pub fn record_acknowledgment(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        event_id: &str,
        verified_delivery_key: &str,
        expected_stamp: LedgerStamp,
    ) -> Result<(bool, LedgerStamp), StoreError> {
        if self.read_only {
            return Err(StoreError::Permissions);
        }
        if !bounded_metadata(verified_delivery_key, 256) {
            return Err(StoreError::InvalidRecord);
        }
        check_work(clock, cx)?;
        self.directory.verify_database_file(&self.file, clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        // Check for duplicate verified_delivery_key across different events
        let existing_key: Option<String> = tx
            .query_row(
                "SELECT event_id FROM ranking_events WHERE verified_delivery_key = ?1",
                [verified_delivery_key],
                |r| r.get(0),
            )
            .optional()?;

        if let Some(other_id) = existing_key {
            if other_id != event_id {
                // Reject duplicate verified delivery key across different events
                return Err(StoreError::RecordConflict);
            }
            // Idempotent re-acknowledgment for same event
            return Ok((true, self.stamp));
        }

        let current_state_str: Option<String> = tx
            .query_row(
                "SELECT exposure_state FROM ranking_events WHERE event_id = ?1",
                [event_id],
                |r| r.get(0),
            )
            .optional()?;

        if current_state_str.is_none() {
            return Err(StoreError::InvalidRecord);
        }

        tx.execute(
            "UPDATE ranking_events SET exposure_state = ?1, verified_delivery_key = ?2 WHERE event_id = ?3",
            params![ExposureState::Acknowledged.as_str(), verified_delivery_key, event_id],
        )?;

        let new_data_gen = expected_stamp
            .data_generation
            .checked_add(1)
            .ok_or(StoreError::GenerationExhausted)?;

        tx.execute(
            "UPDATE store_meta SET data_generation = ?1 WHERE singleton = 1",
            [new_data_gen as i64],
        )?;

        self.directory.verify_database_file(&self.file, clock, cx)?;
        check_work(clock, cx)?;
        tx.commit()?;

        self.stamp.data_generation = new_data_gen;
        Ok((true, self.stamp))
    }
}

pub fn submit_feedback(
    invocation: &ProcessInvocation,
    cx: &Cx,
    location: LedgerLocation,
    req: FeedbackRequest,
) -> Result<FeedbackOutcome, FeedbackError> {
    let clock = invocation.clock();
    let child = cx.clone();
    let res = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let mut store = match open_blocking(clock, &child, LedgerAccess::ExistingOnly, location)
                .map_err(FeedbackError::Store)?
            {
                LedgerOpen::Ready(store) => *store,
                LedgerOpen::Disabled => {
                    return Err(FeedbackError::Store(StoreError::Permissions));
                }
                LedgerOpen::Missing => {
                    return Err(FeedbackError::Store(StoreError::Missing));
                }
                LedgerOpen::ReadOnly(_) => {
                    return Err(FeedbackError::Store(StoreError::Permissions));
                }
            };
            let stamp = store.stamp();
            let (outcome, _) = match req {
                FeedbackRequest::Paired(paired) => {
                    store.record_paired_correction(clock, &child, &paired, stamp)?
                }
                FeedbackRequest::Single(single) => {
                    store.record_single_feedback(clock, &child, &single, stamp)?
                }
            };
            Ok(outcome)
        },
    )
    .map_err(|e| FeedbackError::Store(StoreError::Runtime(e)))?;
    res.value
}

pub fn record_ranking(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
    event: &NewRankingEvent,
    candidates: &[NewRankingCandidate],
    snapshot: Option<&NewRosterSnapshot>,
) -> Result<bool, StoreError> {
    record_ranking_with_attempts(
        invocation,
        cx,
        access,
        location,
        event,
        candidates,
        snapshot,
        &[],
    )
}

/// Record a ranking event and the provider attempts it owns, in one transaction.
#[allow(clippy::too_many_arguments)]
pub fn record_ranking_with_attempts(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
    event: &NewRankingEvent,
    candidates: &[NewRankingCandidate],
    snapshot: Option<&NewRosterSnapshot>,
    attempts: &[NewProviderAttempt],
) -> Result<bool, StoreError> {
    if access == LedgerAccess::Disabled {
        return Ok(false);
    }
    let clock = invocation.clock();
    let child = cx.clone();
    let event = event.clone();
    let candidates = candidates.to_vec();
    let snapshot = snapshot.cloned();
    let attempts = attempts.to_vec();
    let res = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let mut store = match open_blocking(clock, &child, access, location)? {
                LedgerOpen::Ready(store) => *store,
                LedgerOpen::Disabled | LedgerOpen::Missing | LedgerOpen::ReadOnly(_) => {
                    return Ok(false);
                }
            };
            let stamp = store.stamp();
            store.record_ranking_event_with_attempts(
                clock,
                &child,
                &event,
                &candidates,
                snapshot.as_ref(),
                &attempts,
                stamp,
            )?;
            Ok(true)
        },
    )
    .map_err(StoreError::Runtime)?;
    res.value
}

/// Writes one attempt's admission, before anything has been sent on its behalf.
///
/// Called on the sending path rather than at settlement, so that an invocation killed
/// while waiting on the provider still leaves a row naming the attempt it had started.
/// A row written here claims only that the attempt was admitted: it carries no sent time
/// and no usage, because neither exists yet.
pub fn record_attempt_admission(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
    attempt: &NewProviderAttempt,
) -> Result<bool, StoreError> {
    if access == LedgerAccess::Disabled {
        return Ok(false);
    }
    let clock = invocation.clock();
    let child = cx.clone();
    let attempt = attempt.clone();
    let res = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let mut store = match open_blocking(clock, &child, access, location)? {
                LedgerOpen::Ready(store) => *store,
                LedgerOpen::Disabled | LedgerOpen::Missing | LedgerOpen::ReadOnly(_) => {
                    return Ok(false);
                }
            };
            let stamp = store.stamp();
            store.record_provider_attempt(clock, &child, &attempt, stamp)?;
            Ok(true)
        },
    )
    .map_err(StoreError::Runtime)?;
    res.value
}

/// Records that an already-admitted attempt's request reached the wire.
///
/// This is the transition that cannot be recovered afterwards: after it, the provider may
/// have been paid for this attempt whatever becomes of this process, and before it, it
/// certainly has not. See [`LedgerStore::mark_provider_attempt_sent`].
pub fn record_attempt_reached_wire(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
    attempt_id: &str,
    sent_at_unix_ms: u64,
) -> Result<bool, StoreError> {
    if access == LedgerAccess::Disabled {
        return Ok(false);
    }
    let clock = invocation.clock();
    let child = cx.clone();
    let attempt_id = attempt_id.to_string();
    let res = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let mut store = match open_blocking(clock, &child, access, location)? {
                LedgerOpen::Ready(store) => *store,
                LedgerOpen::Disabled | LedgerOpen::Missing | LedgerOpen::ReadOnly(_) => {
                    return Ok(false);
                }
            };
            let stamp = store.stamp();
            store.mark_provider_attempt_sent(clock, &child, &attempt_id, sent_at_unix_ms, stamp)?;
            Ok(true)
        },
    )
    .map_err(StoreError::Runtime)?;
    res.value
}

pub fn record_emission(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
    event_id: &str,
    bytes_written: usize,
) -> Result<bool, StoreError> {
    if access == LedgerAccess::Disabled {
        return Ok(false);
    }
    let clock = invocation.clock();
    let child = cx.clone();
    let event_id = event_id.to_string();
    let res = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let mut store = match open_blocking(clock, &child, access, location)? {
                LedgerOpen::Ready(store) => *store,
                LedgerOpen::Disabled | LedgerOpen::Missing | LedgerOpen::ReadOnly(_) => {
                    return Ok(false);
                }
            };
            let stamp = store.stamp();
            let (recorded, _) =
                store.record_emission(clock, &child, &event_id, bytes_written, stamp)?;
            Ok(recorded)
        },
    )
    .map_err(StoreError::Runtime)?;
    res.value
}

pub fn record_acknowledgment(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
    event_id: &str,
    verified_delivery_key: &str,
) -> Result<bool, StoreError> {
    if access == LedgerAccess::Disabled {
        return Ok(false);
    }
    let clock = invocation.clock();
    let child = cx.clone();
    let event_id = event_id.to_string();
    let verified_delivery_key = verified_delivery_key.to_string();
    let res = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let mut store = match open_blocking(clock, &child, access, location)? {
                LedgerOpen::Ready(store) => *store,
                LedgerOpen::Disabled | LedgerOpen::Missing | LedgerOpen::ReadOnly(_) => {
                    return Ok(false);
                }
            };
            let stamp = store.stamp();
            let (recorded, _) = store.record_acknowledgment(
                clock,
                &child,
                &event_id,
                &verified_delivery_key,
                stamp,
            )?;
            Ok(recorded)
        },
    )
    .map_err(StoreError::Runtime)?;
    res.value
}

pub fn record_observations_with_cursor(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
    observations: &[NewObservation],
    cursor: &SessionCursor,
    expected_cursor_gen: Option<u64>,
) -> Result<bool, StoreError> {
    if access == LedgerAccess::Disabled {
        return Err(StoreError::Permissions);
    }
    let clock = invocation.clock();
    let child = cx.clone();
    let observations = observations.to_vec();
    let cursor = cursor.clone();
    let res = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let mut store = match open_blocking(clock, &child, access, location)? {
                LedgerOpen::Ready(store) => *store,
                LedgerOpen::Missing => {
                    return Err(StoreError::Missing);
                }
                LedgerOpen::Disabled | LedgerOpen::ReadOnly(_) => {
                    return Err(StoreError::Permissions);
                }
            };
            let stamp = store.stamp();
            store.record_observations_with_cursor(
                clock,
                &child,
                &observations,
                &cursor,
                expected_cursor_gen,
                stamp,
            )?;
            Ok(true)
        },
    )
    .map_err(StoreError::Runtime)?;
    res.value
}

#[allow(clippy::too_many_arguments)]
pub fn get_session_cursor(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
    workspace_root: &str,
    session_id: &str,
    agent_branch: &str,
    kind: CursorKind,
) -> Result<Option<SessionCursor>, StoreError> {
    if access == LedgerAccess::Disabled {
        return Err(StoreError::Permissions);
    }
    let clock = invocation.clock();
    let child = cx.clone();
    let workspace_root = workspace_root.to_string();
    let session_id = session_id.to_string();
    let agent_branch = agent_branch.to_string();
    let res = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let store = match open_blocking(clock, &child, access, location)? {
                LedgerOpen::Ready(store) => *store,
                LedgerOpen::ReadOnly(store) => *store,
                LedgerOpen::Missing => {
                    return Err(StoreError::Missing);
                }
                LedgerOpen::Disabled => {
                    return Err(StoreError::Permissions);
                }
            };
            store.get_session_cursor(
                clock,
                &child,
                &workspace_root,
                &session_id,
                &agent_branch,
                kind,
            )
        },
    )
    .map_err(StoreError::Runtime)?;
    res.value
}

pub fn get_session_observations(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
    workspace_root: &str,
    session_id: &str,
) -> Result<Vec<NewObservation>, StoreError> {
    if access == LedgerAccess::Disabled {
        return Err(StoreError::Permissions);
    }
    let clock = invocation.clock();
    let child = cx.clone();
    let workspace_root = workspace_root.to_string();
    let session_id = session_id.to_string();
    let res = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let store = match open_blocking(clock, &child, access, location)? {
                LedgerOpen::Ready(store) => *store,
                LedgerOpen::ReadOnly(store) => *store,
                LedgerOpen::Missing => {
                    return Err(StoreError::Missing);
                }
                LedgerOpen::Disabled => {
                    return Err(StoreError::Permissions);
                }
            };
            store.get_session_observations(clock, &child, &workspace_root, &session_id)
        },
    )
    .map_err(StoreError::Runtime)?;
    res.value
}

#[allow(clippy::too_many_arguments)]
pub fn find_latest_preceding_emission(
    invocation: &ProcessInvocation,
    cx: &Cx,
    access: LedgerAccess,
    location: LedgerLocation,
    workspace_root: &str,
    session_id: &str,
    agent_branch: &str,
    observed_at_unix_ms: u64,
    max_window_ms: u64,
) -> Result<Option<String>, StoreError> {
    if access == LedgerAccess::Disabled {
        return Err(StoreError::Permissions);
    }
    let clock = invocation.clock();
    let child = cx.clone();
    let workspace_root = workspace_root.to_string();
    let session_id = session_id.to_string();
    let agent_branch = agent_branch.to_string();
    let res = run_blocking_leaf(
        invocation,
        cx,
        BlockingLeafKind::Database,
        false,
        move || {
            let store = match open_blocking(clock, &child, access, location)? {
                LedgerOpen::Ready(store) => *store,
                LedgerOpen::ReadOnly(store) => *store,
                LedgerOpen::Missing => {
                    return Err(StoreError::Missing);
                }
                LedgerOpen::Disabled => {
                    return Err(StoreError::Permissions);
                }
            };
            store.find_latest_preceding_emission(
                clock,
                &child,
                &workspace_root,
                &session_id,
                &agent_branch,
                observed_at_unix_ms,
                max_window_ms,
            )
        },
    )
    .map_err(StoreError::Runtime)?;
    res.value
}
