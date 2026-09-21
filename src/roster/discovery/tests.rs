use super::*;
use std::cell::Cell;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

fn tree() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-discovery-budget-{}-{}-{}",
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

fn spec(name: &str) -> RootSpec {
    RootSpec::new(
        SourceId::new(name).unwrap(),
        SourceKind::Generic,
        0,
        Visibility::Unverified,
        CLAUDE_SKILL_FILE,
    )
    .unwrap()
}

fn root_files(sizes: &[usize]) -> DiscoveryPlan {
    let mut plan = DiscoveryPlan::new(HarnessId::new("claude_code").unwrap());
    for (index, size) in sizes.iter().enumerate() {
        let path = tree();
        fs::write(path.join(CLAUDE_SKILL_FILE), vec![b'x'; *size]).unwrap();
        plan.push_root(
            PlannedRoot::open(spec(&format!("fixture-{index}")), &path)
                .unwrap()
                .unwrap(),
        );
    }
    plan
}

fn limits(entries: usize, bytes: usize) -> DiscoveryLimits {
    use crate::limits::LimitUnit;
    DiscoveryLimits::new(
        ResourceLimit::try_new("entries", LimitUnit::Records, entries).unwrap(),
        ResourceLimit::try_new("bytes", LimitUnit::Bytes, bytes).unwrap(),
        MAX_ROOT_DEPTH,
    )
}

#[test]
fn entry_exhaustion_stops_the_whole_plan_after_one_overflow_sentinel() {
    let plan = root_files(&[1; 32]);
    let discovery = plan.discover_with(limits(1, 100));
    assert_eq!(discovery.candidates().len(), 1);
    assert_eq!(discovery.entries_examined(), 2);
    assert_eq!(discovery.diagnostics(), &[Diagnostic::EntryLimitReached]);
    assert!(discovery.is_partial());
}

#[test]
fn byte_exhaustion_cannot_resume_with_smaller_files_in_later_roots() {
    let plan = root_files(&[1, 10, 1]);
    let discovery = plan.discover_with(limits(100, 2));
    assert_eq!(discovery.candidates().len(), 1);
    assert_eq!(discovery.candidates()[0].source().as_str(), "fixture-0");
    assert_eq!(discovery.entries_examined(), 2);
    assert_eq!(discovery.bytes_examined(), 1);
    assert_eq!(discovery.diagnostics(), &[Diagnostic::ByteLimitReached]);
    assert!(discovery.is_partial());
}

#[test]
fn reaching_an_exact_bound_is_not_itself_incomplete() {
    let discovery = root_files(&[4]).discover_with(limits(1, 4));
    assert_eq!(discovery.candidates().len(), 1);
    assert_eq!(discovery.entries_examined(), 1);
    assert_eq!(discovery.bytes_examined(), 4);
    assert!(!discovery.is_partial());
    assert!(discovery.diagnostics().is_empty());
}

#[test]
fn checkpoint_errors_abort_inside_a_walk_and_preserve_the_callers_error() {
    let path = tree();
    for index in 0..100 {
        let directory = path.join(format!("skill-{index:03}"));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join(CLAUDE_SKILL_FILE), "body").unwrap();
    }
    let mut plan = DiscoveryPlan::new(HarnessId::new("claude_code").unwrap());
    plan.push_root(PlannedRoot::open(spec("fixture"), &path).unwrap().unwrap());
    let mut calls = 0;
    let error = plan
        .discover_with_checkpoint(DiscoveryLimits::defaults(), || {
            calls += 1;
            if calls == 40 {
                Err("cancelled")
            } else {
                Ok(())
            }
        })
        .unwrap_err();
    assert_eq!(error, "cancelled");
    assert_eq!(calls, 40, "no further checkpoint or scan after refusal");

    // A new pass is not poisoned by an interrupted directory stream.
    let mut successful_calls = 0;
    let complete = plan
        .discover_with_checkpoint(DiscoveryLimits::defaults(), || {
            successful_calls += 1;
            Ok::<(), &str>(())
        })
        .unwrap();
    assert_eq!(complete.candidates().len(), 100);
    assert!(!complete.is_partial());
    assert!(successful_calls > calls);
}

#[test]
fn even_an_empty_pass_checks_admission_and_completion() {
    let plan = DiscoveryPlan::new(HarnessId::new("claude_code").unwrap());
    assert_eq!(
        plan.discover_with_checkpoint(DiscoveryLimits::defaults(), || Err("deadline"))
            .unwrap_err(),
        "deadline"
    );
    let calls = Cell::new(0);
    plan.discover_with_checkpoint(DiscoveryLimits::defaults(), || {
        calls.set(calls.get() + 1);
        Ok::<(), &str>(())
    })
    .unwrap();
    let completed = calls.get();
    assert!(completed >= 2);
    calls.set(0);
    assert_eq!(
        plan.discover_with_checkpoint(DiscoveryLimits::defaults(), || {
            calls.set(calls.get() + 1);
            if calls.get() == completed {
                Err("deadline")
            } else {
                Ok(())
            }
        })
        .unwrap_err(),
        "deadline"
    );
}

