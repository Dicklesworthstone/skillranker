use skillranker::identity::{HarnessId, SourceId};
use skillranker::limits::{DISCOVERY_FILES, DISCOVERY_PARSED_BYTES, LimitUnit, MIB, ResourceLimit};
use skillranker::roster::Visibility;
use skillranker::roster::discovery::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// Trees are retained for inspection, like the other suites here.
fn temp_tree(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let unique = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "sr-discovery-{name}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create tree");
    path
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().expect("has parent")).expect("create parent");
    fs::write(path, content).expect("write fixture");
}

fn verified() -> Visibility {
    Visibility::Verified {
        contract_version: "claude-code-v1".to_owned(),
    }
}

fn source(id: &str) -> SourceId {
    SourceId::new(id).expect("valid source id")
}

fn spec(id: &str, kind: SourceKind, priority: i32) -> RootSpec {
    RootSpec::new(source(id), kind, priority, verified(), CLAUDE_SKILL_FILE).expect("valid spec")
}

fn plan_with(roots: Vec<(RootSpec, PathBuf)>) -> DiscoveryPlan {
    let mut plan = DiscoveryPlan::new(HarnessId::new("claude_code").expect("harness"));
    for (spec, path) in roots {
        match PlannedRoot::open(spec.clone(), &path).expect("open root") {
            Some(root) => plan.push_root(root),
            None => plan.note_missing(spec.source().clone()),
        }
    }
    plan
}

