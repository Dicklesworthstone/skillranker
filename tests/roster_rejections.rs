//! Metadata rejections must not consume useful candidates' read allowance or
//! let a broken higher-priority slot promote the lower-priority definition.

use skillranker::identity::{HarnessId, SourceId};
use skillranker::limits::{
    DISCOVERY_FILES, DISCOVERY_PARSED_BYTES, DurationMillis, LimitUnit, ResourceLimit,
    SKILL_FILE_BYTES,
};
use skillranker::roster::discovery::{
    CandidateProblem, Diagnostic, DiscoveryLimits, DiscoveryPlan, PlannedRoot, RootSpec,
    SourceKind, claude_code_plan,
};
use skillranker::roster::resolution::{
    ExactResolution, OptionMap, ResolutionError, ResolvedRoster, resolve_claude_plan,
};
use skillranker::roster::revalidation::{self, RevalidationError};
use skillranker::roster::{InvocationKind, Visibility};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn tree() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-rejected-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path // Retained for inspection, as in the other roster suites.
}

fn skill(root: &Path, name: &str, text: &str) -> PathBuf {
    let path = root.join(".claude/skills").join(name).join("SKILL.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    path
}

fn sparse(path: &Path, bytes: usize) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(bytes as u64)
        .unwrap();
}

fn verified() -> Visibility {
    Visibility::Verified {
        contract_version: "rejected-candidate-test-v1".into(),
    }
}

fn runtime() -> (EntryClock, ProcessInvocation, asupersync::Cx) {
    let clock = EntryClock::capture_with(
        DurationMillis::new("total", 30_000, 30_000).unwrap(),
        DurationMillis::new("cleanup", 200, 30_000).unwrap(),
    )
    .unwrap();
    let runtime = ProcessInvocation::from_clock(clock).unwrap();
    let cx = runtime.request_cx().unwrap();
    (clock, runtime, cx)
}

fn resolve(plan: &DiscoveryPlan) -> ResolvedRoster {
    let (clock, runtime, cx) = runtime();
    let roster = resolve_claude_plan(plan, &BTreeMap::new(), &cx, &clock).unwrap();
    assert!(runtime.shutdown());
    roster
}

fn assert_only_helpful(roster: &ResolvedRoster) {
    let names: Vec<_> = roster
        .advisory()
        .map(|s| s.binding.invocation.as_str())
        .collect();
    assert_eq!(names, ["helpful"]);
    let id = &roster.advisory().next().unwrap().binding.id;
    assert_eq!(
        OptionMap::new(roster, std::slice::from_ref(id))
            .unwrap()
            .entries()
            .len(),
        1
    );
}

fn assert_blocked(roster: &ResolvedRoster) {
    assert_eq!(roster.exact_name("blocked"), ExactResolution::Unverified);
    let bindings: Vec<_> = roster
        .skills()
        .iter()
        .flat_map(|s| s.bindings())
        .filter(|b| b.invocation.as_str() == "blocked")
        .collect();
    assert!(
        !bindings.is_empty(),
        "the readable lower-priority binding remains inspectable"
    );
    for binding in bindings {
        assert_eq!(roster.exact_id(&binding.id), ExactResolution::Unverified);
        assert_eq!(
            OptionMap::new(roster, std::slice::from_ref(&binding.id)).unwrap_err(),
            ResolutionError::IneligibleOption
        );
    }
}

#[test]
fn oversized_first_root_cannot_starve_later_healthy_skills() {
    for size in [SKILL_FILE_BYTES.max() + 1, DISCOVERY_PARSED_BYTES.max() + 1] {
        let workspace = tree();
        let home = tree();
        let oversized = skill(&workspace, "private-size-canary", "");
        sparse(&oversized, size);
        let text = "# Helpful\n\nUseful procedure";
        skill(&home, "helpful", text);
        // Each root has one child: breadth-first enumeration must encounter
        // the project rejection before the personal SKILL.md, in any readdir order.
        let plan = claude_code_plan(&workspace, Some(&home), verified()).unwrap();
        let discovery = plan.discover();
        assert_eq!(discovery.candidates().len(), 1);
        assert_eq!(discovery.bytes_examined(), text.len() as u64);
        assert_eq!(discovery.rejected_candidates().len(), 1);
        assert_eq!(
            discovery.rejected_candidates()[0].problem(),
            CandidateProblem::Oversized
        );
        assert!(discovery.is_partial());
        assert!(
            !discovery
                .diagnostics()
                .contains(&Diagnostic::ByteLimitReached)
        );
        assert!(!format!("{:?}", discovery.rejected_candidates()).contains("private-size-canary"));
        let roster = resolve(&plan);
        assert_only_helpful(&roster);
        assert_eq!(roster.diagnostics().len(), 1);
        assert_eq!(roster.diagnostics()[0].1, ResolutionError::Oversized);
        assert_eq!(
            roster.exact_name("private-size-canary"),
            ExactResolution::Missing
        );
    }
}

