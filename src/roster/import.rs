//! Explicit `--roster FILE` import (P2). An explicit roster replaces candidate
//! enumeration, never permissions or path validation. Every file-backed record
//! is re-read through the selected adapter's authorized roots, and each claim
//! it makes (ID, invocation name, digest, eligibility) must equal what the
//! adapter derives from the bytes actually read. Discovery is never run, so an
//! import is never merged with discovered candidates.
//!
//! Text-only records form a separate evaluation-only type. It has no load
//! target, no advisory view and no conversion into a [`ResolvedRoster`], so
//! synthetic input cannot become live hook advice.

use super::discovery::DiscoveryPlan;
use super::resolution::{
    BindingSpec, ResolutionError, ResolvedRoster, SkillEntry, claude_invocation, path_logical_key,
};
use super::{
    DisplayName, InvocationName, InvocationRestrictions, ParsedSkillMetadata, parse_skill_metadata,
};
use crate::authorized_read::{AuthorizedRoot, AuthorizedRoots, ReadError};
use crate::identity::{ContentHash, HarnessId, LogicalSkillKey, SkillId, SourceId};
use crate::limits::{
    DISCOVERY_PARSED_BYTES, EXPLICIT_ROSTER_DEPTH, EXPLICIT_ROSTER_JSON_BYTES,
    EXPLICIT_ROSTER_RECORDS, LimitUnit, ResourceLimit, SKILL_FILE_BYTES,
};
use crate::output::ErrorKind;
use crate::runtime::EntryClock;
use asupersync::Cx;
use nix::fcntl::AtFlags;
use nix::sys::stat::{SFlag, fstatat};
use serde::de::{Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, de::Error as _};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

pub const ROSTER_SCHEMA: &str = "sr.roster.v1";
/// Source namespace for synthetic records; never an adapter-declared source.
pub const SYNTHETIC_SOURCE: &str = "synthetic.text";

/// Manifest object, `skills` array, record object. Typed fields with
/// `deny_unknown_fields` cannot nest deeper, so this fixed shape is the
/// enforced depth, well inside the documented ceiling.
const MANIFEST_DEPTH: usize = 3;
const _: () = assert!(MANIFEST_DEPTH <= EXPLICIT_ROSTER_DEPTH.max());
const RECORD_LIMIT_MESSAGE: &str = "explicit roster record limit";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordProblem {
    /// A field belongs to the other manifest mode.
    WrongRecordKind,
    MissingField,
    InvalidIdentity,
    InvalidDigest,
    /// Absolute, empty, `.` or `..` component: rejected before any read.
    InvalidPath,
    /// The source is not a root the selected adapter declares and opened.
    UnknownSource,
    UnsupportedLayout,
    InvocationMismatch,
    IdMismatch,
    DuplicateDefinition,
    Missing,
    EscapesAuthorizedRoots,
    NotRegularFile,
    TooLarge,
    Unreadable,
    ContentMismatch,
    EligibilityMismatch,
    Metadata,
}

/// Diagnostics carry a record index and a kind only: never a path, name,
/// digest, link target or content from the manifest or the skill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportError {
    TooLarge,
    TooManyRecords,
    Malformed,
    DuplicateKey,
    UnknownField,
    UnsupportedSchema,
    /// No verified import layout exists for the selected adapter.
    UnsupportedHarness,
    HarnessMismatch,
    ModeMismatch,
    Unreadable,
    ByteBudget,
    Record {
        index: usize,
        problem: RecordProblem,
    },
    Resolution(ResolutionError),
    Cancelled,
    Deadline,
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record { index, problem } => {
                write!(f, "explicit roster record {index}: {problem:?}")
            }
            Self::Resolution(error) => write!(f, "explicit roster: {error}"),
            other => write!(f, "explicit roster: {other:?}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl ImportError {
    /// Output-contract kind. Cancellation follows the signal path, not a kind.
    pub const fn kind(self) -> Option<ErrorKind> {
        match self {
            Self::Cancelled | Self::Resolution(ResolutionError::Cancelled) => None,
            Self::Deadline | Self::Resolution(ResolutionError::Deadline) => {
                Some(ErrorKind::Timeout)
            }
            _ => Some(ErrorKind::UnusableRoster),
        }
    }
}

