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

    let test_dir = std::fs::canonicalize("/tmp").unwrap().join(format!(
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

    let test_dir = std::fs::canonicalize("/tmp").unwrap().join(format!(
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

// -----------------------------------------------------------------------------
// 3. Boundary: p2_explicit_resolution (sr-roadmap-l1i.3.6)
// -----------------------------------------------------------------------------

#[test]
fn local_explicit_resolution() {
    use skillranker::authorized_read::{AuthorizedRoot, AuthorizedRoots};
    use skillranker::identity::{LogicalSkillKey, SkillId, SourceId};
    use skillranker::limits::{
        DurationMillis, HOOK_STDIN_BYTES, MAX_EXPLICIT_REQUESTS, SKILL_FILE_BYTES,
    };
    use skillranker::roster::explicit::*;
    use skillranker::roster::resolution::{BindingSpec, ResolvedRoster, SkillEntry};
    use skillranker::roster::{InvocationKind, InvocationName, InvocationRestrictions, Visibility};
    use skillranker::runtime::{EntryClock, ProcessInvocation};
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    // -------------------------------------------------------------------------
    // Sub-case 1: Prompt Directive Parser: Quotes, Code Blocks, Contractions
    // -------------------------------------------------------------------------
    // Quoted directives must be completely ignored
    let quoted_prompt = format!(
        "Please read: \"use skill fake-skill-quoted\" and 'require skill single-quoted'. Also {CANARY}"
    );
    let dirs = parse_prompt_directives(&quoted_prompt);
    assert!(
        dirs.is_empty(),
        "quoted directives must be ignored, got: {dirs:?}"
    );

    // Code blocks (fenced and inline) must be completely ignored
    let code_prompt = "Example in markdown:\n```\nuse skill fenced-code-skill\n```\nAlso see `use skill inline-code-skill`.";
    let dirs = parse_prompt_directives(code_prompt);
    assert!(
        dirs.is_empty(),
        "code block directives must be ignored, got: {dirs:?}"
    );

    // Contractions (don't, can't) must NOT be stripped as quotes
    let contraction_prompt = "Don't use skill bad-tool; user's preference.";
    let dirs = parse_prompt_directives(contraction_prompt);
    assert_eq!(
        dirs,
        vec![ParsedDirective {
            target: "bad-tool".into(),
            kind: DirectiveKind::Exclude,
        }],
        "contractions must be preserved for natural negative directives"
    );

    // Slash commands
    let slash_prompt =
        "/use-skill tool-a\n/require-skill tool-b\n/exclude-skill tool-c\n/no-skill tool-d";
    let dirs = parse_prompt_directives(slash_prompt);
    assert_eq!(
        dirs,
        vec![
            ParsedDirective {
                target: "tool-a".into(),
                kind: DirectiveKind::Require,
            },
            ParsedDirective {
                target: "tool-b".into(),
                kind: DirectiveKind::Require,
            },
            ParsedDirective {
                target: "tool-c".into(),
                kind: DirectiveKind::Exclude,
            },
            ParsedDirective {
                target: "tool-d".into(),
                kind: DirectiveKind::Exclude,
            },
        ]
    );

    // Natural language directives across punctuation
    let natural_prompt = "Please use skill skill-alpha. Also do not use skill skill-beta.";
    let dirs = parse_prompt_directives(natural_prompt);
    assert_eq!(
        dirs,
        vec![
            ParsedDirective {
                target: "skill-alpha".into(),
                kind: DirectiveKind::Require,
            },
            ParsedDirective {
                target: "skill-beta".into(),
                kind: DirectiveKind::Exclude,
            },
        ]
    );

    // -------------------------------------------------------------------------
    // Setup Roster Fixture
    // -------------------------------------------------------------------------
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let test_dir = std::fs::canonicalize("/tmp").unwrap().join(format!(
        "sr-explicit-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&test_dir).unwrap();

    let clock = EntryClock::capture_with(
        DurationMillis::new("test_total", 30_000, 30_000).unwrap(),
        DurationMillis::new("test_cleanup", 200, 30_000).unwrap(),
    )
    .unwrap();
    let runtime = ProcessInvocation::from_clock(clock).unwrap();
    let cx = runtime.request_cx().unwrap();

    fn create_skill_entry(
        test_dir: &Path,
        relative: &str,
        name: &str,
        source: &str,
        priority: Option<i32>,
        restrictions: InvocationRestrictions,
        visibility: Visibility,
    ) -> SkillEntry {
        let roots = AuthorizedRoots::single(AuthorizedRoot::open_absolute(test_dir).unwrap());
        let file_path = test_dir.join(relative);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let content = format!("# {name}\n\nSkill documentation for {name}.");
        fs::write(&file_path, content).unwrap();

        let read = roots
            .read_bounded(0, Path::new(relative), SKILL_FILE_BYTES)
            .unwrap();

        SkillEntry::from_read(
            BindingSpec {
                source: SourceId::new(source).unwrap(),
                logical_key: LogicalSkillKey::new(relative).unwrap(),
                invocation: InvocationName::new(name).unwrap(),
                priority,
                visibility,
                restrictions,
            },
            read,
        )
        .unwrap()
    }

    let allow = InvocationRestrictions {
        agent_invocable: true,
        user_invocable: true,
    };
    let manual_only = InvocationRestrictions {
        agent_invocable: false,
        user_invocable: true,
    };
    let forbidden = InvocationRestrictions {
        agent_invocable: false,
        user_invocable: false,
    };
    let verified = Visibility::Verified {
        contract_version: "test-adapter-v1".into(),
    };

    let mut entries = Vec::new();

    // 1. Create 35 normal skills to test 32 vs 33 limit
    for i in 1..=35 {
        let rel = format!("skills/skill_{i:02}.md");
        let name = format!("skill-{i:02}");
        entries.push(create_skill_entry(
            &test_dir,
            &rel,
            &name,
            "source-main",
            Some(10),
            allow,
            verified.clone(),
        ));
    }

    // 2. Manual-only skill
    entries.push(create_skill_entry(
        &test_dir,
        "skills/manual_tool.md",
        "manual-tool",
        "source-main",
        Some(10),
        manual_only,
        verified.clone(),
    ));

    // 3. Forbidden skill
    entries.push(create_skill_entry(
        &test_dir,
        "skills/forbidden_tool.md",
        "forbidden-tool",
        "source-main",
        Some(10),
        forbidden,
        verified.clone(),
    ));

    // 4. Ambiguous colliding skills (same name, equal priority)
    entries.push(create_skill_entry(
        &test_dir,
        "source_a/ambig_tool.md",
        "ambig-tool",
        "source-a",
        None,
        allow,
        verified.clone(),
    ));
    entries.push(create_skill_entry(
        &test_dir,
        "source_b/ambig_tool.md",
        "ambig-tool",
        "source-b",
        None,
        allow,
        verified.clone(),
    ));

    // 5. Shadowed colliding skills (same name, distinct priority)
    entries.push(create_skill_entry(
        &test_dir,
        "source_hi/shadow_tool.md",
        "shadow-tool",
        "source-hi",
        Some(2),
        allow,
        verified.clone(),
    ));
    entries.push(create_skill_entry(
        &test_dir,
        "source_lo/shadow_tool.md",
        "shadow-tool",
        "source-lo",
        Some(1),
        allow,
        verified.clone(),
    ));

    // 6. Unverified skill
    entries.push(create_skill_entry(
        &test_dir,
        "skills/unverified_tool.md",
        "unverified-tool",
        "source-main",
        Some(10),
        allow,
        Visibility::Unverified,
    ));

    // An adapter-proven alternate invocation of the same physical skill.
    // Read the existing bytes; an alias does not create a second skill file.
    let roots = AuthorizedRoots::single(AuthorizedRoot::open_absolute(&test_dir).unwrap());
    entries.push(
        SkillEntry::from_read(
            BindingSpec {
                source: SourceId::new("source-alias").unwrap(),
                logical_key: LogicalSkillKey::new("skills/skill_02.md").unwrap(),
                invocation: InvocationName::new("second-skill").unwrap(),
                priority: Some(10),
                visibility: verified.clone(),
                restrictions: allow,
            },
            roots
                .read_bounded(0, Path::new("skills/skill_02.md"), SKILL_FILE_BYTES)
                .unwrap(),
        )
        .unwrap(),
    );
    let roster = ResolvedRoster::resolve(entries, false, &cx, &clock).unwrap();

    // -------------------------------------------------------------------------
    // Sub-case 2: Single Valid Skill Resolved Offline (Bypasses Jev / Gating)
    // -------------------------------------------------------------------------
    let req = ExplicitResolutionRequest {
        user_prompt: Some("Please use skill skill-01 to run the test.".into()),
        ..Default::default()
    };
    let res = resolve_explicit_requirements(&req, &roster).expect("must resolve");
    match res {
        ExplicitResolutionResult::Resolved {
            skills,
            excluded_skills,
        } => {
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].invocation.as_str(), "skill-01");
            assert_eq!(skills[0].kind, InvocationKind::Agent);
            assert!(!skills[0].manual_only);
            assert!(excluded_skills.is_empty());
        }
        other => panic!("expected Resolved, got {other:?}"),
    }

    // -------------------------------------------------------------------------
    // Sub-case 3: Manual-Only Skill Resolves Without Authorizing File-Read Bypass
    // -------------------------------------------------------------------------
    let req_manual = ExplicitResolutionRequest {
        user_prompt: Some("Please run skill manual-tool for this operation.".into()),
        ..Default::default()
    };
    let res_manual = resolve_explicit_requirements(&req_manual, &roster).expect("must resolve");
    match res_manual {
        ExplicitResolutionResult::Resolved { skills, .. } => {
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].invocation.as_str(), "manual-tool");
            assert_eq!(skills[0].kind, InvocationKind::ManualOnly);
            assert!(
                skills[0].manual_only,
                "manual_only must be true to indicate user execution"
            );
        }
        other => panic!("expected Resolved for manual-only, got {other:?}"),
    }

    // -------------------------------------------------------------------------
    // Sub-case 4: Missing Skill Fails All-Actionable Output (Unavailable / Exit 5)
    // -------------------------------------------------------------------------
    let req_missing = ExplicitResolutionRequest {
        user_prompt: Some(format!(
            "Please use skill nonexistent-ghost-tool. Secret: {CANARY}"
        )),
        ..Default::default()
    };
    let res_missing =
        resolve_explicit_requirements(&req_missing, &roster).expect("must return result");
    match res_missing {
        ExplicitResolutionResult::Unavailable { unresolved } => {
            assert_eq!(unresolved.len(), 1);
            assert_eq!(unresolved[0].target, "nonexistent-ghost-tool");
            assert_eq!(unresolved[0].reason, UnresolvedReason::Missing);
            assert_canary_not_leaked(&unresolved[0].diagnostic);
        }
        other => panic!("expected Unavailable for missing, got {other:?}"),
    }

    // -------------------------------------------------------------------------
    // Sub-case 5: Ambiguous Skill Fails All-Actionable Output
    // -------------------------------------------------------------------------
    let req_ambig = ExplicitResolutionRequest {
        user_prompt: Some("use skill ambig-tool".into()),
        ..Default::default()
    };
    let res_ambig = resolve_explicit_requirements(&req_ambig, &roster).expect("must return result");
    match res_ambig {
        ExplicitResolutionResult::Unavailable { unresolved } => {
            assert_eq!(unresolved.len(), 1);
            assert_eq!(unresolved[0].target, "ambig-tool");
            assert_eq!(unresolved[0].reason, UnresolvedReason::Ambiguous);
        }
        other => panic!("expected Unavailable for ambiguous, got {other:?}"),
    }

    // -------------------------------------------------------------------------
    // Sub-case 6: Shadowed Skill Fails All-Actionable Output
    // -------------------------------------------------------------------------
    let shadowed_id = SkillId::from_source(
        &SourceId::new("source-lo").unwrap(),
        &LogicalSkillKey::new("source_lo/shadow_tool.md").unwrap(),
    );
    let req_shadowed = ExplicitResolutionRequest {
        context_skill_references: vec![shadowed_id],
        ..Default::default()
    };
    let res_shadowed =
        resolve_explicit_requirements(&req_shadowed, &roster).expect("must return result");
    match res_shadowed {
        ExplicitResolutionResult::Unavailable { unresolved } => {
            assert_eq!(unresolved.len(), 1);
            assert_eq!(unresolved[0].reason, UnresolvedReason::Shadowed);
        }
        other => panic!("expected Unavailable for shadowed, got {other:?}"),
    }

    // -------------------------------------------------------------------------
    // Sub-case 7: Unverified Skill Fails All-Actionable Output
    // -------------------------------------------------------------------------
    let unverified_id = SkillId::from_source(
        &SourceId::new("source-main").unwrap(),
        &LogicalSkillKey::new("skills/unverified_tool.md").unwrap(),
    );
    let req_unverified = ExplicitResolutionRequest {
        context_skill_references: vec![unverified_id],
        ..Default::default()
    };
    let res_unverified =
        resolve_explicit_requirements(&req_unverified, &roster).expect("must return result");
    match res_unverified {
        ExplicitResolutionResult::Unavailable { unresolved } => {
            assert_eq!(unresolved.len(), 1);
            assert_eq!(unresolved[0].reason, UnresolvedReason::Unverified);
        }
        other => panic!("expected Unavailable for unverified, got {other:?}"),
    }

    // -------------------------------------------------------------------------
    // Sub-case 8: Forbidden Skill Fails All-Actionable Output
    // -------------------------------------------------------------------------
    let req_forbidden = ExplicitResolutionRequest {
        user_prompt: Some("use skill forbidden-tool".into()),
        ..Default::default()
    };
    let res_forbidden =
        resolve_explicit_requirements(&req_forbidden, &roster).expect("must return result");
    match res_forbidden {
        ExplicitResolutionResult::Unavailable { unresolved } => {
            assert_eq!(unresolved.len(), 1);
            assert_eq!(unresolved[0].target, "forbidden-tool");
            assert_eq!(unresolved[0].reason, UnresolvedReason::Forbidden);
        }
        other => panic!("expected Unavailable for forbidden, got {other:?}"),
    }

    // -------------------------------------------------------------------------
    // Sub-case 9: Conflicting Directives Fail As ConflictingDirective
    // -------------------------------------------------------------------------
    // Exact name conflict in prompt
    let req_conflict_prompt = ExplicitResolutionRequest {
        user_prompt: Some("use skill skill-01; do not use skill skill-01".into()),
        ..Default::default()
    };
    let res_conflict =
        resolve_explicit_requirements(&req_conflict_prompt, &roster).expect("must return result");
    match res_conflict {
        ExplicitResolutionResult::Unavailable { unresolved } => {
            assert_eq!(unresolved.len(), 1);
            assert_eq!(unresolved[0].target, "skill-01");
            assert_eq!(unresolved[0].reason, UnresolvedReason::ConflictingDirective);
        }
        other => panic!("expected Unavailable for conflict, got {other:?}"),
    }

    // Cross-resolution conflict (name required, ID excluded)
    let skill02_id = SkillId::from_source(
        &SourceId::new("source-main").unwrap(),
        &LogicalSkillKey::new("skills/skill_02.md").unwrap(),
    );
    let req_conflict_cross = ExplicitResolutionRequest {
        cli_required_skills: vec!["skill-02".into()],
        context_excluded_skills: vec![skill02_id.clone()],
        ..Default::default()
    };
    let res_conflict_cross =
        resolve_explicit_requirements(&req_conflict_cross, &roster).expect("must return result");
    match res_conflict_cross {
        ExplicitResolutionResult::Unavailable { unresolved } => {
            assert_eq!(unresolved.len(), 1);
            assert_eq!(unresolved[0].target, "skill-02");
            assert_eq!(unresolved[0].reason, UnresolvedReason::ConflictingDirective);
        }
        other => panic!("expected Unavailable for cross conflict, got {other:?}"),
    }

    // Both directions of name/ID resolution, including a different invocation
    // binding for the same physical file, must honor the exclusion.
    let alias_id = SkillId::from_source(
        &SourceId::new("source-alias").unwrap(),
        &LogicalSkillKey::new("skills/skill_02.md").unwrap(),
    );
    for (required, excluded) in [
        (skill02_id.as_str(), "skill-02"),
        (skill02_id.as_str(), "second-skill"),
        ("second-skill", skill02_id.as_str()),
        ("skill-02", alias_id.as_str()),
    ] {
        let request = ExplicitResolutionRequest {
            cli_required_skills: vec![required.into()],
            cli_excluded_skills: vec![excluded.into()],
            ..Default::default()
        };
        let result = resolve_explicit_requirements(&request, &roster).unwrap();
        let ExplicitResolutionResult::Unavailable { unresolved } = result else {
            panic!("exclusion {excluded} did not veto {required}: {result:?}");
        };
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].reason, UnresolvedReason::ConflictingDirective);
    }
    // Exclusions alone return canonical, sorted, deduplicated identities, and
    // leave a genuinely different requested skill usable.
    let expected = {
        let mut ids = vec![skill02_id.clone(), alias_id.clone()];
        ids.sort();
        ids
    };
    for names in [
        vec!["skill-02", "second-skill", skill02_id.as_str()],
        vec![skill02_id.as_str(), "second-skill", "skill-02"],
    ] {
        let mut request = ExplicitResolutionRequest {
            cli_excluded_skills: names.into_iter().map(str::to_owned).collect(),
            ..Default::default()
        };
        assert_eq!(
            resolve_explicit_requirements(&request, &roster).unwrap(),
            ExplicitResolutionResult::NoneSpecified {
                excluded_skills: expected.clone()
            }
        );
        request.cli_required_skills.push("skill-03".into());
        let ExplicitResolutionResult::Resolved {
            skills,
            excluded_skills,
        } = resolve_explicit_requirements(&request, &roster).unwrap()
        else {
            panic!("unrelated exclusion blocked a valid requirement");
        };
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].invocation.as_str(), "skill-03");
        assert_eq!(excluded_skills, expected);
    }
    // A negative name restricts every collision rather than borrowing the
    // positive resolver's requirement for verified, unambiguous invocation.
    let request = ExplicitResolutionRequest {
        cli_excluded_skills: vec![
            "ambig-tool".into(),
            "unverified-tool".into(),
            "absent-id".into(),
        ],
        ..Default::default()
    };
    let mut expected: Vec<_> = roster
        .skills()
        .iter()
        .flat_map(|skill| skill.bindings())
        .filter(|binding| {
            matches!(
                binding.invocation.as_str(),
                "ambig-tool" | "unverified-tool"
            )
        })
        .map(|binding| binding.id.clone())
        .collect();
    assert_eq!(expected.len(), 3);
    expected.push(SkillId::new("absent-id").unwrap());
    expected.sort();
    assert_eq!(
        resolve_explicit_requirements(&request, &roster).unwrap(),
        ExplicitResolutionResult::NoneSpecified {
            excluded_skills: expected
        }
    );

    // -------------------------------------------------------------------------
    // Sub-case 10: 32 References Succeed; 33 References Reject (Hard Limit)
    // -------------------------------------------------------------------------
    // 32 references -> Ok(Resolved)
    let req_32 = ExplicitResolutionRequest {
        cli_required_skills: (1..=32).map(|i| format!("skill-{i:02}")).collect(),
        ..Default::default()
    };
    let res_32 = resolve_explicit_requirements(&req_32, &roster).expect("32 must succeed");
    match res_32 {
        ExplicitResolutionResult::Resolved { skills, .. } => {
            assert_eq!(skills.len(), 32);
        }
        other => panic!("expected Resolved for 32, got {other:?}"),
    }

    // 33 references -> Err(TooManyExplicitReferences)
    let req_33 = ExplicitResolutionRequest {
        cli_required_skills: (1..=33).map(|i| format!("skill-{i:02}")).collect(),
        ..Default::default()
    };
    let err_33 = resolve_explicit_requirements(&req_33, &roster).unwrap_err();
    assert_eq!(
        err_33,
        ExplicitResolutionError::TooManyExplicitReferences {
            count: 33,
            limit: MAX_EXPLICIT_REQUESTS.max(),
        }
    );

    // -------------------------------------------------------------------------
    // Sub-case 11: Prompt Input Bound Enforcement
    // -------------------------------------------------------------------------
    let huge_prompt = "a".repeat(HOOK_STDIN_BYTES.max() + 1);
    let req_huge = ExplicitResolutionRequest {
        user_prompt: Some(huge_prompt),
        ..Default::default()
    };
    let err_huge = resolve_explicit_requirements(&req_huge, &roster).unwrap_err();
    assert!(
        matches!(err_huge, ExplicitResolutionError::OversizedInput { .. }),
        "expected OversizedInput, got {err_huge:?}"
    );

    // -------------------------------------------------------------------------
    // Sub-case 12: NoneSpecified (No Directives or Exclusions Only)
    // -------------------------------------------------------------------------
    let req_empty = ExplicitResolutionRequest {
        user_prompt: Some("hello world, what are our top skills?".into()),
        ..Default::default()
    };
    let res_empty = resolve_explicit_requirements(&req_empty, &roster).expect("must succeed");
    match res_empty {
        ExplicitResolutionResult::NoneSpecified { excluded_skills } => {
            assert!(excluded_skills.is_empty());
        }
        other => panic!("expected NoneSpecified, got {other:?}"),
    }

    let req_excl_only = ExplicitResolutionRequest {
        user_prompt: Some("do not use skill skill-01".into()),
        ..Default::default()
    };
    let res_excl_only =
        resolve_explicit_requirements(&req_excl_only, &roster).expect("must succeed");
    match res_excl_only {
        ExplicitResolutionResult::NoneSpecified { excluded_skills } => {
            assert_eq!(excluded_skills.len(), 1);
        }
        other => panic!("expected NoneSpecified with exclusions, got {other:?}"),
    }

    assert!(runtime.shutdown());
}
