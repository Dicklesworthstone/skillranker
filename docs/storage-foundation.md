# Qualified cache storage foundation

`skillranker::storage` is a Linux library boundary for opening an explicitly
selected disposable cache and fencing generation changes. It is not yet wired
to ranking or a storage CLI. It does not store provider responses, initialize an
observation ledger, create an allowance store, or create a fingerprint key.
Those operations belong to subsequent roadmap tasks.

## Entry points and effects

`open_cache(&ProcessInvocation, &Cx, CacheAccess, CacheLocation)` returns
`CacheOpen::{Disabled, Missing, Ready}` or a sanitized `StoreError`.

| Access | Behavior |
| --- | --- |
| `Disabled` | Returns before resolving environment/platform paths, inspecting the engine, submitting blocking work, or touching files. Use for no-cache, no-persist and dry-run callers. |
| `ExistingOnly` | Does not create missing directories or the main file. Missing state is a typed miss; an empty existing file is uninitialized. SQLite can create/update WAL shared-memory bookkeeping for an existing store. |
| `Initialize` | Explicitly permits creating the private cache directory/main file and current cache schema. Repeated initialization preserves the incarnation and generation. |

`CacheLocation::Platform` uses `directories::BaseDirs::cache_dir()/sr`
(`$XDG_CACHE_HOME/sr`, falling back to `~/.cache/sr` on Linux).
`Directory(PathBuf)` is a trusted-host override, never a path taken from a
transcript, skill, project-controlled disclosure setting, or provider answer.
Relative paths, parent traversal and paths over 4,096 bytes/128 components are
refused. Debug representations omit paths and store incarnation identifiers.

`CacheStore::advance_generation` consumes the connection, moves it through the
invocation's owned blocking pool and returns it on success. It checks the exact
schema, store incarnation and expected generation inside `BEGIN IMMEDIATE`
before changing the singleton metadata row. A stale writer cannot restore an
old generation. Exhaustion fails without wrapping. There is no public raw SQL
or unfenced mutation escape hatch. Future response records must bind to the
same stamp; this method alone does not implement cache clear or eviction.

## Engine and schema qualification

The dependency is pinned to `rusqlite =0.40.2`, with defaults disabled and
`bundled`, `hooks`, and `limits` enabled. The locked bundle is
`libsqlite3-sys 0.38.2`, SQLite **3.53.2**, source ID:

```text
2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24
```

Before any disk store is opened or created, an in-memory connection reads the
actual linked version and source ID. Both must match this qualified identity
and the version must satisfy the project's 3.51.3 minimum. A different bundle
fails closed until its source and tests are reviewed; a Cargo version string
alone is not engine evidence. `linked_engine()` exposes only this public engine
identity for future doctor integration. See [SQLite's WAL documentation](https://www.sqlite.org/wal.html).

Schema version 1 uses application ID `SRCH` and one strict metadata table with
an opaque 16-byte random incarnation, nonnegative generation, and a fixed schema
identity. The stored DDL and singleton cardinality are checked, so a matching
`user_version` alone cannot authorize mutation. A newer schema is inspected with
a read-only SQLite connection and rejected. This is not a promise of zero WAL
shared-memory bookkeeping. Corrupt, foreign, or incompatible stores are refused
without replacement, downgrade, permission repair, or automatic migration.
No migration, backup, prune, or repair command is implemented here.

## Filesystem boundary

Directory traversal uses descriptor-relative, no-follow opens. Existing ancestors
must be owned by the current effective user or root and must not be writable by
other users; root-owned sticky directories such as `/tmp` are allowed. The final
directory must belong to the effective user with mode `0700`. Main, WAL, SHM and
rollback-journal files must be regular, singly linked, owned by that user and
mode `0600`. Unsafe symlinks, hardlinks, FIFOs and unexpected types are refused.
New directories/files are created with private modes; unsafe existing modes are
not silently changed. Restrictive umasks may cause a safe refusal.

Linux ext4, Btrfs, XFS and tmpfs filesystem identifiers are admitted; network,
FUSE and unknown filesystems are refused. The type is checked for a new
directory's parent before creation and for the final store. This whitelist is a
conservative admission policy, not proof of every underlying storage device's
durability. Other operating systems are currently unqualified and do not export
the module. tmpfs caches disappear on reboot.

Held directory/main descriptors and identity rechecks detect ordinary replacement.
SQLite additionally uses `SQLITE_OPEN_NOFOLLOW`. SQLite still opens a pathname:
this boundary does not isolate an attacker with the same effective UID, root, or
the ability to replace trusted mounts. Such actors can already modify the user's
private data. A private directory is required throughout the connection lifetime.

## Bounds, durability and caller obligations

Every connection verifies WAL and foreign keys. Caches use `synchronous=NORMAL`;
this is disposable state and does not establish the `FULL` durability required
for protected request accounting. There is no transaction spanning provider I/O.
SQLite busy waits are refreshed at transaction/commit admission and bounded by
25 ms and the invocation's remaining work window. A progress handler checks
cancellation/deadline during SQL execution. Temporary tables stay in memory;
SQL/value sizes and attached databases/worker threads are restricted.

The main file and known sidecars share a 64 MiB logical cap. Admission keeps
4 MiB for maintenance plus 1 MiB for the next bounded metadata mutation, and
requires 5 MiB free on the filesystem. A connection page-count ceiling, a
64-page automatic checkpoint interval and a 1 MiB journal-size target complement
pre/post file checks. The only writable data is one fixed-size metadata row.
The journal target is not a hard WAL limit when readers pin frames; subsequent
operations stop at the recording ceiling. Future bulk writes, response bodies,
backup files and temporary copies need their own conservative growth admission
and retention/maintenance tests before being added. External writers and
unrelated disk consumption cannot be globally bounded by these checks.

Close-time checkpointing is disabled to avoid adding an unbudgeted maintenance
checkpoint on drop. WAL files can remain after closing; they are part of the
store and must never be copied or removed independently as a supposed backup.
Cooperative operations run in the accepted owned blocking runtime. Neither a
SQLite progress callback nor a timer interrupts an uninterruptible kernel I/O
wait. Late completion is drained and suppressed by the runtime boundary.

An error after commit or cancellation during finalization can leave a committed
generation even when no successful result was published. Callers must reopen
and inspect state before retrying a required mutation; an error is not proof of
rollback. Optional cache failure degrades to a visible miss in the future caller.
Never infer ledger/accounting readiness from cache readiness.

## Verification

`tests/storage_contract.rs` exercises the real production opener against private
temporary trees and bundled SQLite: actual engine identity, disabled/missing
effects, repeated initialization, private WAL files, path/type/link/mode refusals,
corrupt/foreign/newer stores, schema drift, real lock contention with successful
retry, generation/incarnation fencing, overflow, cancellation, expiry, quota,
directory replacement and abrupt process death across committed/uncommitted
generations. Unit tests separately cover engine qualification and the
filesystem/free-space admission predicates. Synthetic policy inputs are not
evidence of a real mounted network filesystem or an actual disk-full crash.

Run remotely with `RCH_REQUIRE_REMOTE=1 rch exec -- cargo test --locked
--test storage_contract -- --nocapture`, plus the repository's full test, check
and Clippy gates. Test logs contain case/stage/schema, elapsed time, disposition
and public engine identifiers; no private paths or database contents. Test trees
are retained under Linux `/tmp` (RCH's `TMPDIR` can have shared ancestors) to comply with
the repository's deletion policy. Record the exact source/lockfile and remote
worker alongside logs; an unexecuted gate is not a pass.
