use skillranker::context::jsonl::{CursorKind, JsonlError, SkipKind, snapshot_jsonl};
use skillranker::context::{EventKind, Role};
use skillranker::runtime::ProcessInvocation;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn temp_path(label: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "sr-jsonl-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        label
    ))
}

fn write_file(path: &PathBuf, body: &str) {
    let mut file = File::create(path).unwrap();
    file.write_all(body.as_bytes()).unwrap();
    file.sync_all().unwrap();
}

fn append_file(path: &PathBuf, body: &str) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(body.as_bytes()).unwrap();
    file.sync_all().unwrap();
}

fn line(id: &str, role: &str, kind: &str, text: &str) -> String {
    format!(
        "{{\"event_id\":\"{id}\",\"role\":\"{role}\",\"kind\":\"{kind}\",\"text\":\"{text}\"}}\n"
    )
}

fn read(
    path: &Path,
    previous: Option<&skillranker::context::jsonl::JsonlCursor>,
    kind: CursorKind,
) -> skillranker::context::jsonl::JsonlSnapshot {
    let invocation = ProcessInvocation::enter().unwrap();
    let cx = invocation.request_cx().unwrap();
    let out = snapshot_jsonl(&invocation, &cx, path, previous, kind).unwrap();
    let _ = invocation.shutdown();
    out
}

#[test]
fn incomplete_tail_is_deferred_and_complete_records_publish() {
    let path = temp_path("tail");
    write_file(
        &path,
        &(line("e1", "user", "message", "hello") + "{\"event_id\":\"e2\""),
    );
    let snap = read(&path, None, CursorKind::Ranking);
    assert_eq!(snap.events.len(), 1);
    assert_eq!(snap.events[0].event_id.as_ref().unwrap().as_str(), "e1");
    assert!(snap.incomplete_tail);
    assert!(!snap.truncated_history);
    fs::remove_file(&path).unwrap();
}

#[test]
fn append_continues_from_the_same_generation() {
    let path = temp_path("append");
    write_file(&path, &line("e1", "user", "message", "one"));
    let first = read(&path, None, CursorKind::Ranking);
    assert_eq!(first.cursor.generation, 1);
    append_file(&path, &line("e2", "assistant", "message", "two"));
    let second = read(&path, Some(&first.cursor), CursorKind::Ranking);
    assert!(!second.rebuilt);
    assert_eq!(second.cursor.generation, 1);
    assert_eq!(second.events.len(), 1);
    assert_eq!(second.events[0].event_id.as_ref().unwrap().as_str(), "e2");
    assert_eq!(second.cursor.last_event_id.as_ref().unwrap().as_str(), "e2");
    fs::remove_file(&path).unwrap();
}

#[test]
fn truncation_rebuilds_instead_of_skipping_unread_bytes() {
    let path = temp_path("trunc");
    write_file(
        &path,
        &(line("e1", "user", "message", "one") + &line("e2", "assistant", "message", "two")),
    );
    let first = read(&path, None, CursorKind::Ranking);
    assert_eq!(first.events.len(), 2);
    write_file(&path, &line("e9", "user", "message", "replaced-short"));
    let rebuilt = read(&path, Some(&first.cursor), CursorKind::Ranking);
    assert!(rebuilt.rebuilt);
    assert!(rebuilt.cursor.generation > first.cursor.generation);
    assert_eq!(rebuilt.events.len(), 1);
    assert_eq!(rebuilt.events[0].event_id.as_ref().unwrap().as_str(), "e9");
    fs::remove_file(&path).unwrap();
}

#[test]
fn replacement_rebuilds_on_new_file_identity() {
    let path = temp_path("replace");
    let other = temp_path("replace-src");
    write_file(&path, &line("e1", "user", "message", "old"));
    let first = read(&path, None, CursorKind::Ranking);
    write_file(&other, &line("e2", "user", "message", "new"));
    fs::rename(&other, &path).unwrap();
    let rebuilt = read(&path, Some(&first.cursor), CursorKind::Ranking);
    assert!(rebuilt.rebuilt);
    assert_ne!(rebuilt.cursor.identity, first.cursor.identity);
    assert_eq!(rebuilt.events[0].event_id.as_ref().unwrap().as_str(), "e2");
    fs::remove_file(&path).unwrap();
}

