//! Roster snapshots and drift comparison (P2). A snapshot records one resolved
//! roster's membership, status, restrictions and content identity in a
//! workspace/adapter/source namespace. It stores no path: a saved manifest is
//! evidence, never permission to read anything or to restore a removed skill.
//!
//! A diff compares a saved snapshot with fresh authorized discovery in the
//! same namespace. A record missing from a source the fresh scan could not
//! fully observe is an unconfirmed removal, never a confirmed deletion.

use super::discovery::Diagnostic;
use super::inspect::{Listing, listing};
use super::resolution::{ResolutionError, ResolvedRoster};
use crate::authorized_read::{AuthorizedRoot, AuthorizedRoots, ReadError};
use crate::identity::ContentHash;
use crate::limits::{EXPLICIT_ROSTER_JSON_BYTES, EXPLICIT_ROSTER_RECORDS};
use crate::output::ErrorKind;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub const SNAPSHOT_SCHEMA: &str = "sr.roster-snapshot.v1";
pub const DIFF_SCHEMA: &str = "sr.roster-diff.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotError {
    /// The manifest is too large, malformed, or has unknown or duplicate keys.
    Malformed,
    TooLarge,
    TooManyRecords,
    Unreadable,
    /// A different schema, harness, workspace or source namespace.
    Incompatible,
}

impl SnapshotError {
    pub const fn kind(self) -> ErrorKind {
        match self {
            Self::Malformed => ErrorKind::MalformedInput,
            Self::TooLarge | Self::TooManyRecords => ErrorKind::OversizedInput,
            Self::Unreadable => ErrorKind::UnsupportedInput,
            Self::Incompatible => ErrorKind::UnusableRoster,
        }
    }
}

/// Where a roster was observed. Paths are reduced to digests.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Namespace {
    pub harness: String,
    pub workspace: String,
    pub home: Option<String>,
}

impl Namespace {
    /// `workspace` and `home` should be canonical absolute paths.
    pub fn new(harness: &str, workspace: &Path, home: Option<&Path>) -> Self {
        let digest = |path: &Path| {
            ContentHash::from_bytes(path.as_os_str().as_bytes())
                .as_str()
                .to_owned()
        };
        Self {
            harness: harness.to_owned(),
            workspace: digest(workspace),
            home: home.map(digest),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRecord {
    pub skill_id: String,
    pub invocation_name: String,
    pub source: String,
    pub status: String,
    pub agent_invocable: bool,
    pub user_invocable: bool,
    pub content_hash: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema: String,
    pub namespace: Namespace,
    pub snapshot: String,
    pub partial: bool,
    /// Sources this scan could not fully observe.
    pub incomplete_sources: Vec<String>,
    /// A global walk limit or unreadable record left every source incomplete.
    pub all_sources_incomplete: bool,
    pub records: Vec<SnapshotRecord>,
}

/// Coverage of one scan: which sources it could not fully observe.
fn coverage(roster: &ResolvedRoster) -> (BTreeSet<String>, bool) {
    let mut incomplete = BTreeSet::new();
    let mut all = false;
    for diagnostic in roster.source_diagnostics() {
        match diagnostic {
            Diagnostic::RootMissing(_) => {}
            Diagnostic::RootUnreadable(source)
            | Diagnostic::SourceNotEnumerated(source)
            | Diagnostic::DirectoryUnreadable(source)
            | Diagnostic::SymlinkedDirectorySkipped(source)
            | Diagnostic::DepthLimitReached(source)
            | Diagnostic::EntryUnreadable(source) => {
                incomplete.insert(source.as_str().to_owned());
            }
            Diagnostic::EntryLimitReached | Diagnostic::ByteLimitReached => all = true,
        }
    }
    // A record that could not be read might belong to any source.
    all |= roster.diagnostics().iter().any(|(_, error)| {
        matches!(
            error,
            ResolutionError::Read | ResolutionError::ChangedFile | ResolutionError::Limit
        )
    });
    (incomplete, all)
}

pub fn capture(roster: &ResolvedRoster, namespace: Namespace) -> Snapshot {
    let listing: Listing = listing(roster);
    let (incomplete, all) = coverage(roster);
    Snapshot {
        schema: SNAPSHOT_SCHEMA.to_owned(),
        namespace,
        snapshot: listing.evidence().snapshot.as_str().to_owned(),
        partial: listing.evidence().partial,
        incomplete_sources: incomplete.into_iter().collect(),
        all_sources_incomplete: all,
        records: listing
            .records()
            .iter()
            .map(|r| SnapshotRecord {
                skill_id: r.id.as_str().to_owned(),
                invocation_name: r.invocation.clone(),
                source: r.source.clone(),
                status: r.status.to_owned(),
                agent_invocable: r.agent_invocable,
                user_invocable: r.user_invocable,
                content_hash: r.content_hash.clone(),
            })
            .collect(),
    }
}

impl Snapshot {
    /// Serialized manifest, within the explicit-roster bounds.
    pub fn to_bytes(&self) -> Result<Vec<u8>, SnapshotError> {
        if self.records.len() > EXPLICIT_ROSTER_RECORDS.max() {
            return Err(SnapshotError::TooManyRecords);
        }
        let mut bytes = serde_json::to_vec_pretty(self).map_err(|_| SnapshotError::Malformed)?;
        bytes.push(b'\n');
        if bytes.len() > EXPLICIT_ROSTER_JSON_BYTES.max() {
            return Err(SnapshotError::TooLarge);
        }
        Ok(bytes)
    }

    /// Parse a saved manifest. Unknown or duplicate keys (including any path
    /// field) and a different schema are refused.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SnapshotError> {
        if bytes.len() > EXPLICIT_ROSTER_JSON_BYTES.max() {
            return Err(SnapshotError::TooLarge);
        }
        let value = crate::adapter::decode_json(bytes, EXPLICIT_ROSTER_JSON_BYTES.max())
            .map_err(|_| SnapshotError::Malformed)?;
        let count = value
            .get("records")
            .and_then(|records| records.as_array())
            .map_or(0, Vec::len);
        if count > EXPLICIT_ROSTER_RECORDS.max() {
            return Err(SnapshotError::TooManyRecords);
        }
        let snapshot: Self = serde_json::from_value(value).map_err(|_| SnapshotError::Malformed)?;
        if snapshot.schema != SNAPSHOT_SCHEMA {
            return Err(SnapshotError::Incompatible);
        }
        let mut ids = BTreeSet::new();
        if !snapshot
            .records
            .iter()
            .all(|r| ids.insert(r.skill_id.as_str()))
        {
            return Err(SnapshotError::Malformed);
        }
        Ok(snapshot)
    }
}

/// Read a saved manifest as a bounded regular file under its own directory.
pub fn read_snapshot_file(path: &Path) -> Result<Snapshot, SnapshotError> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(SnapshotError::Unreadable);
    };
    let root = AuthorizedRoot::open_absolute(parent).map_err(|_| SnapshotError::Unreadable)?;
    let read = AuthorizedRoots::single(root)
        .read_bounded(0, Path::new(name), EXPLICIT_ROSTER_JSON_BYTES)
        .map_err(|error| match error {
            ReadError::TooLarge { .. } => SnapshotError::TooLarge,
            _ => SnapshotError::Unreadable,
        })?;
    Snapshot::from_bytes(read.bytes())
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Diff {
    pub schema: &'static str,
    pub old_snapshot: String,
    pub new_snapshot: String,
    /// Both scans observed every source they declare.
    pub complete: bool,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Missing now, but its source was not fully observed by one of the scans.
    pub unconfirmed_removals: Vec<String>,
    /// `(old invocation, new invocation)` pairs with identical content.
    pub renamed: Vec<(String, String)>,
    pub content_changed: Vec<String>,
    pub restriction_changed: Vec<String>,
    pub status_changed: Vec<(String, String, String)>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.unconfirmed_removals.is_empty()
            && self.renamed.is_empty()
            && self.content_changed.is_empty()
            && self.restriction_changed.is_empty()
            && self.status_changed.is_empty()
    }
}

