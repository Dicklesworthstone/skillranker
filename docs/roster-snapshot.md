# Roster snapshots and drift (P2)

`sr roster --snapshot FILE` saves the current workspace's resolved roster, and
`sr roster --diff FILE` compares a saved snapshot with fresh discovery. Both
need no key or network. They inspect only the documented Claude roots, with the
adapter's unverified visibility ([inspection](roster-inspection.md)). Relative
`FILE` paths resolve against the workspace.

## Snapshot (`sr.roster-snapshot.v1`)

A snapshot holds:
- the namespace: the harness, and digests of the canonical workspace and home
  paths;
- the roster snapshot digest and its partial flag;
- the sources the scan could not fully observe, or a flag when a walk limit or
  unreadable record made every source incomplete;
- one record per binding: skill ID, invocation name, source, status, both
  invocation flags and content hash.

It stores no path. A saved manifest is evidence, never permission to read
anything or to restore a removed skill.

Export uses the shared atomic export primitive
([contract](atomic-export-contract.md)). The file is owner-only (0600) and is
published atomically without replacing anything. An existing target, including
a symlink, is refused and left intact, and the destination directory must be
private. Snapshots obey the explicit-roster bounds: 32 MiB and 10,000 records.

## Diff (`sr.roster-diff.v1`)

The saved manifest is read as a bounded regular file under its own directory.
Symlinks leaving that directory, FIFOs and devices are refused. Parsing rejects
duplicate keys and unknown fields, so a record that tries to carry a path fails.
A different schema, harness, workspace or home is `unusable-roster` (exit 5);
drift is compared only within one namespace. The diff never modifies its
source.

Records are matched by skill ID and reported by invocation name:

| Field | Meaning |
| --- | --- |
| `added` | New records |
| `removed` | Records missing from a source the fresh scan fully observed |
| `unconfirmed_removals` | Missing, but their source was not fully observed now; never called deleted |
| `renamed` | A removal and an addition with the same content in the same source |
| `content_changed` | Same skill, different content hash, such as a same-name replacement |
| `restriction_changed` | Agent or user invocation changed |
| `status_changed` | Status changed, such as eligible to shadowed |
| `complete` | Both scans observed every declared source |

Changes here also change the roster snapshot digest, which keys cache and
evaluation identities.

## Verification

`tests/roster_snapshot.rs` runs the real binary over actual trees. It covers:
- owner-only, path-free export that refuses to overwrite and leaves the existing file intact;
- an empty diff for an unchanged tree;
- a rename, a same-name replacement, a restriction change, an addition and a confirmed removal;
- an unreadable root producing unconfirmed removals;
- a different workspace or home refused;
- path imports, unknown fields, duplicate keys and another schema refused;
- symlinked and FIFO manifests refused, with the source preserved;
- conflicting flags.
