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
    let bom_crlf = "\u{feff}---\r\nname: bom-skill\r\ndescription: UTF-8 BOM and CRLF support\r\n---\r\n\r\n# BOM Skill\r\n\r\nBody text.\r\n";

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

// -----------------------------------------------------------------------------
// 2. Boundary: p2_atomic_export
// Unit Property Test: tests/roster_contract.rs::atomic_private_exports
// -----------------------------------------------------------------------------

#[test]
fn atomic_private_exports() {
    use skillranker::storage::export::{ExportConfig, ExportError, export_private_atomic};
    use std::fs::{self, DirBuilder};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    let test_dir = std::env::temp_dir().join(format!(
        "sr-export-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    DirBuilder::new().mode(0o700).create(&test_dir).unwrap();

    let target_file = test_dir.join("roster_snapshot.json");
    let content = b"{\"skills\": [\"test-skill\"]}";

    // 1. Successful atomic export with 0600 permissions
    let config = ExportConfig::for_snapshot();
    export_private_atomic(&target_file, content, config)
        .expect("export to new target must succeed");

    // Verify content
    let read_back = fs::read(&target_file).expect("read exported file");
    assert_eq!(read_back, content);

    // Verify owner-only permissions (0600)
    let meta = fs::metadata(&target_file).expect("metadata");
    assert_eq!(
        meta.mode() & 0o777,
        0o600,
        "exported file must have strict owner-only 0600 permissions"
    );

    // 2. No-clobber: target already exists -> must fail with TargetAlreadyExists
    let second_content = b"{\"skills\": [\"second-snapshot\"]}";
    let err_clobber = export_private_atomic(&target_file, second_content, config).unwrap_err();
    assert!(
        matches!(err_clobber, ExportError::TargetAlreadyExists(_)),
        "expected TargetAlreadyExists, got {err_clobber:?}"
    );

    // Verify original content was NOT clobbered or modified
    let read_after = fs::read(&target_file).expect("read after clobber attempt");
    assert_eq!(read_after, content);

    // 3. Symlink rejection: destination is a symlink -> must be rejected
    let symlink_target = test_dir.join("symlink_target.json");
    fs::write(&symlink_target, b"original-target").unwrap();
    let symlink_dest = test_dir.join("export_symlink.json");
    std::os::unix::fs::symlink(&symlink_target, &symlink_dest).unwrap();

    let err_symlink =
        export_private_atomic(&symlink_dest, b"malicious-overwrite", config).unwrap_err();
    assert!(matches!(err_symlink, ExportError::TargetAlreadyExists(_)));
    // Target pointed to by symlink was untouched
    assert_eq!(fs::read(&symlink_target).unwrap(), b"original-target");

    // Broken symlink rejection
    let broken_dest = test_dir.join("broken_symlink.json");
    std::os::unix::fs::symlink(test_dir.join("nonexistent.json"), &broken_dest).unwrap();
    let err_broken = export_private_atomic(&broken_dest, b"test-data", config).unwrap_err();
    assert!(matches!(err_broken, ExportError::TargetAlreadyExists(_)));

    // 4. Oversized payload rejected before write
    let tiny_config = ExportConfig { max_bytes: 10 };
    let large_target = test_dir.join("large.json");
    let err_oversized =
        export_private_atomic(&large_target, b"this-is-longer-than-ten-bytes", tiny_config)
            .unwrap_err();
    assert!(matches!(err_oversized, ExportError::Oversized { .. }));
    assert!(!large_target.exists());

    // 5. Cleanup of partial files: no dangling .sr-partial-* files in directory
    let entries: Vec<_> = fs::read_dir(&test_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|name| name.contains("sr-partial"))
        .collect();
    assert!(
        entries.is_empty(),
        "no partial files should remain in directory: {entries:?}"
    );

    // 6. Safe directory validation: non-existent directory rejected
    let bad_dir_target = test_dir.join("nonexistent_subfolder").join("file.json");
    let err_bad_dir = export_private_atomic(&bad_dir_target, b"data", config).unwrap_err();
    assert!(matches!(err_bad_dir, ExportError::InvalidDirectory(_)));
}

#[test]
fn concurrent_export_race_prevents_clobber() {
    use skillranker::storage::export::{ExportConfig, ExportError, export_private_atomic};
    use std::fs::DirBuilder;
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::Arc;
    use std::thread;

    let test_dir = std::env::temp_dir().join(format!(
        "sr-race-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    DirBuilder::new().mode(0o700).create(&test_dir).unwrap();

    let target_file = Arc::new(test_dir.join("contended_export.json"));
    let mut handles = Vec::new();

    // Spawn 8 concurrent threads all attempting to export to the exact same target
    for i in 0..8 {
        let target = Arc::clone(&target_file);
        handles.push(thread::spawn(move || {
            let content = format!("thread-{i}-content").into_bytes();
            let config = ExportConfig::for_snapshot();
            export_private_atomic(&target, &content, config)
        }));
    }

    let mut successes = 0;
    let mut collision_errors = 0;

    for handle in handles {
        match handle.join().unwrap() {
            Ok(()) => successes += 1,
            Err(ExportError::TargetAlreadyExists(_)) => collision_errors += 1,
            Err(other) => panic!("unexpected error in race test: {other:?}"),
        }
    }

    // Exactly ONE writer must succeed, and all others must get TargetAlreadyExists
    assert_eq!(successes, 1, "exactly one writer must win the race");
    assert_eq!(
        collision_errors, 7,
        "all other writers must detect target collision"
    );
}
