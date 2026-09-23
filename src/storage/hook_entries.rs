//! Hook invocations counted at entry, before any input is read (sr-01h3).
//!
//! A hook that ends before it can record a ledger row leaves no trace: its
//! payload never parsed, so it has no turn identity, or it was starved past
//! its own total deadline. Such turns are exactly the ones an availability
//! gate must count. So at entry, before stdin is read, the hook appends one
//! fixed-size record beside the ledger. `sr stats` compares these entries with
//! the hook rows recorded over the same window, and the difference is the
//! measured count of unrecorded turns rather than a declared residual.
//!
//! Invariants:
//! - Written only beside an existing ledger database whose directory is a
//!   real, owner-only directory, and never under `--no-ledger` or
//!   `--no-persist`. Project configuration has no say.
//! - One `O_APPEND` write of a fixed-size record: a Unix time and a random
//!   token. No content, credentials, paths or session identity.
//! - Never blocks or fails the hook: the open is non-blocking, and every
//!   failure is silent.
//! - Bounded: appends stop at [`HOOK_ENTRIES_MAX_BYTES`]. A full counter is
//!   reported as such, so its counts read as lower bounds.
//! - `sr ledger prune` and `sr ledger clear` apply to it, so entries and rows
//!   cover the same history. An append racing a prune can be lost, which
//!   undercounts rather than invents.

use super::StoreError;
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::{Mode, SFlag, fstat};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub const HOOK_ENTRIES_FILE: &str = "hook-entries.log";
/// About 19,000 entries: months of hook traffic for one person.
pub const HOOK_ENTRIES_MAX_BYTES: u64 = 1024 * 1024;
/// `{unix_ms:020} {token:32 hex}\n`.
const RECORD_LEN: usize = 54;
const TOKEN_HEX: usize = 32;

/// Entries counted over a window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HookEntryCount {
    /// Entries whose time falls inside the window.
    pub entries: u64,
    /// The oldest entry kept. Nothing before it was counted.
    pub first_entry_unix_ms: Option<i64>,
    /// Appends have stopped at the size bound.
    pub full: bool,
    /// Records that are not well formed, such as a torn tail.
    pub unreadable: u64,
}

/// Count this hook invocation. Silent on every failure: the counter never
/// changes what the hook does.
pub fn record_hook_entry(dir: &Path, database_file: &str, now_unix_ms: i64) {
    let _ = try_record(dir, database_file, now_unix_ms);
}

