//! Provider circuit breaker (I03): a best-effort cooldown per endpoint origin,
//! shared by this user's local `sr` processes.
//!
//! Three consecutive transient failures open the circuit for 30 seconds. When
//! the cooldown ends, the next permitted real request becomes the half-open
//! probe; one fenced lease holder owns it, and no background or extra health
//! call is ever made. A failed probe doubles the cooldown up to five minutes;
//! a valid response closes the circuit. A valid longer `Retry-After` refuses
//! attempts until it passes, without making anyone wait for it; the stored
//! value is capped at one hour (`MAX_RETRY_AFTER_MS`), and a valid response
//! clears it.
//!
//! The shared store is created only when a transient failure must be
//! recorded: an origin that has never failed costs one path check per send.
//!
//! Every admission carries the circuit generation it saw. Opening, reopening
//! and closing advance the generation, so a late response admitted under an
//! older generation cannot close a newer open circuit or release a successor's
//! probe lease.
//!
//! This is protection, not enforcement: the store uses ordinary durability,
//! and when it is disabled or unusable the breaker is process-local, which
//! callers report. Authentication failures do not touch it; no safe shared
//! credential identity exists, so they stay invocation-local.

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Inside the private cache directory, beside the response cache.
pub const BREAKER_FILE: &str = "breaker.sqlite3";
pub const OPEN_AFTER_FAILURES: u32 = 3;
pub const BASE_COOLDOWN_MS: u64 = 30_000;
pub const MAX_COOLDOWN_MS: u64 = 300_000;
/// The longest provider `Retry-After` persisted for every process of this
/// origin. A longer value is still honored within the invocation that saw
/// it, but one malformed or hostile header must not cool the endpoint down
/// for days, or in effect forever.
pub const MAX_RETRY_AFTER_MS: u64 = 3_600_000;
const MAX_BREAKER_BYTES: i64 = 1024 * 1024;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// What one attempt was admitted under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ticket {
    pub generation: u64,
    /// `Some` when this attempt is the half-open probe.
    pub probe: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// Open, or a longer `Retry-After` is pending, until this Unix time.
    CoolingDown { until_unix_ms: u64 },
    /// Another process owns the half-open probe.
    ProbeInFlight,
}

/// How one admitted attempt ended, as far as endpoint health is concerned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// A valid provider response.
    Success,
    /// A retryable transport or HTTP failure, with a valid `Retry-After`.
    Transient { retry_after_ms: Option<u64> },
    /// Not evidence about endpoint health: authentication, validation,
    /// cancellation or a local error.
    Neutral,
}

/// Whether the breaker is shared or only protects this process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protection {
    Shared,
    ProcessLocal,
}