fn relatives(discovery: &Discovery) -> Vec<String> {
    discovery
        .candidates()
        .iter()
        .map(|candidate| candidate.relative().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn documented_claude_roots_are_enumerated_without_unrelated_harness_roots() {
    let tree = temp_tree("claude");
    let home = temp_tree("claude-home");
    write(&tree.join(".claude/skills/alpha/SKILL.md"), "alpha");
    write(&tree.join(".claude/skills/nested/beta/SKILL.md"), "beta");
    write(&home.join(".claude/skills/user-one/SKILL.md"), "user");
    // Roots belonging to other harnesses, or to no documented contract at all.
    write(&tree.join(".codex/skills/codex-only/SKILL.md"), "codex");
    write(&tree.join("skills/loose/SKILL.md"), "loose");
    write(&tree.join("mnt/skills/bundled/SKILL.md"), "bundled");
    // An ancestor holding its own project root must not be walked into.
    let ancestor = tree.join("ancestor");
    write(
        &ancestor.join(".claude/skills/ancestor-skill/SKILL.md"),
        "no",
    );
    let workspace = ancestor.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    let plan = claude_code_plan(&tree, Some(&home), verified()).expect("plan");
    let discovery = plan.discover();
    let found = relatives(&discovery);
    assert_eq!(
        found,
        vec![
            "user-one/SKILL.md".to_owned(),
            "alpha/SKILL.md".to_owned(),
            "nested/beta/SKILL.md".to_owned()
        ],
        "only documented Claude roots, personal before project"
    );
    assert!(
        discovery
            .candidates()
            .iter()
            .all(|candidate| !candidate.path().as_path().starts_with(tree.join(".codex"))),
        "no Codex roots"
    );
    // Personal candidates outrank project candidates.
    let sources: Vec<&str> = discovery
        .candidates()
        .iter()
        .map(|candidate| candidate.source().as_str())
        .collect();
    assert_eq!(
        sources,
        vec![
            CLAUDE_USER_SOURCE,
            CLAUDE_PROJECT_SOURCE,
            CLAUDE_PROJECT_SOURCE
        ]
    );
    assert!(discovery.candidates()[0].priority() > discovery.candidates()[2].priority());

    // A workspace whose ancestor has its own root discovers nothing of it.
    let nested_plan = claude_code_plan(&workspace, None, verified()).expect("plan");
    let nested = nested_plan.discover();
    assert!(
        nested.candidates().is_empty(),
        "ancestor traversal must stop"
    );
}

#[test]
fn unenumerated_sources_keep_the_roster_partial() {
    let tree = temp_tree("partial");
    write(&tree.join(".claude/skills/alpha/SKILL.md"), "alpha");
    let plan = claude_code_plan(&tree, None, verified()).expect("plan");

    assert_eq!(plan.unenumerated().len(), CLAUDE_UNENUMERATED_SOURCES.len());
    let discovery = plan.discover();
    assert_eq!(relatives(&discovery), vec!["alpha/SKILL.md".to_owned()]);
    assert!(
        discovery.is_partial(),
        "plugin and managed sources are not enumerated by this build"
    );
    for id in CLAUDE_UNENUMERATED_SOURCES {
        assert!(
            discovery
                .diagnostics()
                .contains(&Diagnostic::SourceNotEnumerated(source(id))),
            "missing disclosure for {id}"
        );
    }
}

#[test]
fn missing_roots_are_normal_and_unreadable_roots_are_partial() {
    let tree = temp_tree("roots");
    write(&tree.join("present/alpha/SKILL.md"), "alpha");
    // A configured root that is a regular file, not a directory.
    let not_a_directory = tree.join("file-root");
    write(&not_a_directory, "not a directory");

    let only_missing = plan_with(vec![(
        spec("configured.absent", SourceKind::Generic, 10),
        tree.join("does-not-exist"),
    )]);
    let discovery = only_missing.discover();
    assert!(discovery.candidates().is_empty());
    assert!(
        !discovery.is_partial(),
        "an absent optional root is normal, not partial"
    );
    assert_eq!(
        discovery.diagnostics(),
        [Diagnostic::RootMissing(source("configured.absent"))]
    );

    let mixed = plan_with(vec![
        (
            spec("configured.present", SourceKind::Generic, 20),
            tree.join("present"),
        ),
        (
            spec("configured.broken", SourceKind::Generic, 10),
            not_a_directory,
        ),
    ]);
    let discovery = mixed.discover();
    assert_eq!(relatives(&discovery), vec!["alpha/SKILL.md".to_owned()]);
    assert!(
        discovery.is_partial(),
        "unreadable root makes the pass partial"
    );
    assert!(
        discovery
            .diagnostics()
            .contains(&Diagnostic::RootUnreadable(source("configured.broken")))
    );
}

#[test]
fn enumeration_stops_at_declared_ceilings() {
    let tree = temp_tree("ceilings");
    for index in 0..12 {
        write(&tree.join(format!("skill-{index}/SKILL.md")), "body");
    }
    let plan = plan_with(vec![(
        spec("configured.ceiling", SourceKind::Generic, 10),
        tree.clone(),
    )]);

    // Defaults are the documented ceilings.
    let defaults = DiscoveryLimits::defaults();
    assert_eq!(defaults.entries(), DISCOVERY_FILES);
    assert_eq!(defaults.bytes(), DISCOVERY_PARSED_BYTES);
    assert_eq!(defaults.entries().max(), 10_000);
    assert_eq!(defaults.bytes().max(), 32 * MIB);
    assert_eq!(defaults.depth(), MAX_ROOT_DEPTH);

    // Honest counterpart: generous bounds enumerate everything.
    let complete = plan.discover_with(DiscoveryLimits::new(
        ResourceLimit::try_new("entries", LimitUnit::Records, 1_000).expect("limit"),
        ResourceLimit::try_new("bytes", LimitUnit::Bytes, 1_000_000).expect("limit"),
        MAX_ROOT_DEPTH,
    ));
    assert_eq!(complete.candidates().len(), 12);
    assert!(!complete.is_partial());

    // A tight entry ceiling stops the pass and reports it.
    let truncated = plan.discover_with(DiscoveryLimits::new(
        ResourceLimit::try_new("entries", LimitUnit::Records, 5).expect("limit"),
        ResourceLimit::try_new("bytes", LimitUnit::Bytes, 1_000_000).expect("limit"),
        MAX_ROOT_DEPTH,
    ));
    assert!(truncated.candidates().len() < 12);
    assert!(truncated.is_partial());
    assert!(
        truncated
            .diagnostics()
            .contains(&Diagnostic::EntryLimitReached)
    );
    assert!(truncated.entries_examined() <= 6);

    // A tight byte ceiling stops it too.
    let byte_bound = plan.discover_with(DiscoveryLimits::new(
        ResourceLimit::try_new("entries", LimitUnit::Records, 1_000).expect("limit"),
        ResourceLimit::try_new("bytes", LimitUnit::Bytes, 6).expect("limit"),
        MAX_ROOT_DEPTH,
    ));
    assert!(byte_bound.is_partial());
    assert!(
        byte_bound
            .diagnostics()
            .contains(&Diagnostic::ByteLimitReached)
    );
    assert!(byte_bound.bytes_examined() <= 6);
}

#[test]
fn deep_trees_stop_at_the_depth_bound() {
    let tree = temp_tree("depth");
    let mut deep = tree.clone();
    for level in 0..4 {
        deep = deep.join(format!("level-{level}"));
    }
    write(&deep.join("SKILL.md"), "deep");
    write(&tree.join("shallow/SKILL.md"), "shallow");
    let plan = plan_with(vec![(
        spec("configured.depth", SourceKind::Generic, 10),
        tree.clone(),
    )]);

    let generous = plan.discover_with(DiscoveryLimits::new(
        DISCOVERY_FILES,
        DISCOVERY_PARSED_BYTES,
        8,
    ));
    assert_eq!(generous.candidates().len(), 2, "both depths found");

    let shallow = plan.discover_with(DiscoveryLimits::new(
        DISCOVERY_FILES,
        DISCOVERY_PARSED_BYTES,
        2,
    ));
    assert_eq!(relatives(&shallow), vec!["shallow/SKILL.md".to_owned()]);
    assert!(shallow.is_partial());
    assert!(
        shallow
            .diagnostics()
            .contains(&Diagnostic::DepthLimitReached(source("configured.depth")))
    );
}

#[test]
fn symlinked_directories_are_skipped_and_symlinked_skill_files_are_flagged() {
    let tree = temp_tree("links");
    let outside = temp_tree("links-outside");
    write(&tree.join("real/SKILL.md"), "real");
    write(&outside.join("target/SKILL.md"), "outside");
    std::os::unix::fs::symlink(outside.join("target"), tree.join("linked-dir")).expect("dir link");
    fs::create_dir_all(tree.join("linked")).expect("link parent");
    std::os::unix::fs::symlink(tree.join("real/SKILL.md"), tree.join("linked/SKILL.md"))
        .expect("file link");

    let plan = plan_with(vec![(
        spec("configured.links", SourceKind::Generic, 10),
        tree.clone(),
    )]);
    let discovery = plan.discover();

    let found = relatives(&discovery);
    assert!(found.contains(&"real/SKILL.md".to_owned()));
    assert!(found.contains(&"linked/SKILL.md".to_owned()), "{found:?}");
    assert!(
        !found.iter().any(|path| path.starts_with("linked-dir")),
        "symlinked directories are not descended: {found:?}"
    );
    assert!(discovery.is_partial());
    assert!(
        discovery
            .diagnostics()
            .contains(&Diagnostic::SymlinkedDirectorySkipped(source(
                "configured.links"
            )))
    );
    let linked = discovery
        .candidates()
        .iter()
        .find(|candidate| candidate.relative() == Path::new("linked/SKILL.md"))
        .expect("linked candidate");
    assert!(linked.via_symlink(), "link is disclosed to the reader");
    let direct = discovery
        .candidates()
        .iter()
        .find(|candidate| candidate.relative() == Path::new("real/SKILL.md"))
        .expect("direct candidate");
    assert!(!direct.via_symlink());
    assert_eq!(direct.identity(), linked.identity(), "same file, one inode");
}

#[test]
fn only_the_declared_skill_file_name_is_a_candidate() {
    let tree = temp_tree("names");
    write(&tree.join("alpha/SKILL.md"), "alpha");
    write(&tree.join("alpha/README.md"), "readme");
    write(&tree.join("alpha/skill.md"), "lowercase");
    write(&tree.join("beta/AGENT.md"), "other harness file");
    let plan = plan_with(vec![(
        spec("configured.names", SourceKind::Generic, 10),
        tree.clone(),
    )]);
    assert_eq!(
        relatives(&plan.discover()),
        vec!["alpha/SKILL.md".to_owned()]
    );

    // A different declared name finds a different set, with no overlap.
    let other = RootSpec::new(
        source("configured.other"),
        SourceKind::Generic,
        10,
        Visibility::Unverified,
        "AGENT.md",
    )
    .expect("spec");
    let other_plan = plan_with(vec![(other, tree.clone())]);
    assert_eq!(
        relatives(&other_plan.discover()),
        vec!["beta/AGENT.md".to_owned()]
    );

    for invalid in ["", "nested/SKILL.md", ".", "..", "with\0nul"] {
        assert_eq!(
            RootSpec::new(
                source("configured.invalid"),
                SourceKind::Generic,
                1,
                Visibility::Unverified,
                invalid,
            ),
            Err(DiscoveryError::InvalidSkillFileName),
            "{invalid:?}"
        );
    }
}

#[test]
fn generic_roots_keep_unverified_visibility_and_diagnostics_stay_private() {
    let tree = temp_tree("visibility");
    write(&tree.join("alpha/SKILL.md"), "secret-body-canary");
    let generic = RootSpec::new(
        source("configured.generic"),
        SourceKind::Generic,
        5,
        Visibility::Unverified,
        CLAUDE_SKILL_FILE,
    )
    .expect("spec");
    let plan = plan_with(vec![(generic, tree.clone())]);
    let discovery = plan.discover();

    assert_eq!(
        discovery.candidates()[0].visibility(),
        &Visibility::Unverified,
        "a configured root without a load contract stays unverified"
    );
    assert_eq!(discovery.candidates()[0].kind(), SourceKind::Generic);

    // Diagnostics name sources, never paths or content.
    let missing = plan_with(vec![(
        spec("configured.gone", SourceKind::Generic, 1),
        tree.join("absent"),
    )]);
    for diagnostic in missing.discover().diagnostics() {
        let rendered = format!("{diagnostic} {diagnostic:?}");
        assert!(!rendered.contains("secret-body-canary"), "{rendered}");
        assert!(
            !rendered.contains(tree.to_str().expect("utf-8 temp path")),
            "{rendered}"
        );
    }
}
