#![cfg(unix)]
use skillranker::context::signals::{DIRTY_PATH_LIMIT, Omission, collect, parse_dirty_paths};
use skillranker::runtime::ProcessInvocation;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn temp() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "sr-signals-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&p).unwrap();
    p
}
fn git_path() -> PathBuf {
    Path::new("/usr/bin/git").canonicalize().unwrap()
}
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new(git_path())
        .current_dir(dir)
        .env_clear()
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(status.status.success(), "fixture git failed");
}
fn collect_at(dir: &Path, roots: &[PathBuf]) -> skillranker::context::signals::ProjectSignals {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let result = invocation
        .runtime()
        .block_on(collect(&cx, &invocation.clock(), dir, roots));
    let _ = invocation.shutdown();
    result
}
#[test]
fn byte_parser_preserves_identity_and_reports_partial_coverage() {
    let result =
        parse_dirty_paths(b" M src/a b.rs\0 M bad\xff\0 M ../escape\0 M control\x1b\0").unwrap();
    assert_eq!(result.paths[0].as_str(), "src/a b.rs");
    assert_eq!(result.paths.len(), 1);
    assert_eq!(result.omitted_non_utf8, 1);
    assert_eq!(result.omitted_unsafe, 2);
    assert!(parse_dirty_paths(b" M partial").is_err());
    assert!(parse_dirty_paths(b" M x\0\0").is_err());
    assert!(parse_dirty_paths(b"R  dst\0src\0").is_err());
    assert!(parse_dirty_paths(b" M x\0 M x\0").is_err());
    let many = (0..101)
        .map(|n| format!(" M file-{n}\0"))
        .collect::<String>();
    let result = parse_dirty_paths(many.as_bytes()).unwrap();
    assert_eq!(result.paths.len(), DIRTY_PATH_LIMIT);
    assert!(result.truncated);
    assert!(parse_dirty_paths(b"").unwrap().paths.is_empty());
}
#[test]
fn actual_git_ignores_fsmonitor_and_handles_detached_worktrees() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp();
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "synthetic@example.invalid"]);
    git(&dir, &["config", "user.name", "Synthetic"]);
    std::fs::write(dir.join("Cargo.toml"), "synthetic fixture\n").unwrap();
    git(&dir, &["add", "Cargo.toml"]);
    git(&dir, &["commit", "-qm", "fixture"]);
    let script = dir.join("monitor");
    std::fs::write(&script, "#!/bin/sh\nprintf invoked > monitor-ran\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(
        &dir,
        &["config", "core.fsmonitor", script.to_str().unwrap()],
    );
    std::fs::write(dir.join("Cargo.toml"), "changed\n").unwrap();
    let roots = [git_path().parent().unwrap().to_path_buf()];
    let result = collect_at(&dir, &roots);
    assert_eq!(result.git_omission, None);
    assert_eq!(result.dirty_paths.unwrap().paths[0].as_str(), "Cargo.toml");
    assert!(result.filenames.contains(&"Cargo.toml"));
    assert!(result.tools_on_path.contains(&"git"));
    assert!(!dir.join("monitor-ran").exists());
    // All fixture setup commands explicitly disable the configured helper too.
    git(
        &dir,
        &["-c", "core.fsmonitor=false", "checkout", "--detach", "-q"],
    );
    let result = collect_at(&dir, &roots);
    assert_eq!(result.git_omission, None);
    let worktree_parent = temp();
    let worktree = worktree_parent.join("linked");
    git(
        &dir,
        &[
            "-c",
            "core.fsmonitor=false",
            "worktree",
            "add",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(worktree.join("Cargo.toml"), "linked changed\n").unwrap();
    let result = collect_at(&worktree, &roots);
    assert_eq!(result.git_omission, None);
    assert_eq!(result.dirty_paths.unwrap().paths[0].as_str(), "Cargo.toml");
    assert_eq!(
        collect_at(&temp(), &roots).git_omission,
        Some(Omission::Unavailable)
    );
    assert!(!dir.join("monitor-ran").exists());
}
#[test]
fn unsupported_git_never_runs_status_or_discovered_tools() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp();
    let tools = temp();
    let fake = tools.join("git");
    let marker = tools.join("unexpected-execution");
    std::fs::write(&fake, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then printf 'git version 2.35.0\\n'; else printf ran > '{}'; exit 99; fi\n", marker.display())).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let cargo = tools.join("cargo");
    std::fs::write(
        &cargo,
        format!("#!/bin/sh\nprintf ran > '{}'\nexit 98\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = collect_at(&dir, &[tools]);
    assert_eq!(result.git_omission, Some(Omission::UnsupportedGit));
    assert!(result.tools_on_path.contains(&"cargo"));
    assert!(result.dirty_paths.is_none());
    assert!(!marker.exists());
}

#[test]
fn actual_git_omits_non_utf8_paths_without_lossy_identity() {
    use std::os::unix::ffi::OsStringExt;
    let dir = temp();
    git(&dir, &["init", "-q"]);
    let name = std::ffi::OsString::from_vec(b"bad\xff".to_vec());
    std::fs::write(dir.join(name), "fixture").unwrap();
    git(&dir, &["add", "--all"]);
    let result = collect_at(&dir, &[git_path().parent().unwrap().to_path_buf()]);
    assert_eq!(result.git_omission, None);
    let paths = result.dirty_paths.unwrap();
    assert_eq!(paths.omitted_non_utf8, 1);
    assert!(paths.paths.is_empty());
}

#[test]
fn symlink_to_workspace_does_not_grant_tool_authority() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = temp();
    let tools = temp();
    let fake = dir.join("git");
    std::fs::write(&fake, "#!/bin/sh\nexit 99\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&fake, tools.join("git")).unwrap();
    let result = collect_at(&dir, &[tools]);
    assert_eq!(result.git_omission, Some(Omission::Unavailable));
    assert!(!result.tools_on_path.contains(&"git"));
    assert!(result.dirty_paths.is_none());
}

#[test]
fn slow_version_probe_is_omitted_within_stage_budget() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp();
    let tools = temp();
    let fake = tools.join("git");
    std::fs::write(&fake, "#!/bin/sh\n/bin/sleep 10\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let start = std::time::Instant::now();
    let result = collect_at(&dir, &[tools]);
    assert_eq!(result.git_omission, Some(Omission::Budget));
    assert!(result.dirty_paths.is_none());
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}
