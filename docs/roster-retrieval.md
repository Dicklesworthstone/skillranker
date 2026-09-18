# Deterministic roster retrieval

`roster::retrieval::retrieve` connects `ResolvedRoster` to the pinned shipping
Quill index. It accepts trusted local exclusions, the three bounded query fields,
restrict-only logical budgets, and the invocation's `Cx` and `EntryClock`.
It performs no network requests, skill execution, filesystem index writes,
subprocess calls, embedding work, or engine fallback.

Explicit user requirements resolve locally before this advisory boundary.
Excluding any binding ID removes its entire physical skill, including aliases.
Only verified, agent-invocable bindings enter retrieval; a manual-only binding
cannot become a search option. Returned candidates borrow the original immutable
roster and retain the actual allowed binding for subsequent `OptionMap` creation.

## Selection policy

Policy `roster-quill-v1` sorts canonical skill IDs before selection or ingest.
With 1–254 eligible physical skills it returns all of them in ID order, without
query compilation or lexical filtering. Zero eligible skills is a typed failure.

For overflow, the existing [literal query compiler](quill-integration.md#bounded-literal-query-compilation)
combines the latest request, active task, and recent errors. Empty analyzed input
and zero search hits report `RetrievalEmpty`; neither establishes that no skill
would help. A terse continuation can still retrieve using task/error evidence.

The adapter builds a fresh in-memory Quill index, ingests documents in stable ID
order with one deterministic shard, commits, and searches offset zero with limit
254 and `exact_count=false`. Quill orders by score descending and global document
ID ascending. The adapter preserves that order and resolves every returned ID
through this request's local document map, rejecting foreign/duplicate IDs,
non-finite scores, parser diagnostics, and inconsistent document counts. Sorting
only after truncation is not used to repair cutoff ties.

## Searchable fields and budgets

The engine remains pinned to FrankenSearch revision
`39047c44c3a92ceb71d25c602913b8b2888e2fe7`, with default/optional features disabled.
Schema label `quill-default-content-title-v1` denotes the pinned default content
and title fields, shipping analyzer/BM25 behavior, and query boosts 1 and 2.
There is no custom schema, alternate BM25 implementation, or oracle dependency.
The dependency qualification tests pin title preference and equal-score cutoff
behavior; retrieval tests also force small-budget segment sealing.

| Document field | Local source | Bound in Unicode scalar values |
|---|---|---:|
| ID | Canonical stable skill ID | Existing identity byte bound |
| Title | Primary callable, other eligible callable aliases, display name; duplicate strings removed | 2,048 total, including separators |
| Content description | Full parsed description | 8,192 |
| Content tags | Tags joined with spaces | 2,048 total, including separators |

Description and tag allowances are separate so a long description does not hide
all tags. Tags are placed in searchable content, never solely in stored metadata.
Names and tags are indexed as text, not interpreted as query operators. Field
truncation is counted; lexical coverage can decrease. Body excerpts are reserved
for later ranking, not silently added to this index's feature policy.

Default logical budgets are 16 MiB Scribe, 4 MiB delta, 32 MiB cumulative document
text/ID bytes, and 1,000,000 query fuel units. Callers can only reduce these
positive budgets. The roster itself already enforces the 10,000-file and 32 MiB
source-read limits. Query construction retains its 128-term/4,096-scalar cap.
Logical engine budgets are not hard process RSS caps: document/container and
allocator overhead are separate. No hook latency target is claimed here.

## Diagnostics and failures

Diagnostics report policy/engine/schema versions, roster/eligible/admitted counts,
partial roster coverage, truncated-document count, indexed bytes, query counts,
and separate build-plus-commit and search timings. Unexecuted stages are `None`;
unknown eligibility is `None`. No query strings, descriptions, paths, or matched
secret values appear in failure messages or diagnostic debug output.

On success, `truncated` means the selected set is smaller than the eligible roster.
`hit_limit_reached` means the returned page is full; it does not claim an exact
number of additional matches. Partial discovery remains partial after retrieval.
On failure, no selection is published; selection flags do not claim coverage.
Cancellation, deadline, fuel exhaustion, index failure, invalid budgets, and
engine-contract failures remain distinct from empty retrieval and never produce
a partial successful candidate list. Quill error strings are not propagated.

The owned in-memory index is dropped after each request. This does not implement
long-lived TUI index caching, rank CLI dispatch, or pre-publication roster/content
revalidation. Callers must still apply the context/privacy policy and revalidate
all decision dependencies before publishing an actionable recommendation.

`tests/roster_retrieval.rs` uses real authorized files, metadata parsing,
resolution, the production query compiler, and Quill. Its structured events
record counts and build/search durations without fixture bodies. These timings
are synthetic local workload observations, not live harness or Jev evidence.
