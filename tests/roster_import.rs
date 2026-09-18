use serde_json::{Value, json};
use skillranker::identity::{ContentHash, HarnessId};
use skillranker::limits::{DurationMillis, EXPLICIT_ROSTER_JSON_BYTES, EXPLICIT_ROSTER_RECORDS};
use skillranker::output::ErrorKind;
use skillranker::roster::discovery::{DiscoveryPlan, claude_code_plan};
use skillranker::roster::import::{
    ImportError, RecordProblem, import_authorized, import_synthetic, read_roster_file,
};
use skillranker::roster::resolution::{ExactResolution, ResolvedRoster, resolve_claude_plan};
use skillranker::roster::{InvocationKind, InvocationRestrictions, Visibility};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const PROJECT: &str = "claude_code.project";
const USER: &str = "claude_code.user";

fn verified() -> Visibility {
    Visibility::Verified {
        contract_version: "test-adapter-v1".into(),
    }
}
fn tree() -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-import-{}-{}-{}",
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
fn skill(name: &str, extra: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: Helps with {name} work.\n{extra}---\n# {name}\n\nBody.\n"
    )
}

/// A workspace and home with documented Claude roots.
struct Fixture {
    workspace: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            workspace: tree(),
            home: tree(),
        };
        fs::create_dir_all(fixture.project_root()).unwrap();
        fs::create_dir_all(fixture.user_root()).unwrap();
        fixture
    }
    fn project_root(&self) -> PathBuf {
        self.workspace.join(".claude/skills")
    }
    fn user_root(&self) -> PathBuf {
        self.home.join(".claude/skills")
    }
    fn project(&self, relative: &str, bytes: &str) -> PathBuf {
        write(&self.project_root(), relative, bytes)
    }
    fn user(&self, relative: &str, bytes: &str) -> PathBuf {
        write(&self.user_root(), relative, bytes)
    }
    fn plan(&self) -> DiscoveryPlan {
        claude_code_plan(&self.workspace, Some(&self.home), verified()).unwrap()
    }
    fn import(&self, manifest: &Value) -> Result<ResolvedRoster, ImportError> {
        self.import_with(manifest, &BTreeMap::new())
    }
    fn import_with(
        &self,
        manifest: &Value,
        overrides: &BTreeMap<String, InvocationRestrictions>,
    ) -> Result<ResolvedRoster, ImportError> {
        let (clock, _runtime, cx) = invocation();
        let bytes = serde_json::to_vec(manifest).unwrap();
        import_authorized(&bytes, &self.plan(), overrides, &cx, &clock)
    }
}
fn files(records: Value) -> Value {
    json!({"schema": "sr.roster.v1", "harness": "claude_code", "mode": "authorized_files", "skills": records})
}
fn texts(records: Value) -> Value {
    json!({"schema": "sr.roster.v1", "harness": "claude_code", "mode": "synthetic_text", "skills": records})
}
fn record(source: &str, path: &str) -> Value {
    json!({"source": source, "path": path})
}
fn claude() -> HarnessId {
    HarnessId::new("claude_code").unwrap()
}
fn problem(result: Result<ResolvedRoster, ImportError>) -> (usize, RecordProblem) {
    match result {
        Err(ImportError::Record { index, problem }) => (index, problem),
        other => panic!("expected a record rejection, got {other:?}"),
    }
}
fn resolved_kind(roster: &ResolvedRoster, name: &str) -> Option<InvocationKind> {
    match roster.exact_name(name) {
        ExactResolution::Resolved { kind, .. } => Some(kind),
        _ => None,
    }
}

