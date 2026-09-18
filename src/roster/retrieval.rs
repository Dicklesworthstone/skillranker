//! Bounded query construction for the pinned default Quill schema.
//! Retrieval uses a fresh bounded in-memory index; no provider, persistence, or fallback effects.

use asupersync::Cx;
use frankensearch_quill::{
    Analyzer, BooleanOperator, DEFAULT_SCHEMA, DefaultQueryParser, Occur, Query,
    query::QueryNode,
    scribe::{FrankensearchTokenizer, TokenAnalyzer},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

pub const QUERY_COMPILER_VERSION: &str = "quill-literal-or-v1";
pub const MAX_QUERY_TERMS: usize = 128;
pub const MAX_QUERY_SCALARS: usize = 4096;
/// Per source, before analysis; at most 12,288 scalars / 49,152 UTF-8 bytes total.
pub const MAX_SOURCE_SCALARS: usize = 4096;

/// Already selected local context, never arbitrary transcript fields. Callers
/// apply disclosure/tool policy first. This compiler is not a redaction boundary.
pub struct QueryInput<'a> {
    pub latest_request: &'a str,
    pub active_task: &'a str,
    pub recent_errors: &'a str,
}

impl fmt::Debug for QueryInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueryInput(<private>)")
    }
}

/// Counts only. Source order is latest request, active task, recent errors.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueryDiagnostics {
    pub input_truncated: [bool; 3],
    pub analyzed_tokens: usize,
    pub duplicate_terms: usize,
    pub omitted_terms: usize,
    pub term_limit_reached: bool,
    pub scalar_limit_reached: bool,
    pub emitted_terms: usize,
    pub query_scalars: usize,
}

/// Only the compiler can construct this validated string. Never serialize or
/// log it: even local search text may contain private session information.
#[derive(Clone, Eq, PartialEq)]
pub struct LiteralQuery(String);

impl LiteralQuery {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LiteralQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LiteralQuery(<private>)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryCompilation {
    /// None means no analyzed terms: retrieval-empty, never match-all/padding.
    pub query: Option<LiteralQuery>,
    pub diagnostics: QueryDiagnostics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryCompileError {
    Cancelled,
    SchemaMismatch,
    UnrepresentableTerms,
    ParserRejected {
        diagnostic_count: usize,
        was_truncated: bool,
    },
    MeaningChanged,
}

impl fmt::Display for QueryCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "query compilation cancelled",
            Self::SchemaMismatch => "query schema does not match the pinned contract",
            Self::UnrepresentableTerms => "no analyzed query term fits the bounded query",
            Self::ParserRejected { .. } => "query parser reported truncation or recovery",
            Self::MeaningChanged => "query parser did not preserve literal disjunction semantics",
        })
    }
}

impl std::error::Error for QueryCompileError {}

struct Term<'a> {
    normalized: String,
    source: &'a str,
}

