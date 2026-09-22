//! Every CLI failure the pipeline constructs must name a documented error kind and carry that
//! kind's documented exit code.
//!
//! `src/output/mod.rs` owns the mapping: the `error_kinds!` macro derives each kind's exit from
//! `CliExit`, so a kind and its code cannot disagree there. `src/pipeline.rs` bypasses that. Its
//! helper takes the two separately —
//!
//! ```ignore
//! fn failure(code: u8, kind: &'static str, message: impl Into<String>) -> PipelineFailure
//! ```
//!
//! — and every call site writes the number by hand beside the string. `src/cli.rs` returns that
//! hand-written `code` as the process exit while the JSON envelope takes `error.code` from
//! `kind.exit_code()`, so a wrong pair makes the process exit and its own published document
//! disagree. A kind string that is not documented at all is worse: the envelope cannot find it in
//! `ErrorKind::ALL` and falls back to guessing from the code, which publishes a *different*
//! documented kind than the one the pipeline named.
//!
//! This test is deliberately source-level. The invariant is a property of the call sites, not of
//! any one execution, and several of the sites are defensive arms that only fire when sr has
//! already violated its own output contract — unreachable by construction from a test, and exactly
//! the places a wrong pair would sit unnoticed. Reading the source is the only way to check all of
//! them at once. See sr-jeua.

use skillranker::output::ErrorKind;

const PIPELINE_SOURCE: &str = include_str!("../src/pipeline.rs");
const CLI_SOURCE: &str = include_str!("../src/cli.rs");

/// A hand-written `(code, kind)` pair and where it is.
#[derive(Debug)]
struct HandWrittenPair {
    /// Read only through `Debug` in the assertion messages, which is the whole point of carrying
    /// it: a failure has to say *which* call site is wrong or it cannot be acted on.
    #[allow(dead_code)]
    line: usize,
    code: u8,
    kind: String,
}

/// Scan for `failure(<digits>, "<kind>"` allowing the arguments to be wrapped across lines.
///
/// Sites that derive the code, such as `failure(err.kind().exit_code() as u8, ...)`, do not match
/// the literal-digit shape and are skipped: those are already correct by construction and are the
/// pattern the fix for this whole class would generalise.
fn hand_written_pairs(source: &str) -> Vec<HandWrittenPair> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut search_from = 0;

    while let Some(offset) = source[search_from..].find("failure(") {
        let start = search_from + offset;
        search_from = start + "failure(".len();

        // `storage_failure(` and `failure_with_details(` are different functions; require a
        // non-identifier character before the name so this does not match them by accident.
        if start > 0 {
            let prev = bytes[start - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }

        let mut cursor = search_from;
        let skip_space = |cursor: &mut usize| {
            while *cursor < bytes.len() && (bytes[*cursor] as char).is_whitespace() {
                *cursor += 1;
            }
        };

        skip_space(&mut cursor);
        let digits_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == digits_start {
            continue; // not a literal code: derived, or the declaration itself
        }
        let Ok(code) = source[digits_start..cursor].parse::<u8>() else {
            continue;
        };

        skip_space(&mut cursor);
        if cursor >= bytes.len() || bytes[cursor] != b',' {
            continue;
        }
        cursor += 1;
        skip_space(&mut cursor);
        if cursor >= bytes.len() || bytes[cursor] != b'"' {
            continue;
        }
        cursor += 1;
        let kind_start = cursor;
        while cursor < bytes.len() && bytes[cursor] != b'"' {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            continue;
        }

        found.push(HandWrittenPair {
            line: source[..start].lines().count(),
            code,
            kind: source[kind_start..cursor].to_string(),
        });
    }

    found
}

