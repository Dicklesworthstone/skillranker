//! Every CLI failure the pipeline constructs must name a documented error kind and carry that
//! kind's documented exit code.
//!
//! `src/output/mod.rs` owns the mapping: the `error_kinds!` macro derives each kind's exit from
//! `CliExit`, so a kind and its code cannot disagree there. `src/pipeline.rs` used to bypass that:
//! its helper took `(code: u8, kind: &'static str, ...)` and every call site wrote the number by hand
//! beside the string. `src/cli.rs` returns that code as the process exit while the JSON envelope
//! takes `error.code` from `kind.exit_code()`, so a wrong pair made the process exit and its own
//! published document disagree, and an undocumented kind string was replaced by a kind guessed from
//! the code. The pipeline's helper now takes an `ErrorKind` and derives the code, so the pipeline
//! must contain no hand-written pair at all. `src/cli.rs` still writes tuples by hand and is checked
//! pair by pair.
//!
//! This test is deliberately source-level. The invariant is a property of the call sites, not of
//! any one execution, and several of the sites are defensive arms that only fire when sr has
//! already violated its own output contract — unreachable by construction from a test, and exactly
//! the places a wrong pair would sit unnoticed. Reading the source is the only way to check all of
//! them at once. See sr-jeua.

use skillranker::output::ErrorKind;

const PIPELINE_SOURCES: &[(&str, &str)] = &[
    ("src/pipeline.rs", include_str!("../src/pipeline.rs")),
    (
        "src/pipeline/roster.rs",
        include_str!("../src/pipeline/roster.rs"),
    ),
    (
        "src/pipeline/cass_source.rs",
        include_str!("../src/pipeline/cass_source.rs"),
    ),
];
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

/// Scan for a raw `(<digits>, "<kebab-kind>"` tuple, the other way to build a failure by hand.
fn raw_tuple_pairs(source: &str) -> Vec<HandWrittenPair> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    for (start, _) in source.match_indices('(') {
        let mut cursor = start + 1;
        while cursor < bytes.len() && (bytes[cursor] as char).is_whitespace() {
            cursor += 1;
        }
        let digits_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        let Ok(code) = source[digits_start..cursor].parse::<u8>() else {
            continue;
        };
        if source[cursor..].starts_with("u8") {
            cursor += 2;
        }
        if cursor >= bytes.len() || bytes[cursor] != b',' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && (bytes[cursor] as char).is_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'"' {
            continue;
        }
        let kind_start = cursor + 1;
        let Some(len) = source[kind_start..].find('"') else {
            continue;
        };
        let kind = &source[kind_start..kind_start + len];
        if kind.contains('-') && kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
            found.push(HandWrittenPair {
                line: source[..start].lines().count(),
                code,
                kind: kind.to_string(),
            });
        }
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
fn the_pipeline_derives_every_failure_code_from_its_kind() {
    let mut derived = 0;
    for (path, source) in PIPELINE_SOURCES {
        let by_hand: Vec<_> = hand_written_pairs(source)
            .into_iter()
            .chain(raw_tuple_pairs(source))
            .collect();
        assert!(
            by_hand.is_empty(),
            "{path} writes a failure's exit code by hand beside its kind; use \
             failure(ErrorKind::..., message) so the code is derived: {by_hand:#?}"
        );
        derived +=
            source.matches("failure(ErrorKind::").count() + source.matches("failure(\n").count();
    }
    // If this trips, the call sites changed shape and the emptiness above proves nothing.
    assert!(
        derived >= 60,
        "only {derived} failure(ErrorKind::..) sites found across the pipeline sources"
    );
}

#[test]
fn every_hand_written_cli_pair_matches_the_documented_mapping() {
    let cli = cli_pairs(CLI_SOURCE);
    // If this trips, the scanner stopped recognising the call sites and the assertion below would
    // pass by vacuity, which is the one way a test like this can lie.
    assert!(
        cli.len() >= 60,
        "only {} (code, kind) tuples were found in src/cli.rs, too few to be the real sites: the \
         scanner has probably stopped matching the code's shape",
        cli.len()
    );
    let mismatched: Vec<_> = cli
        .iter()
        .filter_map(|pair| {
            documented_exit(&pair.kind)
                .filter(|documented| *documented != pair.code)
                .map(|documented| (pair, documented))
        })
        .collect();
    assert!(
        mismatched.is_empty(),
        "src/cli.rs pairs a documented kind with the wrong exit code, so the process exit and the \
         error.code in its own published envelope disagree (found (line, code, kind) with the \
         documented code beside it): {mismatched:#?}"
    );
}

/// The undocumented kinds in `src/cli.rs`, which unlike pipeline.rs's are **reachable by a user**.
///
/// These fall back to a guessed kind because `src/cli.rs` cannot find the string in
/// `ErrorKind::ALL`. All five map to `invalid-usage` or `storage-failure`, which is imprecise but
/// not misleading. (`revision-conflict`, once published as `cache-miss`, is now a documented kind.)
/// This fails if a new one appears or the gap closes.
const KNOWN_UNDOCUMENTED_CLI_KINDS: &[&str] = &[
    "event-not-found",
    "ineligible-alternative",
    "invalid-arguments",
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

#[test]
fn the_raw_tuple_scanner_finds_the_shape_it_guards_against() {
    // The emptiness assertion above is only as good as this scanner, so prove it matches the
    // shapes the transport mapper used before its codes were derived, and ignores lookalikes.
    let sample = r#"
        TransportErrorKind::HttpStatus(429) => {
            (4, "provider-cooldown", "Rate limited by provider".into())
        }
        _ => (
            4u8,
            "network-failure",
            format!("Transport error"),
        ),
        let pair = (3, "plain");
        let range = (0, "x");
    "#;
    let found: Vec<_> = raw_tuple_pairs(sample)
        .into_iter()
        .map(|pair| (pair.code, pair.kind))
        .collect();
    assert_eq!(
        found,
        [
            (4, "provider-cooldown".to_string()),
            (4, "network-failure".to_string())
        ]
    );
}
