# Output contract v1

`skillranker::output::OutputDocument` validates and serializes bounded JSON.
It is an immutable data envelope: accepting a document does not establish user
consent, current roster visibility, execution, delivery, or quality. The module
has no filesystem, subprocess, network, persistence, or hook publication effects.

Use `from_json` at an external byte boundary. It rejects duplicate keys at every
depth, trailing JSON, incompatible versions, invalid discriminants, and malformed
mandatory fields. `from_value` accepts locally constructed values; a permissive
parser upstream cannot recover duplicate keys. `to_json` returns a complete buffer
before a caller begins publication. All three enforce the 2 MiB encoded limit;
nesting is limited to 64 levels. Unknown additive fields retain their JSON values,
including nested nulls and 64-bit integers, through a round trip. Whitespace and
object key order are not preserved. Unknown fields never grant authority.

The public [README example](../README.md#json-output), comprehensive-plan example,
and `tests/fixtures/output-*.json` are checked by `tests/output_contract.rs`.
Every fixture is synthetic protocol data, including fixtures illustrating a
recorded report; none establishes a live evaluation or a passed product gate.
Illustrative hashes have valid syntax and do not identify actual skill files.

## Decisions and nullability

All full decisions have `schema_version: 1`, opaque `event_id`, `harness`, a
kebab-case `reason`, `context_quality`, `quality`, `roster`, stage estimates,
`skills`, `omitted_rank_mass`, cache/model/usage metadata, persistence status,
warnings, `warnings_omitted`, and `elapsed_ms`.

| Decision | Skills | Model estimates | Ordinary CLI exit |
| --- | --- | --- | --- |
| `ranked` | Nonempty, at most 32, consecutive ranks and distinct stable IDs | Evaluated probabilities, phase, and scores | 0 |
| `explicit` | Nonempty locally resolved references | All estimates, phase, omitted mass, and model identities null; provider usage zero | 0 |
| `abstain` | Empty | Evaluated stages retain values; other stages null | 0 |
| `unavailable` | Empty | Available stage values may remain; absent stages null | Required error category |

Each skill separates `skill_id`, display `name`, validated `invocation_name`,
nullable local `path`, and a 64-character lowercase hexadecimal `content_hash`.
`sr rank` also labels each skill's `visibility` as `verified` or `unverified`.
Claude's documented precedence is provisional, so its results are
`unverified`, and the decision carries an `unverified-visibility` warning.
When `sr rank` discovers its session, the first warning discloses the choice:
`discovered-session` for the workspace's only session, or `latest-session` for
an explicit `--latest` choice. Its count is the number of eligible sessions.
Reserved `__none__` is never a skill ID. The schema validates identity syntax;
only the roster/publication boundary can establish that a target is loadable.

`wide_probability`, `rerank_probability`, and `fits` are provider estimates;
`rank_score` is a local normalized score. All are finite and in [0,1]. Returned
wide probabilities sum to at most one; returned rerank probabilities plus the
none probability sum to at most one. A returned
ranked candidate must beat `none_probability` strictly. Returned scores plus
`omitted_rank_mass` sum to one within 0.0001 rounding tolerance; top-K output
does not renormalize its displayed subset. The schema cannot recompute a scoring
formula or validate the complete provider distribution from this selected subset.
`choice_confidence` and `none_probability` are both null before rerank evaluation.
Zero is an evaluated estimate, never an encoding for an unevaluated stage.

`quality` requires five booleans: `prompt_complete`, `task_anchor_known`,
`history_windowed`, `attachments_omitted`, and `source_gaps`. Summary values are
`complete`, `prompt_only`, `partial`, and `insufficient`. Ranked results require
a complete prompt and known anchor; insufficient context permits only unavailable
output. Windowed history and omitted nonessential attachments are representable.
Partial roster positives still require independent target revalidation; this
schema does not authorize them or turn a partial result into a global negative.

Roster counts are bounded by 10,000 discovered, 254 wide candidates and 32
shortlist entries, with subset count checks. `retrieval` is `full`, `quill-bm25`,
or `not-evaluated`. Required `provenance` contains a snapshot digest,
`policy_version`, and nullable `wide_set_id`/`rerank_set_id` digests. Candidate-set
IDs are present when the corresponding set is populated; they identify the
comparison scope, not successful provider execution. Snapshot and set digests
use `ContentHash`'s encoding; the roster builder supplies their canonical bytes.

Cache fields require strict booleans and nullable age. Stale cached decisions
cannot be serialized as fresh results. A full cache hit has zero current request,
attempt, and token usage. Default v1 usage permits at most two logical requests
and four HTTP attempts, with unknown-usage attempts explicitly counted. Returned
model identities remain separate from the requested alias; a missing immutable
revision remains null. Persistence is `recorded`, `disabled`, or `unavailable`;
recorded metadata does not prove harness delivery.

Those three states describe what actually backed the run. `disabled` means an
effect flag turned persistence off (`--no-cache`, `--no-ledger`, `--no-persist`
or `--dry-run`). `recorded` means at least one qualified store backed this run:
the cache that served and recorded its provider responses, a committed ledger
event, or both. The word is deliberately coarse and is not a claim that both
stores were healthy; `cache` reports cache activity separately, and a recorded
ledger event is visible only through the ledger itself. `unavailable` means
persistence was permitted but no store
could be used: no session identity to scope one, a directory the store refuses,
a replaced store, or an unqualified engine. A run that wanted a cache and could
not use one also carries the `cache-unavailable` warning, because an uncached
run that says nothing is indistinguishable from a cached one and silently pays
for every response twice. The warning names no path, mode or errno.

## Statistics envelope

`sr stats` emits a non-actionable report: `kind: "stats"`, `actionable: false`, and a
`window` naming both ends plus `covers_all_retained_records` when no `--since` was
given. It never contains raw examples and never enables adaptation.

Counts are reported with their own denominators rather than as rates. `totals` is the
sum of `channels`, which keep separate denominators for `cli`, hook and any other
recorded `mode_channel`, so a shadow run and a CLI run are never averaged together.
Within each channel, `decisions` separates `ranked`, `explicit`, `abstained`,
`operational_failures` and `in_flight_or_killed`; the last is an invocation that
wrote itself down and never finished, whose cost is unknown rather than zero, and it
is counted as neither a failure nor a delivery. `delivery` counts `emitted` against
`prepared_not_emitted`, because writing no bytes is not delivered advice.
`latency_ms` reports `samples` alongside `p50`, `p95` and
`excluded_unfinished`, and the percentiles are absent rather than zero when nothing
finished. `attempts` separates `completed`, `failed`, `unknown_or_unsettled` and
`admitted_only`, plus `events_without_recorded_attempts` — named for what was
observed, since a reused response and a record written before attempts existed look
identical and neither is evidence of a cache hit.

`cost_per_useful_suggestion` covers only the judged cohort and its matched attempts.
It reports `estimable: false` with a `reason` of `no-useful-labels`,
`unknown-usage-present` or `no-matched-attempts`, and the known attempt and token
counts regardless, so the report degrades to counts rather than to silence. Its
`monetary` field is separately not estimable while no versioned pricing configuration
exists. Adoption is never reported as usefulness, and token counts are never
converted into saved labour.

`not_recorded` names each measure the ledger cannot observe, with why: muted or
suppressed output, because snoozes are trusted configuration; cache reuse, because no
cache column exists on a recorded event; acknowledged delivery, because no harness
confirmation exists to record; and estimated cost. These are named rather than
reported as `0`, since a zero that means "never recorded" is indistinguishable from a
measurement.

`sr stats` requires a ledger. An absent, disabled or unreadable store is a typed
storage failure, never an empty report, because "there is nothing to read" and
"nothing happened" are different claims.

## Failure envelopes

A failure before input admission may contain just version, unavailable decision,
and `error: {code, kind, message, hint, retryable}`. It must not fabricate a session,
roster, scores, or model metadata. `OutputDocument::failure` constructs this form
with fixed diagnostics. Optional unresolved references contain `reference` and a
`missing`, `ambiguous`, or `restricted` reason; they never substitute suggestions.

| Exit | Stable `error.kind` values |
| --- | --- |
| 2 | `invalid-usage`, `invalid-configuration` |
| 3 | `missing-session`, `ambiguous-session`, `superseded` |
| 4 | `provider-failure`, `authentication`, `network-failure`, `request-budget`, `provider-cooldown`, `budget-state` |
| 5 | `empty-roster`, `unusable-roster`, `unresolved-explicit`, `incomplete-roster`, `roster-changed`, `retrieval-empty`, `retrieval-failure` |
| 6 | `timeout` |
| 7 | `malformed-input`, `oversized-input`, `unsupported-input`, `unsupported-source-mode`, `insufficient-context`, `output-limit` |
| 8 | `network-denied` |
| 9 | `storage-failure` |
| 10 | `invalid-provider-response` |
| 11 | `cache-miss` |

Codes and kinds must agree. Missing TypeSafe credentials use `authentication`;
credentials themselves do not grant network consent. Retryability describes a
fresh invocation with the intended inputs, not permission to retry past policy
or deadline limits. Signals and broken pipes remain platform outcomes.

Warnings contain bounded `kind`, positive or zero `count`, and `message`; there
are at most 32 detail objects plus an omitted-detail count. Known text fields are
bounded to 4,096 bytes and reject control characters. Schema errors and `Debug`
never echo rejected keys, values, parser messages, or document bodies. Explicit
serialization preserves data, so callers must still redact disclosure fields;
validation is not a secret scanner or a terminal renderer.

## Non-actionable artifacts

Artifacts require `schema_version: 1`, `kind: demo|replay|report|preview`,
and `actionable: false`. Previews are described in
[stateless dry-run previews](#stateless-dry-run-previews); the rules below
apply to demo, replay and report artifacts. They have no top-level decision or hook envelope. Demo and
replay carry a `historical` decision, optionally with a `recomputed` decision;
a missing historical decision is representable only in a partial run. Historical
decisions are validated as inert data, including unavailable/error decisions.
Demo evidence is synthetic and has `gate_status: not-applicable`.

`completeness` records `cases_requested`, `cases_completed`, `stages_required`,
`stages_completed`, and `evidence_compatible`. Counts are unsigned and bounded
(10,000 cases and 20,000 stages), and completed counts cannot exceed requested
counts. A complete run must account for all declared cases and stages. The
evaluator must derive these counts from the requested cohort; the serializer
cannot authenticate a supplied cohort or evidence claim.

| Run status | Allowed gate statuses | Meaning of exit 0 |
| --- | --- | --- |
| `complete` | `passed`, `failed`, `not-established`, `not-applicable` | Artifact generation succeeded |
| `partial` | `failed`, `not-established`, `not-applicable` | Available partial work was reported |

`passed` additionally requires compatible, nonsynthetic evidence, a nonempty
case cohort, and nonempty required stages. Missing comparison stages use
partial/not-established. A known failed gate may be reported even if other work
is partial. A fatal current error is allowed only in a partial artifact and
retains its nonzero code; historical errors do not change report execution status.
Promotion must verify cohort identity and actual evidence as well as these fields.

## Stateless dry-run previews

`sr rank --dry-run` emits a `kind: preview` artifact
([example](../tests/fixtures/output-preview.v1.json)). It is never actionable,
has no top-level decision, and always has `stateless: true`. Nothing was sent
and no key, store, lock or log was created; `effects` is the invocation's effect
receipt showing that.

A preview carries exactly one of two results:

- `provider_request`: the requests a matching `--no-persist` run would send.
  `stages` starts with `wide` and adds `rerank` only for explicit
  `--shortlist-ids` evidence; a network-free run cannot know the model's own
  shortlist. Each stage has the exact redacted request as `request` text, its
  `request_bytes` (which must equal the text's length, at most 96 KiB), and its
  candidate count. `disclosure` is the disclosure receipt for the rendered
  context. The preview corresponds to `--no-persist`: a persistent run may
  include additional historical evidence and send different bytes.
- `local_decision`: the decision that ends the run before any request, such as
  explicit resolution, local abstention or an unavailable result. The preview's
  exit is that decision's exit.

Shortlist IDs must be distinct wide candidates, at most the shortlist size, and
are accepted only with `--dry-run`; anything else is `invalid-usage`.

## Snapshot-bound traces

An optional `trace` contains `cursor`, `total`, `entries`, `next_offset`, and optional `next_cursor`.
`TraceCursor` binds version, snapshot digest, query digest, and offset.
`resume(current_snapshot, current_query, total)` rejects changed snapshots or
queries and invalid offsets. Consumers must supply current expected identities;
an imported page cannot establish currentness by comparing its own IDs to itself.
This cursor is unrelated to transcript ingestion positions.
An embedded trace must also match its enclosing decision's roster snapshot.

Pages contain at most 128 entries; traces contain at most 80,000 entries (eight
stages over the bounded 10,000-skill roster). A nonfinal page must make progress
and advance exactly by its entry count; a final page has null `next_offset` and null `next_cursor`.
Each entry records stable `skill_id`, stage, status, nullable finite `value` and
`threshold`, and nullable reason. Lexical scores may exceed one; these generic
trace operands are not probability fields.

An explicit decision traces the resolved requirements by default. A requested
manual-only skill passes explicit visibility; advisory eligibility is not run.
Quill, wide, fit/none, and ordering stages are `not-evaluated` with null operands
and reason. Publication means the explicit decision is ready to emit, not that
stdout delivery was acknowledged. With `--why-not`, an unrequested target has
only snapshot membership evidence; no advisory evaluation or publication is
claimed for it. An unknown target remains `not-in-snapshot`.

Stages are `discovery`, `visibility`, `local-policy`, `quill-admission`,
`wide-shortlist`, `fit-none`, `ordering`, and `publication`. Statuses are `passed`,
`excluded`, `not-evaluated`, and `not-in-snapshot`. The latter two require null
operands and reason. Reasons for observed stages are local identifiers, not
invented model reasoning. Actual trace production and CLI pagination belong to
their downstream implementation beads.
