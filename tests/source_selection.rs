use skillranker::context::source::*;
use skillranker::identity::{
    AdapterId, AdapterVersion, AgentId, BranchId, HarnessId, SessionId, SessionIdentity, SourceId,
    SourceProvenance, WorkspaceId,
};
use skillranker::limits::DISCOVERY_FILES;
use skillranker::output::ErrorKind;
use skillranker::roster::LocalPath;
use std::cell::Cell;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn path(value: impl Into<PathBuf>) -> LocalPath {
    LocalPath::new(value.into())
}

fn workspace() -> WorkspaceId {
    WorkspaceId::new("workspace-main".into()).unwrap()
}

fn candidate(session: &str, time: i64) -> SessionCandidate {
    SessionCandidate {
        target: SourceTarget::ClaudeTranscript(path(format!("{session}.jsonl"))),
        identity: SessionIdentity {
            source: SourceProvenance::Native {
                adapter: AdapterId::new("claude_code".into()).unwrap(),
                version: AdapterVersion::new("1".into()).unwrap(),
            },
            workspace: Some(workspace()),
            session: Some(SessionId::new(session.into()).unwrap()),
            agent: None,
            branch: None,
            epoch: None,
        },
        last_activity_unix_ms: Some(time),
        remote: false,
    }
}

fn inventory(candidates: Vec<SessionCandidate>) -> SessionInventory {
    SessionInventory {
        candidates,
        complete: true,
    }
}

fn selected(outcome: SelectionOutcome) -> SourceSelection {
    match outcome {
        SelectionOutcome::Selected(selection) => selection,
        SelectionOutcome::NeedsChoice(_) => panic!("expected a selected source"),
    }
}

fn explicit(options: &SourceOptions) -> Result<SourceSelection, SourceError> {
    options
        .resolve(workspace(), SourcePolicy::default(), false, |_, _| {
            panic!("explicit source must never discover another session")
        })
        .map(selected)
}

#[test]
fn exact_source_selection() {
    let cases = [
        (
            SourceOptions {
                claude_hook: true,
                ..Default::default()
            },
            SourceTarget::ClaudeHookStdin,
        ),
        (
            SourceOptions {
                context: Some(path("-")),
                ..Default::default()
            },
            SourceTarget::NormalizedStdin,
        ),
        (
            SourceOptions {
                context: Some(path("context.json")),
                ..Default::default()
            },
            SourceTarget::NormalizedFile(path("context.json")),
        ),
        (
            SourceOptions {
                transcript: Some(path("session.jsonl")),
                harness: Some(HarnessId::new("claude_code".into()).unwrap()),
                ..Default::default()
            },
            SourceTarget::ClaudeTranscript(path("session.jsonl")),
        ),
        (
            SourceOptions {
                cass_session: Some(path("exact/session.jsonl")),
                ..Default::default()
            },
            SourceTarget::CassSession(path("exact/session.jsonl")),
        ),
    ];
    for (options, expected) in cases {
        let selection = explicit(&options).unwrap();
        assert_eq!(selection.target(), &expected);
        assert_eq!(selection.reason(), SelectionReason::Explicit);
        assert_eq!(selection.workspace(), &workspace());
        assert!(selection.expected_identity().is_none());
    }
}

#[test]
fn conflicting_flags_and_harness_errors_precede_discovery() {
    for bits in 0u32..16 {
        if bits.count_ones() < 2 {
            continue;
        }
        let options = SourceOptions {
            claude_hook: bits & 1 != 0,
            context: (bits & 2 != 0).then(|| path("-")),
            transcript: (bits & 4 != 0).then(|| path("a")),
            cass_session: (bits & 8 != 0).then(|| path("b")),
            ..Default::default()
        };
        assert_eq!(
            explicit(&options).unwrap_err(),
            SourceError::ConflictingFlags
        );
    }
    let native = SourceOptions {
        transcript: Some(path("session.jsonl")),
        ..Default::default()
    };
    assert_eq!(explicit(&native).unwrap_err(), SourceError::HarnessRequired);
    let unsupported = SourceOptions {
        harness: Some(HarnessId::new("unverified-harness".into()).unwrap()),
        ..native
    };
    assert_eq!(
        explicit(&unsupported).unwrap_err().kind(),
        ErrorKind::UnsupportedInput
    );
    for options in [
        SourceOptions {
            context: Some(path("context.json")),
            latest: true,
            ..Default::default()
        },
        SourceOptions {
            harness: Some(HarnessId::new("claude_code".into()).unwrap()),
            ..Default::default()
        },
    ] {
        assert_eq!(
            explicit(&options).unwrap_err(),
            SourceError::ConflictingFlags
        );
    }
}

