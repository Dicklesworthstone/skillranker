//! Connected P2 checks: real skill trees, production resolution/Quill and sr.
//! Synthetic adapter visibility below qualifies these local components only.
use asupersync::Cx;
use serde_json::json;
use skillranker::limits::DurationMillis;
use skillranker::roster::Visibility;
use skillranker::roster::discovery::claude_code_plan;
use skillranker::roster::explicit::{
    ExplicitResolutionRequest, ExplicitResolutionResult, resolve_explicit_requirements,
};
use skillranker::roster::resolution::{ResolvedRoster, resolve_claude_plan};
use skillranker::roster::retrieval::{
    QueryInput, RetrievalBudget, RetrievalError, RetrievalMethod, retrieve,
};
use skillranker::roster::revalidation::{RevalidationError, capture, revalidate_claude};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Tree {
    root: PathBuf,
}
impl Tree {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-roster-e2e-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workspace/.claude/skills")).unwrap();
        std::fs::create_dir_all(root.join("home/.claude/skills")).unwrap();
        Self { root }
    }
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn skill(&self, name: &str, display: &str, extra: &str) -> PathBuf {
        let dir = self.workspace().join(".claude/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("SKILL.md");
        std::fs::write(&file, format!("---\nname: {display}\ndescription: sharedterm skill for deterministic local checks\n{extra}---\nPublic body.\n")).unwrap();
        file
    }
    fn resolve(&self, clock: &EntryClock, cx: &Cx) -> ResolvedRoster {
        let plan = claude_code_plan(&self.workspace(), Some(&self.home()), visibility()).unwrap();
        resolve_claude_plan(&plan, &BTreeMap::new(), cx, clock).unwrap()
    }
    fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_sr"))
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .current_dir(self.workspace())
            .output()
            .unwrap()
    }
}
fn visibility() -> Visibility {
    Visibility::Verified {
        contract_version: "synthetic-local-p2-v1".into(),
    }
}
fn runtime() -> (EntryClock, ProcessInvocation, Cx) {
    let clock = EntryClock::capture_with(
        DurationMillis::new("test", 30_000, 30_000).unwrap(),
        DurationMillis::new("cleanup", 200, 30_000).unwrap(),
    )
    .unwrap();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    (clock, invocation, cx)
}

#[test]
fn roster_failure_cases() {
    for count in [0, 1, 254, 255, 1000] {
        let tree = Tree::new();
        for n in 0..count {
            tree.skill(&format!("skill-{n:04}"), &format!("Skill {n}"), "");
        }
        let (clock, invocation, cx) = runtime();
        let started = Instant::now();
        let roster = tree.resolve(&clock, &cx);
        assert_eq!(roster.skills().len(), count);
        assert!(
            roster.is_partial(),
            "unenumerated managed/plugin roots remain explicit"
        );
        let excluded = BTreeSet::new();
        let result = invocation.runtime().block_on(retrieve(
            &roster,
            &excluded,
            QueryInput {
                latest_request: "sharedterm",
                active_task: "",
                recent_errors: "",
            },
            RetrievalBudget::default(),
            &cx,
            &clock,
        ));
        if count == 0 {
            assert_eq!(
                result.unwrap_err().kind,
                RetrievalError::NoEligibleCandidates
            );
        } else {
            let selection = result.unwrap();
            assert_eq!(selection.candidates.len(), count.min(254));
            assert_eq!(
                selection.diagnostics.method,
                Some(if count > 254 {
                    RetrievalMethod::QuillBm25
                } else {
                    RetrievalMethod::FullRoster
                })
            );
            assert_eq!(selection.diagnostics.eligible_count, Some(count));
            assert!(selection.diagnostics.partial_roster);
            assert_eq!(selection.diagnostics.build_commit_us.is_some(), count > 254);
            // Every admitted candidate resolves to the actual authorized roster.
            for candidate in &selection.candidates {
                assert!(
                    roster
                        .skills()
                        .iter()
                        .any(|s| s.record().id == candidate.record.id)
                );
            }
            let request = ExplicitResolutionRequest {
                cli_required_skills: vec!["skill-0000".into()],
                ..Default::default()
            };
            let ExplicitResolutionResult::Resolved { skills, .. } =
                resolve_explicit_requirements(&request, &roster).unwrap()
            else {
                panic!("valid explicit target must resolve");
            };
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].invocation.as_str(), "skill-0000");
            eprintln!(
                "{}",
                json!({"schema_version":1,"case_id":if count == 1 { "1-skill".to_owned() } else { format!("{count}-skills") },"stage":"quill","build_commit_us":selection.diagnostics.build_commit_us,"search_us":selection.diagnostics.search_us,"selected":selection.candidates.len(),"partial":true})
            );
        }
        // The user-visible CLI must enumerate the same scope, with bounded pages.
        let output = tree.cli(&["roster", "--json"]);
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        let page: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(page["total"], count);
        assert_eq!(page["partial"], true);
        assert_eq!(page["records"].as_array().unwrap().len(), count.min(128));
        assert!(invocation.shutdown());
        eprintln!(
            "{}",
            json!({"schema_version":1,"case_id":if count == 1 { "1-skill".to_owned() } else { format!("{count}-skills") },"stage":"complete","assertion":"all_scale_invariants_pass","elapsed_ms":started.elapsed().as_millis(),"status":"passed","provider_requests":0})
        );
    }
}