fn record(index: usize, problem: RecordProblem) -> ImportError {
    ImportError::Record { index, problem }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    AuthorizedFiles,
    SyntheticText,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    harness: String,
    mode: Mode,
    #[serde(deserialize_with = "bounded_records")]
    skills: Vec<RecordWire>,
}

/// One shape for both modes so parsing never buffers the array to learn the
/// mode; fields from the other mode are rejected after parsing.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordWire {
    id: Option<String>,
    invocation: Option<String>,
    source: Option<String>,
    path: Option<String>,
    content_hash: Option<String>,
    agent_invocable: Option<bool>,
    user_invocable: Option<bool>,
    text: Option<String>,
}

/// Stops at the first record past the ceiling instead of materializing them all.
fn bounded_records<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<RecordWire>, D::Error> {
    struct Records;
    impl<'de> Visitor<'de> for Records {
        type Value = Vec<RecordWire>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an array of roster records")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut records = Vec::new();
            while let Some(record) = seq.next_element::<RecordWire>()? {
                if records.len() == EXPLICIT_ROSTER_RECORDS.max() {
                    return Err(A::Error::custom(RECORD_LIMIT_MESSAGE));
                }
                records.push(record);
            }
            Ok(records)
        }
    }
    d.deserialize_seq(Records)
}

fn parse(bytes: &[u8], harness: &HarnessId, mode: Mode) -> Result<Vec<RecordWire>, ImportError> {
    if bytes.len() > EXPLICIT_ROSTER_JSON_BYTES.max() {
        return Err(ImportError::TooLarge);
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    // serde messages can quote rejected keys; classify, then drop the text.
    let manifest = Manifest::deserialize(&mut decoder).map_err(|error| {
        let message = error.to_string();
        if message.starts_with(RECORD_LIMIT_MESSAGE) {
            ImportError::TooManyRecords
        } else if message.starts_with("duplicate field") {
            ImportError::DuplicateKey
        } else if message.starts_with("unknown field") {
            ImportError::UnknownField
        } else {
            ImportError::Malformed
        }
    })?;
    decoder.end().map_err(|_| ImportError::Malformed)?;
    if manifest.schema != ROSTER_SCHEMA {
        return Err(ImportError::UnsupportedSchema);
    }
    let declared = HarnessId::new(manifest.harness).map_err(|_| ImportError::Malformed)?;
    if &declared != harness {
        return Err(ImportError::HarnessMismatch);
    }
    if manifest.mode != mode {
        return Err(ImportError::ModeMismatch);
    }
    Ok(manifest.skills)
}

fn budget(cx: &Cx, clock: &EntryClock) -> Result<(), ImportError> {
    if cx.is_cancel_requested() {
        return Err(ImportError::Cancelled);
    }
    clock
        .admit_new_work()
        .map(|_| ())
        .map_err(|_| ImportError::Deadline)
}

/// Read the `--roster` file itself: a bounded regular file whose symlinks, if
/// any, stay under its own directory. Devices and FIFOs are refused. `path`
/// must be absolute; the caller resolves a relative argument first.
pub fn read_roster_file(path: &Path) -> Result<Vec<u8>, ImportError> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(ImportError::Unreadable);
    };
    let root = AuthorizedRoot::open_absolute(parent).map_err(|_| ImportError::Unreadable)?;
    match AuthorizedRoots::single(root).read_bounded(0, Path::new(name), EXPLICIT_ROSTER_JSON_BYTES)
    {
        Ok(read) => Ok(read.bytes().to_vec()),
        Err(ReadError::TooLarge { .. }) => Err(ImportError::TooLarge),
        Err(_) => Err(ImportError::Unreadable),
    }
}

