//! Integration tests for skill metadata and frontmatter parsing.
//!
//! Satisfies contract boundary `p2_frontmatter_parsing` (sr-roadmap-l1i.3.3)
//! mapped in `tests/contract_matrix.toml`.

use skillranker::roster::frontmatter::*;
use skillranker::roster::{ParseWarning, UsageKind};

const CANARY: &str = "canary-secret-token-99887766";

fn assert_canary_not_leaked(text: &str) {
    assert!(
        !text.contains(CANARY),
        "security violation: private text leaked in diagnostic: {text}"
    );
}

// -----------------------------------------------------------------------------
// 1. Boundary: p2_frontmatter_parsing
// -----------------------------------------------------------------------------

#[test]
fn frontmatter_limits_and_fallbacks() {
    // 1. Valid frontmatter parsing with all standard fields
    let valid_doc = r#"---
name: cargo-test-runner
description: Runs cargo tests with remote RCH isolation and summarizes failures.
disable-model-invocation: true
user-invocable: false
usage: workflow
aliases:
  - test-runner
  - rch-tester
tags:
  - rust
  - testing
phases:
  - execution
  - verification
---

# Cargo Test Runner

Detailed markdown body explaining how the runner invokes RCH.
"#;

    let parsed = parse_skill_metadata(valid_doc.as_bytes()).expect("valid metadata must parse");
    assert_eq!(parsed.name.as_deref(), Some("cargo-test-runner"));
    assert_eq!(
        parsed.description,
        "Runs cargo tests with remote RCH isolation and summarizes failures."
    );
    assert!(!parsed.agent_invocable);
    assert!(!parsed.user_invocable);
    assert_eq!(parsed.usage_kind, UsageKind::Workflow);
    assert_eq!(parsed.aliases, vec!["test-runner", "rch-tester"]);
    assert_eq!(parsed.tags.len(), 2);
    assert_eq!(parsed.phases.len(), 2);
    assert!(parsed.has_frontmatter);
    assert!(parsed.parse_warnings.is_empty());

    // 2. Missing frontmatter: fallback to H1 title and first paragraph
    let no_fm_doc = r#"# Code Review Helper

Analyzes git diffs and reports security findings according to the checklist.

## Usage

Run this skill before committing any changes.
"#;

    let parsed_fallback =
        parse_skill_metadata(no_fm_doc.as_bytes()).expect("fallback document must parse");
    assert_eq!(parsed_fallback.name.as_deref(), Some("Code Review Helper"));
    assert_eq!(
        parsed_fallback.description,
        "Analyzes git diffs and reports security findings according to the checklist."
    );
    assert!(parsed_fallback.agent_invocable);
    assert!(parsed_fallback.user_invocable);
    assert_eq!(parsed_fallback.usage_kind, UsageKind::Unknown);
    assert!(!parsed_fallback.has_frontmatter);
    assert_eq!(
        parsed_fallback.parse_warnings,
        vec![ParseWarning::MissingFrontmatter]
    );

    // 3. Frontmatter size limit: > 16 KiB must be rejected
    let huge_fm = format!(
        "---\nname: huge-skill\ndescription: {}\n---\n# Body\n",
        "a".repeat(17 * 1024)
    );
    let err_fm = parse_skill_metadata(huge_fm.as_bytes()).unwrap_err();
    assert!(
        matches!(err_fm, FrontmatterError::FrontmatterTooLarge(_)),
        "expected FrontmatterTooLarge, got: {err_fm:?}"
    );

    // 4. File size limit: > 256 KiB must be rejected
    let huge_file = format!("# Large Skill\n\n{}", "content line\n".repeat(25 * 1024));
    assert!(huge_file.len() > MAX_SKILL_FILE_BYTES);
    let err_file = parse_skill_metadata(huge_file.as_bytes()).unwrap_err();
    assert!(
        matches!(err_file, FrontmatterError::FileTooLarge(_)),
        "expected FileTooLarge, got: {err_file:?}"
    );
}

