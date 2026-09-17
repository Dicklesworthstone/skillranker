//! Bounded query construction for the pinned default Quill schema.
//! This module has no roster, provider, persistence, or fallback effects.

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

#[cfg(test)]
mod tests {
    use super::*;

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
