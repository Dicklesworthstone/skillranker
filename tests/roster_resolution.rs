use skillranker::authorized_read::{AuthorizedRoot, AuthorizedRoots};
use skillranker::identity::{LogicalSkillKey, SourceId};
use skillranker::limits::{DurationMillis, SKILL_FILE_BYTES};
use skillranker::roster::discovery::claude_code_plan;
use skillranker::roster::resolution::*;
use skillranker::roster::{InvocationKind, InvocationName, InvocationRestrictions, Visibility};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const ALLOW: InvocationRestrictions = InvocationRestrictions {
    agent_invocable: true,
    user_invocable: true,
};
fn verified() -> Visibility {
    Visibility::Verified {
        contract_version: "test-adapter-v1".into(),
    }
}
fn tree() -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-resolution-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path // Retained for inspection.
}
fn write(root: &Path, relative: &str, bytes: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    path
}
fn invocation() -> (EntryClock, ProcessInvocation, asupersync::Cx) {
    let clock = EntryClock::capture_with(
        DurationMillis::new("test_total", 30_000, 30_000).unwrap(),
        DurationMillis::new("test_cleanup", 200, 30_000).unwrap(),
    )
    .unwrap();
    let runtime = ProcessInvocation::from_clock(clock).unwrap();
    let cx = runtime.request_cx().unwrap();
    (clock, runtime, cx)
}
fn entry(
    root: &Path,
    relative: &str,
    name: &str,
    source: &str,
    priority: Option<i32>,
    restrictions: InvocationRestrictions,
) -> SkillEntry {
    let roots = AuthorizedRoots::single(AuthorizedRoot::open_absolute(root).unwrap());
    let read = roots
        .read_bounded(0, Path::new(relative), SKILL_FILE_BYTES)
        .unwrap();
    SkillEntry::from_read(
        BindingSpec {
            source: SourceId::new(source).unwrap(),
            logical_key: LogicalSkillKey::new(relative).unwrap(),
            invocation: InvocationName::new(name).unwrap(),
            priority,
            visibility: verified(),
            restrictions,
        },
        read,
    )
    .unwrap()
}
#[test]
fn actual_claude_roots_resolve_personal_winner_and_directory_invocation() {
    let root = tree();
    let home = tree();
    write(
        &root,
        ".claude/skills/deploy/SKILL.md",
        "---\nname: Same Display\ndescription: project\n---\nbody",
    );
    write(
        &home,
        ".claude/skills/deploy/SKILL.md",
        "---\nname: Same Display\ndescription: personal\ndisable-model-invocation: true\n---\nbody",
    );
    write(
        &root,
        ".claude/skills/inspect/SKILL.md",
        "---\nname: Same Display\ndescription: inspect\nuser-invocable: false\naliases: [forged]\n---\nbody",
    );
    let (clock, runtime, cx) = invocation();
    let plan = claude_code_plan(&root, Some(&home), verified()).unwrap();
    let roster = resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap();
    assert!(roster.is_partial()); // Plugin/managed inventories are not enumerated.
    assert_eq!(roster.skills().len(), 3);
    assert!(matches!(
        roster.exact_name("deploy"),
        ExactResolution::Resolved {
            kind: InvocationKind::ManualOnly,
            ..
        }
    ));
    assert!(matches!(
        roster.exact_name("inspect"),
        ExactResolution::Resolved {
            kind: InvocationKind::Agent,
            ..
        }
    ));
    assert_eq!(roster.exact_name("Same Display"), ExactResolution::Missing);
    assert_eq!(roster.exact_name("forged"), ExactResolution::Missing);
    let advice: Vec<_> = roster.advisory().collect();
    assert_eq!(advice.len(), 1);
    assert_eq!(advice[0].binding.invocation.as_str(), "inspect");
    assert_eq!(advice[0].record.display_name.as_str(), "Same Display");
    assert_eq!(
        roster
            .skills()
            .iter()
            .flat_map(|s| s.bindings())
            .filter(|b| matches!(b.visibility, Visibility::Shadowed { .. }))
            .count(),
        1
    );
    assert!(runtime.shutdown());
}
#[test]
fn collision_requires_comparable_known_unique_precedence() {
    let root = tree();
    write(&root, "a.md", "# Shared\n\na");
    write(&root, "b.md", "# Shared\n\nb");
    let (clock, runtime, cx) = invocation();
    for priorities in [(Some(2), Some(1)), (None, Some(1)), (Some(1), Some(1))] {
        let roster = ResolvedRoster::resolve(
            vec![
                entry(&root, "a.md", "call", "a", priorities.0, ALLOW),
                entry(&root, "b.md", "call", "b", priorities.1, ALLOW),
            ],
            false,
            &cx,
            &clock,
        )
        .unwrap();
        if priorities.0 == Some(2) {
            assert_eq!(roster.advisory().count(), 1);
            assert!(matches!(
                roster.exact_name("call"),
                ExactResolution::Resolved { .. }
            ));
        } else {
            assert_eq!(roster.advisory().count(), 0);
            assert_eq!(roster.exact_name("call"), ExactResolution::Ambiguous);
        }
    }
    let roster = ResolvedRoster::resolve(
        vec![
            entry(&root, "a.md", "one", "a", None, ALLOW),
            entry(&root, "b.md", "two", "b", None, ALLOW),
        ],
        false,
        &cx,
        &clock,
    )
    .unwrap();
    assert_eq!(roster.advisory().count(), 2); // Display collision does not matter.
    assert!(runtime.shutdown());
}
#[test]
fn canonical_aliases_preserve_restrictions_and_cannot_bypass_option_admission() {
    let root = tree();
    write(&root, "a.md", "# Display\n\nbody");
    fs::hard_link(root.join("a.md"), root.join("b.md")).unwrap();
    let (clock, runtime, cx) = invocation();
    let manual = InvocationRestrictions {
        agent_invocable: false,
        user_invocable: true,
    };
    let roster = ResolvedRoster::resolve(
        vec![
            entry(&root, "a.md", "allowed", "a", Some(1), ALLOW),
            entry(&root, "b.md", "manual", "b", Some(2), manual),
        ],
        false,
        &cx,
        &clock,
    )
    .unwrap();
    assert_eq!(roster.skills().len(), 1);
    assert_eq!(roster.skills()[0].record().aliases.len(), 1);
    let bindings = roster.skills()[0].bindings();
    let denied = bindings
        .iter()
        .find(|b| b.invocation.as_str() == "manual")
        .unwrap();
    assert_eq!(
        OptionMap::new(&roster, std::slice::from_ref(&denied.id)).unwrap_err(),
        ResolutionError::IneligibleOption
    );
    assert!(matches!(
        roster.exact_id(&denied.id),
        ExactResolution::Resolved {
            kind: InvocationKind::ManualOnly,
            ..
        }
    ));
    let allowed = roster.advisory().next().unwrap();
    let map = OptionMap::new(&roster, std::slice::from_ref(&allowed.binding.id)).unwrap();
    assert!(matches!(map.resolve("o000"), Ok(ResolvedOption::Skill(_))));
    for invalid in [
        "allowed",
        "a.md",
        "../../outside",
        "o001",
        allowed.binding.id.as_str(),
    ] {
        assert_eq!(
            map.resolve(invalid).unwrap_err(),
            ResolutionError::UnknownOption
        );
    }
    assert!(matches!(map.resolve("__none__"), Ok(ResolvedOption::None)));
    assert_eq!(
        OptionMap::new(
            &roster,
            &[allowed.binding.id.clone(), allowed.binding.id.clone()]
        )
        .unwrap_err(),
        ResolutionError::DuplicateOption
    );
    let same_name = ResolvedRoster::resolve(
        vec![
            entry(&root, "a.md", "same", "a", Some(1), ALLOW),
            entry(&root, "b.md", "same", "b", Some(2), manual),
        ],
        false,
        &cx,
        &clock,
    )
    .unwrap();
    assert_eq!(same_name.advisory().count(), 0);
    assert!(matches!(
        same_name.exact_name("same"),
        ExactResolution::Resolved {
            kind: InvocationKind::ManualOnly,
            ..
        }
    ));
    assert!(runtime.shutdown());
}
#[test]
fn stable_identity_changes_content_hash_and_detects_inconsistent_alias_reads() {
    let root = tree();
    write(&root, "a.md", "# A\n\nold");
    let (clock, runtime, cx) = invocation();
    let before = entry(&root, "a.md", "a", "a", Some(1), ALLOW);
    fs::write(root.join("a.md"), "# A\n\nnew").unwrap();
    let after = entry(&root, "a.md", "a", "a", Some(1), ALLOW);
    let old = ResolvedRoster::resolve(vec![before.clone()], false, &cx, &clock).unwrap();
    let new = ResolvedRoster::resolve(vec![after], false, &cx, &clock).unwrap();
    assert_eq!(old.skills()[0].record().id, new.skills()[0].record().id);
    assert_ne!(
        old.skills()[0].record().source_content,
        new.skills()[0].record().source_content
    );
    let alias = entry(&root, "a.md", "alias", "alias", Some(1), ALLOW);
    assert_eq!(
        ResolvedRoster::resolve(vec![before, alias], false, &cx, &clock).unwrap_err(),
        ResolutionError::ChangedFile
    );
    let duplicate = entry(&root, "a.md", "a", "a", Some(1), ALLOW);
    assert_eq!(
        ResolvedRoster::resolve(vec![duplicate.clone(), duplicate], false, &cx, &clock)
            .unwrap_err(),
        ResolutionError::DuplicateId
    );
    assert!(runtime.shutdown());
}
#[test]
fn option_boundaries_order_and_debug_privacy() {
    let root = tree();
    let (clock, runtime, cx) = invocation();
    let mut entries = Vec::new();
    for i in 0..255 {
        let file = format!("{i}.md");
        write(&root, &file, "# PrivateCanary\n\ncontent");
        entries.push(entry(
            &root,
            &file,
            &format!("private-canary-{i}"),
            "source",
            Some(1),
            ALLOW,
        ));
    }
    let roster = ResolvedRoster::resolve(entries, false, &cx, &clock).unwrap();
    let selected: Vec<_> = roster.advisory().map(|s| s.binding.id.clone()).collect();
    assert!(OptionMap::new(&roster, &[]).unwrap().entries().is_empty());
    assert_eq!(
        OptionMap::new(&roster, &selected[..1])
            .unwrap()
            .entries()
            .len(),
        1
    );
    let map = OptionMap::new(&roster, &selected[..254]).unwrap();
    assert_eq!(map.entries().len(), 254);
    let mut reverse = selected[..254].to_vec();
    reverse.reverse();
    let reversed = OptionMap::new(&roster, &reverse).unwrap();
    assert_eq!(
        map.entries()
            .iter()
            .map(|(o, s)| (o, &s.binding.id))
            .collect::<Vec<_>>(),
        reversed
            .entries()
            .iter()
            .map(|(o, s)| (o, &s.binding.id))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        OptionMap::new(&roster, &selected).unwrap_err(),
        ResolutionError::Limit
    );
    for debug in [format!("{roster:?}"), format!("{map:?}")] {
        assert!(!debug.contains("PrivateCanary"));
        assert!(!debug.contains("private-canary"));
        assert!(!debug.contains(root.to_str().unwrap()));
    }
    runtime.cancel_user(&cx);
    assert_eq!(
        ResolvedRoster::resolve(Vec::new(), false, &cx, &clock).unwrap_err(),
        ResolutionError::Cancelled
    );
    assert!(runtime.shutdown());
}
#[test]
fn trusted_override_cannot_enable_disabled_skill_and_forbidden_winner_has_no_fallback() {
    let root = tree();
    let home = tree();
    write(&root, ".claude/skills/run/SKILL.md", "# Run\n\nallowed");
    write(
        &home,
        ".claude/skills/run/SKILL.md",
        "---\ndisable-model-invocation: true\nuser-invocable: false\n---\nbody",
    );
    let (clock, runtime, cx) = invocation();
    let plan = claude_code_plan(&root, Some(&home), verified()).unwrap();
    let roster =
        resolve_claude_plan(&plan, &BTreeMap::from([("run".into(), ALLOW)]), &cx, &clock).unwrap();
    assert_eq!(roster.exact_name("run"), ExactResolution::Forbidden);
    assert_eq!(roster.advisory().count(), 0);
    assert!(runtime.shutdown());
}