#[test]
fn oversize_complete_line_is_skipped_neighbors_kept() {
    let path = temp_path("oversize");
    let huge = "x".repeat(skillranker::limits::ONE_TRANSCRIPT_RECORD_BYTES.max() + 1);
    let body = format!(
        "{}{{\"event_id\":\"big\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"{huge}\"}}\n{}",
        line("e1", "user", "message", "before"),
        line("e3", "user", "message", "after")
    );
    write_file(&path, &body);
    let snap = read(&path, None, CursorKind::Ranking);
    assert_eq!(snap.events.len(), 2);
    assert_eq!(snap.events[0].event_id.as_ref().unwrap().as_str(), "e1");
    assert_eq!(snap.events[1].event_id.as_ref().unwrap().as_str(), "e3");
    assert!(snap.skipped.iter().any(|s| s.kind == SkipKind::Oversize));
    fs::remove_file(&path).unwrap();
}

#[test]
fn corrupt_completed_record_is_sanitized_and_does_not_publish_body() {
    let path = temp_path("corrupt");
    write_file(
        &path,
        &(line("e1", "user", "message", "ok")
            + "{not-json}\n"
            + &line("e2", "user", "message", "ok2")),
    );
    let snap = read(&path, None, CursorKind::Ranking);
    assert_eq!(snap.events.len(), 2);
    assert!(snap.skipped.iter().any(|s| s.kind == SkipKind::Corrupt));
    fs::remove_file(&path).unwrap();
}

#[test]
fn duplicate_key_completed_record_is_skipped() {
    let path = temp_path("dup");
    write_file(
        &path,
        "{\"event_id\":\"e1\",\"event_id\":\"e2\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"x\"}\n",
    );
    let snap = read(&path, None, CursorKind::Ranking);
    assert!(snap.events.is_empty());
    assert!(
        snap.skipped
            .iter()
            .any(|s| s.kind == SkipKind::DuplicateKey)
    );
    fs::remove_file(&path).unwrap();
}

#[test]
fn tool_result_without_invocation_in_window_marks_incomplete() {
    let path = temp_path("tool-gap");
    let invocation = "{\"event_id\":\"i1\",\"role\":\"tool\",\"kind\":\"tool_invocation\",\"text\":\"\",\"tool\":{\"name\":\"read\",\"status\":\"attempted\",\"call_id\":\"c1\"}}\n";
    let result = "{\"event_id\":\"r1\",\"role\":\"tool\",\"kind\":\"tool_result\",\"text\":\"\",\"tool\":{\"name\":\"read\",\"status\":\"unknown\",\"call_id\":\"c1\"}}\n";
    write_file(&path, &(invocation.to_string() + result));
    let complete = read(&path, None, CursorKind::Ranking);
    assert!(!complete.missing_tool_counterpart);

    write_file(&path, result);
    let gap = read(&path, None, CursorKind::Ranking);
    assert!(gap.missing_tool_counterpart);
    assert_eq!(gap.events.len(), 1);
    assert_eq!(gap.events[0].kind, EventKind::ToolResult);
    fs::remove_file(&path).unwrap();
}

#[test]
fn tail_window_does_not_fabricate_a_cut_prefix() {
    let path = temp_path("window");
    let filler = "a".repeat(8 * 1024);
    let mut body = line("cut-me", "user", "message", "prefix-outside-cap");
    for i in 0..300 {
        body.push_str(&format!(
            "{{\"event_id\":\"p{i}\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"{filler}\"}}\n"
        ));
    }
    write_file(&path, &body);
    let snap = read(&path, None, CursorKind::Ranking);
    assert!(snap.truncated_history);
    assert!(
        snap.events
            .iter()
            .all(|e| e.event_id.as_ref().map(|id| id.as_str()) != Some("cut-me"))
    );
    assert!(!snap.events.is_empty());
    assert_eq!(snap.events[0].role, Role::User);
    fs::remove_file(&path).unwrap();
}

