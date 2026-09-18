use skillranker::identity::SkillId;
use skillranker::limits::DurationMillis;
use skillranker::output::ErrorKind;
use skillranker::roster::discovery::claude_code_plan;
use skillranker::roster::resolution::{ExactResolution, ResolvedRoster, resolve_claude_plan};
use skillranker::roster::revalidation::{
    Dependencies, RevalidationError, capture, revalidate_claude,
};
use skillranker::roster::{InvocationRestrictions, Visibility};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn tree() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-revalidation-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path // Retained for inspection.
}

fn visibility() -> Visibility {
    Visibility::Verified {
        contract_version: "test-adapter-v1".into(),
    }
}

fn clock(total_ms: u64) -> (EntryClock, ProcessInvocation, asupersync::Cx) {
    let clock = EntryClock::capture_with(
        DurationMillis::new("test", total_ms, 60_000).unwrap(),
        DurationMillis::new("cleanup", 5, 60_000).unwrap(),
    )
    .unwrap();
    let runtime = ProcessInvocation::from_clock(clock).unwrap();
    let cx = runtime.request_cx().unwrap();
    (clock, runtime, cx)
}

fn write_skill(root: &Path, name: &str, extra: &str, body: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} skill.\n{extra}---\n{body}\n"),
    )
    .unwrap();
}

/// A workspace with project skills a..e and a manual-only skill m, and an
/// empty personal root. Plugin and managed sources keep the roster partial.
struct Workspace {
    workspace: PathBuf,
    home: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let this = Self {
            workspace: tree(),
            home: tree(),
        };
        for name in ["a", "b", "c", "d", "e"] {
            write_skill(&this.project(), name, "", "Body.");
        }
        write_skill(
            &this.project(),
            "m",
            "disable-model-invocation: true\n",
            "Manual.",
        );
        fs::create_dir_all(this.user()).unwrap();
        this
    }
    fn project(&self) -> PathBuf {
        self.workspace.join(".claude/skills")
    }
    fn user(&self) -> PathBuf {
        self.home.join(".claude/skills")
    }
    fn roster(&self) -> ResolvedRoster {
        let (clock, _runtime, cx) = clock(30_000);
        let plan = claude_code_plan(&self.workspace, Some(&self.home), visibility()).unwrap();
        resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap()
    }
    fn revalidate(&self, captured: &Dependencies) -> Result<(), RevalidationError> {
        let (clock, _runtime, cx) = clock(30_000);
        revalidate_claude(
            captured,
            &self.workspace,
            Some(&self.home),
            visibility(),
            &BTreeMap::new(),
            &cx,
            &clock,
        )
        .map(|validated| assert!(validated.validated_at >= validated.captured_at))
    }
}

fn binding(roster: &ResolvedRoster, name: &str) -> SkillId {
    match roster.exact_name(name) {
        ExactResolution::Resolved { id, .. } => id.clone(),
        other => panic!("{name} did not resolve: {other:?}"),
    }
}

/// An advisory decision: wide set a..e, shortlist a..c, and a returned before
/// scoring removed b and c.
fn advisory_capture(w: &Workspace) -> Dependencies {
    let roster = w.roster();
    assert!(
        roster.is_partial(),
        "unenumerated sources keep the scope partial"
    );
    let wide: Vec<SkillId> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|n| binding(&roster, n))
        .collect();
    let (clock, _runtime, _cx) = clock(30_000);
    capture(&roster, &wide, &clock).unwrap()
}

#[test]
fn an_unchanged_partial_scope_revalidates() {
    let w = Workspace::new();
    let captured = advisory_capture(&w);
    assert_eq!(w.revalidate(&captured), Ok(()));
    // A change outside every dependency does not invalidate: the manual-only
    // skill's body is not decision content and its restrictions are unchanged.
    write_skill(
        &w.project(),
        "m",
        "disable-model-invocation: true\n",
        "Edited manual.",
    );
    assert_eq!(w.revalidate(&captured), Ok(()));
}

#[test]
fn a_changed_wide_candidate_outside_the_shortlist_invalidates() {
    let w = Workspace::new();
    let captured = advisory_capture(&w);
    // `d` was indexed and asked about in the wide stage but never shortlisted;
    // the returned skill `a` is unchanged.
    write_skill(&w.project(), "d", "", "Changed body.");
    assert_eq!(w.revalidate(&captured), Err(RevalidationError::Changed));
}

#[test]
fn a_new_overflow_candidate_invalidates() {
    let w = Workspace::new();
    let captured = advisory_capture(&w);
    write_skill(&w.project(), "f", "", "A new match.");
    assert_eq!(w.revalidate(&captured), Err(RevalidationError::Changed));
}