/// A relative path of plain components; anything else is refused before I/O.
fn plain_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn read_problem(error: ReadError) -> RecordProblem {
    match error {
        ReadError::NotFound => RecordProblem::Missing,
        ReadError::EscapesAuthorizedRoots => RecordProblem::EscapesAuthorizedRoots,
        ReadError::NotRegularFile(_) => RecordProblem::NotRegularFile,
        ReadError::TooLarge { .. } => RecordProblem::TooLarge,
        ReadError::InvalidRelativePath | ReadError::TooManyComponents => RecordProblem::InvalidPath,
        _ => RecordProblem::Unreadable,
    }
}

fn claims_match(claimed: (Option<bool>, Option<bool>), actual: InvocationRestrictions) -> bool {
    claimed
        .0
        .is_none_or(|agent| agent == actual.agent_invocable)
        && claimed.1.is_none_or(|user| user == actual.user_invocable)
}

struct Planned {
    index: usize,
    id: SkillId,
    root: usize,
    spec: BindingSpec,
    relative: String,
    content: Option<ContentHash>,
    eligibility: (Option<bool>, Option<bool>),
}

/// Import a file-backed roster for the selected adapter's plan. `overrides`
/// are trusted restrict-only settings keyed by callable name, as in discovery.
/// All-or-nothing: any failed record rejects the whole manifest.
pub fn import_authorized(
    bytes: &[u8],
    plan: &DiscoveryPlan,
    overrides: &BTreeMap<String, InvocationRestrictions>,
    cx: &Cx,
    clock: &EntryClock,
) -> Result<ResolvedRoster, ImportError> {
    budget(cx, clock)?;
    // Only Claude's layout rule is verified; other adapters cannot import yet.
    if plan.harness().as_str() != crate::adapter::CLAUDE_CODE_ID {
        return Err(ImportError::UnsupportedHarness);
    }
    let wires = parse(bytes, plan.harness(), Mode::AuthorizedFiles)?;
    let opened: Vec<_> = plan
        .roots()
        .iter()
        .filter_map(|planned| planned.root().map(|root| (planned.spec(), root)))
        .collect();
    let roots = AuthorizedRoots::new(
        opened
            .iter()
            .map(|(_, root)| root.try_clone())
            .collect::<Result<_, _>>()
            .map_err(|_| ImportError::Unreadable)?,
    );

    // Every claim that needs no I/O is checked for every record before any read.
    let mut planned = Vec::with_capacity(wires.len());
    let mut seen = BTreeSet::new();
    for (index, wire) in wires.into_iter().enumerate() {
        budget(cx, clock)?;
        if wire.text.is_some() {
            return Err(record(index, RecordProblem::WrongRecordKind));
        }
        let (Some(source), Some(relative)) = (wire.source, wire.path) else {
            return Err(record(index, RecordProblem::MissingField));
        };
        let source =
            SourceId::new(source).map_err(|_| record(index, RecordProblem::InvalidIdentity))?;
        if !plain_relative(&relative) {
            return Err(record(index, RecordProblem::InvalidPath));
        }
        let Some(root) = opened.iter().position(|(spec, _)| spec.source() == &source) else {
            return Err(record(index, RecordProblem::UnknownSource));
        };
        let (spec, root_dir) = opened[root];
        let invocation = claude_invocation(spec.kind(), Path::new(&relative))
            .ok_or_else(|| record(index, RecordProblem::UnsupportedLayout))?;
        if wire
            .invocation
            .is_some_and(|claimed| claimed != invocation.as_str())
        {
            return Err(record(index, RecordProblem::InvocationMismatch));
        }
        // The declared path, as discovery would name it, fixes the stable ID.
        let logical_key = path_logical_key(&root_dir.absolute_path().join(&relative))
            .map_err(|_| record(index, RecordProblem::InvalidIdentity))?;
        let id = SkillId::from_source(&source, &logical_key);
        if wire.id.is_some_and(|claimed| claimed != id.as_str()) {
            return Err(record(index, RecordProblem::IdMismatch));
        }
        if !seen.insert(id.clone()) {
            return Err(record(index, RecordProblem::DuplicateDefinition));
        }
        let content = wire
            .content_hash
            .map(ContentHash::parse)
            .transpose()
            .map_err(|_| record(index, RecordProblem::InvalidDigest))?;
        let restrictions =
            overrides
                .get(invocation.as_str())
                .copied()
                .unwrap_or(InvocationRestrictions {
                    agent_invocable: true,
                    user_invocable: true,
                });
        planned.push(Planned {
            index,
            id,
            root,
            spec: BindingSpec {
                source,
                logical_key,
                invocation,
                priority: Some(spec.priority()),
                visibility: spec.visibility().clone(),
                restrictions,
            },
            relative,
            content,
            eligibility: (wire.agent_invocable, wire.user_invocable),
        });
    }

    let mut entries = Vec::with_capacity(planned.len());
    let mut claims = BTreeMap::new();
    let mut total = 0usize;
    for item in planned {
        budget(cx, clock)?;
        let index = item.index;
        // Discovery never descends a symlinked directory; neither does import.
        // Containment does not rest on this check: a directory swapped for a
        // link afterwards still resolves only inside authorized roots.
        let directory = item.relative.split('/').next().unwrap_or_default();
        match fstatat(
            opened[item.root].1.as_fd(),
            directory,
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Ok(stat)
                if SFlag::from_bits_truncate(stat.st_mode & SFlag::S_IFMT.bits())
                    == SFlag::S_IFDIR => {}
            Ok(_) => return Err(record(index, RecordProblem::UnsupportedLayout)),
            Err(nix::errno::Errno::ENOENT) => return Err(record(index, RecordProblem::Missing)),
            Err(_) => return Err(record(index, RecordProblem::Unreadable)),
        }
        let remaining = DISCOVERY_PARSED_BYTES.max().saturating_sub(total);
        if remaining == 0 {
            return Err(ImportError::ByteBudget);
        }
        let limit = ResourceLimit::try_new(
            "roster_import_read",
            LimitUnit::Bytes,
            remaining.min(SKILL_FILE_BYTES.max()),
        )
        .map_err(|_| ImportError::ByteBudget)?;
        let read = roots
            .read_bounded(item.root, Path::new(&item.relative), limit)
            .map_err(|error| record(index, read_problem(error)))?;
        total += read.len();
        if item
            .content
            .as_ref()
            .is_some_and(|claimed| claimed != read.content_hash())
        {
            return Err(record(index, RecordProblem::ContentMismatch));
        }
        let entry = SkillEntry::from_read(item.spec, read).map_err(|error| match error {
            ResolutionError::Metadata => record(index, RecordProblem::Metadata),
            other => ImportError::Resolution(other),
        })?;
        claims.insert(item.id, (index, item.eligibility));
        entries.push(entry);
    }

    // The manifest is the complete inventory by declaration, so not partial.
    let roster =
        ResolvedRoster::resolve(entries, false, cx, clock).map_err(|error| match error {
            ResolutionError::Cancelled => ImportError::Cancelled,
            ResolutionError::Deadline => ImportError::Deadline,
            other => ImportError::Resolution(other),
        })?;
    for skill in roster.skills() {
        for binding in skill.bindings() {
            if let Some(&(index, claimed)) = claims.get(&binding.id)
                && !claims_match(claimed, binding.restrictions)
            {
                return Err(record(index, RecordProblem::EligibilityMismatch));
            }
        }
    }
    budget(cx, clock)?;
    Ok(roster)
}

