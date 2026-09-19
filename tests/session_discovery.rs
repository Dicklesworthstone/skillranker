#![cfg(unix)]

use skillranker::context::discovery::{HEAD_BYTES, attribution, discover_claude_sessions};
use skillranker::identity::WorkspaceId;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn tree() -> (PathBuf, PathBuf, PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "sr-session-discovery-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = std::fs::canonicalize(workspace).unwrap();
    let projects = root.join("projects");
    let directory = projects.join(workspace.to_str().unwrap().replace('/', "-"));
    std::fs::create_dir_all(&directory).unwrap();
    (workspace, projects, directory)
}

fn record(workspace: &std::path::Path, session: &str) -> String {
    serde_json::json!({"cwd": workspace, "sessionId": session}).to_string() + "\n"
}

#[test]
fn unresolved_neighbor_prevents_a_false_unique_inventory() {
    for unresolved in ["not json\n".to_owned(), "{}\n".to_owned(), "{".to_owned()] {
        let (workspace, projects, directory) = tree();
        std::fs::write(directory.join("known.jsonl"), record(&workspace, "known")).unwrap();
        std::fs::write(directory.join("unknown.jsonl"), unresolved).unwrap();
        let inventory = discover_claude_sessions(
            &[projects],
            &workspace,
            &WorkspaceId::new(workspace.to_str().unwrap()).unwrap(),
        );
        assert_eq!(
            inventory.candidates.len(),
            1,
            "retain the verified candidate"
        );
        assert!(!inventory.complete, "unresolved is not proven absent");
    }
}

#[test]
fn a_head_bound_cannot_hide_another_session() {
    let (workspace, projects, directory) = tree();
    std::fs::write(directory.join("known.jsonl"), record(&workspace, "known")).unwrap();
    let delayed = " ".repeat(HEAD_BYTES) + &record(&workspace, "delayed");
    std::fs::write(directory.join("delayed.jsonl"), delayed).unwrap();
    let inventory = discover_claude_sessions(
        &[projects],
        &workspace,
        &WorkspaceId::new(workspace.to_str().unwrap()).unwrap(),
    );
    assert_eq!(inventory.candidates.len(), 1);
    assert!(!inventory.complete);
}

#[test]
fn verified_foreign_session_does_not_prevent_unique_discovery() {
    let (workspace, projects, directory) = tree();
    std::fs::write(directory.join("known.jsonl"), record(&workspace, "known")).unwrap();
    std::fs::write(
        directory.join("foreign.jsonl"),
        record(std::path::Path::new("/another/workspace"), "foreign"),
    )
    .unwrap();
    let inventory = discover_claude_sessions(
        &[projects.clone(), projects],
        &workspace,
        &WorkspaceId::new(workspace.to_str().unwrap()).unwrap(),
    );
    assert!(inventory.complete);
    assert_eq!(inventory.candidates.len(), 1);
    assert_eq!(
        inventory.candidates[0]
            .identity
            .session
            .as_ref()
            .unwrap()
            .as_str(),
        "known"
    );
}

#[test]
fn duplicate_identity_keys_cannot_attribute_a_transcript() {
    let duplicate = b"{\"cwd\":\"/foreign\",\"cwd\":\"/workspace\",\"sessionId\":\"s\"}\n";
    assert!(attribution(duplicate, "s", "/workspace").is_none());
    let duplicate = b"{\"cwd\":\"/workspace\",\"sessionId\":\"other\",\"sessionId\":\"s\"}\n";
    assert!(attribution(duplicate, "s", "/workspace").is_none());
    assert!(
        attribution(
            b"{\"cwd\":\"/workspace\",\"sessionId\":\"s\"}\n",
            "s",
            "/workspace"
        )
        .is_some()
    );
}

#[test]
fn conflicting_identity_later_in_the_head_invalidates_attribution() {
    let head = b"{\"cwd\":\"/workspace\",\"sessionId\":\"s\"}\n{\"sessionId\":\"other\"}\n";
    assert!(attribution(head, "s", "/workspace").is_none());
    let consistent = b"{\"cwd\":\"/workspace\",\"sessionId\":\"s\"}\n{\"cwd\":\"/later/cd\",\"sessionId\":\"s\"}\n";
    assert!(attribution(consistent, "s", "/workspace").is_some());
}