#[test]
fn malformed_winner_and_unsupported_layout_withhold_authority() {
    let root = tree();
    let home = tree();
    write(
        &root,
        ".claude/skills/run/SKILL.md",
        "# Run\n\nvalid project",
    );
    write(
        &home,
        ".claude/skills/run/SKILL.md",
        "---\nname: first\nname: duplicate\n---\nbody",
    );
    let (clock, runtime, cx) = invocation();
    let plan = claude_code_plan(&root, Some(&home), verified()).unwrap();
    let bad = resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap();
    assert_eq!(bad.advisory().count(), 0);
    assert_eq!(bad.exact_name("run"), ExactResolution::Unverified);
    assert!(
        bad.diagnostics()
            .iter()
            .any(|(_, e)| *e == ResolutionError::Metadata)
    );
    fs::write(
        home.join(".claude/skills/run/SKILL.md"),
        "# Fixed\n\nvalid personal",
    )
    .unwrap();
    let good = resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap();
    assert_eq!(good.advisory().count(), 1);
    write(
        &root,
        ".claude/skills/nested/deep/SKILL.md",
        "# Unsupported\n\nbody",
    );
    let unknown = resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap();
    assert!(unknown.is_partial());
    assert_eq!(unknown.advisory().count(), 0);
    assert!(
        unknown
            .diagnostics()
            .iter()
            .any(|(_, e)| *e == ResolutionError::UnsupportedLayout)
    );
    assert!(runtime.shutdown());
}