#[test]
fn a_nonadvancing_failed_directory_stream_is_not_retried() {
    let mut discovery = Discovery::default();
    let source = SourceId::new("fixture").unwrap();
    let calls = Cell::new(0);
    let mut broken = std::iter::repeat_with(|| {
        calls.set(calls.get() + 1);
        Err::<(), _>(Errno::EIO)
    });
    assert!(discovery.next_entry(broken.next(), &source).is_none());
    assert_eq!(calls.get(), 1);
    assert_eq!(
        discovery.diagnostics(),
        &[Diagnostic::EntryUnreadable(source)]
    );
    assert!(discovery.is_partial());
}

#[test]
fn failed_stream_preserves_preceding_entries_and_leaves_following_entries_unread() {
    let mut discovery = Discovery::default();
    let source = SourceId::new("fixture").unwrap();
    let mut iterator = [Ok(1), Err(Errno::EIO), Ok(2)].into_iter();
    let mut found = Vec::new();
    while let Some(entry) = discovery.next_entry(iterator.next(), &source) {
        found.push(entry);
    }
    assert_eq!(found, vec![1]);
    assert_eq!(iterator.next(), Some(Ok(2)));
    assert_eq!(discovery.diagnostics().len(), 1);
}

#[test]
fn candidate_metadata_uses_the_open_directory_not_a_replaced_parent_path() {
    let path = tree();
    let outside = tree();
    fs::create_dir(path.join("alpha")).unwrap();
    fs::write(path.join("alpha/SKILL.md"), "original").unwrap();
    fs::write(outside.join(CLAUDE_SKILL_FILE), "outside-canary").unwrap();
    let expected = fs::metadata(path.join("alpha/SKILL.md")).unwrap();
    let planned = PlannedRoot::open(spec("fixture"), &path).unwrap().unwrap();
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let directory = openat(
        planned.root().unwrap().as_fd(),
        "alpha",
        flags,
        Mode::empty(),
    )
    .unwrap();
    fs::rename(path.join("alpha"), path.join("retained-alpha")).unwrap();
    symlink(&outside, path.join("alpha")).unwrap();

    let mut discovery = Discovery::default();
    discovery.push_candidate(
        &planned,
        directory.as_fd(),
        Path::new("alpha"),
        OsStr::new(CLAUDE_SKILL_FILE),
        false,
        DiscoveryLimits::defaults(),
    );
    assert_eq!(discovery.candidates().len(), 1);
    let candidate = &discovery.candidates()[0];
    assert_eq!(
        candidate.identity(),
        FileIdentity::new(expected.dev(), expected.ino())
    );
    assert_eq!(candidate.size(), expected.len());
    // Metadata discovery grants no new read authority to the replacement link.
    let roots = crate::authorized_read::AuthorizedRoots::single(
        planned.root().unwrap().try_clone().unwrap(),
    );
    assert!(
        roots
            .read_bounded(0, candidate.relative(), crate::limits::SKILL_FILE_BYTES)
            .is_err()
    );
}

#[test]
fn shallower_skills_in_every_root_precede_another_roots_nested_support_files() {
    let project = tree();
    let personal = tree();
    let configured = tree();
    for root in [&project, &personal, &configured] {
        fs::create_dir(root.join("alpha")).unwrap();
        fs::write(root.join("alpha/SKILL.md"), "body").unwrap();
    }
    let references = project.join("alpha/references");
    fs::create_dir(&references).unwrap();
    for index in 0..100 {
        fs::write(references.join(format!("note-{index}")), "note").unwrap();
    }
    // 3 root entries + 3 SKILL.md entries + 1 references directory.
    // The next entry is the sole overflow sentinel, not another skill.
    for order in [[0, 1, 2], [2, 1, 0]] {
        let roots = [&project, &personal, &configured];
        let mut plan = DiscoveryPlan::new(HarnessId::new("claude_code").unwrap());
        for index in order {
            plan.push_root(
                PlannedRoot::open(spec(&format!("root-{index}")), roots[index])
                    .unwrap()
                    .unwrap(),
            );
        }
        let discovery = plan.discover_with(limits(7, 100));
        assert_eq!(discovery.candidates().len(), 3);
        assert_eq!(discovery.entries_examined(), 8);
        assert_eq!(discovery.diagnostics(), &[Diagnostic::EntryLimitReached]);
        let sources: Vec<_> = discovery
            .candidates()
            .iter()
            .map(|c| c.source().as_str())
            .collect();
        assert_eq!(sources, vec!["root-0", "root-1", "root-2"]);
        assert_eq!(
            discovery.candidates(),
            plan.discover_with(limits(7, 100)).candidates(),
            "fresh streams preserve the same bounded snapshot"
        );
    }
}