#[test]
fn many_oversized_files_do_not_accumulate_a_fictitious_read_budget() {
    let workspace = tree();
    let home = tree();
    for index in 0..129 {
        let path = skill(&workspace, &format!("oversized-{index:03}"), "");
        sparse(&path, SKILL_FILE_BYTES.max() + 1);
    }
    skill(&home, "helpful", "# Helpful\n\nbody");
    let plan = claude_code_plan(&workspace, Some(&home), verified()).unwrap();
    let discovery = plan.discover();
    assert_eq!(discovery.rejected_candidates().len(), 129);
    assert_eq!(discovery.candidates().len(), 1);
    assert!(discovery.bytes_examined() < SKILL_FILE_BYTES.max() as u64);
    let roster = resolve(&plan);
    assert_only_helpful(&roster);
    assert_eq!(roster.diagnostics().len(), 129);
    assert!(
        roster
            .diagnostics()
            .iter()
            .all(|(_, e)| *e == ResolutionError::Oversized)
    );
}

#[test]
fn nonregular_personal_slots_cannot_promote_the_project_definition() {
    for kind in ["fifo", "directory", "linked-fifo", "linked-directory"] {
        let workspace = tree();
        let home = tree();
        let outside = tree();
        skill(&workspace, "helpful", "# Helpful\n\nbody");
        skill(&workspace, "blocked", "# Project\n\nMust not be promoted");
        let path = home.join(".claude/skills/blocked/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        match kind {
            "fifo" => nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap(),
            "directory" => fs::create_dir(&path).unwrap(),
            "linked-fifo" => {
                let target = outside.join("fifo");
                nix::unistd::mkfifo(&target, nix::sys::stat::Mode::S_IRWXU).unwrap();
                symlink(target, &path).unwrap();
            }
            "linked-directory" => symlink(&outside, &path).unwrap(),
            _ => unreachable!(),
        }
        let plan = claude_code_plan(&workspace, Some(&home), verified()).unwrap();
        let discovery = plan.discover();
        assert_eq!(discovery.rejected_candidates().len(), 1, "{kind}");
        assert_eq!(
            discovery.rejected_candidates()[0].problem(),
            CandidateProblem::NotRegularFile
        );
        let roster = resolve(&plan);
        assert_only_helpful(&roster);
        assert_blocked(&roster);
        assert_eq!(roster.diagnostics().len(), 1);
        assert_eq!(roster.diagnostics()[0].1, ResolutionError::Read);
    }
}

#[test]
fn rejected_slots_share_the_pass_wide_entry_limit() {
    let base = tree();
    let mut plan = DiscoveryPlan::new(HarnessId::new("claude_code").unwrap());
    for index in 0..3 {
        let root = base.join(format!("root-{index}"));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("SKILL.md"), "").unwrap();
        sparse(&root.join("SKILL.md"), SKILL_FILE_BYTES.max() + 1);
        let spec = RootSpec::new(
            SourceId::new(format!("root-{index}")).unwrap(),
            SourceKind::Generic,
            0,
            verified(),
            "SKILL.md",
        )
        .unwrap();
        plan.push_root(PlannedRoot::open(spec, &root).unwrap().unwrap());
    }
    let limited = plan.discover_with(DiscoveryLimits::new(
        ResourceLimit::try_new("entries", LimitUnit::Records, 2).unwrap(),
        DISCOVERY_PARSED_BYTES,
        8,
    ));
    assert_eq!(limited.entries_examined(), 3); // The one global overflow sentinel.
    assert_eq!(limited.rejected_candidates().len(), 2);
    assert!(limited.candidates().is_empty());
    assert_eq!(limited.bytes_examined(), 0);
    assert_eq!(limited.diagnostics(), [Diagnostic::EntryLimitReached]);
}