#[test]
fn same_file_winning_aliases_intersect_restrictions_even_with_a_loser() {
    let root = tree();
    write(&root, "a.md", "# A\n\nbody");
    write(&root, "b.md", "# B\n\nbody");
    let (clock, runtime, cx) = invocation();
    let manual = InvocationRestrictions {
        agent_invocable: false,
        user_invocable: true,
    };
    let roster = ResolvedRoster::resolve(
        vec![
            entry(&root, "a.md", "call", "a", Some(2), ALLOW),
            entry(&root, "a.md", "call", "alias", Some(2), manual),
            entry(&root, "b.md", "call", "b", Some(1), ALLOW),
        ],
        false,
        &cx,
        &clock,
    )
    .unwrap();
    assert_eq!(roster.advisory().count(), 0);
    assert!(matches!(
        roster.exact_name("call"),
        ExactResolution::Resolved {
            kind: InvocationKind::ManualOnly,
            ..
        }
    ));
    assert!(runtime.shutdown());
}

#[test]
fn mixed_contracts_are_ambiguous_and_unverified_singletons_stay_unverified() {
    let root = tree();
    write(&root, "a.md", "# A\n\nbody");
    write(&root, "b.md", "# B\n\nbody");
    let (clock, runtime, cx) = invocation();
    let roots = AuthorizedRoots::single(AuthorizedRoot::open_absolute(&root).unwrap());
    let make = |visibility| {
        SkillEntry::from_read(
            BindingSpec {
                source: SourceId::new("other").unwrap(),
                logical_key: LogicalSkillKey::new("b.md").unwrap(),
                invocation: InvocationName::new("call").unwrap(),
                priority: Some(3),
                visibility,
                restrictions: ALLOW,
            },
            roots
                .read_bounded(0, Path::new("b.md"), SKILL_FILE_BYTES)
                .unwrap(),
        )
        .unwrap()
    };
    for visibility in [
        Visibility::Unverified,
        Visibility::Verified {
            contract_version: "different-contract".into(),
        },
    ] {
        let roster = ResolvedRoster::resolve(
            vec![
                entry(&root, "a.md", "call", "a", Some(2), ALLOW),
                make(visibility),
            ],
            false,
            &cx,
            &clock,
        )
        .unwrap();
        assert_eq!(roster.exact_name("call"), ExactResolution::Ambiguous);
        assert_eq!(roster.advisory().count(), 0);
    }
    let roster =
        ResolvedRoster::resolve(vec![make(Visibility::Unverified)], false, &cx, &clock).unwrap();
    assert_eq!(roster.exact_name("call"), ExactResolution::Unverified);
    assert!(runtime.shutdown());
}

