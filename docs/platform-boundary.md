# Native portability: the smallest independently compilable boundary

Issue #3 originally crossed seven files, not just the coordinator's SQLite
identity call. Since that report, main gained separate Linux/macOS filesystem
admission and native installer routing. This change preserves those concurrent
changes; it neither broadens filesystem admission nor qualifies a release.

## Boundary

`src/sqlite_engine.rs` owns only linked SQLite identity qualification. It opens an
in-memory connection and checks the minimum, exact numeric version, exact version
text, and exact source ID. Its error preserves typed SQLite failures for adapter
classification while redacting diagnostic details. The existing
`storage::linked_engine()` signature, identity type, and constants remain a
compatibility facade, including the existing StoreError mapping.

`src/platform_path.rs` owns only path spelling. Linux and other non-macOS paths
are unchanged. macOS expands only verified root-owned `/tmp` and `/var` aliases
with their expected link targets. It does not canonicalize user symlinks or erase
parent components. Persistent stores retain their descriptor/no-follow,
ownership, filesystem, quota, schema, and transaction checks.

Callers needing these two operations can use the leaves without importing the
persistent-storage module. A general portable-storage trait would be a much
larger and less useful first boundary: replay, ledger, and command handlers still
need their actual persistence behavior, not successful no-op stubs.

## Isolated native compile slice

On a standalone native Mac with the repository's pinned toolchain and Python
3.11 or newer, run from the checkout:

```sh
python3 scripts/check_platform_boundary.py --native-macos
```

Use `--offline` as well when the dependency cache is populated. On Linux, omit
`--native-macos` to run the corresponding host checks; that is not native macOS
evidence. On a shared build fleet, arrange execution on a native Mac worker using
the existing RCH/DSR infrastructure, not a Linux worker with forged cfg values.

The driver creates a temporary test-only crate that refers to the production
leaf source files by path. It does not include `storage`, replay, ledger, the
coordinator, the CLI, or copied Rust implementations. It mirrors the root
rusqlite dependency settings and projects the reachable lock entries. A
conservative metadata-only Cargo update normalizes the smaller feature graph;
every resulting dependency version, source, and checksum must still match the
root lock before `cargo test --locked` may run. The root manifest and lock are
never rewritten. The temporary crate is not a production workspace split.

The driver prints the host and Rust compiler identity, explicitly selects the
compiler's host target, refuses `--native-macos` on non-Darwin hosts, and refuses
ambient Rust flags that could simulate a cfg. Any resolution/build/test failure
remains a failure; removing qualification checks or `--locked` is not a fallback.

A passing leaf slice does not prove the full library, binary, native filesystem
behavior, or installer is qualified. The next integration gates remain:

```sh
cargo fmt --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Run these on each claimed native target and execute the existing filesystem,
cache, ledger, and installer tests. Keep issue #3 open until that evidence exists.

## Evidence for this extraction

The Python driver contract tests run independently:

```sh
python3 tests/platform_boundary_contract.py -v
```

Eight driver tests passed in the implementation sandbox, including lock closure,
source/checksum preservation, dependency-change rejection, production-source
references, and refusal of a Linux host as native macOS evidence. Python syntax
checks also passed. These are driver tests, not Rust or filesystem tests.

The sandbox has no Cargo/rustc and is not macOS. Both missing-toolchain and
non-native checks returned explicit failures before compilation. The new Rust
identity, error-adapter, alias-policy, and native symlink tests were added but not
executed there. Neither the isolated Cargo invocation nor full/native checks are
claimed as passed by this document.