#[test]
fn verified_inventory_replaces_discovery_with_discovery_identities() {
    let f = Fixture::new();
    let review = skill("review", "");
    f.project("review/SKILL.md", &review);
    f.user("deploy/SKILL.md", &skill("deploy", ""));
    f.project("shared/SKILL.md", &skill("shared", ""));
    f.user("shared/SKILL.md", &skill("shared-personal", ""));
    // Present on disk but absent from the manifest: an import never adds it.
    f.project("extra/SKILL.md", &skill("extra", ""));

    let (clock, _runtime, cx) = invocation();
    let discovered = resolve_claude_plan(&f.plan(), &BTreeMap::new(), &cx, &clock).unwrap();
    let discovered_id = |name: &str| match discovered.exact_name(name) {
        ExactResolution::Resolved { id, .. } => id.as_str().to_owned(),
        other => panic!("discovery did not resolve {name}: {other:?}"),
    };

    let manifest = files(json!([
        {"source": PROJECT, "path": "review/SKILL.md", "invocation": "review",
         "id": discovered_id("review"),
         "content_hash": ContentHash::from_bytes(review.as_bytes()).as_str(),
         "agent_invocable": true, "user_invocable": true},
        record(USER, "deploy/SKILL.md"),
        record(PROJECT, "shared/SKILL.md"),
        record(USER, "shared/SKILL.md"),
    ]));
    let roster = f.import(&manifest).unwrap();

    assert!(
        !roster.is_partial(),
        "a complete explicit inventory is not partial"
    );
    assert_eq!(roster.skills().len(), 4);
    assert_eq!(
        roster.advisory().count(),
        3,
        "the shadowed project copy is not advisory"
    );
    assert_eq!(roster.exact_name("extra"), ExactResolution::Missing);
    for name in ["review", "deploy"] {
        assert_eq!(resolved_kind(&roster, name), Some(InvocationKind::Agent));
    }
    // Same-name skills keep the adapter's personal-over-project precedence,
    // and every imported ID equals the ID discovery assigns to that file.
    match roster.exact_name("shared") {
        ExactResolution::Resolved { id, .. } => assert_eq!(id.as_str(), discovered_id("shared")),
        other => panic!("shared did not resolve: {other:?}"),
    }
    for name in ["review", "deploy"] {
        match roster.exact_name(name) {
            ExactResolution::Resolved { id, .. } => assert_eq!(id.as_str(), discovered_id(name)),
            other => panic!("{name} did not resolve: {other:?}"),
        }
    }
    let empty = f.import(&files(json!([]))).unwrap();
    assert_eq!(empty.skills().len(), 0);
}

#[test]
fn escaped_and_unauthorized_paths_are_refused() {
    let f = Fixture::new();
    f.user("deploy/SKILL.md", &skill("deploy", ""));
    let outside = tree();
    write(&outside, "evil/SKILL.md", &skill("evil", ""));
    let escape = f.project_root().join("evil");
    fs::create_dir_all(&escape).unwrap();
    symlink(outside.join("evil/SKILL.md"), escape.join("SKILL.md")).unwrap();
    symlink(
        f.user_root().join("deploy"),
        f.project_root().join("linked"),
    )
    .unwrap();
    let fifo = f.project_root().join("fifo");
    fs::create_dir_all(&fifo).unwrap();
    nix::unistd::mkfifo(&fifo.join("SKILL.md"), nix::sys::stat::Mode::S_IRWXU).unwrap();
    // A symlinked skill *file* into another authorized root stays loadable.
    let alias = f.project_root().join("alias");
    fs::create_dir_all(&alias).unwrap();
    symlink(
        f.user_root().join("deploy/SKILL.md"),
        alias.join("SKILL.md"),
    )
    .unwrap();

    for path in [
        "../outside/SKILL.md",
        "/etc/passwd",
        "review//SKILL.md",
        "./review/SKILL.md",
        "review/../deploy/SKILL.md",
        "",
    ] {
        assert_eq!(
            problem(f.import(&files(json!([record(PROJECT, path)])))),
            (0, RecordProblem::InvalidPath),
        );
    }
    let cases = [
        ("evil/SKILL.md", RecordProblem::EscapesAuthorizedRoots),
        ("linked/SKILL.md", RecordProblem::UnsupportedLayout),
        ("fifo/SKILL.md", RecordProblem::NotRegularFile),
        ("ghost/SKILL.md", RecordProblem::Missing),
        ("deep/nested/SKILL.md", RecordProblem::UnsupportedLayout),
        ("review/README.md", RecordProblem::UnsupportedLayout),
    ];
    for (path, expected) in cases {
        let manifest = files(json!([
            record(USER, "deploy/SKILL.md"),
            record(PROJECT, path)
        ]));
        assert_eq!(problem(f.import(&manifest)), (1, expected), "{path}");
    }
    let roster = f
        .import(&files(json!([record(PROJECT, "alias/SKILL.md")])))
        .unwrap();
    assert_eq!(resolved_kind(&roster, "alias"), Some(InvocationKind::Agent));
}

