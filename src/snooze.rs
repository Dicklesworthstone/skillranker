//! Trusted, bounded, session-scoped advisory snoozes (I04).
//!
//! A snooze is a trusted-user configuration entry, never a usefulness label:
//! it withholds one skill, or every advisory candidate, from advisory selection
//! in one recorded event's workspace, session and agent branch until it
//! expires. Explicit skill requests still resolve. Ranking only reads the
//! file, even with the ledger or persistence disabled, and never writes expiry
//! cleanup; only an explicit `sr snooze --apply` rewrites it.
//!
//! An entry whose creation time lies in the future means the wall clock moved
//! backwards. Its expiry is uncertain, so it stays muted until the clock
//! passes it or the user clears it, and the anomaly is reported.

use serde::Deserialize;
use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Relative to the trusted user configuration root, beside `sr/config.toml`.
pub const SNOOZE_FILE: &str = "sr/snoozes.toml";
pub const MAX_SNOOZES: usize = 128;
pub const MIN_SNOOZE_MS: u64 = 60_000;
pub const MAX_SNOOZE_MS: u64 = 24 * 60 * 60_000;
pub const SNOOZE_SCHEMA_VERSION: u32 = 1;
/// Bound on each identity field; event and skill IDs are far shorter.
const MAX_FIELD_BYTES: usize = 4096;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnoozeError {
    /// The file is not a valid bounded snooze set.
    Malformed(String),
    /// The event has no verified session or agent-branch identity.
    Unattributed,
    InvalidDuration(String),
    TooMany,
    LockBusy,
    ExternalModification,
    Io(String),
}

impl fmt::Display for SnoozeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "snooze file is invalid: {why}"),
            Self::Unattributed => f.write_str(
                "the event has no verified session and agent-branch identity; \
                 a snooze cannot be scoped to it",
            ),
            Self::InvalidDuration(why) => write!(f, "invalid snooze duration: {why}"),
            Self::TooMany => write!(
                f,
                "at most {MAX_SNOOZES} unexpired snoozes may exist; clear a scope first"
            ),
            Self::LockBusy => f.write_str("another snooze change holds the lock"),
            Self::ExternalModification => {
                f.write_str("the snooze file changed during this update; nothing was written")
            }
            Self::Io(why) => write!(f, "snooze file could not be written: {why}"),
        }
    }
}

impl std::error::Error for SnoozeError {}

/// The workspace, session and agent branch a snooze applies to, exactly as
/// the ledger recorded them for the event.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SnoozeScope {
    pub workspace_root: String,
    pub session_id: String,
    pub agent_branch: String,
}