#[test]
fn a_new_candidate_replacing_another_at_the_same_count_invalidates() {
    let w = Workspace::new();
    let captured = advisory_capture(&w);
    // The manual-only `m` disappears while a new agent-invocable `f` appears:
    // the roster size is unchanged, but `f` is a new overflow candidate.
    fs::remove_file(w.project().join("m").join("SKILL.md")).unwrap();
    fs::remove_dir(w.project().join("m")).unwrap();
    write_skill(&w.project(), "f", "", "A new match.");
    assert_eq!(w.revalidate(&captured), Err(RevalidationError::Changed));
}

#[test]
fn a_new_shadowing_source_invalidates() {
    let w = Workspace::new();
    let captured = advisory_capture(&w);
    // A personal skill with a shortlisted name now outranks the project copy.
    write_skill(&w.user(), "b", "", "Personal b.");
    assert_eq!(w.revalidate(&captured), Err(RevalidationError::Changed));
}

#[test]
fn a_changed_restriction_on_a_removed_shortlist_candidate_invalidates() {
    let w = Workspace::new();
    let captured = advisory_capture(&w);
    // `c` was shortlisted and removed by scoring; its restriction still matters.
    // Trusted settings withdraw agent invocation while its bytes stay the same.
    let overrides = BTreeMap::from([(
        "c".to_owned(),
        InvocationRestrictions {
            agent_invocable: false,
            user_invocable: true,
        },
    )]);
    let (clock, _runtime, cx) = clock(30_000);
    let result = revalidate_claude(
        &captured,
        &w.workspace,
        Some(&w.home),
        visibility(),
        &overrides,
        &cx,
        &clock,
    );
    assert_eq!(result, Err(RevalidationError::Changed));
}

#[test]
fn explicit_targets_revalidate_content_and_name_precedence() {
    let w = Workspace::new();
    let roster = w.roster();
    let manual = match roster.exact_name("m") {
        ExactResolution::Resolved { id, .. } => id.clone(),
        other => panic!("manual-only m should resolve explicitly: {other:?}"),
    };
    let (c, _runtime, _cx) = clock(30_000);
    let captured = capture(&roster, [&manual], &c).unwrap();
    assert_eq!(w.revalidate(&captured), Ok(()));
    write_skill(
        &w.project(),
        "m",
        "disable-model-invocation: true\n",
        "Edited manual.",
    );
    assert_eq!(w.revalidate(&captured), Err(RevalidationError::Changed));

    let w = Workspace::new();
    let roster = w.roster();
    let manual = binding(&roster, "m");
    let captured = capture(&roster, [&manual], &c).unwrap();
    // A personal `m` would now win the name the user asked for.
    write_skill(&w.user(), "m", "", "Personal m.");
    assert_eq!(w.revalidate(&captured), Err(RevalidationError::Changed));
}

#[test]
fn an_unreadable_scope_is_incomplete_not_changed() {
    let w = Workspace::new();
    let captured = advisory_capture(&w);
    // Replace the project root with a file: the rescan cannot observe it.
    let moved = w.workspace.join(".claude/skills-moved");
    fs::rename(w.project(), &moved).unwrap();
    fs::write(w.project(), "not a directory").unwrap();
    assert_eq!(w.revalidate(&captured), Err(RevalidationError::Incomplete));
    assert_eq!(
        RevalidationError::Incomplete.kind(),
        Some(ErrorKind::IncompleteRoster)
    );
    assert_eq!(
        RevalidationError::Changed.kind(),
        Some(ErrorKind::RosterChanged)
    );
}

#[test]
fn deadline_and_cancellation_are_reported_without_extension() {
    let w = Workspace::new();
    let captured = advisory_capture(&w);
    // The dependencies are unchanged, yet an exhausted deadline is not extended.
    let (expired, _runtime, cx) = clock(20);
    std::thread::sleep(std::time::Duration::from_millis(40));
    let result = revalidate_claude(
        &captured,
        &w.workspace,
        Some(&w.home),
        visibility(),
        &BTreeMap::new(),
        &cx,
        &expired,
    );
    assert_eq!(result, Err(RevalidationError::Deadline));
    assert_eq!(RevalidationError::Deadline.kind(), Some(ErrorKind::Timeout));

    let (clock, runtime, cx) = clock(30_000);
    runtime.cancel_user(&cx);
    let result = revalidate_claude(
        &captured,
        &w.workspace,
        Some(&w.home),
        visibility(),
        &BTreeMap::new(),
        &cx,
        &clock,
    );
    assert_eq!(result, Err(RevalidationError::Cancelled));
}

#[test]
fn undeclared_dependencies_are_rejected_at_capture() {
    let w = Workspace::new();
    let roster = w.roster();
    let (c, _runtime, _cx) = clock(30_000);
    let unknown = SkillId::new("s_not_in_this_roster").unwrap();
    assert_eq!(
        capture(&roster, [&unknown], &c).unwrap_err(),
        RevalidationError::UnknownDependency
    );
}