#[test]
fn duplicate_definitions_and_keys_are_rejected() {
    let f = Fixture::new();
    f.project("review/SKILL.md", &skill("review", ""));
    f.user("review/SKILL.md", &skill("review", ""));
    let twice = files(json!([
        record(PROJECT, "review/SKILL.md"),
        record(PROJECT, "review/SKILL.md"),
    ]));
    assert_eq!(
        problem(f.import(&twice)),
        (1, RecordProblem::DuplicateDefinition)
    );
    // Distinct files in distinct sources are distinct definitions.
    let distinct = files(json!([
        record(PROJECT, "review/SKILL.md"),
        record(USER, "review/SKILL.md"),
    ]));
    assert_eq!(f.import(&distinct).unwrap().skills().len(), 2);

    let (clock, _runtime, cx) = invocation();
    let raw =
        |text: &str| import_authorized(text.as_bytes(), &f.plan(), &BTreeMap::new(), &cx, &clock);
    let record_key = r#"{"schema":"sr.roster.v1","harness":"claude_code","mode":"authorized_files","skills":[{"source":"claude_code.project","path":"review/SKILL.md","path":"other/SKILL.md"}]}"#;
    assert_eq!(raw(record_key).unwrap_err(), ImportError::DuplicateKey);
    let top_key = r#"{"schema":"sr.roster.v1","harness":"claude_code","mode":"synthetic_text","mode":"authorized_files","skills":[]}"#;
    assert_eq!(raw(top_key).unwrap_err(), ImportError::DuplicateKey);

    let synthetic = texts(json!([
        {"invocation": "review", "text": skill("review", "")},
        {"invocation": "review", "text": skill("other", "")},
    ]));
    assert_eq!(
        import_synthetic(&serde_json::to_vec(&synthetic).unwrap(), &claude()).unwrap_err(),
        ImportError::Record {
            index: 1,
            problem: RecordProblem::DuplicateDefinition
        },
    );
}