#[test]
fn non_tty_and_json_shape_do_not_select_stdin() {
    let hook_bytes = br#"{"session_id":"private","hook_event_name":"UserPromptSubmit"}"#;
    let mut stdin = Cursor::new(hook_bytes);
    let resolved = SourceOptions::default()
        .resolve(workspace(), SourcePolicy::default(), false, |_, _| {
            Ok(inventory(vec![candidate("unique", 1)]))
        })
        .unwrap();
    selected(resolved)
        .read(|selection| -> Result<(), SourceError> {
            assert!(!selection.target().reads_stdin());
            Ok(())
        })
        .unwrap();
    assert_eq!(stdin.position(), 0);

    let normalized = explicit(&SourceOptions {
        context: Some(path("-")),
        ..Default::default()
    })
    .unwrap();
    let result = normalized.read(|selection| -> Result<(), SourceError> {
        assert_eq!(selection.target(), &SourceTarget::NormalizedStdin);
        let mut bytes = Vec::new();
        stdin.read_to_end(&mut bytes).unwrap();
        // The chosen normalized parser rejects hook-shaped data; no sniff/fallback.
        serde_json::from_slice::<skillranker::context::NormalizedContext>(&bytes)
            .map(|_| ())
            .map_err(|_| SourceError::InvalidInventory)
    });
    assert_eq!(result, Err(SourceError::InvalidInventory));
    assert_eq!(stdin.position() as usize, hook_bytes.len());
}

#[test]
fn local_modes_refuse_cass_before_any_discovery_or_reader() {
    let options = SourceOptions {
        cass_session: Some(path("archive/session")),
        ..Default::default()
    };
    for policy in [
        SourcePolicy {
            offline: true,
            ..Default::default()
        },
        SourcePolicy {
            dry_run: true,
            ..Default::default()
        },
        SourcePolicy {
            local_only: true,
            ..Default::default()
        },
    ] {
        let result = options.resolve(workspace(), policy, true, |_, _| {
            panic!("must not launch cass")
        });
        let error = result.unwrap_err();
        assert_eq!(error, SourceError::CassUnavailableInMode);
        assert_eq!(error.kind().exit_code() as u8, 7);
        assert!(error.hint().contains("normalized"));
        let direct = SourceOptions {
            context: Some(path("context.json")),
            ..Default::default()
        };
        assert!(
            direct
                .resolve(workspace(), policy, false, |_, _| panic!("no discovery"))
                .is_ok()
        );
    }
    assert!(explicit(&options).is_ok()); // No credential/network inference in selection.
    for policy in [
        SourcePolicy {
            offline: true,
            allow_network: true,
            ..Default::default()
        },
        SourcePolicy {
            dry_run: true,
            allow_network: true,
            ..Default::default()
        },
        SourcePolicy {
            local_only: true,
            allow_network: true,
            ..Default::default()
        },
    ] {
        assert_eq!(
            options
                .resolve(workspace(), policy, false, |_, _| panic!("no discovery"))
                .unwrap_err(),
            SourceError::ConflictingFlags
        );
    }
}

#[test]
fn concurrent_sessions_require_choice_even_when_one_is_newer() {
    let options = SourceOptions::default();
    let sessions = inventory(vec![candidate("first", 1), candidate("second", 2)]);
    assert_eq!(
        options
            .resolve(workspace(), SourcePolicy::default(), false, |_, _| Ok(
                sessions.clone()
            ))
            .unwrap_err(),
        SourceError::AmbiguousSession
    );
    let SelectionOutcome::NeedsChoice(choices) = options
        .resolve(workspace(), SourcePolicy::default(), true, |_, _| {
            Ok(sessions)
        })
        .unwrap()
    else {
        panic!("must ask for selection")
    };
    assert_eq!(choices.candidates().len(), 2);
    assert_eq!(
        choices.clone().choose(2).unwrap_err(),
        SourceError::InvalidChoice
    );
    let choice = choices.choose(0).unwrap();
    assert_eq!(choice.reason(), SelectionReason::InteractiveChoice);
    assert_eq!(
        choice
            .expected_identity()
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .as_str(),
        "first"
    );
}

#[test]
fn latest_is_explicit_disclosed_and_rejects_unknown_or_tied_recency() {
    let options = SourceOptions {
        latest: true,
        ..Default::default()
    };
    // An earlier tie must not poison a later unique maximum.
    let selected = selected(
        options
            .resolve(workspace(), SourcePolicy::default(), false, |_, _| {
                Ok(inventory(vec![
                    candidate("a", 1),
                    candidate("b", 1),
                    candidate("c", 2),
                ]))
            })
            .unwrap(),
    );
    assert_eq!(selected.reason(), SelectionReason::LatestRequested);
    assert_eq!(selected.candidate_count(), 3);
    assert_eq!(
        selected
            .expected_identity()
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .as_str(),
        "c"
    );
    for candidates in [
        vec![candidate("a", 2), candidate("b", 2)],
        vec![
            candidate("a", 2),
            SessionCandidate {
                last_activity_unix_ms: None,
                ..candidate("b", 1)
            },
        ],
    ] {
        assert_eq!(
            options
                .resolve(workspace(), SourcePolicy::default(), true, |_, _| Ok(
                    inventory(candidates)
                ))
                .unwrap_err(),
            SourceError::AmbiguousSession
        );
    }
}