#[test]
fn code_fence_isolation_for_headings_and_sections() {
    // A bash comment `# shell comment` inside code fence must NOT be treated as H1
    let doc = r#"```bash
# This is a bash script comment, not a markdown H1 heading
cargo build --release
```

# Real Skill Title

Real skill description paragraph.
"#;

    let parsed = parse_skill_metadata(doc.as_bytes()).expect("parsed doc");
    assert_eq!(parsed.name.as_deref(), Some("Real Skill Title"));
    assert_eq!(parsed.description, "Real skill description paragraph.");

    // Tilde code fence ~~~
    let tilde_doc = r#"~~~python
# Python comment
import sys
~~~

# Tilde Skill

Tilde skill description.
"#;
    let parsed_tilde = parse_skill_metadata(tilde_doc.as_bytes()).expect("parsed tilde");
    assert_eq!(parsed_tilde.name.as_deref(), Some("Tilde Skill"));
    assert_eq!(parsed_tilde.description, "Tilde skill description.");
}

#[test]
fn folded_and_literal_multiline_descriptions() {
    let folded_doc = r#"---
name: folded-skill
description: >
  This is a multiline
  folded description that
  should be joined by single spaces.

  And this should be separated by newline.
---
# Body
"#;

    let parsed = parse_skill_metadata(folded_doc.as_bytes()).expect("parsed folded");
    assert_eq!(
        parsed.description,
        "This is a multiline folded description that should be joined by single spaces.\nAnd this should be separated by newline."
    );

    let literal_doc = r#"---
name: literal-skill
description: |
  Line 1
  Line 2
  Line 3
---
# Body
"#;

    let parsed_lit = parse_skill_metadata(literal_doc.as_bytes()).expect("parsed literal");
    assert_eq!(parsed_lit.description, "Line 1\nLine 2\nLine 3");
}

#[test]
fn duplicate_keys_are_strictly_rejected() {
    let duplicate_doc = r#"---
name: skill-one
description: First description
name: skill-two
---
# Body
"#;

    let err = parse_skill_metadata(duplicate_doc.as_bytes()).unwrap_err();
    assert_eq!(err, FrontmatterError::DuplicateKey);
    let err_msg = format!("{err}");
    assert_eq!(err_msg, "duplicate key rejected in skill frontmatter");
}

#[test]
fn yaml_anchors_and_aliases_are_forbidden() {
    let anchor_doc = r#"---
name: &default_name my-skill
alias_name: *default_name
description: Test skill
---
# Body
"#;

    let err = parse_skill_metadata(anchor_doc.as_bytes()).unwrap_err();
    assert_eq!(err, FrontmatterError::AliasForbidden);
}

#[test]
fn unclosed_frontmatter_rejected() {
    let unclosed = r#"---
name: unclosed-skill
description: Missing ending delimiter
"#;

    let err = parse_skill_metadata(unclosed.as_bytes()).unwrap_err();
    assert_eq!(err, FrontmatterError::UnclosedFrontmatter);
}

#[test]
fn utf8_bom_and_crlf_handling() {
    // Document with UTF-8 BOM and CRLF newlines
    let bom_crlf = format!(
        "\u{feff}---\r\nname: bom-skill\r\ndescription: UTF-8 BOM and CRLF support\r\n---\r\n\r\n# BOM Skill\r\n\r\nBody text.\r\n"
    );

    let parsed = parse_skill_metadata(bom_crlf.as_bytes()).expect("parsed bom crlf");
    assert_eq!(parsed.name.as_deref(), Some("bom-skill"));
    assert_eq!(parsed.description, "UTF-8 BOM and CRLF support");
    assert!(parsed.has_frontmatter);
}

#[test]
fn dynamic_substitutions_remain_inert_text() {
    let doc = r#"---
name: runner-$(whoami)
description: Run !`cat /etc/passwd` and ${SECRET} with {{template_arg}}.
---
# Body with `rm -rf /` and $(rm -rf /)
"#;

    let parsed = parse_skill_metadata(doc.as_bytes()).expect("parsed substitutions");
    assert_eq!(parsed.name.as_deref(), Some("runner-$(whoami)"));
    assert_eq!(
        parsed.description,
        "Run !`cat /etc/passwd` and ${SECRET} with {{template_arg}}."
    );
}

#[test]
fn parser_errors_never_echo_raw_yaml_or_canary_secrets() {
    // Malformed YAML with secret canary
    let bad_yaml =
        format!("---\nname: my-skill\ninvalid-syntax: {CANARY} [unclosed bracket\n---\n");
    let err = parse_skill_metadata(bad_yaml.as_bytes()).unwrap_err();
    let err_str = format!("{err}");
    let err_debug = format!("{err:?}");

    assert_canary_not_leaked(&err_str);
    assert_canary_not_leaked(&err_debug);

    // Duplicate key with canary value
    let dup_canary = format!("---\nname: {}\nname: other\n---\n", CANARY);
    let dup_err = parse_skill_metadata(dup_canary.as_bytes()).unwrap_err();
    assert_canary_not_leaked(&format!("{dup_err}"));
    assert_canary_not_leaked(&format!("{dup_err:?}"));
}

#[test]
fn wide_and_body_excerpt_scalar_truncation() {
    let long_desc = "a".repeat(300);
    let long_body = "b".repeat(1500);
    let doc =
        format!("---\nname: long-skill\ndescription: {long_desc}\n---\n# Title\n\n{long_body}");

    let parsed = parse_skill_metadata(doc.as_bytes()).expect("parsed long");
    assert_eq!(parsed.description.len(), 300);
    assert_eq!(parsed.description_short.as_str().chars().count(), 160);
    assert_eq!(parsed.body_excerpt.as_str().chars().count(), 700);
}