#[test]
fn authorized_file_symlink_deduplicates_but_escape_withholds_authority() {
    use std::os::unix::fs::symlink;
    let root = tree();
    let outside = tree();
    let original = write(
        &root,
        ".claude/skills/original/SKILL.md",
        "# Original\n\nbody",
    );
    fs::create_dir_all(root.join(".claude/skills/alias")).unwrap();
    symlink(&original, root.join(".claude/skills/alias/SKILL.md")).unwrap();
    let (clock, runtime, cx) = invocation();
    let plan = claude_code_plan(&root, None, verified()).unwrap();
    let good = resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap();
    assert_eq!(good.skills().len(), 1);
    assert_eq!(good.skills()[0].bindings().len(), 2);
    assert_eq!(good.advisory().count(), 1);
    let outside_file = write(&outside, "SKILL.md", "# Outside\n\nprivate");
    fs::create_dir_all(root.join(".claude/skills/escape")).unwrap();
    symlink(outside_file, root.join(".claude/skills/escape/SKILL.md")).unwrap();
    let bad = resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap();
    assert!(bad.is_partial());
    assert_eq!(bad.advisory().count(), 0);
    assert!(
        bad.diagnostics()
            .iter()
            .any(|(_, e)| *e == ResolutionError::Read)
    );
    assert!(runtime.shutdown());
}