impl Protection {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::ProcessLocal => "process-local",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Row {
    generation: u64,
    open: bool,
    failures: u32,
    cooldown_ms: u64,
    open_until_ms: u64,
    probe_token: Option<String>,
    probe_expires_ms: u64,
    retry_after_until_ms: u64,
}

/// The pure transitions, shared by the store and the process-local fallback.
impl Row {
    fn admit(&mut self, now: u64, lease_ms: u64) -> Result<Ticket, Refusal> {
        if now < self.retry_after_until_ms {
            return Err(Refusal::CoolingDown {
                until_unix_ms: self.retry_after_until_ms,
            });
        }
        if !self.open {
            return Ok(Ticket {
                generation: self.generation,
                probe: None,
            });
        }
        if now < self.open_until_ms {
            return Err(Refusal::CoolingDown {
                until_unix_ms: self.open_until_ms,
            });
        }
        if self.probe_token.is_some() && now < self.probe_expires_ms {
            return Err(Refusal::ProbeInFlight);
        }
        let token = format!(
            "{:x}-{:x}-{:x}",
            now,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        self.probe_token = Some(token.clone());
        self.probe_expires_ms = now.saturating_add(lease_ms);
        Ok(Ticket {
            generation: self.generation,
            probe: Some(token),
        })
    }

    fn open_for(&mut self, now: u64, cooldown_ms: u64) {
        self.generation += 1;
        self.open = true;
        self.failures = 0;
        self.cooldown_ms = cooldown_ms;
        self.open_until_ms = now.saturating_add(cooldown_ms);
        self.probe_token = None;
        self.probe_expires_ms = 0;
    }

    fn settle(&mut self, ticket: &Ticket, outcome: Outcome, now: u64) {
        // An attempt admitted under another generation says nothing about the
        // current circuit.
        if ticket.generation != self.generation {
            return;
        }
        let owns_probe = self.open && ticket.probe.is_some() && ticket.probe == self.probe_token;
        match outcome {
            Outcome::Success if owns_probe => {
                self.generation += 1;
                *self = Row {
                    generation: self.generation,
                    ..Row::default()
                };
            }
            Outcome::Success if !self.open => {
                self.failures = 0;
                self.retry_after_until_ms = 0;
            }
            Outcome::Success => {}
            Outcome::Transient { retry_after_ms } => {
                if let Some(delay) = retry_after_ms {
                    let delay = delay.min(MAX_RETRY_AFTER_MS);
                    self.retry_after_until_ms =
                        self.retry_after_until_ms.max(now.saturating_add(delay));
                }
                if owns_probe {
                    let next = (self.cooldown_ms.max(BASE_COOLDOWN_MS) * 2).min(MAX_COOLDOWN_MS);
                    self.open_for(now, next);
                } else if !self.open {
                    self.failures += 1;
                    if self.failures >= OPEN_AFTER_FAILURES {
                        self.open_for(now, BASE_COOLDOWN_MS);
                    }
                }
            }
            Outcome::Neutral if owns_probe => {
                // The probe proved nothing; let the next request probe.
                self.probe_token = None;
                self.probe_expires_ms = 0;
            }
            Outcome::Neutral => {}
        }
    }
}

/// A breaker over one origin, shared through `path` or local to this process.
pub struct Breaker {
    path: Option<PathBuf>,
    origin: String,
    busy: Duration,
    local: std::sync::Mutex<Row>,
    degraded: std::sync::atomic::AtomicBool,
}

impl Breaker {
    /// `path` is `None` when persistent runtime state is disabled.
    pub fn new(path: Option<PathBuf>, origin: &str) -> Self {
        Self {
            path,
            origin: origin.to_owned(),
            busy: Duration::from_millis(25),
            local: std::sync::Mutex::new(Row::default()),
            degraded: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn for_cache_dir(cache_dir: Option<&Path>, origin: &str) -> Self {
        Self::new(cache_dir.map(|dir| dir.join(BREAKER_FILE)), origin)
    }

    /// Bounds each SQLite busy wait, e.g. by the invocation's remaining time.
    pub fn with_busy_wait(mut self, busy: Duration) -> Self {
        self.busy = busy.min(Duration::from_millis(25));
        self
    }

    pub fn protection(&self) -> Protection {
        if self.path.is_some() && !self.degraded.load(Ordering::Relaxed) {
            Protection::Shared
        } else {
            Protection::ProcessLocal
        }
    }

    pub fn admit(&self, now: u64, lease_ms: u64) -> Result<Ticket, Refusal> {
        if let Some(result) = self.shared(false, |row| row.admit(now, lease_ms)) {
            return result;
        }
        self.local.lock().map_or(
            Ok(Ticket {
                generation: 0,
                probe: None,
            }),
            |mut row| row.admit(now, lease_ms),
        )
    }

    pub fn settle(&self, ticket: &Ticket, outcome: Outcome, now: u64) {
        // Only a failure is worth creating the shared store for.
        let create = matches!(outcome, Outcome::Transient { .. });
        if self
            .shared(create, |row| row.settle(ticket, outcome, now))
            .is_none()
            && let Ok(mut row) = self.local.lock()
        {
            row.settle(ticket, outcome, now);
        }
    }

    /// Runs one transition in an immediate transaction on the shared store.
    /// `None` when the store is disabled or unusable: then the process-local
    /// row applies, and the breaker reports itself process-local.
    fn shared<T>(&self, create: bool, change: impl FnOnce(&mut Row) -> T) -> Option<T> {
        let path = self.path.as_ref()?;
        if self.degraded.load(Ordering::Relaxed) {
            return None;
        }
        // No store yet means no failure was ever recorded: nothing to read,
        // and no reason to create one for a success.
        if !create && std::fs::symlink_metadata(path).is_err() {
            return None;
        }
        let result = transact(path, &self.origin, self.busy, change);
        if result.is_none() {
            self.degraded.store(true, Ordering::Relaxed);
        }
        result
    }
}

fn transact<T>(
    path: &Path,
    origin: &str,
    busy: Duration,
    change: impl FnOnce(&mut Row) -> T,
) -> Option<T> {
    let mut connection = open(path, busy).ok()?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .ok()?;
    let mut row = tx
        .query_row(
            "SELECT generation, open, failures, cooldown_ms, open_until_ms, probe_token,
                    probe_expires_ms, retry_after_until_ms
             FROM breaker_state WHERE origin = ?1",
            [origin],
            |r| {
                Ok(Row {
                    generation: r.get::<_, i64>(0)? as u64,
                    open: r.get::<_, i64>(1)? != 0,
                    failures: r.get::<_, i64>(2)? as u32,
                    cooldown_ms: r.get::<_, i64>(3)? as u64,
                    open_until_ms: r.get::<_, i64>(4)? as u64,
                    probe_token: r.get(5)?,
                    probe_expires_ms: r.get::<_, i64>(6)? as u64,
                    retry_after_until_ms: r.get::<_, i64>(7)? as u64,
                })
            },
        )
        .optional()
        .ok()?
        .unwrap_or_default();
    let before = row.clone();
    let value = change(&mut row);
    if row != before {
        tx.execute(
            "INSERT INTO breaker_state (origin, generation, open, failures, cooldown_ms,
                 open_until_ms, probe_token, probe_expires_ms, retry_after_until_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (origin) DO UPDATE SET generation = excluded.generation,
                 open = excluded.open, failures = excluded.failures,
                 cooldown_ms = excluded.cooldown_ms, open_until_ms = excluded.open_until_ms,
                 probe_token = excluded.probe_token, probe_expires_ms = excluded.probe_expires_ms,
                 retry_after_until_ms = excluded.retry_after_until_ms",
            params![
                origin,
                row.generation as i64,
                i64::from(row.open),
                i64::from(row.failures),
                row.cooldown_ms as i64,
                row.open_until_ms as i64,
                row.probe_token,
                row.probe_expires_ms as i64,
                row.retry_after_until_ms as i64
            ],
        )
        .ok()?;
    }
    tx.commit().ok()?;
    Some(value)
}

fn open(path: &Path, busy: Duration) -> rusqlite::Result<Connection> {
    // NOFOLLOW refuses a symlink anywhere in the path, so spell macOS's
    // trusted /tmp and /var aliases as their /private targets first.
    let path = &crate::platform_path::storage_path(path.to_path_buf());
    if let Some(parent) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(parent)
            .map_err(|_| rusqlite::Error::InvalidPath(parent.to_path_buf()))?;
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| rusqlite::Error::InvalidPath(path.to_path_buf()))?;
    }
    connection.busy_timeout(busy)?;
    connection.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    let _: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    let page_size: i64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    connection.pragma_update(None, "max_page_count", MAX_BREAKER_BYTES / page_size.max(1))?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS breaker_state (
             origin TEXT PRIMARY KEY,
             generation INTEGER NOT NULL,
             open INTEGER NOT NULL CHECK(open IN (0, 1)),
             failures INTEGER NOT NULL,
             cooldown_ms INTEGER NOT NULL,
             open_until_ms INTEGER NOT NULL,
             probe_token TEXT,
             probe_expires_ms INTEGER NOT NULL,
             retry_after_until_ms INTEGER NOT NULL
         ) STRICT;",
    )?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAIL: Outcome = Outcome::Transient {
        retry_after_ms: None,
    };
    const LEASE: u64 = 3_000;