#[test]
fn subagents_and_branches_never_collapse_into_one_session() {
    let a = candidate("same-session", 1);
    let mut b = a.clone();
    b.identity.agent = Some(AgentId::new("child".into()).unwrap());
    b.identity.branch = Some(BranchId::new("fork".into()).unwrap());
    assert_eq!(
        SourceOptions::default()
            .resolve(workspace(), SourcePolicy::default(), false, |_, _| Ok(
                inventory(vec![a.clone(), b.clone()])
            ))
            .unwrap_err(),
        SourceError::AmbiguousSession
    );
    let SelectionOutcome::NeedsChoice(choices) = SourceOptions::default()
        .resolve(workspace(), SourcePolicy::default(), true, |_, _| {
            Ok(inventory(vec![a, b.clone()]))
        })
        .unwrap()
    else {
        panic!("explicit branch choice required")
    };
    assert_eq!(
        choices.choose(1).unwrap().expected_identity(),
        Some(&b.identity)
    );
}

#[test]
fn incomplete_duplicate_remote_and_mismatched_inventories_fail_closed() {
    let a = candidate("a", 1);
    let mut remote = a.clone();
    remote.remote = true;
    let mut mismatched = a.clone();
    mismatched.target = SourceTarget::CassSession(path("a"));
    let mut missing_id = a.clone();
    missing_id.identity.session = None;
    let mut unknown_workspace = a.clone();
    unknown_workspace.identity.workspace = None;
    for (sessions, error) in [
        (
            SessionInventory {
                candidates: vec![a.clone()],
                complete: false,
            },
            SourceError::IncompleteInventory,
        ),
        (
            inventory(vec![a.clone(), a.clone()]),
            SourceError::InvalidInventory,
        ),
        (inventory(vec![remote]), SourceError::RemoteSource),
        (inventory(vec![mismatched]), SourceError::InvalidInventory),
        (inventory(vec![missing_id]), SourceError::InvalidInventory),
        (
            inventory(vec![a.clone(), unknown_workspace]),
            SourceError::IncompleteInventory,
        ),
        (inventory(vec![]), SourceError::MissingSession),
        (
            inventory(vec![a.clone(); DISCOVERY_FILES.max() + 1]),
            SourceError::InventoryLimit,
        ),
    ] {
        for latest in [false, true] {
            assert_eq!(
                SourceOptions {
                    latest,
                    ..Default::default()
                }
                .resolve(workspace(), SourcePolicy::default(), true, |_, _| Ok(
                    sessions.clone()
                ))
                .unwrap_err(),
                error
            );
        }
    }
    assert!(
        SourceOptions::default()
            .resolve(workspace(), SourcePolicy::default(), false, |_, _| Ok(
                inventory(vec![a])
            ))
            .is_ok()
    );
}

#[test]
fn cass_source_identity_survives_selection_and_local_policy_reaches_discovery() {
    let mut cass = candidate("archive-session", 1);
    cass.target = SourceTarget::CassSession(path("archive.json"));
    cass.identity.source = SourceProvenance::Cass {
        source: Some(SourceId::new("local-archive".into()).unwrap()),
        version: AdapterVersion::new("0.8.0".into()).unwrap(),
    };
    let selection = selected(
        SourceOptions::default()
            .resolve(workspace(), SourcePolicy::default(), false, |_, _| {
                Ok(inventory(vec![cass.clone()]))
            })
            .unwrap(),
    );
    assert_eq!(selection.expected_identity(), Some(&cass.identity));
    let policy = SourcePolicy {
        offline: true,
        ..Default::default()
    };
    assert_eq!(
        SourceOptions::default()
            .resolve(
                workspace(),
                policy,
                false,
                |bound_workspace, restriction| {
                    assert_eq!(bound_workspace, &workspace());
                    assert!(!restriction.cass_allowed());
                    Ok(inventory(vec![cass]))
                }
            )
            .unwrap_err(),
        SourceError::CassUnavailableInMode
    );
}

fn temp_tree() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "sr-source-selection-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&root).unwrap();
    root // Retained, never removed automatically: repository no-deletion rule.
}