/// Compile deduplicated literal terms, interleaving sources so a terse request
/// retains task/error evidence and a long request cannot consume every slot.
/// All input scanning, allocations, analysis and parser work are bounded by the
/// constants above. Check the invocation Cx between bounded synchronous stages.
pub fn compile_query(
    cx: &Cx,
    input: QueryInput<'_>,
) -> Result<QueryCompilation, QueryCompileError> {
    let mut diagnostics = QueryDiagnostics::default();
    let mut sources = [Vec::new(), Vec::new(), Vec::new()];
    let mut analyzer = FrankensearchTokenizer::default();
    for (i, source) in [input.latest_request, input.active_task, input.recent_errors]
        .into_iter()
        .enumerate()
    {
        cx.checkpoint().map_err(|_| QueryCompileError::Cancelled)?;
        let (bounded, truncated) = bounded_source(source);
        diagnostics.input_truncated[i] = truncated;
        analyzer.analyze(Analyzer::FrankensearchDefault, bounded, &mut |token| {
            diagnostics.analyzed_tokens += 1;
            sources[i].push(Term {
                normalized: token.text.clone(),
                // Render the original token spelling: Unicode lowercase is not
                // always idempotent under tokenization (e.g. dotted capital I).
                // Quill then analyzes it exactly once, as it does for documents.
                source: &bounded[token.offset_from..token.offset_to],
            });
        });
    }

    let mut seen = BTreeSet::new();
    let mut expected = BTreeSet::new();
    let mut rendered = String::new();
    let rounds = sources.iter().map(Vec::len).max().unwrap_or(0);
    for round in 0..rounds {
        cx.checkpoint().map_err(|_| QueryCompileError::Cancelled)?;
        for terms in &sources {
            let Some(term) = terms.get(round) else {
                continue;
            };
            if !seen.insert(term.normalized.as_str()) {
                diagnostics.duplicate_terms += 1;
                continue;
            }
            if diagnostics.emitted_terms == MAX_QUERY_TERMS {
                diagnostics.term_limit_reached = true;
                diagnostics.omitted_terms += 1;
                continue;
            }
            let literal = quote_literal(term.source);
            let separator = if rendered.is_empty() { "" } else { " OR " };
            let added = separator.len() + literal.chars().count();
            if diagnostics.query_scalars + added > MAX_QUERY_SCALARS {
                diagnostics.scalar_limit_reached = true;
                diagnostics.omitted_terms += 1;
                continue;
            }
            rendered.push_str(separator);
            rendered.push_str(&literal);
            diagnostics.query_scalars += added;
            diagnostics.emitted_terms += 1;
            expected.insert(term.normalized.as_str());
        }
    }
    cx.checkpoint().map_err(|_| QueryCompileError::Cancelled)?;
    if rendered.is_empty() {
        if diagnostics.analyzed_tokens != 0 {
            return Err(QueryCompileError::UnrepresentableTerms);
        }
        return Ok(QueryCompilation {
            query: None,
            diagnostics,
        });
    }
    validate_query(&rendered, &expected)?;
    cx.checkpoint().map_err(|_| QueryCompileError::Cancelled)?;
    Ok(QueryCompilation {
        query: Some(LiteralQuery(rendered)),
        diagnostics,
    })
}

fn validate_query(rendered: &str, expected: &BTreeSet<&str>) -> Result<(), QueryCompileError> {
    let parser =
        DefaultQueryParser::new(DEFAULT_SCHEMA).map_err(|_| QueryCompileError::SchemaMismatch)?;
    let parsed = parser.parse(rendered);
    // Upstream recovery diagnostics can contain source fragments; never return
    // their strings or the AST through our errors or Debug surface.
    if parsed.was_truncated || !parsed.diagnostics.is_empty() {
        return Err(QueryCompileError::ParserRejected {
            diagnostic_count: parsed.diagnostics.len(),
            was_truncated: parsed.was_truncated,
        });
    }
    if !is_literal_disjunction(rendered) || !preserves_terms(&parsed.query, expected) {
        return Err(QueryCompileError::MeaningChanged);
    }
    Ok(())
}

fn is_literal_disjunction(mut rendered: &str) -> bool {
    // The lenient parser can erase unquoted punctuation without a diagnostic
    // (for example, alpha* becomes alpha). An AST match alone therefore cannot
    // establish that the renderer supplied only quoted literals and ORs.
    loop {
        let Some(body) = rendered.strip_prefix('"') else {
            return false;
        };
        let mut chars = body.char_indices();
        loop {
            match chars.next() {
                Some((_, '\\')) => {
                    if !matches!(chars.next(), Some((_, '\\' | '"'))) {
                        return false;
                    }
                }
                Some((end, '"')) => {
                    rendered = &body[end + 1..];
                    break;
                }
                Some(_) => {}
                None => return false,
            }
        }
        if rendered.is_empty() {
            return true;
        }
        let Some(rest) = rendered.strip_prefix(" OR ") else {
            return false;
        };
        rendered = rest;
    }
}

fn bounded_source(source: &str) -> (&str, bool) {
    let Some((end, next)) = source.char_indices().nth(MAX_SOURCE_SCALARS) else {
        return (source, false);
    };
    let mut prefix = &source[..end];
    // A cutoff inside a word must not invent a searchable prefix of that word.
    // Scan backwards only within the bounded prefix, never through the tail.
    if next.is_alphanumeric() {
        prefix = prefix.trim_end_matches(char::is_alphanumeric);
    }
    (prefix, true)
}

