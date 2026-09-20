//! Qualified Linux observation ledger storage.
//!
//! Stores execution events, session cursors, roster snapshots, candidate scoring,
//! provider attempts, observations, judgments, feedback proposals, and calibrations
//! under `$XDG_DATA_HOME/sr/ledger.sqlite3` with strict owner-only permissions (0o700 dir, 0o600 file).
//!
//! Every mutation is fenced by store incarnation, schema generation, and data generation.

use crate::blocking::{BlockingLeafKind, remaining_busy_wait, run_blocking_leaf};
use crate::runtime::{EntryClock, ProcessInvocation};
use crate::storage::{EngineIdentity, StoreError, check_work, linked_engine};
use asupersync::Cx;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat, mkdirat};
use nix::sys::statfs::{
    BTRFS_SUPER_MAGIC, EXT4_SUPER_MAGIC, FsType, TMPFS_MAGIC, XFS_SUPER_MAGIC, fstatfs,
};
use nix::sys::statvfs::fstatvfs;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, config::DbConfig, limits::Limit,
    params,
};
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
pub const MAX_BUSY_WAIT_MS: u64 = 25;

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

#[derive(Clone, Copy, Eq, PartialEq)]
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
    Ready(Box<LedgerStore>),
}

pub struct LedgerStore {
    connection: Connection,
    directory: PrivateLedgerDirectory,
    stamp: LedgerStamp,
    engine: EngineIdentity,
}

