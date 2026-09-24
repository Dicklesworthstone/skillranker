# Relevance Corpus Contract (sr-uv2v) — Pre-Registration

This document freezes, **before any labeling begins**, the frame, adjudication
rubric, split assignment, privacy position, and mechanical exit gate for the
relevance corpus that the promotion gates consume. It exists because the gates
forbid freezing after the fact: a corpus whose rules were written after its
labels were seen cannot support an inferential claim.

Status: pre-registered v2 (2026-09-24; §3.1 clauses and §3.3 revised after the §10
pilot). Changing any rule in this document after case adjudication has started
invalidates the affected epoch; the corrected rules apply only to prospectively
collected cases. v1 labelled only the diagnostic pilot and opens no corpus epoch.

## 1. What the corpus feeds

`sr-roadmap-l1i.8.1` executes the frozen held-out relevance and overflow
promotion evaluation. Its denominators come from this corpus alone:

| Requirement | Source of truth here |
|---|---|
| ≥300 adjudicated primary cases from independent task families | §2 frame, §5 splits |
| ≥150 positive, ≥100 no-match, ≥50 near-miss | §3 rubric classes, §6 strata |
| ≥50 positive overflow cases (rosters >254 eligible skills) | §2.4 overflow frame |
| One preselected primary case per family | §2.3 family rule |
| Labels from independent review of the full visible roster | §4 adjudication |

A **declared narrower population** is permitted by the README and by this
contract, but it must be declared in the corpus manifest (§6) *before* the
pilot's labels are inspected, and every report against the corpus must carry
that declaration. A diagnostic corpus labelled diagnostic outranks a general
corpus assembled from adoption data.

## 2. Frame

### 2.1 Eligible sessions

A case is a (workspace, session, agent branch, moment) tuple where:

1. The workspace's full visible roster at that moment is captured as a bounded
   roster manifest (skill identities, invocation names, usage kinds, content
   and restriction digests — never bodies).
2. The user's current request and constraints are recoverable in redacted form
   sufficient to judge relevance without the raw session.
3. The session's producer consents to retention under §7.
4. The moment is a **user-initiated** turn. SkillRanker also ranks turns that a harness
   message started, such as a Claude `<task-notification>` arriving while the agent
   is idle, because the agent does act next (sr-jdji). Those turns have no user
   request to judge relevance against. The frame classifies each turn by its
   transcript record, which Claude marks `origin.kind = task-notification`, and
   excludes harness-initiated turns from relevance and needless-suggestion
   denominators (sr-i2u7). In the first live shadow evaluations they were 10 of 14
   turns. The hook cannot make this split, because Claude writes the mark after the
   hook starts. Operational availability still counts them, since it measures whether
   the hook works.

Shadow-mode traffic (`sr-1uf4`) is the preferred source: no advice was
injected, so the selector under evaluation cannot have influenced the session.
Constructed cases are permitted for strata the frame cannot supply (§2.4) and
must be marked `constructed: true`; constructed cases never count toward
production-prevalence claims.

### 2.2 Independence of task families

Two cases belong to different task families only when they differ in **task
intent**, not merely in wording, session, or surface details. The same bug,
feature, document, or operational goal attacked twice — in one session or
across sessions, by one agent or several — is one family. Re-running a task
after a selector change does not create a new family. The family register
assigns each `family_id` exactly one split before any labels are seen (§5).

### 2.3 Primary case

Exactly one case per family is marked `primary_family_case: true`, chosen by
the family's registered rule (§5.2) before outcomes are inspected. Only
primary cases enter promotion denominators; sibling variants are reported
separately with family-aware uncertainty.

### 2.4 Overflow stratum

An overflow case requires a roster with more than 254 eligible skills at case
time. If the frame cannot supply ≥50 real positive overflow cases, the
manifest must either declare a narrower population without an overflow
coverage claim, or explicitly designate a genuinely large skill library as the
overflow frame with its provenance recorded. Overflow cases constructed by
padding a roster beyond 254 are constructed cases (§2.1) and support the
retrieval-coverage gate only, never prevalence.

## 3. Adjudication rubric (frozen)

The adjudicator sees: the redacted current request and constraints, the full
visible roster (names, descriptions, usage kinds, restrictions), and the task
context summary. The adjudicator does **not** see the selector's
recommendation, scores, or provider output (blindness, §4.3).

### 3.1 Acceptable set (`acceptable_additional_invocations_y`)

A skill is **acceptable** for a case when loading it at that moment would
materially help the user's current request under their stated constraints:

- Its procedure applies to the task as stated, not to a task one might
  misread the request as.
- Its restrictions permit the intended use (manual-only skills are never
  acceptable advisory candidates).
- The help is material: a skill whose entire relevant content is already
  present in context adds nothing and is not acceptable.
- Planning, analysis, writing, and explanation skills are eligible; acting on
  files is not a prerequisite.
- A skill that fits only partly under a constraint is acceptable when it offers a
  permitted path that helps materially. For example, a release skill covering both a
  forbidden and an allowed build path is acceptable if the request forbids only the
  first.