impl SnoozeScope {
    /// A recorded event's scope. The pipeline records a placeholder session
    /// (`session-0`) or branch (`unresolved`) when attribution is missing, and a
    /// pre-context failure has no context at all; none of them may carry a
    /// mute, which would otherwise cover every unattributed session.
    pub fn from_event(
        event_id: &str,
        workspace_root: &str,
        session_id: &str,
        agent_branch: &str,
    ) -> Result<Self, SnoozeError> {
        if event_id.starts_with("pre-context-")
            || session_id == "session-0"
            || agent_branch == "unresolved"
            || [workspace_root, session_id, agent_branch]
                .iter()
                .any(|field| field.is_empty() || field.len() > MAX_FIELD_BYTES)
        {
            return Err(SnoozeError::Unattributed);
        }
        Ok(Self {
            workspace_root: workspace_root.to_owned(),
            session_id: session_id.to_owned(),
            agent_branch: agent_branch.to_owned(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SnoozeEntry {
    pub event_id: String,
    pub workspace_root: String,
    pub session_id: String,
    pub agent_branch: String,
    /// `None` mutes every advisory candidate in the scope.
    #[serde(default)]
    pub skill_id: Option<String>,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

impl SnoozeEntry {
    fn in_scope(&self, scope: &SnoozeScope) -> bool {
        self.workspace_root == scope.workspace_root
            && self.session_id == scope.session_id
            && self.agent_branch == scope.agent_branch
    }

    pub fn status(&self, now_unix_ms: Option<u64>) -> EntryStatus {
        match now_unix_ms {
            // No readable clock: nothing can be shown to have expired.
            None => EntryStatus::UncertainExpiry,
            Some(now) if now < self.created_at_unix_ms => EntryStatus::UncertainExpiry,
            Some(now) if now < self.expires_at_unix_ms => EntryStatus::Active,
            Some(_) => EntryStatus::Expired,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryStatus {
    Active,
    /// Muted: the clock reads before the entry's creation, or is unreadable.
    UncertainExpiry,
    Expired,
}

impl EntryStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::UncertainExpiry => "uncertain-expiry",
            Self::Expired => "expired",
        }
    }
    pub const fn mutes(self) -> bool {
        !matches!(self, Self::Expired)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnoozeFile {
    schema_version: u32,
    #[serde(default)]
    snooze: Vec<SnoozeEntry>,
}

/// A validated snooze file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SnoozeSet {
    entries: Vec<SnoozeEntry>,
}

/// The advisory controls in effect for one scope at one time.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScopeControls {
    pub all: bool,
    pub skills: BTreeSet<String>,
    /// Muting entries whose expiry the clock cannot establish.
    pub uncertain_expiry: usize,
}

impl ScopeControls {
    pub fn is_empty(&self) -> bool {
        !self.all && self.skills.is_empty()
    }

    pub fn mutes(&self, skill_id: &str) -> bool {
        self.all || self.skills.contains(skill_id)
    }
}

impl SnoozeSet {
    /// Strictly decodes bounded TOML: unknown or duplicate keys, a foreign
    /// schema, more than the entry limit, out-of-range durations, oversized
    /// fields and two entries for the same scope and target are all refused.
    pub fn decode(text: &str) -> Result<Self, SnoozeError> {
        let file: SnoozeFile =
            toml::from_str(text).map_err(|_| SnoozeError::Malformed("not strict TOML".into()))?;
        if file.schema_version != SNOOZE_SCHEMA_VERSION {
            return Err(SnoozeError::Malformed("unsupported schema_version".into()));
        }
        if file.snooze.len() > MAX_SNOOZES {
            return Err(SnoozeError::Malformed(format!(
                "more than {MAX_SNOOZES} entries"
            )));
        }
        let mut targets = BTreeSet::new();
        for entry in &file.snooze {
            let fields = [
                entry.event_id.as_str(),
                entry.workspace_root.as_str(),
                entry.session_id.as_str(),
                entry.agent_branch.as_str(),
                entry.skill_id.as_deref().unwrap_or("-"),
            ];
            if fields
                .iter()
                .any(|field| field.is_empty() || field.len() > MAX_FIELD_BYTES)
            {
                return Err(SnoozeError::Malformed("empty or oversized field".into()));
            }
            let duration = entry
                .expires_at_unix_ms
                .saturating_sub(entry.created_at_unix_ms);
            if !(MIN_SNOOZE_MS..=MAX_SNOOZE_MS).contains(&duration) {
                return Err(SnoozeError::Malformed("duration out of range".into()));
            }
            if !targets.insert(target_key(entry)) {
                return Err(SnoozeError::Malformed(
                    "two entries for one scope and target".into(),
                ));
            }
        }
        Ok(Self {
            entries: file.snooze,
        })
    }

    pub fn entries(&self) -> &[SnoozeEntry] {
        &self.entries
    }

    pub fn controls(&self, scope: &SnoozeScope, now_unix_ms: Option<u64>) -> ScopeControls {
        let mut controls = ScopeControls::default();
        for entry in self.entries.iter().filter(|entry| entry.in_scope(scope)) {
            let status = entry.status(now_unix_ms);
            if !status.mutes() {
                continue;
            }
            if status == EntryStatus::UncertainExpiry {
                controls.uncertain_expiry += 1;
            }
            match &entry.skill_id {
                Some(skill) => {
                    controls.skills.insert(skill.clone());
                }
                None => controls.all = true,
            }
        }
        controls
    }

    /// Canonical file bytes. Hand-encoded: only the parse half of `toml` is
    /// compiled in, and the grammar used here is small.
    pub fn encode(&self) -> String {
        let mut out = format!(
            "# Managed by `sr snooze`; trusted advisory controls, not usefulness labels.\n\
             schema_version = {SNOOZE_SCHEMA_VERSION}\n"
        );
        for entry in &self.entries {
            out.push_str("\n[[snooze]]\n");
            for (key, value) in [
                ("event_id", Some(&entry.event_id)),
                ("workspace_root", Some(&entry.workspace_root)),
                ("session_id", Some(&entry.session_id)),
                ("agent_branch", Some(&entry.agent_branch)),
                ("skill_id", entry.skill_id.as_ref()),
            ] {
                if let Some(value) = value {
                    out.push_str(&format!("{key} = {}\n", toml_string(value)));
                }
            }
            out.push_str(&format!(
                "created_at_unix_ms = {}\nexpires_at_unix_ms = {}\n",
                entry.created_at_unix_ms, entry.expires_at_unix_ms
            ));
        }
        out
    }
}

fn target_key(entry: &SnoozeEntry) -> (String, String, String, Option<String>) {
    (
        entry.workspace_root.clone(),
        entry.session_id.clone(),
        entry.agent_branch.clone(),
        entry.skill_id.clone(),
    )
}

fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `90s`, `30m`, `2h`: a positive integer and one unit, within 1 minute to
/// 24 hours.
pub fn parse_duration(text: &str) -> Result<u64, SnoozeError> {
    let bad = || SnoozeError::InvalidDuration(format!("'{text}'; use e.g. 30m or 2h"));
    let split = text
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .ok_or_else(bad)?;
    let (digits, unit) = text.split_at(split);
    let amount: u64 = digits.parse().map_err(|_| bad())?;
    let scale = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return Err(bad()),
    };
    let millis = amount.checked_mul(scale).ok_or_else(bad)?;
    if !(MIN_SNOOZE_MS..=MAX_SNOOZE_MS).contains(&millis) {
        return Err(SnoozeError::InvalidDuration(format!(
            "'{text}' is outside 1m to 24h"
        )));
    }
    Ok(millis)
}

/// The wall clock in Unix milliseconds, or `None` when it reads before 1970.
pub fn wall_clock_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnoozeChange {
    Snooze {
        event_id: String,
        scope: SnoozeScope,
        /// `None` for every advisory candidate.
        skill_id: Option<String>,
        duration_ms: u64,
    },
    Clear {
        scope: SnoozeScope,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedChange {
    pub next: SnoozeSet,
    /// An entry for the same scope and target was replaced.
    pub renewed: bool,
    pub cleared: usize,
    /// Expired entries this explicit write drops.
    pub expired_pruned: usize,
    pub expires_at_unix_ms: Option<u64>,
}

/// The file after `change`, pruning entries that have certainly expired. It
/// needs a readable clock: an expiry cannot be computed from an unknown now.
pub fn plan(
    current: &SnoozeSet,
    change: &SnoozeChange,
    now_unix_ms: u64,
) -> Result<PlannedChange, SnoozeError> {
    let mut entries = Vec::with_capacity(current.entries.len() + 1);
    let mut expired_pruned = 0;
    for entry in &current.entries {
        if entry.status(Some(now_unix_ms)) == EntryStatus::Expired {
            expired_pruned += 1;
        } else {
            entries.push(entry.clone());
        }
    }
    let mut planned = PlannedChange {
        next: SnoozeSet::default(),
        renewed: false,
        cleared: 0,
        expired_pruned,
        expires_at_unix_ms: None,
    };
    match change {
        SnoozeChange::Clear { scope } => {
            let before = entries.len();
            entries.retain(|entry| !entry.in_scope(scope));
            planned.cleared = before - entries.len();
        }
        SnoozeChange::Snooze {
            event_id,
            scope,
            skill_id,
            duration_ms,
        } => {
            if !(MIN_SNOOZE_MS..=MAX_SNOOZE_MS).contains(duration_ms) {
                return Err(SnoozeError::InvalidDuration("outside 1m to 24h".into()));
            }
            let expires = now_unix_ms
                .checked_add(*duration_ms)
                .ok_or_else(|| SnoozeError::InvalidDuration("clock overflow".into()))?;
            let before = entries.len();
            entries.retain(|entry| !(entry.in_scope(scope) && &entry.skill_id == skill_id));
            planned.renewed = entries.len() != before;
            if entries.len() >= MAX_SNOOZES {
                return Err(SnoozeError::TooMany);
            }
            entries.push(SnoozeEntry {
                event_id: event_id.clone(),
                workspace_root: scope.workspace_root.clone(),
                session_id: scope.session_id.clone(),
                agent_branch: scope.agent_branch.clone(),
                skill_id: skill_id.clone(),
                created_at_unix_ms: now_unix_ms,
                expires_at_unix_ms: expires,
            });
            planned.expires_at_unix_ms = Some(expires);
        }
    }
    planned.next = SnoozeSet { entries };
    Ok(planned)
}

/// Serializes cooperating writers with an advisory `flock` on a persistent
/// sibling lock file, as hook installation does.
struct SnoozeLock {
    _flock: nix::fcntl::Flock<File>,
}

impl SnoozeLock {
    fn acquire(file: &Path) -> Result<Self, SnoozeError> {
        let lock_path = file.with_extension("toml.sr-lock");
        let handle = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| SnoozeError::Io(e.kind().to_string()))?;
        let flock = nix::fcntl::Flock::lock(handle, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|_| SnoozeError::LockBusy)?;
        Ok(Self { _flock: flock })
    }
}

fn read_existing(file: &Path) -> Result<Option<Vec<u8>>, SnoozeError> {
    match fs::symlink_metadata(file) {
        Ok(meta) if !meta.file_type().is_file() => Err(SnoozeError::Malformed(
            "the snooze path is not a regular file".into(),
        )),
        Ok(meta) if meta.len() > crate::limits::CONFIG_FILE_BYTES.max() as u64 => Err(
            SnoozeError::Malformed("file exceeds the configuration bound".into()),
        ),
        Ok(_) => fs::read(file)
            .map(Some)
            .map_err(|e| SnoozeError::Io(e.kind().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SnoozeError::Io(e.kind().to_string())),
    }
}

pub fn decode_bytes(bytes: &[u8]) -> Result<SnoozeSet, SnoozeError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| SnoozeError::Malformed("not UTF-8".into()))?;
    SnoozeSet::decode(text)
}

/// The current snooze set under `user_root`, for previews and doctor.
pub fn load(user_root: &Path) -> Result<SnoozeSet, SnoozeError> {
    match read_existing(&user_root.join(SNOOZE_FILE))? {
        Some(bytes) => decode_bytes(&bytes),
        None => Ok(SnoozeSet::default()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Applied {
    pub planned: PlannedChange,
    pub backup: Option<PathBuf>,
}

/// Applies `change` under the lock: read, plan, back up the previous bytes
/// privately, write a sibling temporary file, then re-read the target and
/// replace it only if no external editor changed it meanwhile. Atomic rename
/// is not compare-and-swap; an edit landing between that re-read and the
/// rename is not detected, and cooperating `sr` writers are serialized by the
/// lock instead.
pub fn apply(
    user_root: &Path,
    change: &SnoozeChange,
    now_unix_ms: u64,
) -> Result<Applied, SnoozeError> {
    let file = user_root.join(SNOOZE_FILE);
    let parent = file
        .parent()
        .ok_or_else(|| SnoozeError::Io("no parent directory".into()))?;
    create_private_dir(parent)?;
    let _lock = SnoozeLock::acquire(&file)?;
    let original = read_existing(&file)?;
    let current = match &original {
        Some(bytes) => decode_bytes(bytes)?,
        None => SnoozeSet::default(),
    };
    let planned = plan(&current, change, now_unix_ms)?;
    let backup = match &original {
        Some(bytes) => Some(
            crate::installer::write_backup(&file, bytes)
                .map_err(|e| SnoozeError::Io(e.to_string()))?,
        ),
        None => None,
    };
    let tmp = parent.join(format!(
        "snoozes.toml.sr-tmp.{}_{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let write = || -> std::io::Result<()> {
        let mut handle = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        handle.write_all(planned.next.encode().as_bytes())?;
        handle.sync_all()
    };
    write().map_err(|e| SnoozeError::Io(e.kind().to_string()))?;
    let replace = || -> Result<(), SnoozeError> {
        if read_existing(&file)? != original {
            return Err(SnoozeError::ExternalModification);
        }
        fs::rename(&tmp, &file).map_err(|e| SnoozeError::Io(e.kind().to_string()))
    };
    if let Err(error) = replace() {
        // Our own never-published temporary file, not user data.
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(Applied { planned, backup })
}

fn create_private_dir(dir: &Path) -> Result<(), SnoozeError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| SnoozeError::Io(e.kind().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(session: &str, branch: &str) -> SnoozeScope {
        SnoozeScope::from_event("ev", "/w", session, branch).unwrap()
    }

    fn snooze(skill: Option<&str>, session: &str) -> SnoozeChange {
        SnoozeChange::Snooze {
            event_id: "ev".into(),
            scope: scope(session, "main"),
            skill_id: skill.map(str::to_owned),
            duration_ms: 30 * 60_000,
        }
    }

    #[test]
    fn unattributed_events_cannot_carry_a_mute() {
        for (event, session, branch) in [
            ("pre-context-1", "s", "main"),
            ("ev", "session-0", "main"),
            ("ev", "s", "unresolved"),
            ("ev", "", "main"),
        ] {
            assert_eq!(
                SnoozeScope::from_event(event, "/w", session, branch),
                Err(SnoozeError::Unattributed)
            );
        }
        assert!(SnoozeScope::from_event("ev", "/w", "s", "main").is_ok());
    }

    #[test]
    fn a_skill_snooze_mutes_only_its_scope_until_expiry() {
        let now = 1_000_000;
        let planned = plan(&SnoozeSet::default(), &snooze(Some("a"), "s1"), now).unwrap();
        let set = SnoozeSet::decode(&planned.next.encode()).unwrap();
        assert_eq!(set, planned.next, "encode/decode round trip");
        let here = set.controls(&scope("s1", "main"), Some(now + 1));
        assert!(here.mutes("a") && !here.mutes("b") && !here.all);
        assert!(set.controls(&scope("s2", "main"), Some(now)).is_empty());
        assert!(set.controls(&scope("s1", "sub"), Some(now)).is_empty());
        assert!(
            set.controls(&scope("s1", "main"), Some(now + 30 * 60_000))
                .is_empty(),
            "expired at its expiry instant"
        );
    }

    #[test]
    fn a_clock_behind_creation_keeps_the_mute_and_says_so() {
        let now = 10_000_000;
        let set = plan(&SnoozeSet::default(), &snooze(None, "s1"), now)
            .unwrap()
            .next;
        for clock in [Some(now - 1), None] {
            let controls = set.controls(&scope("s1", "main"), clock);
            assert!(controls.all);
            assert_eq!(controls.uncertain_expiry, 1);
        }
    }

    #[test]
    fn renewing_replaces_and_clearing_removes_only_that_scope() {
        let now = 1_000_000;
        let one = plan(&SnoozeSet::default(), &snooze(Some("a"), "s1"), now)
            .unwrap()
            .next;
        let two = plan(&one, &snooze(Some("a"), "s2"), now).unwrap().next;
        let renewed = plan(&two, &snooze(Some("a"), "s1"), now + 5).unwrap();
        assert!(renewed.renewed);
        assert_eq!(renewed.next.entries().len(), 2);
        let cleared = plan(
            &renewed.next,
            &SnoozeChange::Clear {
                scope: scope("s1", "main"),
            },
            now + 6,
        )
        .unwrap();
        assert_eq!(cleared.cleared, 1);
        assert!(
            cleared
                .next
                .controls(&scope("s2", "main"), Some(now + 7))
                .mutes("a")
        );
    }

    #[test]
    fn the_entry_limit_counts_only_unexpired_entries() {
        let now = 1_000_000;
        let mut set = SnoozeSet::default();
        for i in 0..MAX_SNOOZES {
            set = plan(&set, &snooze(Some(&format!("s{i}")), "s1"), now)
                .unwrap()
                .next;
        }
        assert_eq!(
            plan(&set, &snooze(Some("extra"), "s1"), now),
            Err(SnoozeError::TooMany)
        );
        let later = now + 31 * 60_000;
        let pruned = plan(&set, &snooze(Some("extra"), "s1"), later).unwrap();
        assert_eq!(pruned.expired_pruned, MAX_SNOOZES);
        assert_eq!(pruned.next.entries().len(), 1);
    }

    #[test]
    fn strict_decoding_refuses_malformed_files() {
        let entry = "[[snooze]]\nevent_id = \"e\"\nworkspace_root = \"/w\"\nsession_id = \"s\"\n\
                     agent_branch = \"main\"\ncreated_at_unix_ms = 0\nexpires_at_unix_ms = 60000\n";
        assert!(SnoozeSet::decode(&format!("schema_version = 1\n{entry}")).is_ok());
        for bad in [
            format!("schema_version = 2\n{entry}"),
            format!("schema_version = 1\n{entry}{entry}"),
            format!("schema_version = 1\n{entry}color = \"red\"\n"),
            "schema_version = 1\nschema_version = 1\n".to_owned(),
            format!("schema_version = 1\n{}", entry.replace("60000", "59999")),
            format!("schema_version = 1\n{}", entry.replace("60000", "86400001")),
        ] {
            assert!(
                matches!(SnoozeSet::decode(&bad), Err(SnoozeError::Malformed(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn durations_are_bounded() {
        assert_eq!(parse_duration("30m"), Ok(1_800_000));
        assert_eq!(parse_duration("24h"), Ok(MAX_SNOOZE_MS));
        assert_eq!(parse_duration("60s"), Ok(MIN_SNOOZE_MS));
        for bad in ["59s", "25h", "0m", "m", "30", "1d", "-5m", "1h30m", ""] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn strings_with_quotes_and_controls_round_trip() {
        let set = SnoozeSet {
            entries: vec![SnoozeEntry {
                event_id: "e\"v\\1".into(),
                workspace_root: "/w/\u{7}tab\t".into(),
                session_id: "s".into(),
                agent_branch: "main".into(),
                skill_id: Some("é-skill".into()),
                created_at_unix_ms: 0,
                expires_at_unix_ms: 60_000,
            }],
        };
        assert_eq!(SnoozeSet::decode(&set.encode()).unwrap(), set);
    }
}
