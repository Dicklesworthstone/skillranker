# Local eligibility (P4)

`src/eligibility.rs` decides which candidates may be ranked at all, and which
local decision applies when none may. It is pure: it reads no files, calls no
provider and does not score. Scoring the survivors (utility and normalization)
is a separate boundary.

## Explicit first

`route` reads the explicit-resolution result before any advisory stage:

| Explicit result | Route | Provider call |
| --- | --- | --- |
| All references resolved | `Explicit`: emit them, keeping each kind | None |
| Any reference missing, ambiguous, forbidden or conflicting | `Unresolved`: `unavailable / explicit-resolution` | None |
| No explicit request | `Advisory`, carrying the explicit exclusions | Continue |

A manual-only skill resolves as a `manual_only` explicit reference. It never
becomes an advisory candidate, and nothing here grants a file load that would
bypass its restriction.

## Advisory stages

Stages run in a fixed order. Each removed candidate records the first stage that
removed it, and the stage that removes the last candidate names the decision:

| Stage | Removes | Decision when it empties the set |
| --- | --- | --- |
| Policy | Candidates that are not agent-invocable or are explicitly excluded | `abstain / excluded` |
| Reuse | Proven-present reusable references | `abstain / already-loaded` |
| Fit | `fit < threshold` (default 0.30; equal survives) | `abstain / low-fit` |
| None check | Each candidate whose own raw rerank probability is `<= p(__none__)` | `abstain / no-shortlist-match` |

`admit` runs the policy and reuse stages. Run it before the wide call, so a
roster whose candidates are all excluded or already loaded abstains with no
provider request; a roster with no advisory candidate at all is
`unavailable / empty-roster`. `after_rerank` repeats those stages on the
shortlist, then applies fit and the none check. Its estimates must cover exactly
the shortlist, and any other membership is `unavailable / roster-changed`.

The none check is per candidate, after fit filtering. With rerank A=0.70,
B=0.10, none=0.20 and fits A=0.10, B=0.80, fit removes A, and B does not beat
none, so the result abstains. With fits A=0.31 and B=0.999, only A survives,
although B would dominate a fit-weighted blend. Priors, phase and blended scores
are never inputs, so they cannot re-admit a removed candidate. Comparisons fail
closed: a non-finite fit or probability removes the candidate.

## Reference reuse

A candidate is suppressed only when all of these hold:

- Its metadata marks it `usage: reference`. Workflows and unknown usage kinds
  always stay eligible for repeat invocation.
- It does not run in a forked context (`context: fork`) and has no
  invocation-time content (`$ARGUMENTS`, `$N`, `${CLAUDE_…}` or
  `` !`command` ``). The frontmatter parser records both facts. Detection is
  deliberately broad, since a false positive only withholds suppression.
- The active branch is resolved, and a load of this skill was observed by an
  identified event on that branch. Loads on sibling forks, or without an event
  ID, prove nothing.
- The load happened in the current context epoch. Each extracted load keeps the
  epoch of its own event, so a compaction after it invalidates it.
- The observed source version is known and equals the candidate's. An unknown
  version never matches. Rendered hashes, when both are known, must match too.
- The load had no dynamic arguments and was not turn-scoped.

"Not loaded" is never a dismissal, and ignoring an earlier suggestion never
demotes a skill.

## Verification

`tests/eligibility_contract.rs` resolves a real Claude roster from files. It
drives loaded state through real branch resolution and load extraction, and
covers:
- both documented none/fit examples;
- inclusive fit and abstaining ties;
- non-finite estimates;
- changed shortlist membership;
- a proven-present reference abstaining before any provider call, and removed after rerank;
- compaction invalidating reuse, with a post-compaction success twin;
- unknown or older versions, dynamic arguments and turn scope;
- forked, dynamic and sibling-branch loads;
- repeatable workflows and unknown kinds;
- exclusions and an empty roster;
- explicit routing, including manual-only success without bypass.