/// A text-only record parsed with the same metadata parser as real skills.
/// It has no source file and no load target.
#[derive(Clone)]
pub struct SyntheticSkill {
    id: SkillId,
    invocation: InvocationName,
    display_name: DisplayName,
    content: ContentHash,
    restrictions: InvocationRestrictions,
    metadata: ParsedSkillMetadata,
}

impl SyntheticSkill {
    pub fn id(&self) -> &SkillId {
        &self.id
    }
    pub fn invocation(&self) -> &InvocationName {
        &self.invocation
    }
    pub fn display_name(&self) -> &DisplayName {
        &self.display_name
    }
    pub fn content_hash(&self) -> &ContentHash {
        &self.content
    }
    pub fn restrictions(&self) -> InvocationRestrictions {
        self.restrictions
    }
    pub fn metadata(&self) -> &ParsedSkillMetadata {
        &self.metadata
    }
}

impl fmt::Debug for SyntheticSkill {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntheticSkill")
            .field("id", &self.id)
            .field("restrictions", &self.restrictions)
            .finish_non_exhaustive()
    }
}

/// Evaluation-only inventory. Deliberately offers no advisory view, exact
/// resolution, option map or conversion into a [`ResolvedRoster`].
#[derive(Clone)]
pub struct SyntheticRoster {
    skills: Vec<SyntheticSkill>,
}