fn quote_literal(term: &str) -> String {
    let mut escaped = String::from("\"");
    for ch in term.chars() {
        if matches!(ch, '\\' | '"') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('"');
    escaped
}

fn preserves_terms(query: &Query, expected: &BTreeSet<&str>) -> bool {
    let mut pending = vec![query.root_id()];
    let mut actual = BTreeMap::new();
    while let Some(id) = pending.pop() {
        match query.node(id) {
            QueryNode::Term { fields, text } if !fields.is_empty() => {
                let mask = actual.entry(text.as_str()).or_insert(0_u8);
                for field in fields {
                    let bit = match (field.field_id, field.boost) {
                        (1, 1.0) => 1,
                        (2, 2.0) => 2,
                        _ => return false,
                    };
                    if *mask & bit != 0 {
                        return false;
                    }
                    *mask |= bit;
                }
            }
            QueryNode::Boolean {
                clauses,
                operator: None | Some(BooleanOperator::Or),
            } if clauses.iter().all(|clause| clause.occur == Occur::Should) => {
                pending.extend(clauses.iter().map(|clause| clause.query));
            }
            _ => return false,
        }
    }
    // Quill expands each unfielded literal into an implicit OR of separate
    // content/title leaves. Require each field exactly once for every term;
    // merely collecting term strings would miss a dropped or duplicated field.
    actual.len() == expected.len()
        && actual
            .iter()
            .all(|(text, mask)| *mask == 3 && expected.contains(text))
}

/// Versioned mapping/selection policy, separate from the pinned engine version.
pub const RETRIEVAL_VERSION: &str = "roster-quill-v1";
pub const RETRIEVAL_SCHEMA: &str = "quill-default-content-title-v1";
pub const MAX_CANDIDATES: usize = 254;
pub const TITLE_SCALARS: usize = 2048;
pub const DESCRIPTION_SCALARS: usize = 8192;
pub const TAG_SCALARS: usize = 2048;

/// Logical engine budgets, not an RSS limit. Callers may restrict these values.
#[derive(Clone, Copy, Debug)]
pub struct RetrievalBudget {
    pub scribe_bytes: usize,
    pub delta_bytes: usize,
    pub document_bytes: usize,
    pub query_fuel: u64,
}
impl Default for RetrievalBudget {
    fn default() -> Self {
        Self {
            scribe_bytes: 16 * 1024 * 1024,
            delta_bytes: 4 * 1024 * 1024,
            document_bytes: 32 * 1024 * 1024,
            query_fuel: 1_000_000,
        }
    }
}
impl RetrievalBudget {
    fn valid(self) -> bool {
        let cap = Self::default();
        self.scribe_bytes > 0
            && self.scribe_bytes <= cap.scribe_bytes
            && self.delta_bytes > 0
            && self.delta_bytes <= cap.delta_bytes
            && self.document_bytes > 0
            && self.document_bytes <= cap.document_bytes
            && self.query_fuel > 0
            && self.query_fuel <= cap.query_fuel
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetrievalMethod {
    FullRoster,
    QuillBm25,
}
#[derive(Clone, Debug)]
pub struct RetrievalDiagnostics {
    pub policy_version: &'static str,
    pub engine_version: &'static str,
    pub schema_version: &'static str,
    pub method: Option<RetrievalMethod>,
    pub roster_count: usize,
    pub eligible_count: Option<usize>,
    pub admitted_count: usize,
    pub partial_roster: bool,
    /// Candidates omitted relative to the eligible roster, not an exact match count.
    pub truncated: bool,
    /// The page filled; more matches may exist, without requesting exact count.
    pub hit_limit_reached: bool,
    pub truncated_documents: usize,
    pub indexed_bytes: usize,
    pub build_commit_us: Option<u64>,
    pub search_us: Option<u64>,
    pub query: Option<QueryDiagnostics>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetrievalError {
    InvalidBudget,
    NoEligibleCandidates,
    RetrievalEmpty,
    Query(QueryCompileError),
    InputLimit,
    Cancelled,
    Deadline,
    Index,
    QueryFuel,
    EngineContract,
}
#[derive(Clone, Debug)]
pub struct RetrievalFailure {
    pub kind: RetrievalError,
    pub diagnostics: Box<RetrievalDiagnostics>,
}
impl fmt::Display for RetrievalFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "roster retrieval: {:?}", self.kind)
    }
}
impl std::error::Error for RetrievalFailure {}
#[derive(Debug)]
pub struct RetrievalSelection<'a> {
    /// Borrowed from this immutable roster; pass binding IDs into OptionMap.
    pub candidates: Vec<super::resolution::AdvisorySkill<'a>>,
    pub diagnostics: RetrievalDiagnostics,
}
fn retrieval_checkpoint(cx: &Cx, clock: &crate::runtime::EntryClock) -> Result<(), RetrievalError> {
    clock
        .admit_new_work()
        .map_err(|_| RetrievalError::Deadline)?;
    cx.checkpoint().map_err(|_| RetrievalError::Cancelled)
}
fn elapsed_us(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}
fn engine_error(error: frankensearch_quill::QuillIndexError) -> RetrievalError {
    match error {
        frankensearch_quill::QuillIndexError::Cancelled { .. } => RetrievalError::Cancelled,
        frankensearch_quill::QuillIndexError::QueryFuelExhausted { .. } => {
            RetrievalError::QueryFuel
        }
        _ => RetrievalError::Index, // Never retain upstream strings containing local text.
    }
}
/// Bound text by scalar count with separators inside the bound, retaining a
/// truncation flag. No field can smuggle query operators into the query compiler.
fn bounded_fields<'a>(fields: impl IntoIterator<Item = &'a str>, cap: usize) -> (String, bool) {
    let mut result = String::new();
    let mut used = 0;
    let mut truncated = false;
    for field in fields {
        if field.is_empty() {
            continue;
        }
        if !result.is_empty() {
            if used == cap {
                truncated = true;
                break;
            }
            result.push(' ');
            used += 1;
        }
        for c in field.chars() {
            if used == cap {
                truncated = true;
                break;
            }
            result.push(c);
            used += 1;
        }
        if truncated {
            break;
        }
    }
    (result, truncated)
}

