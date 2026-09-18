# Roster inspection (P2)

`sr roster [--json] [--limit N] [--cursor TOKEN]` lists the skills the selected
harness could load from the current workspace. It needs no API key, network
or state. It discovers only the documented Claude project and personal roots
([discovery](roster-discovery.md)), resolves them
([resolution](roster-resolution.md)), and reports each binding with its
[evidence](roster-evidence.md) status.

The foundation capabilities record Claude's adapter support and visibility
semantics as unverified. Inspection therefore claims no precedence:
- a skill is `unverified`;
- same-name skills are `ambiguous`, with no winner.

A record still shows its invocation restrictions, so a manual-only skill is
visible as `agent_invocable: false`. This build prints JSON with or without
`--json`; table rendering belongs to the output renderer.

## Output (`sr.roster-listing.v1`)

| Field | Meaning |
| --- | --- |
| `snapshot` | Digest of the resolved roster membership (evidence `snapshot_id`) |
| `partial` | The scan could not observe every documented source |
| `total`, `start` | Record count and this page's first index |
| `counts` | Skills, bindings, verified, shadowed, ambiguous, unverified, manual-only, forbidden and advisory |
| `source_causes`, `record_causes` | Discovery outcomes and excluded records, by stable code |
| `records` | One per binding in stable skill-ID order, with the fields below |
| `next_cursor` | Continuation token, or `null` on the last page |

Each record holds `skill_id`, `invocation_name`, `name`, `source`, `status`
(`eligible` or the first stable reason), both invocation flags, `usage_kind`,
`content_hash` and the number of `aliases`. No local path is printed.

## Pages

`--limit` accepts 1 to 128 (the default is 128). `next_cursor` encodes the
snapshot and the next offset. A continuation is recomputed from a fresh scan,
and it is accepted only when that scan has the same snapshot. Otherwise it
fails with `roster-changed` (exit 5), and the listing must restart without
`--cursor`. Concatenating the pages of one snapshot reproduces the whole
listing exactly. These cursors are unrelated to transcript ingestion cursors.

Malformed limits or cursors, including a cursor past the end, are
`invalid-usage` (exit 2). With `--json` or a piped stdout, errors are JSON
failure envelopes on stdout, and stderr stays empty. Messages contain no path,
file content or credential.

## Verification

`tests/roster_cli.rs` runs the real binary over actual trees with a cleared
environment. It covers:
- status, restriction, count and partial-source reporting, with a key present
  that is never echoed;
- three pages of 300 skills that concatenate to the whole snapshot, plus a
  second page size that agrees;
- a change between pages that forces a restart;
- bad limits and cursors, including one past the end;
- piped output without `--json`;
- an empty workspace;
- help without discovery.

## Output privacy and connected verification

Listing serialization scans full display, invocation and source labels with the
shared redactor. An uninspectable label is withheld as `[REDACTED]`; exact local
identities remain internal for resolution and snapshot comparison. Ordinary
readable labels are unchanged. This does not make arbitrary private prose public.

Run `scripts/e2e/run.sh --suite roster --artifacts EXISTING_DIRECTORY` for real
Rust filesystem, CLI, Quill, explicit-resolution and publication-revalidation
checks through required-remote RCH. The suite retains stdout and stderr and fails
on any selected target failure. Its 0/1/254/255/1000 cases record counts and cold
index timings, not private metadata. The synthetic adapter visibility used in
library tests does not establish an installed harness's compatibility. These are
local integration checks, not live Jev or recommendation-quality measurements.