impl SyntheticRoster {
    pub fn skills(&self) -> &[SyntheticSkill] {
        &self.skills
    }
}

impl fmt::Debug for SyntheticRoster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntheticRoster")
            .field("skills", &self.skills.len())
            .finish()
    }
}

/// Import a text-only roster for offline evaluation. IDs derive from the
/// synthetic namespace and the invocation name, so each name appears once.
pub fn import_synthetic(bytes: &[u8], harness: &HarnessId) -> Result<SyntheticRoster, ImportError> {
    let wires = parse(bytes, harness, Mode::SyntheticText)?;
    let source = SourceId::new(SYNTHETIC_SOURCE).map_err(|_| ImportError::Malformed)?;
    let mut skills = Vec::with_capacity(wires.len());
    let mut seen = BTreeSet::new();
    for (index, wire) in wires.into_iter().enumerate() {
        if wire.source.is_some() || wire.path.is_some() {
            return Err(record(index, RecordProblem::WrongRecordKind));
        }
        let (Some(invocation), Some(text)) = (wire.invocation, wire.text) else {
            return Err(record(index, RecordProblem::MissingField));
        };
        let invocation = InvocationName::new(invocation)
            .map_err(|_| record(index, RecordProblem::InvalidIdentity))?;
        let logical_key = LogicalSkillKey::new(invocation.as_str())
            .map_err(|_| record(index, RecordProblem::InvalidIdentity))?;
        let id = SkillId::from_source(&source, &logical_key);
        if wire.id.is_some_and(|claimed| claimed != id.as_str()) {
            return Err(record(index, RecordProblem::IdMismatch));
        }
        if !seen.insert(id.clone()) {
            return Err(record(index, RecordProblem::DuplicateDefinition));
        }
        if SKILL_FILE_BYTES.check(text.len()).is_err() {
            return Err(record(index, RecordProblem::TooLarge));
        }
        let content = ContentHash::from_bytes(text.as_bytes());
        let claimed = wire
            .content_hash
            .map(ContentHash::parse)
            .transpose()
            .map_err(|_| record(index, RecordProblem::InvalidDigest))?;
        if claimed.is_some_and(|claimed| claimed != content) {
            return Err(record(index, RecordProblem::ContentMismatch));
        }
        let metadata = parse_skill_metadata(text.as_bytes())
            .map_err(|_| record(index, RecordProblem::Metadata))?;
        let restrictions = InvocationRestrictions {
            agent_invocable: metadata.agent_invocable,
            user_invocable: metadata.user_invocable,
        };
        if !claims_match((wire.agent_invocable, wire.user_invocable), restrictions) {
            return Err(record(index, RecordProblem::EligibilityMismatch));
        }
        skills.push(SyntheticSkill {
            display_name: DisplayName::from_text(
                metadata.name.as_deref().unwrap_or(invocation.as_str()),
            ),
            id,
            invocation,
            content,
            restrictions,
            metadata,
        });
    }
    Ok(SyntheticRoster { skills })
}