fn try_record(dir: &Path, database_file: &str, now_unix_ms: i64) -> Option<()> {
    let uid = nix::unistd::getuid().as_raw();
    let directory = std::fs::symlink_metadata(dir).ok()?;
    if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o077 != 0 {
        return None;
    }
    // Count only where the hook can record rows: beside an existing ledger.
    if !std::fs::symlink_metadata(dir.join(database_file))
        .ok()?
        .is_file()
    {
        return None;
    }
    let fd = open(
        &dir.join(HOOK_ENTRIES_FILE),
        OFlag::O_WRONLY
            | OFlag::O_APPEND
            | OFlag::O_CREAT
            | OFlag::O_NOFOLLOW
            | OFlag::O_NONBLOCK
            | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .ok()?;
    let stat = fstat(&fd).ok()?;
    if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG
        || stat.st_uid != uid
        || stat.st_mode & 0o077 != 0
        || u64::try_from(stat.st_size).ok()? + RECORD_LEN as u64 > HOOK_ENTRIES_MAX_BYTES
    {
        return None;
    }
    let record = format!(
        "{:020} {}\n",
        now_unix_ms.max(0),
        super::ledger::generate_random_hex(TOKEN_HEX / 2)
    );
    debug_assert_eq!(record.len(), RECORD_LEN);
    // One write of fewer bytes than PIPE_BUF: appended whole or not at all.
    File::from(fd).write_all(record.as_bytes()).ok()
}

/// Count entries with `since_unix_ms <= time <= until_unix_ms`. A missing
/// counter is an empty one.
pub fn count_hook_entries(
    dir: &Path,
    since_unix_ms: i64,
    until_unix_ms: i64,
) -> Result<HookEntryCount, StoreError> {
    let Some(bytes) = read_entries(dir)? else {
        return Ok(HookEntryCount::default());
    };
    let mut count = HookEntryCount {
        full: bytes.len() as u64 + RECORD_LEN as u64 > HOOK_ENTRIES_MAX_BYTES,
        ..HookEntryCount::default()
    };
    for record in bytes.chunks(RECORD_LEN) {
        match parse_record(record) {
            Some(at) => {
                count.first_entry_unix_ms =
                    Some(count.first_entry_unix_ms.map_or(at, |first| first.min(at)));
                if (since_unix_ms..=until_unix_ms).contains(&at) {
                    count.entries += 1;
                }
            }
            None => count.unreadable += 1,
        }
    }
    Ok(count)
}

/// Drop entries older than `cutoff_unix_ms`, as `sr ledger prune` drops rows.
/// Returns how many were removed.
pub fn prune_hook_entries(dir: &Path, cutoff_unix_ms: i64) -> Result<u64, StoreError> {
    let Some(bytes) = read_entries(dir)? else {
        return Ok(0);
    };
    let mut kept = Vec::with_capacity(bytes.len());
    let mut removed = 0;
    for record in bytes.chunks(RECORD_LEN) {
        match parse_record(record) {
            Some(at) if at < cutoff_unix_ms => removed += 1,
            // Unreadable records stay: pruning deletes by age, not by doubt.
            _ => kept.extend_from_slice(record),
        }
    }
    if removed > 0 {
        replace_entries(dir, &kept)?;
    }
    Ok(removed)
}

/// Remove every entry, as `sr ledger clear` removes history. Returns how many
/// records there were.
pub fn clear_hook_entries(dir: &Path) -> Result<u64, StoreError> {
    let Some(bytes) = read_entries(dir)? else {
        return Ok(0);
    };
    let records = bytes.len().div_ceil(RECORD_LEN) as u64;
    if records > 0 {
        replace_entries(dir, &[])?;
    }
    Ok(records)
}

fn parse_record(record: &[u8]) -> Option<i64> {
    if record.len() != RECORD_LEN || record[20] != b' ' || record[RECORD_LEN - 1] != b'\n' {
        return None;
    }
    if !record[21..RECORD_LEN - 1]
        .iter()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    std::str::from_utf8(&record[..20]).ok()?.parse().ok()
}

fn read_entries(dir: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    let fd = match open(
        &dir.join(HOOK_ENTRIES_FILE),
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return Ok(None),
        Err(Errno::ELOOP) => return Err(StoreError::UnsafePath),
        Err(_) => return Err(StoreError::Io),
    };
    let stat = fstat(&fd).map_err(|_| StoreError::Io)?;
    if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
        return Err(StoreError::UnsafePath);
    }
    let mut bytes = Vec::new();
    // Appends stop at the bound; a racing writer adds at most one record more.
    File::from(fd)
        .take(HOOK_ENTRIES_MAX_BYTES + RECORD_LEN as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| StoreError::Io)?;
    Ok(Some(bytes))
}