#[test]
fn false_identity_content_and_eligibility_claims_are_rejected() {
    let f = Fixture::new();
    let review = skill("review", "");
    f.project("review/SKILL.md", &review);
    f.project(
        "manual/SKILL.md",
        &skill("manual", "disable-model-invocation: true\n"),
    );
    let with = |extra: Value| {
        let mut entry = record(PROJECT, "review/SKILL.md");
        entry
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        files(json!([entry]))
    };
    let other_bytes = ContentHash::from_bytes(b"different bytes");
    let cases = [
        (
            json!({"content_hash": other_bytes.as_str()}),
            RecordProblem::ContentMismatch,
        ),
        (
            json!({"content_hash": "not-a-digest"}),
            RecordProblem::InvalidDigest,
        ),
        (
            json!({"invocation": "deploy"}),
            RecordProblem::InvocationMismatch,
        ),
        (
            json!({"id": "s_0000000000000000000000000000000000000000000000000000000000000000"}),
            RecordProblem::IdMismatch,
        ),
        (
            json!({"agent_invocable": false}),
            RecordProblem::EligibilityMismatch,
        ),
    ];
    for (claim, expected) in cases {
        assert_eq!(
            problem(f.import(&with(claim.clone()))),
            (0, expected),
            "{claim}"
        );
    }
    // Honest claims about the same file are accepted.
    let honest = with(json!({
        "content_hash": ContentHash::from_bytes(review.as_bytes()).as_str(),
        "invocation": "review", "agent_invocable": true, "user_invocable": true,
    }));
    assert!(f.import(&honest).is_ok());

    // A manifest cannot grant eligibility the file itself withholds.
    let manual = |agent: bool| {
        files(json!([{"source": PROJECT, "path": "manual/SKILL.md", "agent_invocable": agent}]))
    };
    assert_eq!(
        problem(f.import(&manual(true))),
        (0, RecordProblem::EligibilityMismatch)
    );
    let roster = f.import(&manual(false)).unwrap();
    assert_eq!(roster.advisory().count(), 0);
    assert_eq!(
        resolved_kind(&roster, "manual"),
        Some(InvocationKind::ManualOnly)
    );

    // Nor eligibility trusted settings withhold.
    let overrides = BTreeMap::from([(
        "review".to_owned(),
        InvocationRestrictions {
            agent_invocable: false,
            user_invocable: true,
        },
    )]);
    assert_eq!(
        problem(f.import_with(&with(json!({"agent_invocable": true})), &overrides)),
        (0, RecordProblem::EligibilityMismatch),
    );
    // Visibility and precedence come only from the adapter; claiming them is invalid.
    for field in ["visibility", "priority"] {
        let claim = with(Value::Object(serde_json::Map::from_iter([(
            field.to_owned(),
            json!("verified"),
        )])));
        assert_eq!(
            f.import(&claim).unwrap_err(),
            ImportError::UnknownField,
            "{field}"
        );
    }
}

#[test]
fn namespace_and_mode_mismatches_are_rejected() {
    let f = Fixture::new();
    f.project("review/SKILL.md", &skill("review", ""));
    let mut foreign = files(json!([record(PROJECT, "review/SKILL.md")]));
    foreign["harness"] = json!("codex");
    assert_eq!(
        f.import(&foreign).unwrap_err(),
        ImportError::HarnessMismatch
    );
    for source in ["codex.user", "claude_code.plugin", "claude_code.managed"] {
        assert_eq!(
            problem(f.import(&files(json!([record(source, "review/SKILL.md")])))),
            (0, RecordProblem::UnknownSource),
            "{source}",
        );
    }
    let (clock, _runtime, cx) = invocation();
    let codex_plan = DiscoveryPlan::new(HarnessId::new("codex").unwrap());
    let codex_manifest = serde_json::to_vec(&json!({
        "schema": "sr.roster.v1", "harness": "codex", "mode": "authorized_files", "skills": []
    }))
    .unwrap();
    assert_eq!(
        import_authorized(&codex_manifest, &codex_plan, &BTreeMap::new(), &cx, &clock).unwrap_err(),
        ImportError::UnsupportedHarness,
    );

    let synthetic = texts(json!([{"invocation": "review", "text": skill("review", "")}]));
    assert_eq!(f.import(&synthetic).unwrap_err(), ImportError::ModeMismatch);
    let authorized =
        serde_json::to_vec(&files(json!([record(PROJECT, "review/SKILL.md")]))).unwrap();
    assert_eq!(
        import_synthetic(&authorized, &claude()).unwrap_err(),
        ImportError::ModeMismatch
    );
    let mut other_harness = texts(json!([]));
    other_harness["harness"] = json!("codex");
    assert_eq!(
        import_synthetic(&serde_json::to_vec(&other_harness).unwrap(), &claude()).unwrap_err(),
        ImportError::HarnessMismatch,
    );

    // Each mode's records cannot carry the other mode's fields.
    let texted = files(json!([{"source": PROJECT, "path": "review/SKILL.md", "text": "x"}]));
    assert_eq!(
        problem(f.import(&texted)),
        (0, RecordProblem::WrongRecordKind)
    );
    let pathed = texts(json!([{"invocation": "review", "text": "x", "path": "review/SKILL.md"}]));
    assert_eq!(
        import_synthetic(&serde_json::to_vec(&pathed).unwrap(), &claude()).unwrap_err(),
        ImportError::Record {
            index: 0,
            problem: RecordProblem::WrongRecordKind
        },
    );
}