#[test]
fn serialized_roster_redacts_untrusted_display_names() {
    let tree = Tree::new();
    tree.skill("ordinary", "Ordinary display name", "");
    tree.skill("private", "api_key=synthetic-private-roster-value", "");
    let output = tree.cli(&["roster", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        !text.contains("synthetic-private-roster-value"),
        "display metadata must be redacted before serialization"
    );
    let page: serde_json::Value = serde_json::from_str(&text).unwrap();
    let records = page["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().any(|r| r["name"] == "Ordinary display name"));
    assert!(
        records
            .iter()
            .any(|r| r["name"].as_str().unwrap().contains("[REDACTED]"))
    );
}

#[test]
fn changed_membership_and_hostile_metadata_never_become_publishable() {
    let tree = Tree::new();
    let file = tree.skill("good", "Good", "");
    tree.skill("manual", "Manual", "disable-model-invocation: true\n");
    let (clock, invocation, cx) = runtime();
    let roster = tree.resolve(&clock, &cx);
    let request = ExplicitResolutionRequest {
        cli_required_skills: vec!["manual".into()],
        ..Default::default()
    };
    let ExplicitResolutionResult::Resolved { skills, .. } =
        resolve_explicit_requirements(&request, &roster).unwrap()
    else {
        panic!("manual explicit requirement must remain available");
    };
    assert!(skills[0].manual_only);
    let content: Vec<_> = roster
        .skills()
        .iter()
        .map(|s| s.record().id.clone())
        .collect();
    let dependency = capture(&roster, &content, &clock).unwrap();
    let validate = || {
        revalidate_claude(
            &dependency,
            &tree.workspace(),
            Some(&tree.home()),
            visibility(),
            &BTreeMap::new(),
            &cx,
            &clock,
        )
    };
    assert!(validate().is_ok(), "unchanged real files must revalidate");
    std::fs::write(
        file,
        "---\nname: Good\ndescription: changed\n---\nChanged body\n",
    )
    .unwrap();
    assert_eq!(validate(), Err(RevalidationError::Changed));
    // Malformed frontmatter excludes only the malformed record and discloses coverage.
    let malformed = tree.skill("malformed", "Malformed", "");
    std::fs::write(malformed, "---\nname: [unsupported]\n---\nbody\n").unwrap();
    let roster = tree.resolve(&clock, &cx);
    assert!(roster.is_partial());
    assert_eq!(roster.skills().len(), 2);
    assert!(
        matches!(
            resolve_explicit_requirements(
                &ExplicitResolutionRequest {
                    cli_required_skills: vec!["manual".into()],
                    ..Default::default()
                },
                &roster
            )
            .unwrap(),
            ExplicitResolutionResult::Unavailable { .. }
        ),
        "an unreadable competing definition must revoke invocation authority"
    );
    assert!(invocation.shutdown());
}

#[test]
fn metadata_strings_reject_collections_but_keep_quoted_text() {
    use skillranker::roster::parse_skill_metadata;
    for field in ["name", "description"] {
        for value in ["[unsupported]", "{nested: value}"] {
            let content = format!("---\n{field}: {value}\n---\nbody\n");
            assert!(parse_skill_metadata(content.as_bytes()).is_err());
        }
        for value in ["\"[readable text]\"", "'{readable text}'", "ordinary text"] {
            let content = format!("---\n{field}: {value}\n---\nbody\n");
            assert!(parse_skill_metadata(content.as_bytes()).is_ok());
        }
    }
}

#[test]
fn revalidation_keeps_capture_origin_and_cannot_restart_deadlines() {
    use skillranker::roster::revalidation::revalidate;
    use std::time::Duration;
    let tree = Tree::new();
    tree.skill("ordinary", "Ordinary", "");
    let (clock, invocation, cx) = runtime();
    let roster = tree.resolve(&clock, &cx);
    let content: Vec<_> = roster
        .skills()
        .iter()
        .map(|s| s.record().id.clone())
        .collect();
    std::thread::sleep(Duration::from_millis(20));
    let dependencies = capture(&roster, &content, &clock).unwrap();
    let (fresh, fresh_invocation, _) = runtime();
    let validated = revalidate(&dependencies, &roster, &fresh).unwrap();
    assert!(validated.validated_at >= validated.captured_at);

    let short = EntryClock::capture_with(
        DurationMillis::new("test", 200, 30_000).unwrap(),
        DurationMillis::new("cleanup", 20, 30_000).unwrap(),
    )
    .unwrap();
    let short_dependencies = capture(&roster, &content, &short).unwrap();
    assert!(revalidate(&short_dependencies, &roster, &fresh).is_ok());
    std::thread::sleep(Duration::from_millis(220));
    assert_eq!(
        revalidate(&short_dependencies, &roster, &fresh),
        Err(RevalidationError::Deadline)
    );
    assert_eq!(
        revalidate(&dependencies, &roster, &short),
        Err(RevalidationError::Deadline)
    );
    assert!(matches!(
        capture(&roster, &content, &short),
        Err(RevalidationError::Deadline)
    ));
    assert!(invocation.shutdown());
    assert!(fresh_invocation.shutdown());
}
