# Filesystems for the enforced request allowance

The optional `sr budget` allowance is enforcement state, not a disposable
response cache. Its guard, lock, accounting database and directory locations
must pass the persistent-local filesystem prerequisite as well as their
existing ownership, mode, link, schema and generation checks.

The shared classifier uses native `fstatfs` on held descriptors. Linux uses
Linux-gated filesystem identifiers; macOS uses native filesystem names. It does
not infer a filesystem from a pathname and does not import the storage tree
into the cross-platform allowance or engine-qualification boundary.

| Filesystem class | Existing cache/ledger classifier | Enforced allowance |
|---|---|---|
| Linux ext4, Btrfs, XFS | Unchanged: accepted | Accepted as a filesystem prerequisite |
| Linux tmpfs | Unchanged: accepted | Refused: volatile storage |
| macOS APFS, HFS | Unchanged: accepted | Accepted as a filesystem prerequisite |
| Network, FUSE, overlay, ramfs, unknown types | Unchanged: refused | Refused |

An accepted type is **not** certification of a device, VFS, sync implementation,
mount configuration, release architecture or power-loss behavior. In particular,
loop-backed filesystems, external cleanup, malicious same-user changes and
consistent rollback/loss of all enforcement evidence are not detected by type
classification. All original crash/recovery and native qualification gates remain.

SQLite WAL/FULL and file/directory fsync do not make tmpfs persistent. The kernel
keeps tmpfs in virtual memory and loses its contents on unmount. SQLite's WAL
protocol also does not support multi-host use through a network filesystem.
See the [kernel tmpfs documentation](https://docs.kernel.org/filesystems/tmpfs.html)
and [SQLite WAL documentation](https://www.sqlite.org/wal.html).

## Refusal and compatibility

Check the nearest existing directory before creating a missing component, and
check existing guard/lock files and accounting objects through their descriptors.
Accounting sidecars cannot be separately mounted on another device to bypass the
directory's policy. Revalidation retains the checks around publication and debits.
No chmod repair, automatic relocation, schema reset or charge refund is performed.

A configured allowance on unsuitable storage returns an allowance-state error;
provider admission is withheld. Existing files are preserved. The guard itself
must be persistent: losing it could otherwise make an enabled allowance appear
unconfigured. A genuinely absent guard still admits the ordinary unguarded path,
including with persistence disabled, without creating files. This change cannot
recognize a previously configured legacy guard after it has already disappeared.

Do not delete a rejected guard or initialize fresh accounting to escape the
refusal. Preserve and restore the guard with its complete consistent accounting
history to an appropriate private location. Moving only a SQLite main file while
its WAL is live is not a valid migration. No automated migration is added here.

## Verification boundary

`tests/allowance_filesystem_contract.rs` uses real filesystems, not a simulated
SQLite backend. Its persistent fixtures default to `/tmp`, overridable with
`SR_TEST_PERSISTENT_ROOT`. Linux volatile fixtures default to `/dev/shm`,
overridable with `SR_TEST_TMPFS_ROOT`. These must exist on the actual executor
with the stated filesystem types; unsuitable fixtures fail explicitly. Use the
normal required RCH lane for compilation, or the standalone Cargo gates where
RCH is absent. The cache tmpfs success case must remain successful alongside the
allowance refusal cases.

The native boundary runner also freezes and compiles `src/filesystem_policy.rs`
with the production engine, path and platform modules. It still excludes the
storage tree. Passing that slice does not establish full application compilation,
filesystem durability, native macOS behavior or any release gate.