#[test]
fn malformed_and_oversized_manifests_fail_closed() {
    let f = Fixture::new();
    f.project("review/SKILL.md", &skill("review", ""));
    f.project("broken/SKILL.md", "---\nname: [unclosed\n---\nbody\n");
    let (clock, _runtime, cx) = invocation();
    let raw = |bytes: &[u8]| import_authorized(bytes, &f.plan(), &BTreeMap::new(), &cx, &clock);

    let valid = serde_json::to_vec(&files(json!([record(PROJECT, "review/SKILL.md")]))).unwrap();
    assert!(raw(&valid).is_ok());
    let mut trailing = valid.clone();
    trailing.extend_from_slice(b" {}");
    let deep = format!(
        r#"{{"schema":"sr.roster.v1","harness":"claude_code","mode":"authorized_files","skills":[{{"source":{}}}]}}"#,
        "[".repeat(100_000)
    );
    for bytes in [
        b"not json".as_slice(),
        trailing.as_slice(),
        br#"{"schema":"sr.roster.v1","harness":"claude_code","mode":"authorized_files","skills":{}}"#,
        br#"{"schema":"sr.roster.v1","harness":"claude_code","mode":"authorized_files"}"#,
        br#"{"schema":"sr.roster.v1","harness":"claude_code","mode":"live","skills":[]}"#,
        br#"{"schema":"sr.roster.v1","harness":"claude_code","mode":"authorized_files","skills":[{"source":"claude_code.project","path":"review/SKILL.md","agent_invocable":"yes"}]}"#,
        deep.as_bytes(),
    ] {
        assert_eq!(raw(bytes).unwrap_err(), ImportError::Malformed);
    }
    let mut schema = files(json!([]));
    schema["schema"] = json!("sr.roster.v2");
    assert_eq!(
        f.import(&schema).unwrap_err(),
        ImportError::UnsupportedSchema
    );
    assert_eq!(
        problem(f.import(&files(json!([{"source": PROJECT}])))),
        (0, RecordProblem::MissingField),
    );
    assert_eq!(
        problem(f.import(&files(json!([record(PROJECT, "broken/SKILL.md")])))),
        (0, RecordProblem::Metadata),
    );

    // Exactly the byte ceiling is accepted; one byte more is refused unparsed.
    let mut at_limit = valid.clone();
    at_limit.resize(EXPLICIT_ROSTER_JSON_BYTES.max(), b' ');
    assert!(raw(&at_limit).is_ok());
    at_limit.push(b' ');
    assert_eq!(raw(&at_limit).unwrap_err(), ImportError::TooLarge);

    // Exactly the record ceiling is accepted; one record more is refused.
    let records = |count: usize| {
        let skills: Vec<Value> = (0..count)
            .map(|i| json!({"invocation": format!("s{i}"), "text": "---\ndescription: d\n---\n"}))
            .collect();
        serde_json::to_vec(&texts(Value::Array(skills))).unwrap()
    };
    let limit = EXPLICIT_ROSTER_RECORDS.max();
    assert_eq!(
        import_synthetic(&records(limit), &claude())
            .unwrap()
            .skills()
            .len(),
        limit
    );
    assert_eq!(
        import_synthetic(&records(limit + 1), &claude()).unwrap_err(),
        ImportError::TooManyRecords,
    );
    assert_eq!(
        ImportError::TooManyRecords.kind(),
        Some(ErrorKind::UnusableRoster)
    );
}