- Judge only against what the request states. Do not assume an unstated user profile
  (a filer type, a jurisdiction) or unstated environment details (the project's
  language, how a swarm is managed).

The set may have several members (order-free) or be empty. Naming a skill
"acceptable" is not a claim it is the single best one.

### 3.2 No-match

A case is **no-match** when the acceptable set is empty: no visible skill
would materially help. A no-match label requires the adjudicator to have
reviewed the full roster, not the shortlist. "I would not have used one" is
not sufficient; the test is material help, not personal habit.

### 3.3 Near-miss

A case is **near-miss** when (a) its acceptable set is non-empty, and (b) at
least one visible skill is a **strong distractor**: it is not acceptable, yet a
reasonable selector reading only names and descriptions would be *more* likely to
pick it than the best acceptable skill. An ordinary related-but-wrong skill does not
qualify, because on a large roster one always exists (§10). Each near-miss case
names its strong-distractor skill IDs. When a case is double-judged, it is near-miss
only if both adjudicators name at least one strong distractor in common, and its
recorded IDs are those they share. A case with an empty acceptable set is no-match
even if a tempting skill exists — the near-miss class exists to measure
wrong-choice pressure where a right choice also exists.

### 3.4 Explicit requests and exclusions

Requests naming a skill explicitly are resolved locally by the product and
are excluded from advisory metrics; they may be retained as separate explicit
cases. User exclusions are constraints and bind the acceptable set.

### 3.5 Unjudgeable

If the redacted record is insufficient to judge (missing attachment,
unrecoverable constraint), the case is `unjudgeable` with the reason
recorded. Unjudgeable cases stay in the frame and in conservative
harm-or-unresolved accounting; they never silently become no-match or are
dropped from denominators that promised them.

## 4. Adjudication protocol

### 4.1 Adjudicators

Each adjudicator is registered in the manifest with an opaque identity, their
relationship to the project, and the sessions they may not judge (any session
they participated in). An adjudicator is **not independent** of a case if
they produced the session, produced or saw the selector's output for it, or
have a stake in the selector's measured performance on it.

### 4.2 Double judgment

A declared fraction of cases (pilot: at least 40%; full corpus: at least 15%)
is adjudicated independently by two adjudicators. Disagreements are resolved
by discussion to a single label with the disagreement recorded; the
disagreement rate is a pilot exit metric (§8).

### 4.3 Blindness

Adjudication records include a blindness attestation: the adjudicator affirms
they did not see the recommendation, ranking, scores, or provider answers for
the case before labeling. Records lacking the attestation fail validation.

### 4.4 What adoption never proves

Observed loads, ignored suggestions, and snoozes are telemetry, never labels.
No adoption statistic enters the corpus.

## 5. Split assignment

### 5.1 Rule

Splits are `training`, `validation`, and `holdout`, assigned **by family**
before labels are inspected: every case of a family lands in exactly one
split. The assignment rule (a keyed hash of `family_id` with the key recorded
in the manifest) is reproducible without being manipulable after the fact.

### 5.2 Freeze

When a split's membership is complete for the declared population, the
manifest records the split's ordered case-ID list and its digest. Any case
added to or removed from a frozen split invalidates the freeze; the validator
(§9) fails. Priors are fit on training, thresholds chosen on validation, and
the frozen policy evaluated once on holdout.

## 6. Manifest