    fn temp() -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "sr-breaker-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ))
            .join(BREAKER_FILE)
    }

    fn fail(breaker: &Breaker, now: u64) {
        let ticket = breaker.admit(now, LEASE).unwrap();
        breaker.settle(&ticket, FAIL, now);
    }

    #[test]
    fn three_transient_failures_open_a_thirty_second_cooldown() {
        for breaker in [Breaker::new(Some(temp()), "o"), Breaker::new(None, "o")] {
            fail(&breaker, 0);
            fail(&breaker, 1);
            assert!(
                breaker.admit(2, LEASE).is_ok(),
                "two failures keep it closed"
            );
            fail(&breaker, 2);
            assert_eq!(
                breaker.admit(3, LEASE),
                Err(Refusal::CoolingDown {
                    until_unix_ms: 2 + BASE_COOLDOWN_MS
                })
            );
        }
    }

    #[test]
    fn a_success_resets_the_streak_and_neutral_outcomes_do_not_count() {
        let breaker = Breaker::new(Some(temp()), "o");
        fail(&breaker, 0);
        fail(&breaker, 1);
        let ok = breaker.admit(2, LEASE).unwrap();
        breaker.settle(&ok, Outcome::Success, 2);
        fail(&breaker, 3);
        fail(&breaker, 4);
        for now in 5..10 {
            let ticket = breaker.admit(now, LEASE).unwrap();
            breaker.settle(&ticket, Outcome::Neutral, now);
        }
        assert!(breaker.admit(10, LEASE).is_ok());
    }

