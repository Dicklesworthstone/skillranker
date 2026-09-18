//! Local roster inspection (P2). One record per binding, with its status
//! reason, restrictions and content identity, in stable skill-ID order, plus
//! the roster's counts and partial-source causes. Pages carry a cursor bound
//! to the roster snapshot: a continuation against a changed roster is refused
//! rather than silently mixing two observations. These cursors are unrelated
//! to transcript ingestion cursors.
//!
//! Inspection needs no key, network or state: it only reads the resolved
//! roster through the evidence module.

use super::evidence::{Counts, RetrievalView, RosterEvidence, summarize, trace};
use super::resolution::ResolvedRoster;
use crate::identity::SkillId;
use crate::output::ErrorKind;
use crate::privacy::redaction::{REDACTION_MARKER, Redactor};
use serde_json::{Value, json};

pub const LISTING_SCHEMA: &str = "sr.roster-listing.v1";
pub const MAX_PAGE: usize = crate::output::MAX_TRACE_PAGE_ITEMS;
const CURSOR_PREFIX: &str = "r1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageError {
    InvalidLimit,
    InvalidCursor,
    /// The cursor belongs to a different roster snapshot; restart the listing.
    RosterChanged,
}

impl PageError {
    pub const fn kind(self) -> ErrorKind {
        match self {
            Self::InvalidLimit | Self::InvalidCursor => ErrorKind::InvalidUsage,
            Self::RosterChanged => ErrorKind::RosterChanged,
        }
    }
}

/// One binding's inspection record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    pub id: SkillId,
    pub invocation: String,
    pub display_name: String,
    pub source: String,
    /// `eligible`, or the first stable reason that removes it from advice.
    pub status: &'static str,
    pub agent_invocable: bool,
    pub user_invocable: bool,
    pub usage_kind: &'static str,
    pub content_hash: String,
    /// Other bindings of the same file.
    pub aliases: usize,
}

/// A complete listing bound to one roster snapshot.
#[derive(Clone, Debug)]
pub struct Listing {
    evidence: RosterEvidence,
    records: Vec<Record>,
}

impl Listing {
    pub fn records(&self) -> &[Record] {
        &self.records
    }
    pub fn evidence(&self) -> &RosterEvidence {
        &self.evidence
    }
}

pub fn listing(roster: &ResolvedRoster) -> Listing {
    let mut records = Vec::new();
    for skill in roster.skills() {
        let record = skill.record();
        for binding in skill.bindings() {
            let traced = trace(roster, None, &RetrievalView::NotEvaluated, &binding.id);
            records.push(Record {
                id: binding.id.clone(),
                invocation: binding.invocation.as_str().to_owned(),
                display_name: record.display_name.as_str().to_owned(),
                source: binding.source.as_str().to_owned(),
                status: traced
                    .decisive()
                    .map_or("eligible", |reason| reason.as_str()),
                agent_invocable: binding.restrictions.agent_invocable,
                user_invocable: binding.restrictions.user_invocable,
                usage_kind: match record.usage_kind {
                    super::UsageKind::Reference => "reference",
                    super::UsageKind::Workflow => "workflow",
                    super::UsageKind::Unknown => "unknown",
                },
                content_hash: record.source_content.as_str().to_owned(),
                aliases: skill.bindings().len() - 1,
            });
        }
    }
    records.sort_by(|a, b| a.id.cmp(&b.id));
    Listing {
        evidence: summarize(roster, None, &RetrievalView::NotEvaluated),
        records,
    }
}

/// One page of a listing and the cursor for the next, if any.
#[derive(Clone, Debug)]
pub struct Page<'a> {
    pub listing: &'a Listing,
    pub start: usize,
    pub records: &'a [Record],
    pub next_cursor: Option<String>,
}

fn cursor(listing: &Listing, offset: usize) -> String {
    format!(
        "{CURSOR_PREFIX}.{}.{offset}",
        listing.evidence.snapshot.as_str()
    )
}

fn parse_cursor(listing: &Listing, token: &str) -> Result<usize, PageError> {
    let mut parts = token.split('.');
    let (Some(prefix), Some(snapshot), Some(offset), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(PageError::InvalidCursor);
    };
    let well_formed = prefix == CURSOR_PREFIX
        && snapshot.len() == 64
        && snapshot
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !well_formed {
        return Err(PageError::InvalidCursor);
    }
    let offset: usize = offset.parse().map_err(|_| PageError::InvalidCursor)?;
    if snapshot != listing.evidence.snapshot.as_str() {
        return Err(PageError::RosterChanged);
    }
    if offset == 0 || offset >= listing.records.len() {
        return Err(PageError::InvalidCursor);
    }
    Ok(offset)
}

/// Select a page. Without a cursor the listing starts at the beginning; a
/// cursor continues only against the same snapshot it was issued for.
pub fn page<'a>(
    listing: &'a Listing,
    cursor_token: Option<&str>,
    limit: usize,
) -> Result<Page<'a>, PageError> {
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(PageError::InvalidLimit);
    }
    let start = match cursor_token {
        Some(token) => parse_cursor(listing, token)?,
        None => 0,
    };
    let end = (start + limit).min(listing.records.len());
    Ok(Page {
        listing,
        start,
        records: &listing.records[start..end],
        next_cursor: (end < listing.records.len()).then(|| cursor(listing, end)),
    })
}

fn counts_json(counts: &Counts) -> Value {
    json!({
        "skills": counts.skills,
        "bindings": counts.bindings,
        "verified": counts.verified,
        "shadowed": counts.shadowed,
        "ambiguous": counts.ambiguous,
        "unverified": counts.unverified,
        "manual_only": counts.manual_only,
        "forbidden": counts.forbidden,
        "advisory": counts.advisory,
    })
}

impl Page<'_> {
    pub fn to_json(&self) -> Value {
        let evidence = &self.listing.evidence;
        let records: Vec<Value> = self
            .records
            .iter()
            .map(|r| {
                json!({
                    "skill_id": r.id.as_str(),
                    "invocation_name": redacted_label(&r.invocation),
                    "name": redacted_label(&r.display_name),
                    "source": redacted_label(&r.source),
                    "status": r.status,
                    "agent_invocable": r.agent_invocable,
                    "user_invocable": r.user_invocable,
                    "usage_kind": r.usage_kind,
                    "content_hash": r.content_hash,
                    "aliases": r.aliases,
                })
            })
            .collect();
        json!({
            "schema": LISTING_SCHEMA,
            "evidence_version": evidence.version,
            "snapshot": evidence.snapshot.as_str(),
            "partial": evidence.partial,
            "total": self.listing.records.len(),
            "start": self.start,
            "counts": counts_json(&evidence.counts),
            "source_causes": evidence.source_causes,
            "record_causes": evidence.record_causes,
            "records": records,
            "next_cursor": self.next_cursor,
        })
    }
}

/// Local inspection is an output boundary too. Scan complete metadata before
/// serialization; retain exact names internally for resolution and snapshots.
/// An uninspectable label is withheld, never copied into an error message.
fn redacted_label(text: &str) -> String {
    Redactor::default()
        .redact_field(text)
        .map(|field| field.into_string())
        .unwrap_or_else(|_| REDACTION_MARKER.to_owned())
}