impl fmt::Debug for LedgerStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LedgerStore")
            .field("engine", &self.engine)
            .field("stamp", &self.stamp)
            .finish_non_exhaustive()
    }
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewJudgment {
    pub judgment_id: String,
    pub attributed_event_id: String,
    pub skill_id: String,
    pub label: JudgmentLabel,
    pub label_version: u32,
    pub provenance: String,
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
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

fn local_filesystem(kind: FsType) -> bool {
    matches!(
        kind,
        EXT4_SUPER_MAGIC | BTRFS_SUPER_MAGIC | XFS_SUPER_MAGIC | TMPFS_MAGIC
    )
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
    identity: (u64, u64),
}

impl PrivateLedgerDirectory {
    pub fn open(
        path: PathBuf,
        create: bool,
        clock: EntryClock,
        cx: &Cx,
    ) -> Result<Self, StoreError> {
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
                    if !local_filesystem(fstatfs(&handle).map_err(io_error)?.filesystem_type()) {
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
        };
        directory.admit_space()?;
        Ok(directory)
    }

    pub fn database_path(&self) -> PathBuf {
        self.path.join(LEDGER_FILE)
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
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let name = format!("{LEDGER_FILE}{suffix}");
            match fstatat(&self.handle, name.as_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => {
                    owned_regular(&stat, uid)?;
                    bytes = bytes
                        .checked_add(u64::try_from(stat.st_size).map_err(|_| StoreError::Quota)?)
                        .ok_or(StoreError::Quota)?;
                }
                Err(Errno::ENOENT) => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        if bytes > LEDGER_QUOTA_BYTES - LEDGER_MAINTENANCE_RESERVE_BYTES {
            return Err(StoreError::Quota);
        }
        Ok(bytes)
    }

    pub fn admit_space(&self) -> Result<(), StoreError> {
        if !local_filesystem(fstatfs(&self.handle).map_err(io_error)?.filesystem_type()) {
            return Err(StoreError::UnsupportedFilesystem);
        }
        let bytes = self.inspect_files()?;
        let stat = fstatvfs(&self.handle).map_err(io_error)?;
        let available = u128::from(stat.blocks_available()) * u128::from(stat.fragment_size());
        recording_capacity(bytes, available)
    }

    pub fn open_database_file(
        &self,
        create: bool,
        clock: EntryClock,
        cx: &Cx,
    ) -> Result<File, StoreError> {
        self.revalidate(clock, cx)?;
        self.admit_space()?;
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
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn schema_version(connection: &Connection) -> Result<i64, StoreError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > i64::from(LEDGER_SCHEMA_VERSION) {
        return Err(StoreError::NewerSchema { version });
    }
    Ok(version)
}

fn read_stamp(connection: &Connection) -> Result<LedgerStamp, StoreError> {
    if schema_version(connection)? != i64::from(LEDGER_SCHEMA_VERSION) {
        return Err(StoreError::IncompatibleSchema);
    }
    let app: i64 = connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if app != LEDGER_APPLICATION_ID {
        return Err(StoreError::WrongStore);
    }
    let objects: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*'",
        [],
        |row| row.get(0),
    )?;
    if objects != TABLES.len() as i64 {
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
    if schema_gen < 1 || data_gen < 1 || schema != LEDGER_SCHEMA_ID {
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

fn initialize(connection: &mut Connection, clock: EntryClock, cx: &Cx) -> Result<(), StoreError> {
    configure(connection, clock, cx)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_work(clock, cx)?;
    if schema_version(&tx)? == i64::from(LEDGER_SCHEMA_VERSION) {
        read_stamp(&tx)?;
        return Ok(());
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
    Ok(())
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
    let directory =
        PrivateLedgerDirectory::open(path, access == LedgerAccess::Initialize, clock, cx)?;
    let file = directory.open_database_file(access == LedgerAccess::Initialize, clock, cx)?;
    let database_path = directory.database_path();
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | if access == LedgerAccess::Initialize {
            OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::empty()
        };
    let mut connection = Connection::open_with_flags(&database_path, flags)?;
    drop(file);
    if access == LedgerAccess::Initialize {
        initialize(&mut connection, clock, cx)?;
    } else {
        configure(&connection, clock, cx)?;
    }
    let stamp = read_stamp(&connection)?;
    Ok(LedgerOpen::Ready(Box::new(LedgerStore {
        connection,
        directory,
        stamp,
        engine,
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

// -----------------------------------------------------------------------------
// LedgerStore Implementations
// -----------------------------------------------------------------------------

impl LedgerStore {
    pub fn stamp(&self) -> LedgerStamp {
        self.stamp
    }

    pub fn engine(&self) -> &EngineIdentity {
        &self.engine
    }

    pub fn database_path(&self) -> PathBuf {
        self.directory.database_path()
    }

    /// Advances the data generation and deletes mutable history, fencing stale writers.
    pub fn clear(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        expected_stamp: LedgerStamp,
    ) -> Result<LedgerStamp, StoreError> {
        check_work(clock, cx)?;
        self.directory.revalidate(clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

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
        check_work(clock, cx)?;
        self.directory.revalidate(clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        tx.execute(
            "INSERT OR IGNORE INTO roster_snapshots (
                snapshot_id, workspace_root, adapter, total_candidates,
                eligible_candidates, membership_coverage, members_json, created_at_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                snapshot.snapshot_id,
                snapshot.workspace_root,
                snapshot.adapter,
                snapshot.total_candidates as i64,
                snapshot.eligible_candidates as i64,
                snapshot.membership_coverage.as_str(),
                snapshot.members_json,
                snapshot.created_at_unix_ms as i64,
            ],
        )?;

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
        check_work(clock, cx)?;
        self.directory.revalidate(clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        if let Some(snap) = snapshot {
            tx.execute(
                "INSERT OR IGNORE INTO roster_snapshots (
                    snapshot_id, workspace_root, adapter, total_candidates,
                    eligible_candidates, membership_coverage, members_json, created_at_unix_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    snap.snapshot_id,
                    snap.workspace_root,
                    snap.adapter,
                    snap.total_candidates as i64,
                    snap.eligible_candidates as i64,
                    snap.membership_coverage.as_str(),
                    snap.members_json,
                    snap.created_at_unix_ms as i64,
                ],
            )?;
        }

        tx.execute(
            "INSERT INTO ranking_events (
                event_id, verified_delivery_key, workspace_root, session_id,
                agent_branch, mode_channel, policy_version, schema_version,
                decision, reason, exposure_state, elapsed_ms, created_at_unix_ms,
                input_tokens, output_tokens, snapshot_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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

        for cand in candidates {
            tx.execute(
                "INSERT INTO ranking_candidates (
                    event_id, stage, skill_id, skill_version, raw_probability,
                    normalized_probability, fit_score, rank_score, rank_position,
                    excluded, exclusion_reason
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
        check_work(clock, cx)?;
        self.directory.revalidate(clock, cx)?;
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

        Ok(cursor)
    }

    pub fn record_provider_attempt(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        attempt: &NewProviderAttempt,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        check_work(clock, cx)?;
        self.directory.revalidate(clock, cx)?;
        self.directory.admit_space()?;
        refresh_busy_limit(&self.connection, clock, cx)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_stamp(&tx, expected_stamp)?;

        tx.execute(
            "INSERT INTO provider_attempts (
                attempt_id, owner_event_id, stage, request_fingerprint,
                admitted_at_unix_ms, sent_at_unix_ms, completed_at_unix_ms,
                status, input_tokens, output_tokens, http_status, error_kind
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
        check_work(clock, cx)?;
        self.directory.revalidate(clock, cx)?;
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
        check_work(clock, cx)?;
        self.directory.revalidate(clock, cx)?;
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

        tx.commit()?;
        Ok(())
    }

    pub fn record_judgment(
        &mut self,
        clock: EntryClock,
        cx: &Cx,
        judgment: &NewJudgment,
        expected_stamp: LedgerStamp,
    ) -> Result<(), StoreError> {
        check_work(clock, cx)?;
        self.directory.revalidate(clock, cx)?;
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
        check_work(clock, cx)?;
        self.directory.revalidate(clock, cx)?;
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
        check_work(clock, cx)?;
        self.directory.revalidate(clock, cx)?;
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

        tx.commit()?;
        Ok(())
    }
}