    #[test]
    fn one_probe_owner_and_a_doubling_capped_cooldown() {
        let path = temp();
        let a = Breaker::new(Some(path.clone()), "o");
        let b = Breaker::new(Some(path), "o");
        for now in 0..3 {
            fail(&a, now);
        }
        let reopen = 2 + BASE_COOLDOWN_MS;
        let probe = a.admit(reopen, LEASE).unwrap();
        assert!(probe.probe.is_some());
        assert_eq!(b.admit(reopen + 1, LEASE), Err(Refusal::ProbeInFlight));
        a.settle(&probe, FAIL, reopen + 2);
        assert_eq!(
            b.admit(reopen + 3, LEASE),
            Err(Refusal::CoolingDown {
                until_unix_ms: reopen + 2 + 2 * BASE_COOLDOWN_MS
            })
        );
        // Failed probes double until the five-minute cap.
        let mut now = reopen + 2;
        let mut cooldown = 2 * BASE_COOLDOWN_MS;
        for _ in 0..6 {
            now += cooldown;
            let probe = b.admit(now, LEASE).unwrap();
            b.settle(&probe, FAIL, now);
            cooldown = (cooldown * 2).min(MAX_COOLDOWN_MS);
        }
        assert_eq!(
            a.admit(now + 1, LEASE),
            Err(Refusal::CoolingDown {
                until_unix_ms: now + MAX_COOLDOWN_MS
            })
        );
        // A successful probe closes the circuit for everyone.
        let probe = a.admit(now + MAX_COOLDOWN_MS, LEASE).unwrap();
        a.settle(&probe, Outcome::Success, now + MAX_COOLDOWN_MS);
        assert_eq!(
            b.admit(now + MAX_COOLDOWN_MS + 1, LEASE).unwrap().probe,
            None
        );
    }

    #[test]
    fn an_expired_probe_lease_passes_to_the_next_request() {
        let breaker = Breaker::new(Some(temp()), "o");
        for now in 0..3 {
            fail(&breaker, now);
        }
        let at = 2 + BASE_COOLDOWN_MS;
        let stalled = breaker.admit(at, LEASE).unwrap();
        assert_eq!(breaker.admit(at + 1, LEASE), Err(Refusal::ProbeInFlight));
        let successor = breaker.admit(at + LEASE, LEASE).unwrap();
        assert_ne!(successor.probe, stalled.probe);
        // The stalled owner's late failure cannot reopen or release the
        // successor's lease: it no longer owns the probe.
        breaker.settle(&stalled, FAIL, at + LEASE + 1);
        assert_eq!(
            breaker.admit(at + LEASE + 2, LEASE),
            Err(Refusal::ProbeInFlight)
        );
        breaker.settle(&successor, Outcome::Success, at + LEASE + 3);
        assert!(breaker.admit(at + LEASE + 4, LEASE).is_ok());
    }