/// Select advisory candidates. Exact user requirements are resolved separately
/// before this boundary. Excluding any binding excludes its whole physical file.
/// The borrowed snapshot cannot be substituted by a provider-returned name/path.
pub async fn retrieve<'a>(
    roster: &'a super::resolution::ResolvedRoster,
    excluded: &BTreeSet<crate::identity::SkillId>,
    input: QueryInput<'_>,
    budget: RetrievalBudget,
    cx: &Cx,
    clock: &crate::runtime::EntryClock,
) -> Result<RetrievalSelection<'a>, RetrievalFailure> {
    let mut diagnostics = RetrievalDiagnostics {
        policy_version: RETRIEVAL_VERSION,
        engine_version: frankensearch_quill::FRANKENSEARCH_QUILL_CRATE_VERSION,
        schema_version: RETRIEVAL_SCHEMA,
        method: None,
        roster_count: roster.skills().len(),
        eligible_count: None,
        admitted_count: 0,
        partial_roster: roster.is_partial(),
        truncated: false,
        hit_limit_reached: false,
        truncated_documents: 0,
        indexed_bytes: 0,
        build_commit_us: None,
        search_us: None,
        query: None,
    };
    let outcome: Result<Vec<super::resolution::AdvisorySkill<'a>>, RetrievalError> = async {
        retrieval_checkpoint(cx, clock)?;
        if !budget.valid() {
            return Err(RetrievalError::InvalidBudget);
        }
        if excluded.len() > crate::limits::DISCOVERY_FILES.max() {
            return Err(RetrievalError::InputLimit);
        }
        let mut selected = Vec::new();
        for skill in roster.skills() {
            retrieval_checkpoint(cx, clock)?;
            if skill.bindings().iter().any(|b| excluded.contains(&b.id)) {
                continue;
            }
            if let Some(binding) = skill.bindings().iter().find(|b| {
                b.restrictions.agent_invocable
                    && matches!(b.visibility, super::Visibility::Verified { .. })
            }) {
                selected.push(super::resolution::AdvisorySkill {
                    record: skill.record(),
                    binding,
                });
            }
        }
        selected.sort_by(|a, b| a.record.id.cmp(&b.record.id));
        retrieval_checkpoint(cx, clock)?;
        diagnostics.eligible_count = Some(selected.len());
        diagnostics.method = Some(RetrievalMethod::FullRoster);
        if selected.is_empty() {
            return Err(RetrievalError::NoEligibleCandidates);
        }
        if selected.len() <= MAX_CANDIDATES {
            diagnostics.admitted_count = selected.len();
            return Ok(selected);
        }
        diagnostics.method = Some(RetrievalMethod::QuillBm25);
        let compiled = compile_query(cx, input).map_err(RetrievalError::Query)?;
        diagnostics.query = Some(compiled.diagnostics);
        let query = compiled.query.ok_or(RetrievalError::RetrievalEmpty)?;
        retrieval_checkpoint(cx, clock)?;
        let started = std::time::Instant::now();
        let build_result = async {
            let mut docs = Vec::with_capacity(selected.len());
            // ResolvedRoster guarantees canonical IDs are unique. The map is local
            // to these exact documents, not reconstructed from a search string.
            let by_id: BTreeMap<_, _> = roster
                .skills()
                .iter()
                .map(|s| (&s.record().id, s))
                .collect();
            for skill in &selected {
                retrieval_checkpoint(cx, clock)?;
                let resolved = by_id
                    .get(&skill.record.id)
                    .ok_or(RetrievalError::EngineContract)?;
                let mut unique_names = BTreeSet::new();
                let names = std::iter::once(skill.binding.invocation.as_str())
                    .chain(
                        resolved
                            .bindings()
                            .iter()
                            .filter(|b| {
                                b.restrictions.agent_invocable
                                    && matches!(b.visibility, super::Visibility::Verified { .. })
                            })
                            .map(|b| b.invocation.as_str()),
                    )
                    .chain(std::iter::once(skill.record.display_name.as_str()))
                    .filter(|name| unique_names.insert(*name));
                let (title, title_cut) = bounded_fields(names, TITLE_SCALARS);
                let (description, description_cut) = bounded_fields(
                    [skill.record.description_full.as_str()],
                    DESCRIPTION_SCALARS,
                );
                let (tags, tags_cut) =
                    bounded_fields(skill.record.tags.iter().map(|t| t.as_str()), TAG_SCALARS);
                let content = format!("{description}\n{tags}");
                let bytes = title
                    .len()
                    .checked_add(content.len())
                    .and_then(|n| n.checked_add(skill.record.id.as_str().len()))
                    .ok_or(RetrievalError::InputLimit)?;
                diagnostics.indexed_bytes = diagnostics
                    .indexed_bytes
                    .checked_add(bytes)
                    .ok_or(RetrievalError::InputLimit)?;
                if diagnostics.indexed_bytes > budget.document_bytes {
                    return Err(RetrievalError::InputLimit);
                }
                diagnostics.truncated_documents +=
                    usize::from(title_cut || description_cut || tags_cut);
                docs.push(
                    frankensearch_core::IndexableDocument::new(skill.record.id.as_str(), content)
                        .with_title(title),
                );
            }
            let config = frankensearch_quill::QuillConfig {
                scribe_shard_budget_bytes: budget.scribe_bytes,
                delta_budget_bytes: budget.delta_bytes,
                max_ingest_shards: 1,
                deterministic_ingest: true,
                query_fuel_budget: budget.query_fuel,
                glob_expansion_limit: MAX_QUERY_TERMS,
                max_visibility_lag_ms: u64::MAX,
                ..frankensearch_quill::QuillConfig::default()
            };
            let index = frankensearch_quill::QuillIndex::in_memory(config).map_err(engine_error)?;
            index
                .index_documents(cx, &docs)
                .await
                .map_err(engine_error)?;
            retrieval_checkpoint(cx, clock)?;
            index.commit(cx).await.map_err(engine_error)?;
            retrieval_checkpoint(cx, clock)?;
            Ok(index)
        }
        .await;
        diagnostics.build_commit_us = Some(elapsed_us(started));
        retrieval_checkpoint(cx, clock)?;
        let index = build_result?;
        let started = std::time::Instant::now();
        let page = index
            .search_paginated(cx, query.as_str(), MAX_CANDIDATES, 0, false)
            .map_err(engine_error);
        diagnostics.search_us = Some(elapsed_us(started));
        retrieval_checkpoint(cx, clock)?;
        let page = page?;
        if !page.diagnostics.is_empty()
            || page.total_count.is_some()
            || page.doc_count != selected.len() as u64
            || page.hits.len() > MAX_CANDIDATES
        {
            return Err(RetrievalError::EngineContract);
        }
        let snapshot: BTreeMap<_, _> = selected
            .iter()
            .map(|s| (s.record.id.as_str(), *s))
            .collect();
        let mut seen = BTreeSet::new();
        let mut hits = Vec::with_capacity(page.hits.len());
        for hit in page.hits.iter() {
            if !hit.score.is_finite() || !seen.insert(hit.document_id.as_str()) {
                return Err(RetrievalError::EngineContract);
            }
            hits.push(
                *snapshot
                    .get(hit.document_id.as_str())
                    .ok_or(RetrievalError::EngineContract)?,
            );
        }
        if hits.is_empty() {
            return Err(RetrievalError::RetrievalEmpty);
        }
        diagnostics.admitted_count = hits.len();
        diagnostics.hit_limit_reached = hits.len() == MAX_CANDIDATES;
        diagnostics.truncated = hits.len() < selected.len();
        retrieval_checkpoint(cx, clock)?;
        Ok(hits)
    }
    .await;
    match outcome {
        Ok(candidates) => Ok(RetrievalSelection {
            candidates,
            diagnostics,
        }),
        Err(kind) => Err(RetrievalFailure {
            kind,
            diagnostics: Box::new(diagnostics),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_failures_are_sanitized_and_never_become_empty_retrieval() {
        use frankensearch_quill::{QuillConfig, QuillIndex, QuillIndexError};
        let invalid = QuillConfig {
            delta_budget_bytes: 0,
            ..QuillConfig::default()
        };
        let error = match QuillIndex::in_memory(invalid) {
            Ok(_) => panic!("invalid engine config accepted"),
            Err(error) => error,
        };
        assert_eq!(engine_error(error), RetrievalError::Index);
        assert!(QuillIndex::in_memory(QuillConfig::default()).is_ok());
        // Pure error-boundary canary, not simulated storage-corruption evidence.
        let private = engine_error(QuillIndexError::InvalidState {
            detail: "PrivateEngineCanary".into(),
        });
        assert_eq!(private, RetrievalError::Index);
        assert!(!format!("{private:?}").contains("PrivateEngineCanary"));
    }

    #[test]
    fn parser_recovery_and_truncation_are_failures_with_text_free_diagnostics() {
        let expected = BTreeSet::from(["privatecanary"]);
        let error = validate_query("\"privatecanary", &expected).unwrap_err();
        assert!(matches!(
            error,
            QueryCompileError::ParserRejected {
                diagnostic_count: 1..,
                was_truncated: false,
            }
        ));
        assert!(!format!("{error:?}: {error}").contains("privatecanary"));
        let oversized = "x ".repeat(frankensearch_quill::MAX_QUERY_LENGTH);
        assert!(matches!(
            validate_query(&oversized, &expected),
            Err(QueryCompileError::ParserRejected {
                was_truncated: true,
                ..
            })
        ));
        assert_eq!(validate_query("\"privatecanary\"", &expected), Ok(()));
    }

    #[test]
    fn syntactically_valid_but_changed_meaning_is_refused() {
        let expected = BTreeSet::from(["alpha", "beta"]);
        for (case, query) in [
            "alpha AND beta",
            "title:alpha OR beta",
            "alpha* OR beta",
            "\"alpha beta\"",
            "alpha OR gamma",
            "alpha^4 OR beta",
            "*",
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                validate_query(query, &expected),
                Err(QueryCompileError::MeaningChanged),
                "syntax fixture {case}"
            );
        }
        assert_eq!(validate_query("\"alpha\" OR \"beta\"", &expected), Ok(()));
    }

    #[test]
    fn literal_quoting_counts_delimiters_and_escapes() {
        let rendered = quote_literal("a\"b\\猫");
        assert_eq!(rendered, "\"a\\\"b\\\\猫\"");
        assert_eq!(rendered.chars().count(), 9);
        assert_eq!(quote_literal("OR"), "\"OR\"");
        assert!(is_literal_disjunction(&rendered));
        assert!(is_literal_disjunction("\"alpha\" OR \"beta\""));
        for invalid in ["alpha* OR beta", "\"alpha\" OR ", "\"a\\q\"", "\"alpha"] {
            assert!(!is_literal_disjunction(invalid));
        }
    }

    #[test]
    fn field_expansion_requires_both_fields_once_with_their_exact_boosts() {
        use frankensearch_quill::{BooleanClause, QueryField};
        let leaf = |field, boost| {
            BooleanClause::new(
                Occur::Should,
                Query::term(vec![QueryField::new(field, boost)], "alpha".into()),
            )
        };
        let expected = BTreeSet::from(["alpha"]);
        let honest = Query::boolean(vec![leaf(1, 1.0), leaf(2, 2.0)], None);
        assert!(preserves_terms(&honest, &expected));
        for clauses in [
            vec![leaf(1, 1.0)],
            vec![leaf(1, 1.0), leaf(1, 1.0), leaf(2, 2.0)],
            vec![leaf(1, 1.0), leaf(2, 4.0)],
        ] {
            assert!(!preserves_terms(&Query::boolean(clauses, None), &expected));
        }
    }
}