/// Scan `src/cli.rs` for its own stringly-typed shape, `(<digits>u8, "<kind>"`.
///
/// `cli.rs` maps domain errors to CLI failures with the same hazard as pipeline.rs and a different
/// spelling, which is why scanning only pipeline.rs missed it: these sites are reachable by a user.
/// `sr feedback` on an event whose roster snapshot is incomplete lands on one of them.
fn cli_pairs(source: &str) -> Vec<HandWrittenPair> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut search_from = 0;

    while let Some(offset) = source[search_from..].find("u8,") {
        let marker = search_from + offset;
        search_from = marker + "u8,".len();

        // Walk back over the literal digits.
        let mut digits_end = marker;
        let mut digits_start = marker;
        while digits_start > 0 && bytes[digits_start - 1].is_ascii_digit() {
            digits_start -= 1;
        }
        if digits_start == digits_end {
            continue;
        }
        let Ok(code) = source[digits_start..digits_end].parse::<u8>() else {
            continue;
        };
        digits_end = digits_start; // only used to keep the line-count read below honest

        let mut cursor = search_from;
        while cursor < bytes.len() && (bytes[cursor] as char).is_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'"' {
            continue;
        }
        cursor += 1;
        let kind_start = cursor;
        while cursor < bytes.len() && bytes[cursor] != b'"' {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            continue;
        }
        let kind = &source[kind_start..cursor];
        if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
            continue;
        }

        found.push(HandWrittenPair {
            line: source[..digits_end].lines().count(),
            code,
            kind: kind.to_string(),
        });
    }

    found
}

fn documented_exit(kind: &str) -> Option<u8> {
    ErrorKind::ALL
        .iter()
        .find(|k| k.as_str() == kind)
        .map(|k| k.exit_code() as u8)
}

#[test]
fn every_hand_written_failure_pair_matches_the_documented_mapping() {
    let pairs = hand_written_pairs(PIPELINE_SOURCE);

    // If this trips, the scanner stopped recognising the call sites and the rest of the assertions
    // below would pass by vacuity, which is the one way a test like this can lie.
    assert!(
        pairs.len() >= 40,
        "only {} hand-written failure pairs were found in src/pipeline.rs, which is too few to be \
         the real call sites: the scanner has probably stopped matching the code's shape, so this \
         test would be passing without checking anything",
        pairs.len()
    );

    let cli = cli_pairs(CLI_SOURCE);
    assert!(
        cli.len() >= 60,
        "only {} (code, kind) tuples were found in src/cli.rs, too few to be the real sites: the \
         scanner has probably stopped matching the code's shape",
        cli.len()
    );

    let mut mismatched = Vec::new();
    for pair in pairs.iter().chain(cli.iter()) {
        if let Some(documented) = documented_exit(&pair.kind)
            && documented != pair.code
        {
            mismatched.push((pair, documented));
        }
    }

    assert!(
        mismatched.is_empty(),
        "src/pipeline.rs pairs a documented kind with the wrong exit code, so the process exit and \
         the error.code in its own published envelope disagree (found (line, code, kind) with the \
         documented code beside it): {:#?}",
        mismatched
    );
}

/// The kinds `src/pipeline.rs` names that the documented set does not contain.
///
/// These are a known gap, not an accident, and they are listed here rather than fixed because
/// choosing their replacement is a contract decision rather than a bug fix. `src/cli.rs` cannot find
/// them in `ErrorKind::ALL`, so it guesses a kind from the exit code and publishes *that* instead:
/// `contract-violation` is published as `empty-roster` and `ambiguous-branch` as `malformed-input`.
/// Telling a caller its roster is empty when sr's own output envelope failed validation is the
/// misleading part.
///
/// Every one of these sites is a defensive arm that fires only when sr has already violated its own
/// output contract (`OutputDocument::from_value` or `with_trace` rejecting a document sr built), so
/// none is reachable by a user today, which is why this is a ratchet and not a failing gate. The
/// honest fix needs an owner's judgement: the documented set has no kind meaning "our own envelope
/// failed validation", `output-limit` fits the trace-bound cases but not "Optional storage warning
/// is invalid", and adding a kind changes docs/output-contract.md. See sr-jeua, which lays out the
/// options.
///
/// This test fails if anyone adds a NEW undocumented kind, and fails when the gap is closed, at
/// which point delete it along with the list.
const KNOWN_UNDOCUMENTED_KINDS: &[&str] = &["ambiguous-branch", "contract-violation"];
const KNOWN_UNDOCUMENTED_SITES: usize = 9;