`skillranker.corpus_manifest.v1` records: declared population and its
justification (or the explicit narrower declaration); per-stratum minimums
(positive / no-match / near-miss / overflow-positive) **as declared**;
family register with split assignments; frozen split case-ID lists and
digests; rubric version (this document's revision); adjudicator registry;
double-judgment fraction; the selector identity string whose outputs
adjudicators must not have seen; consent/retention references (§7); and the
dataset digest. Counts are validated against the declared minimums, never
against hardcoded promotion numbers — a pilot manifest that declares 18 cases
is valid as a pilot and cannot carry a promotion claim.

## 7. Privacy and retention (maintainer decision, defaulted here)

Corpus cases contain redacted prose and roster manifests, never raw sessions,
bodies, secrets, or hash keys. Default retention mirrors ledger policy:
30-day logical retention unless the producer consents to longer; deletion
removes the case from future epochs but cannot rewrite a published freeze
(that freeze's epoch is closed instead). Each case records its consent
reference. Cases without a consent reference fail validation.

## 8. Pilot protocol (before any full-corpus scheduling)

1. Freeze this document and a pilot manifest declaring 15–25 cases spanning
   positive, no-match, and near-miss, drawn from shadow traffic if `sr-1uf4`
   is running, otherwise from constructed cases marked as such.
2. Adjudicate under §3–§4, recording wall-clock cost per case, the
   double-judged disagreement rate, and every rubric consultation for an
   unanticipated situation.
3. Outcomes, all useful: (a) cost and disagreement are acceptable → schedule
   the full corpus with a measured per-case cost; (b) cost is prohibitive →
   declare the narrower population the pilot justifies; (c) disagreement is
   high or the rubric was repeatedly consulted → revise the rubric first.
   Outcome (c) is the most valuable and the cheapest to discover early.

## 9. Mechanical exit gate

`python3 scripts/validate_corpus.py --manifest MANIFEST --cases CASES.jsonl`
is the corpus's definition of done. It fails on:

- duplicate case IDs or duplicate JSON object keys;
- a case in more than one split, or a family spanning splits;
- per-stratum counts below the manifest's declared minimums;
- a case missing its roster manifest, adjudication record, blindness
  attestation, or consent reference;
- an adjudicator identity matching the selector identity or an adjudicator
  judging a session they produced;
- a near-miss case with an empty acceptable set or no named near-miss skill;
- labels or near-miss IDs absent from the case's roster, or naming a
  manual-only skill as acceptable;
- more than one primary case per family, or a family with none;
- a case in a frozen split that is absent from the frozen case-ID list, or a
  listed case missing from the corpus (digest mismatch).

The validator's output, not a case count, is this bead's exit evidence.

## 10. Pilot result — 2026-09-24 (outcome (c): revise the rubric first)

Run under maintainer-delegated decisions (sr-uv2v comment of 2026-09-23). The pilot used
blinded AI adjudicators, which is acceptable for a diagnostic pilot only; any promotion
claim still requires human adjudication. It ran 18 **constructed** requests, one family
each, against this workspace's real 205-skill Claude roster. Each description was
verified byte-for-byte by BLAKE3 against the roster's content hash. Every case was judged
independently by two fresh subagent adjudicators (A, B), which saw only this rubric, the
roster and the request, never SkillRanker's or Jev's output. The 11 cases whose label sets
differed went to a third blinded adjudicator (C), and each skill's final label is its
majority vote. Case data stays owner-only outside the repository (§7).

`scripts/validate_corpus.py` passed: 18 cases, 18 families, 18 double-judged, dataset
digest `929bf33b65b903fac37bec33cec76878ab6f82cac9c4eed67af6b0fc2cdded62`.

| Measure | Result |
|---|---|
| Cost | ≈43 s per adjudicator for 18 cases (≈2.4 s per case); tie-break 36 s for 11 cases. Human cost not measured |
| Class agreement (A vs B) | 18/18 |
| Acceptable-set agreement | 17/18 exact (mean Jaccard 0.98) |
| Near-miss existence agreement | 13/13 helpful cases: A and B always named at least one near-miss in common |
| Near-miss-set exact agreement | 7/18; the lists differed only in extra members |
| Rubric gaps noted | 8 notes in 6 of 18 cases |
| Final strata | positive 0, near-miss 13, no-match 5 |

Findings that require a revision before any corpus is built:

1. **The acceptable set is reproducible, and so is the existence of a near-miss.** The
   near-miss lists differ in their extra members, but whenever a case had an acceptable
   skill, both adjudicators named at least one near-miss skill in common.
2. **The positive/near-miss split is degenerate on a large roster.** Because some
   plausible wrong skill always exists among 205, every case with a non-empty
   acceptable set is a near-miss under §3.3, and none is `positive`. The classes do not
   discriminate, and README's targets (≥150 positive, ≥50 near-miss) cannot both be met
   as written.
3. **Where the rubric gaps recurred:** a skill that fits only partly under a constraint
   (it covers a forbidden path as well as a permitted one); a profile-specific skill
   when the request states no profile; environment assumptions the request leaves
   unstated (NTM-managed swarm, project language); and whether tempting skills are
   recorded for a no-match case.

Proposed v2, tracked as sr-20n7: (a) keep the three case kinds, but redefine near-miss
by a sharper question: *is there a wrong skill a reasonable selector would more likely
pick than the best acceptable one?* A case is `near_miss_advisory` only when both
adjudicators answer yes and agree on at least one such skill; its reproducibility is to
be measured before adoption. An agreement rule on plain near-miss existence does not
help, since it still labels every helpful case a near-miss. (c) Rubric clauses for
partial-fit-under-constraint (acceptable when a permitted path exists and the constraint
is stated), missing profile (judge only against what the request states), and unstated
environment (never assume it; judge the request as written). Adopting v2 starts a new
epoch; no v1 labels exist beyond this pilot.

### 10.1 v2 check — the strong-distractor question (adopted)

Two new blinded adjudicators (D, E) re-judged all 18 pilot cases under the v2 wording
above, independently of A, B and C. They agreed on whether a strong distractor exists
in 18 of 18 cases, and in both cases where one exists they named the same skill:
`gh-actions` for a release request that forbids GitHub Actions, and a
Drizzle/Postgres migration skill for a SQLite migration. Their acceptable sets agreed
with each other on 17 of 18 cases, and on 16 of 18 with the v1 majority labels
reached by different adjudicators. The strata now discriminate: 11 positive, 2
near-miss, 5 no-match. Each adjudicator took about 30 s for 18 cases. v2 keeps the
three case kinds, so README's promotion table, the evaluation policy and
`scripts/validate_corpus.py` stand unchanged.
