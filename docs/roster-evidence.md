# Roster discovery and exclusion evidence (P2)

`src/roster/evidence.rs` supplies the stable reason codes that explain where a
candidate left the roster pipeline. Rendering them through `--explain` and
`--why-not`, and adding the later stages (wide shortlist, fit and none,
ordering, publication), belongs to the ranking boundary.

Evidence is read only from outputs the pipeline already produced: the resolved
roster, the local policy the ranking applied, and the retrieval result. It
opens no file, runs no discovery and changes no candidate set. A skill written
after the snapshot therefore stays `not-in-snapshot`, and computing evidence
cannot alter a retrieval result or a ranking.

## Stages and reasons

| Stage | Code | Reasons | Hint |
| --- | --- | --- | --- |
| Discovery | `discovery` | `not-in-snapshot` | `inspect-roster` |
| Visibility | `visibility` | `shadowed`, `ambiguous` | `inspect-precedence` |
| | | `unverified` | `verify-adapter-visibility` |
| Restrictions | `restrictions` | `manual-only` | `request-explicitly` |
| | | `forbidden` | `check-invocation-restrictions` |
| Local policy | `local-policy` | `excluded` | `review-exclusions` |
| | | `already-loaded` | none |
| Retrieval | `retrieval` | `not-retrieved` | `refine-request` |

`trace` reports every stage for one skill ID as `passed`, an exclusion reason,
or `not-evaluated`. The decisive reason is the first exclusion. A stage that
did not run is `not-evaluated`, never a zero score: local policy when no policy
view is given, and retrieval when it was not run or failed operationally. A
failure such as cancellation is not a lexical miss; only a completed retrieval
that did not admit the candidate is `not-retrieved`.

After a visibility, restriction or explicit exclusion, later stages did not run
for that candidate. Retrieval does see loaded references, however, so a loaded
candidate reports its retrieval outcome after `already-loaded`. Local policy
and retrieval act per skill file, so a binding counts as excluded or retrieved
when any binding of the same file is. Pass the same exclusion set that
retrieval used.

Hints are allowlisted identifiers for the renderer. None of them runs a
command, enables networking, lifts an exclusion or lowers a gate.

## Summary

`summarize` returns counts of skills, bindings, verified, shadowed, ambiguous,
unverified, manual-only, forbidden and advisory candidates. Policy and retrieval
counts are `None` when those stages were not evaluated. It also returns:

- the roster's `partial` flag;
- discovery outcomes by code (`root-missing`, `root-unreadable`,
  `source-not-enumerated`, `symlinked-directory-skipped`, walk limits), which
  `ResolvedRoster::source_diagnostics` now retains from Claude discovery;
- records excluded during resolution, by code (`unsupported-layout`,
  `malformed-metadata`, `unreadable`, `changed-during-read`, `limit`, and
  others);
- at most 32 `(skill ID, reason)` details in stable ID order, with the rest
  counted in `omitted_details`.

`snapshot_id` is a `ContentHash` over the resolved membership: each file's
identity and content hash, and each binding's ID, source, callable name,
visibility, restrictions and the partial flag. The same roster gives the same
digest, and a content change moves it.

## Verification

`tests/roster_evidence.rs` resolves real files and runs real Quill retrieval
over more than 254 candidates. It covers:
- every reason with an all-passed success counterpart;
- the unknown ID;
- local policy that was not run, retrieval that returned empty, and retrieval
  that was cancelled;
- the first decisive cause when a candidate has several;
- identical final decisions with different causes;
- counts, bounded details with the omitted count, and snapshot stability and
  change;
- identical candidate sets with and without evidence collection, and no
  rediscovery of a skill written later;
- partial-source and record causes, with a clean-plan counterpart;
- stable codes.