#[test]
fn the_undocumented_kinds_are_only_the_known_gap_and_it_has_not_grown() {
    let pairs = hand_written_pairs(PIPELINE_SOURCE);
    let undocumented: Vec<_> = pairs
        .iter()
        .filter(|p| documented_exit(&p.kind).is_none())
        .collect();

    let mut kinds: Vec<&str> = undocumented.iter().map(|p| p.kind.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();

    assert_eq!(
        kinds, KNOWN_UNDOCUMENTED_KINDS,
        "the set of undocumented error kinds in src/pipeline.rs changed. Adding one means \
         src/cli.rs will publish some other documented kind guessed from the exit code in its \
         place; removing one means the gap closed and this ratchet should be deleted. Sites: {:#?}",
        undocumented
    );

    assert_eq!(
        undocumented.len(),
        KNOWN_UNDOCUMENTED_SITES,
        "the number of call sites using an undocumented kind changed, so the known gap grew or \
         shrank without this ratchet being updated: {:#?}",
        undocumented
    );
}

/// The undocumented kinds in `src/cli.rs`, which unlike pipeline.rs's are **reachable by a user**.
///
/// Two of these publish something materially untrue, because `src/cli.rs` cannot find the string in
/// `ErrorKind::ALL` and falls back to guessing from the exit code:
///
/// - `missing-snapshot` (10) is published as **`invalid-provider-response`**. It is raised when the
///   roster snapshot retained for an event is incomplete, which is a local coverage condition with
///   nothing to do with the provider. Reached by walking the documented adoption loop: rank natively,
///   observe, then `sr feedback --skill <invocation name>`. The documented set already contains
///   `incomplete-roster` (5), which says exactly what happened.
/// - `revision-conflict` (11) is published as **`cache-miss`**. Exit 11 is documented as an
///   offline/cache-only miss, so a caller told its explicit feedback mutation lost a revision check
///   hears "nothing was cached" and may retry rather than re-read. There is no documented conflict
///   kind, which is why this one needs an owner's decision rather than my guess.
///
/// The other five map to `invalid-usage` or `storage-failure`, which is imprecise but not misleading.
///
/// Listed rather than fixed for the same reason as the pipeline set: picking replacements edits
/// docs/output-contract.md. See sr-jeua. This fails if a new one appears or the gap closes.
const KNOWN_UNDOCUMENTED_CLI_KINDS: &[&str] = &[
    "event-not-found",
    "ineligible-alternative",
    "invalid-arguments",
    "missing-snapshot",
    "revision-conflict",
    "serialization-failure",
    "skill-not-found",
];

#[test]
fn the_undocumented_cli_kinds_are_only_the_known_gap_and_it_has_not_grown() {
    let pairs = cli_pairs(CLI_SOURCE);
    let undocumented: Vec<_> = pairs
        .iter()
        .filter(|p| documented_exit(&p.kind).is_none())
        .collect();

    let mut kinds: Vec<&str> = undocumented.iter().map(|p| p.kind.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();

    assert_eq!(
        kinds, KNOWN_UNDOCUMENTED_CLI_KINDS,
        "the set of undocumented error kinds in src/cli.rs changed. These are user-reachable, so a \
         new one means users are being handed a kind guessed from the exit code; a removed one means \
         the gap closed and this ratchet should shrink. Sites: {:#?}",
        undocumented
    );
}

#[test]
fn the_documented_mapping_itself_covers_every_kind_the_pipeline_uses() {
    // The companion to the case above, and the reason it is not enough on its own: that one would
    // also pass if `ErrorKind::ALL` were empty. This pins the direction the output contract's table
    // is authoritative in, using the real accessors rather than a copy of the table.
    for kind in ErrorKind::ALL {
        let exit = kind.exit_code() as u8;
        assert!(
            (2..=11).contains(&exit),
            "{} maps to exit {}, outside the documented ordinary CLI range 2..=11",
            kind.as_str(),
            exit
        );
        assert_eq!(
            documented_exit(kind.as_str()),
            Some(exit),
            "{} is not findable by its own wire string",
            kind.as_str()
        );
    }
}