/// Replace the counter atomically with `bytes`, owner-only.
fn replace_entries(dir: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let staged = dir.join(format!(
        "{HOOK_ENTRIES_FILE}.{}.tmp",
        super::ledger::generate_random_hex(8)
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(OFlag::O_NOFOLLOW.bits())
            .open(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&staged, dir.join(HOOK_ENTRIES_FILE))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
        return Err(StoreError::Io);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const LEDGER: &str = "ledger.sqlite3";

    /// An owner-only directory holding a stand-in ledger file.
    fn ledger_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sr-hook-entries-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        std::fs::write(dir.join(LEDGER), b"").unwrap();
        dir
    }

    fn file_len(dir: &Path) -> u64 {
        std::fs::metadata(dir.join(HOOK_ENTRIES_FILE)).map_or(0, |m| m.len())
    }

    #[test]
    fn entries_are_fixed_size_owner_only_and_counted_by_window() {
        let dir = ledger_dir();
        for at in [1_000, 2_000, 3_000] {
            record_hook_entry(&dir, LEDGER, at);
        }
        assert_eq!(file_len(&dir), 3 * RECORD_LEN as u64);
        let mode = std::fs::metadata(dir.join(HOOK_ENTRIES_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let count = count_hook_entries(&dir, 1_500, 3_000).unwrap();
        assert_eq!(count.entries, 2);
        assert_eq!(count.first_entry_unix_ms, Some(1_000));
        assert!(!count.full);
        assert_eq!(count.unreadable, 0);
        // A missing counter is an empty one, with no first entry.
        let empty = ledger_dir();
        assert_eq!(
            count_hook_entries(&empty, 0, i64::MAX).unwrap(),
            HookEntryCount::default()
        );
    }

    #[test]
    fn nothing_is_written_where_the_hook_could_not_record_rows() {
        // No ledger database beside it.
        let dir = ledger_dir();
        std::fs::remove_file(dir.join(LEDGER)).unwrap();
        record_hook_entry(&dir, LEDGER, 1_000);
        assert_eq!(file_len(&dir), 0);
        // A directory that others can write to.
        let shared = ledger_dir();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
        record_hook_entry(&shared, LEDGER, 1_000);
        assert_eq!(file_len(&shared), 0);
        // A counter that is a symlink is neither followed nor read.
        let linked = ledger_dir();
        let target = linked.join("elsewhere");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, linked.join(HOOK_ENTRIES_FILE)).unwrap();
        record_hook_entry(&linked, LEDGER, 1_000);
        assert_eq!(std::fs::metadata(&target).unwrap().len(), 0);
        assert!(count_hook_entries(&linked, 0, i64::MAX).is_err());
    }

    #[test]
    fn a_full_counter_stops_appending_and_says_its_counts_are_lower_bounds() {
        let dir = ledger_dir();
        let record = format!("{:020} {}\n", 5_000, "a".repeat(TOKEN_HEX));
        let records = (HOOK_ENTRIES_MAX_BYTES as usize) / RECORD_LEN;
        std::fs::write(dir.join(HOOK_ENTRIES_FILE), record.repeat(records)).unwrap();
        std::fs::set_permissions(
            dir.join(HOOK_ENTRIES_FILE),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let before = file_len(&dir);
        record_hook_entry(&dir, LEDGER, 6_000);
        assert_eq!(file_len(&dir), before, "a full counter refuses the append");
        let count = count_hook_entries(&dir, 0, i64::MAX).unwrap();
        assert!(count.full);
        assert_eq!(count.entries, records as u64);
    }

    #[test]
    fn prune_and_clear_follow_the_ledger() {
        let dir = ledger_dir();
        for at in [1_000, 2_000, 3_000] {
            record_hook_entry(&dir, LEDGER, at);
        }
        // A torn record is unreadable, never a counted entry, and pruning by
        // age keeps it.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(HOOK_ENTRIES_FILE))
            .unwrap();
        file.write_all(b"torn").unwrap();
        assert_eq!(count_hook_entries(&dir, 0, i64::MAX).unwrap().unreadable, 1);
        assert_eq!(prune_hook_entries(&dir, 2_500).unwrap(), 2);
        let count = count_hook_entries(&dir, 0, i64::MAX).unwrap();
        assert_eq!((count.entries, count.unreadable), (1, 1));
        assert_eq!(count.first_entry_unix_ms, Some(3_000));
        let mode = std::fs::metadata(dir.join(HOOK_ENTRIES_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the replacement stays owner-only");
        assert_eq!(clear_hook_entries(&dir).unwrap(), 2);
        assert_eq!(
            count_hook_entries(&dir, 0, i64::MAX).unwrap(),
            HookEntryCount::default()
        );
        // Appends resume after a clear.
        record_hook_entry(&dir, LEDGER, 4_000);
        assert_eq!(count_hook_entries(&dir, 0, i64::MAX).unwrap().entries, 1);
    }
}