    #[test]
    fn an_obsolete_success_cannot_close_a_newer_open_circuit() {
        let breaker = Breaker::new(Some(temp()), "o");
        let early = breaker.admit(0, LEASE).unwrap();
        for now in 1..4 {
            fail(&breaker, now);
        }
        breaker.settle(&early, Outcome::Success, 5);
        assert!(matches!(
            breaker.admit(6, LEASE),
            Err(Refusal::CoolingDown { .. })
        ));
    }

    #[test]
    fn a_longer_retry_after_refuses_until_it_passes() {
        let breaker = Breaker::new(Some(temp()), "o");
        let ticket = breaker.admit(0, LEASE).unwrap();
        breaker.settle(
            &ticket,
            Outcome::Transient {
                retry_after_ms: Some(120_000),
            },
            0,
        );
        assert_eq!(
            breaker.admit(1, LEASE),
            Err(Refusal::CoolingDown {
                until_unix_ms: 120_000
            })
        );
        assert!(breaker.admit(120_000, LEASE).is_ok());
    }

    #[test]
    fn a_huge_retry_after_is_capped_at_an_hour() {
        // A day, and `Retry-After: 99999999999` (10^14 ms, which still fits a
        // u64): without a cap the origin would refuse for a day, or in effect
        // forever, across every process.
        for delay in [86_400_000, 100_000_000_000_000, u64::MAX] {
            let breaker = Breaker::new(Some(temp()), "o");
            let ticket = breaker.admit(0, LEASE).unwrap();
            breaker.settle(
                &ticket,
                Outcome::Transient {
                    retry_after_ms: Some(delay),
                },
                0,
            );
            assert_eq!(
                breaker.admit(1, LEASE),
                Err(Refusal::CoolingDown {
                    until_unix_ms: MAX_RETRY_AFTER_MS
                })
            );
            assert!(breaker.admit(MAX_RETRY_AFTER_MS, LEASE).is_ok());
        }
    }

    #[test]
    fn a_valid_response_clears_a_stored_retry_after() {
        let breaker = Breaker::new(Some(temp()), "o");
        // Two attempts in flight; the first is told to retry after a minute,
        // the second then gets a valid answer.
        let first = breaker.admit(0, LEASE).unwrap();
        let second = breaker.admit(0, LEASE).unwrap();
        breaker.settle(
            &first,
            Outcome::Transient {
                retry_after_ms: Some(60_000),
            },
            0,
        );
        assert!(
            breaker.admit(1, LEASE).is_err(),
            "the Retry-After is honored"
        );
        breaker.settle(&second, Outcome::Success, 2);
        assert!(breaker.admit(3, LEASE).is_ok(), "a valid response lifts it");
    }

    #[test]
    fn an_origin_that_never_failed_creates_no_store() {
        let path = temp();
        let breaker = Breaker::new(Some(path.clone()), "o");
        for now in 0..3 {
            let ticket = breaker.admit(now, LEASE).unwrap();
            breaker.settle(&ticket, Outcome::Success, now);
            let ticket = breaker.admit(now, LEASE).unwrap();
            breaker.settle(&ticket, Outcome::Neutral, now);
        }
        assert!(!path.exists(), "a success created the shared store");
        assert_eq!(breaker.protection(), Protection::Shared);
        fail(&breaker, 3);
        assert!(path.exists(), "a transient failure is recorded durably");
    }

    #[test]
    fn an_unusable_store_degrades_to_process_local_protection() {
        let dir = temp();
        std::fs::create_dir_all(&dir).unwrap(); // a directory where the file belongs
        let breaker = Breaker::new(Some(dir), "o");
        assert_eq!(breaker.protection(), Protection::Shared);
        for now in 0..3 {
            fail(&breaker, now);
        }
        assert_eq!(breaker.protection(), Protection::ProcessLocal);
        assert!(matches!(
            breaker.admit(3, LEASE),
            Err(Refusal::CoolingDown { .. })
        ));
    }
}