#[test]
fn failed_chosen_source_never_reads_a_successful_neighbor() {
    let root = temp_tree();
    let neighbor = root.join("neighbor.json");
    std::fs::write(&neighbor, b"neighbor conversation").unwrap();
    let missing = root.join("missing.json");
    let calls = Cell::new(0);
    let options = SourceOptions {
        context: Some(path(missing)),
        ..Default::default()
    };
    let failure = explicit(&options).unwrap().read(|selection| {
        calls.set(calls.get() + 1);
        let SourceTarget::NormalizedFile(path) = selection.target() else {
            panic!("wrong reader")
        };
        std::fs::read(path.as_path()).map_err(|error| error.kind())
    });
    assert_eq!(failure, Err(std::io::ErrorKind::NotFound));
    assert_eq!(calls.get(), 1);
    let success = explicit(&SourceOptions {
        context: Some(path(neighbor)),
        ..Default::default()
    })
    .unwrap()
    .read(|selection| {
        let SourceTarget::NormalizedFile(path) = selection.target() else {
            panic!("wrong reader")
        };
        std::fs::read(path.as_path())
    })
    .unwrap();
    assert_eq!(success, b"neighbor conversation");
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .current_dir(root)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "user.name=Source Test",
            "-c",
            "user.email=source@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "temporary Git fixture command failed"
    );
    output.stdout
}

#[test]
fn linked_worktrees_are_distinct_despite_a_shared_git_directory() {
    let root = temp_tree();
    let main = root.join("main");
    let linked = root.join("linked");
    std::fs::create_dir(&main).unwrap();
    git(
        &main,
        &["init", "--quiet", "--template=", "--initial-branch=main"],
    );
    git(
        &main,
        &["commit", "--quiet", "--allow-empty", "-m", "fixture"],
    );
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    let common = |p: &Path| {
        git(
            p,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
    };
    assert_eq!(common(&main), common(&linked));
    // Trusted test resolver keys the exact canonical worktree, not common-dir.
    let id = |p: &Path| {
        WorkspaceId::new(
            blake3::hash(p.canonicalize().unwrap().as_os_str().as_encoded_bytes())
                .to_hex()
                .to_string(),
        )
        .unwrap()
    };
    let main_id = id(&main);
    let linked_id = id(&linked);
    assert_ne!(main_id, linked_id);
    let mut a = candidate("main-session", 1);
    a.identity.workspace = Some(main_id.clone());
    a.target = SourceTarget::ClaudeTranscript(path(main.join("session.jsonl")));
    std::fs::write(main.join("session.jsonl"), b"main-session").unwrap();
    let mut b = candidate("linked-newer-session", 99);
    b.identity.workspace = Some(linked_id.clone());
    b.target = SourceTarget::ClaudeTranscript(path(linked.join("session.jsonl")));
    std::fs::write(linked.join("session.jsonl"), b"linked-newer-session").unwrap();
    let mut prefix = candidate("prefix-collision", 999);
    prefix.identity.workspace =
        Some(WorkspaceId::new(format!("{}-suffix", main_id.as_str())).unwrap());
    let sessions = inventory(vec![a.clone(), b.clone(), prefix]);
    for (workspace, expected) in [(main_id, a), (linked_id, b)] {
        let selection = selected(
            SourceOptions::default()
                .resolve(workspace, SourcePolicy::default(), false, |_, _| {
                    Ok(sessions.clone())
                })
                .unwrap(),
        );
        assert_eq!(selection.expected_identity(), Some(&expected.identity));
        assert_eq!(selection.reason(), SelectionReason::UniqueInWorkspace);
        assert_eq!(selection.candidate_count(), 1);
        let bytes = selection
            .read(|binding| {
                let SourceTarget::ClaudeTranscript(path) = binding.target() else {
                    panic!("wrong source reader")
                };
                std::fs::read(path.as_path())
            })
            .unwrap();
        assert_eq!(
            bytes,
            expected.identity.session.unwrap().as_str().as_bytes()
        );
    }
}

#[test]
fn paths_are_bounded_and_diagnostics_do_not_expose_private_input() {
    for raw in [String::new(), "x".repeat(4097), "private\0path".into()] {
        assert_eq!(
            explicit(&SourceOptions {
                context: Some(path(raw)),
                ..Default::default()
            })
            .unwrap_err(),
            SourceError::InvalidPath
        );
    }
    let canary = "PRIVATE_SOURCE_CANARY";
    let options = SourceOptions {
        context: Some(path(canary)),
        ..Default::default()
    };
    let selection = explicit(&options).unwrap();
    assert!(!format!("{options:?} {selection:?}").contains(canary));
    let unsupported = SourceOptions {
        transcript: Some(path(canary)),
        harness: Some(HarnessId::new(canary.into()).unwrap()),
        ..Default::default()
    };
    let error = explicit(&unsupported).unwrap_err();
    assert!(!format!("{unsupported:?} {error:?} {error} {}", error.hint()).contains(canary));
}