#[test]
fn synthetic_text_supports_evaluation_but_not_live_resolution() {
    let review = skill("review", "");
    let manual = skill("manual", "disable-model-invocation: true\n");
    let manifest = texts(json!([
        {"invocation": "review", "text": review,
         "content_hash": ContentHash::from_bytes(review.as_bytes()).as_str()},
        {"invocation": "manual", "text": manual, "agent_invocable": false},
    ]));
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let roster = import_synthetic(&bytes, &claude()).unwrap();
    let again = import_synthetic(&bytes, &claude()).unwrap();
    assert_eq!(roster.skills().len(), 2);
    let first = &roster.skills()[0];
    assert_eq!(
        first.id(),
        again.skills()[0].id(),
        "IDs are stable across imports"
    );
    assert_ne!(roster.skills()[0].id(), roster.skills()[1].id());
    assert_eq!(first.invocation().as_str(), "review");
    assert_eq!(
        first.content_hash(),
        &ContentHash::from_bytes(review.as_bytes())
    );
    assert!(first.metadata().description.contains("review"));
    assert!(first.restrictions().agent_invocable);
    assert!(!roster.skills()[1].restrictions().agent_invocable);

    // The live path refuses the same manifest outright.
    let f = Fixture::new();
    assert_eq!(f.import(&manifest).unwrap_err(), ImportError::ModeMismatch);

    let lie = |extra: Value| {
        let mut entry = json!({"invocation": "manual", "text": manual});
        entry
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        import_synthetic(
            &serde_json::to_vec(&texts(json!([entry]))).unwrap(),
            &claude(),
        )
        .unwrap_err()
    };
    let wrong_hash = ContentHash::from_bytes(b"x");
    for (extra, expected) in [
        (
            json!({"agent_invocable": true}),
            RecordProblem::EligibilityMismatch,
        ),
        (
            json!({"content_hash": wrong_hash.as_str()}),
            RecordProblem::ContentMismatch,
        ),
        (json!({"id": "s_forged"}), RecordProblem::IdMismatch),
    ] {
        assert_eq!(
            lie(extra),
            ImportError::Record {
                index: 0,
                problem: expected
            }
        );
    }
    let malformed = texts(json!([{"invocation": "bad", "text": "---\nname: [unclosed\n---\n"}]));
    assert_eq!(
        import_synthetic(&serde_json::to_vec(&malformed).unwrap(), &claude()).unwrap_err(),
        ImportError::Record {
            index: 0,
            problem: RecordProblem::Metadata
        },
    );
}

#[test]
fn diagnostics_never_echo_manifest_or_skill_content() {
    let f = Fixture::new();
    f.project(
        "review/SKILL.md",
        &skill("CANARYNAME", "CANARYFIELD: CANARYVALUE\n"),
    );
    let outcomes = [
        f.import(&files(json!([record(PROJECT, "CANARYPATH/SKILL.md")]))),
        f.import(&files(
            json!([{"source": PROJECT, "path": "review/SKILL.md", "CANARYKEY": 1}]),
        )),
        f.import(&files(json!([record("CANARY SOURCE", "review/SKILL.md")]))),
        f.import(&files(
            json!([{"source": PROJECT, "path": "review/SKILL.md", "invocation": "CANARYCLAIM"}]),
        )),
    ];
    for outcome in outcomes {
        let error = outcome.unwrap_err();
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains("CANARY"), "{rendered}");
        }
    }
    let roster = f
        .import(&files(json!([record(PROJECT, "review/SKILL.md")])))
        .unwrap();
    assert!(!format!("{roster:?}").contains("CANARY"));
    let synthetic = texts(json!([{"invocation": "review", "text": skill("CANARYNAME", "")}]));
    let synthetic = import_synthetic(&serde_json::to_vec(&synthetic).unwrap(), &claude()).unwrap();
    assert!(!format!("{synthetic:?}").contains("CANARY"));
    assert!(!format!("{:?}", synthetic.skills()[0]).contains("CANARY"));
}