/// Compare a saved snapshot with a fresh one from the same namespace.
/// Records are identified by skill ID and reported by invocation name.
pub fn diff(old: &Snapshot, new: &Snapshot) -> Result<Diff, SnapshotError> {
    if old.schema != new.schema || old.namespace != new.namespace {
        return Err(SnapshotError::Incompatible);
    }
    let before: BTreeMap<&str, &SnapshotRecord> = old
        .records
        .iter()
        .map(|r| (r.skill_id.as_str(), r))
        .collect();
    let after: BTreeMap<&str, &SnapshotRecord> = new
        .records
        .iter()
        .map(|r| (r.skill_id.as_str(), r))
        .collect();
    // A removal is confirmed only if the fresh scan fully observed its source.
    let unobserved = |source: &str| {
        new.all_sources_incomplete || new.incomplete_sources.iter().any(|s| s == source)
    };
    let mut result = Diff {
        schema: DIFF_SCHEMA,
        old_snapshot: old.snapshot.clone(),
        new_snapshot: new.snapshot.clone(),
        complete: !(old.all_sources_incomplete
            || new.all_sources_incomplete
            || !old.incomplete_sources.is_empty()
            || !new.incomplete_sources.is_empty()),
        ..Diff::default()
    };
    let mut removed: Vec<&SnapshotRecord> = Vec::new();
    let mut added: Vec<&SnapshotRecord> = Vec::new();
    for (id, record) in &before {
        match after.get(id) {
            None => removed.push(record),
            Some(now) => {
                if now.content_hash != record.content_hash {
                    result.content_changed.push(now.invocation_name.clone());
                }
                if (now.agent_invocable, now.user_invocable)
                    != (record.agent_invocable, record.user_invocable)
                {
                    result.restriction_changed.push(now.invocation_name.clone());
                }
                if now.status != record.status {
                    result.status_changed.push((
                        now.invocation_name.clone(),
                        record.status.clone(),
                        now.status.clone(),
                    ));
                }
            }
        }
    }
    for (id, record) in &after {
        if !before.contains_key(id) {
            added.push(record);
        }
    }
    // A removal and an addition with the same content in the same source is
    // a rename; pair each at most once, in stable order.
    let mut claimed = BTreeSet::new();
    for gone in removed {
        let rename = added.iter().position(|now| {
            now.content_hash == gone.content_hash
                && now.source == gone.source
                && !claimed.contains(&now.skill_id)
        });
        match rename {
            Some(index) => {
                claimed.insert(added[index].skill_id.clone());
                result.renamed.push((
                    gone.invocation_name.clone(),
                    added[index].invocation_name.clone(),
                ));
            }
            None if unobserved(&gone.source) => result
                .unconfirmed_removals
                .push(gone.invocation_name.clone()),
            None => result.removed.push(gone.invocation_name.clone()),
        }
    }
    for now in added {
        if !claimed.contains(&now.skill_id) {
            result.added.push(now.invocation_name.clone());
        }
    }
    Ok(result)
}