#[test]
fn expired_deadline_rejects_even_empty_resolution() {
    let clock = EntryClock::capture_with(
        DurationMillis::new("total", 20, 100).unwrap(),
        DurationMillis::new("cleanup", 5, 100).unwrap(),
    )
    .unwrap();
    // Keep the harness cleanup budget independent of the deliberately expired
    // resolver deadline; otherwise shutdown has only 1 ms to drain threads.
    let (_, runtime, cx) = invocation();
    std::thread::sleep(std::time::Duration::from_millis(25));
    assert_eq!(
        ResolvedRoster::resolve(Vec::new(), false, &cx, &clock).unwrap_err(),
        ResolutionError::Deadline
    );
    assert!(runtime.shutdown());
}

#[test]
fn roster_aggregate_limits_are_enforced_before_alias_deduplication() {
    let root = tree();
    write(&root, "a.md", "# A\n\nbody");
    let (clock, runtime, cx) = invocation();
    let item = entry(&root, "a.md", "a", "a", Some(1), ALLOW);
    assert_eq!(
        ResolvedRoster::resolve(vec![item; 10_001], false, &cx, &clock).unwrap_err(),
        ResolutionError::Limit
    );
    fs::write(root.join("a.md"), "x".repeat(256 * 1024)).unwrap();
    let large = entry(&root, "a.md", "a", "a", Some(1), ALLOW);
    assert_eq!(
        ResolvedRoster::resolve(vec![large; 129], false, &cx, &clock).unwrap_err(),
        ResolutionError::Limit
    );
    assert!(runtime.shutdown());
}