#[test]
fn roster_file_reads_are_bounded_regular_files() {
    let dir = tree();
    let path = dir.join("roster.json");
    fs::write(&path, b"{}").unwrap();
    assert_eq!(read_roster_file(&path).unwrap(), b"{}");
    let fifo = dir.join("roster.fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
    assert_eq!(
        read_roster_file(&fifo).unwrap_err(),
        ImportError::Unreadable
    );
    assert_eq!(
        read_roster_file(&dir.join("absent.json")).unwrap_err(),
        ImportError::Unreadable
    );
    let big = dir.join("big.json");
    fs::write(&big, vec![b' '; EXPLICIT_ROSTER_JSON_BYTES.max() + 1]).unwrap();
    assert_eq!(read_roster_file(&big).unwrap_err(), ImportError::TooLarge);
}

#[test]
fn cancellation_stops_import_before_reading() {
    let f = Fixture::new();
    f.project("review/SKILL.md", &skill("review", ""));
    let (clock, runtime, cx) = invocation();
    runtime.cancel_user(&cx);
    let bytes = serde_json::to_vec(&files(json!([record(PROJECT, "review/SKILL.md")]))).unwrap();
    let error = import_authorized(&bytes, &f.plan(), &BTreeMap::new(), &cx, &clock).unwrap_err();
    assert_eq!(error, ImportError::Cancelled);
    assert_eq!(error.kind(), None);
}

/// Every path under `root` with its type, size, mode, modification time and
/// bytes. Access times are excluded: reading may update them.
fn tree_state(root: &Path) -> BTreeMap<PathBuf, (u32, u64, i64, i64, ContentHash)> {
    use std::os::unix::fs::MetadataExt;
    let mut state = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = fs::symlink_metadata(&path).unwrap();
        let bytes = if meta.is_file() {
            fs::read(&path).unwrap()
        } else {
            Vec::new()
        };
        if meta.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                pending.push(entry.unwrap().path());
            }
        }
        state.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            (
                meta.mode(),
                meta.len(),
                meta.mtime(),
                meta.mtime_nsec(),
                ContentHash::from_bytes(&bytes),
            ),
        );
    }
    state
}

#[test]
fn importing_a_roster_mutates_nothing() {
    let f = Fixture::new();
    f.project("review/SKILL.md", &skill("review", ""));
    f.user("deploy/SKILL.md", &skill("deploy", ""));
    let manifest_dir = tree();
    let manifest = files(json!([
        record(PROJECT, "review/SKILL.md"),
        record(USER, "deploy/SKILL.md"),
    ]));
    let manifest_path = manifest_dir.join("roster.json");
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let before = (
        tree_state(&f.workspace),
        tree_state(&f.home),
        tree_state(&manifest_dir),
    );
    let bytes = read_roster_file(&manifest_path).unwrap();
    let (clock, _runtime, cx) = invocation();
    let roster = import_authorized(&bytes, &f.plan(), &BTreeMap::new(), &cx, &clock).unwrap();
    assert_eq!(roster.skills().len(), 2);
    // A refused import and a synthetic import are equally read-only.
    let refused = files(json!([record(PROJECT, "../escape/SKILL.md")]));
    assert!(f.import(&refused).is_err());
    let synthetic = texts(json!([
        {"invocation": "text", "text": skill("text", "")}
    ]));
    import_synthetic(&serde_json::to_vec(&synthetic).unwrap(), &claude()).unwrap();
    let after = (
        tree_state(&f.workspace),
        tree_state(&f.home),
        tree_state(&manifest_dir),
    );
    assert_eq!(before, after, "an import changed the filesystem");
    // Positive twin: the observer does see a real change.
    f.project("review/SKILL.md", &skill("review", "usage: workflow\n"));
    assert_ne!(tree_state(&f.workspace), after.0);
}