#[test]
fn the_real_cumulative_byte_limit_still_bounds_readable_candidates() {
    let workspace = tree();
    let count = DISCOVERY_PARSED_BYTES.max() / SKILL_FILE_BYTES.max();
    for index in 0..=count {
        let path = skill(&workspace, &format!("bounded-{index:03}"), "");
        sparse(&path, SKILL_FILE_BYTES.max());
    }
    let plan = claude_code_plan(&workspace, None, verified()).unwrap();
    let discovery = plan.discover();
    assert_eq!(discovery.candidates().len(), count);
    assert_eq!(
        discovery.bytes_examined(),
        DISCOVERY_PARSED_BYTES.max() as u64
    );
    assert!(discovery.rejected_candidates().is_empty());
    assert!(discovery.entries_examined() <= DISCOVERY_FILES.max());
    assert!(
        discovery
            .diagnostics()
            .contains(&Diagnostic::ByteLimitReached)
    );
}

#[test]
fn unsupported_rejected_layouts_do_not_claim_callable_names() {
    let workspace = tree();
    let home = tree();
    skill(&workspace, "helpful", "# Helpful\n\nbody");
    let nested = skill(&home, "examples/helpful", "");
    sparse(&nested, DISCOVERY_PARSED_BYTES.max() + 1);
    let plan = claude_code_plan(&workspace, Some(&home), verified()).unwrap();
    let roster = resolve(&plan);
    assert_only_helpful(&roster);
    assert_eq!(roster.diagnostics().len(), 1);
    assert_eq!(
        roster.diagnostics()[0].1,
        ResolutionError::UnsupportedLayout
    );
}

#[test]
fn moving_a_rejection_to_a_competing_name_invalidates_publication() {
    let workspace = tree();
    let home = tree();
    skill(&workspace, "helpful", "# Helpful\n\nbody");
    let oversized = skill(&home, "unrelated", "");
    sparse(&oversized, DISCOVERY_PARSED_BYTES.max() + 1);
    let plan = claude_code_plan(&workspace, Some(&home), verified()).unwrap();
    let (clock, runtime, cx) = runtime();
    let initial = resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap();
    assert_only_helpful(&initial);
    let ids: BTreeSet<_> = initial.advisory().map(|s| s.binding.id.clone()).collect();
    let captured = revalidation::capture(&initial, &ids, &clock).unwrap();
    assert!(revalidation::revalidate(&captured, &initial, &clock).is_ok());
    fs::rename(
        home.join(".claude/skills/unrelated"),
        home.join(".claude/skills/helpful"),
    )
    .unwrap();
    let fresh_plan = claude_code_plan(&workspace, Some(&home), verified()).unwrap();
    let fresh = resolve_claude_plan(&fresh_plan, &BTreeMap::new(), &cx, &clock).unwrap();
    assert_eq!(initial.source_diagnostics(), fresh.source_diagnostics());
    assert_eq!(initial.diagnostics(), fresh.diagnostics());
    assert_eq!(fresh.exact_name("helpful"), ExactResolution::Unverified);
    assert_eq!(fresh.advisory().count(), 0);
    assert_eq!(
        revalidation::revalidate(&captured, &fresh, &clock).unwrap_err(),
        RevalidationError::Changed
    );
    assert!(runtime.shutdown());
}

#[test]
fn repairing_a_rejected_override_requires_fresh_resolution_and_keeps_restrictions() {
    let workspace = tree();
    let home = tree();
    skill(&workspace, "helpful", "# Helpful\n\nbody");
    skill(&workspace, "blocked", "# Project\n\nbody");
    let oversized = skill(&home, "blocked", "");
    sparse(&oversized, DISCOVERY_PARSED_BYTES.max() + 1);
    let plan = claude_code_plan(&workspace, Some(&home), verified()).unwrap();
    let initial = resolve(&plan);
    assert_only_helpful(&initial);
    assert_blocked(&initial);
    fs::write(
        &oversized,
        "---\ndisable-model-invocation: true\n---\nManual procedure",
    )
    .unwrap();
    let repaired = resolve(&plan);
    assert_only_helpful(&repaired);
    assert!(matches!(
        repaired.exact_name("blocked"),
        ExactResolution::Resolved {
            kind: InvocationKind::ManualOnly,
            ..
        }
    ));
    assert!(repaired.diagnostics().is_empty());
    assert_blocked(&initial); // No mutation can retroactively grant authority to the old snapshot.
}