#[test]
fn symlink_and_directory_are_rejected_before_open() {
    // The property is the rejection reason, not the deadline. Under the default 3 s a
    // starved worker reported Deadline before the path check ran (sr-87gj class).
    let generous = || {
        skillranker::runtime::EntryClock::capture()
            .unwrap()
            .with_total(
                skillranker::limits::DurationMillis::new("test_deadline_ms", 60_000, 600_000)
                    .unwrap(),
            )
            .unwrap()
    };
    let invocation = ProcessInvocation::from_clock(generous()).unwrap();
    let cx = invocation.request_cx().unwrap();
    let dir = temp_path("dir");
    fs::create_dir(&dir).unwrap();
    let err = snapshot_jsonl(&invocation, &cx, &dir, None, CursorKind::Ranking).unwrap_err();
    assert_eq!(err, JsonlError::UnsafePath);
    let _ = invocation.shutdown();

    let target = temp_path("link-target");
    write_file(&target, &line("e1", "user", "message", "x"));
    let link = temp_path("link");
    symlink(&target, &link).unwrap();
    let invocation = ProcessInvocation::from_clock(generous()).unwrap();
    let cx = invocation.request_cx().unwrap();
    let err = snapshot_jsonl(&invocation, &cx, &link, None, CursorKind::Ranking).unwrap_err();
    assert_eq!(err, JsonlError::UnsafePath);
    let _ = invocation.shutdown();
    fs::remove_file(&target).unwrap();
    fs::remove_file(&link).unwrap();
    fs::remove_dir(&dir).unwrap();
}

#[test]
fn observation_cursor_does_not_advance_across_unread_bytes() {
    let path = temp_path("obs");
    let filler = "b".repeat(64 * 1024);
    let mut body = String::new();
    for i in 0..200 {
        body.push_str(&format!(
            "{{\"event_id\":\"o{i}\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"{filler}\"}}\n"
        ));
    }
    write_file(&path, &body);
    let snap = read(&path, None, CursorKind::Observation);
    assert_eq!(snap.events[0].event_id.as_ref().unwrap().as_str(), "o0");
    assert!(snap.unread_backlog);
    assert!(snap.cursor.byte_offset < fs::metadata(&path).unwrap().len());
    fs::remove_file(&path).unwrap();
}

/// A resumed observation read rewinds a bounded overlap behind its cursor and no further
/// (sr-jgez). Records are 64 KiB so the byte overlap, not the record budget, is what binds:
/// the window must re-read the records just behind the watermark (so a tool pair straddling
/// it can be repaired), must not reach back past the overlap, and must still deliver what was
/// appended after it.
#[test]
fn a_resumed_observation_window_rewinds_exactly_the_bounded_overlap() {
    use skillranker::context::jsonl::OBSERVATION_REPAIR_OVERLAP_BYTES;
    let path = temp_path("overlap");
    let filler = "c".repeat(64 * 1024);
    let record = |id: &str| {
        format!(
            "{{\"event_id\":\"{id}\",\"role\":\"user\",\"kind\":\"message\",\"text\":\"{filler}\"}}\n"
        )
    };
    let record_len = record("o00").len() as u64;
    let old: Vec<String> = (0..40).map(|i| format!("o{i:02}")).collect();
    write_file(&path, &old.iter().map(|id| record(id)).collect::<String>());
    let first = read(&path, None, CursorKind::Observation);
    assert_eq!(first.events.len(), 40);
    assert!(!first.unread_backlog);
    let watermark = first.cursor.byte_offset;
    assert_eq!(watermark, fs::metadata(&path).unwrap().len());
    assert!(
        watermark > 2 * OBSERVATION_REPAIR_OVERLAP_BYTES,
        "the file must be large enough for the overlap to bind"
    );

    append_file(&path, &(record("n0") + &record("n1")));
    let second = read(&path, Some(&first.cursor), CursorKind::Observation);
    assert!(!second.rebuilt, "a valid cursor is resumed, not rebuilt");
    let ids: Vec<&str> = second
        .events
        .iter()
        .map(|event| event.event_id.as_ref().unwrap().as_str())
        .collect();
    let reread: Vec<&&str> = ids.iter().filter(|id| id.starts_with('o')).collect();
    assert!(
        ids.contains(&"o39"),
        "the record just behind the watermark is re-read: {ids:?}"
    );
    assert!(
        ids.ends_with(&["n0", "n1"]),
        "appended records still arrive: {ids:?}"
    );
    assert!(
        !ids.contains(&"o00"),
        "nothing beyond the overlap is re-read: {ids:?}"
    );
    let max_reread = OBSERVATION_REPAIR_OVERLAP_BYTES.div_ceil(record_len);
    assert!(
        !reread.is_empty() && reread.len() as u64 <= max_reread,
        "re-read {} old records; the {OBSERVATION_REPAIR_OVERLAP_BYTES}-byte overlap holds at most \
         {max_reread}",
        reread.len()
    );
    fs::remove_file(&path).unwrap();
}
