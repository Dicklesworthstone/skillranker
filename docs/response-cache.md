# Per-Stage Validated Response Cache and Truthful Freshness

Implementation documentation for boundary `sr-roadmap-l1i.5.8`.

## Architectural Overview

SkillRanker enforces strict per-stage isolation and truthful freshness for provider responses across Stage 1 (Wide retrieval) and Stage 2 (Rerank choice).

```text
Incoming Stage Query (Key, Namespace, Stage, Fingerprint)
  -> MemoryResponseCache::get(&CacheLookupQuery)
       -> Exact Hash Match on (Namespace, Stage, Fingerprint)?
            NO  -> CacheLookupResult::Miss
            YES -> Evaluate Freshness (Receipt Timestamp, TTL, Model, Revision, Clock)
                     Fresh -> CacheLookupResult::Hit { entry, age_ms, remaining_ttl_ms }
                     Stale -> CacheLookupResult::Stale { entry, status } (Non-actionable)
```

## Freshness & Invalidation Rules

1. **Truthful Freshness & Strict TTL**:
   - Cache entries record `received_at_unix_ms` when first received from the provider.
   - Default TTL is 600 seconds (10 minutes maximum). Age is strictly elapsed wall-clock time (`now_unix_ms - received_at_unix_ms`).
   - Read operations **never** update `received_at_unix_ms` or renew TTL. Repeated reads observe monotonically decreasing `remaining_ttl_ms`.
2. **Wall-Clock Rollback Rejection**:
   - If the system clock rolls backward (`now_unix_ms < received_at_unix_ms`), the entry is flagged with `FreshnessStatus::ClockRollback` and rejected as stale.
   - Clock rollbacks or NTP skews cannot artificially extend cache validity or resurrect expired data.
3. **Model & Revision Sensitivity**:
   - Lookups require matching both `model` and `model_revision`. Any mismatch results in `FreshnessStatus::RevisionMismatch`, invalidating the cache.
4. **Shortlist Sensitivity**:
   - Stage 2 (Rerank) request fingerprints incorporate the exact shortlist $M$, candidate IDs, and content hashes. Any candidate change causes an immediate Stage 2 cache miss, preventing the reuse of decisions computed for an alien candidate set.
5. **Stage Pairing & Unversioned Alias Safety**:
   - Under an unversioned model alias (where underlying weights may shift transparently), combining an old cached wide stage with a fresh rerank stage is strictly prohibited (`validate_stage_pair_coherence` returns `CacheError::UnversionedPairMismatch`).
   - The engine must refresh both stages together or report unavailable.
6. **Hook Safety & Non-Actionable Stale Inspection**:
   - Live hook execution consumes only `fresh_entry()`, which strictly returns `None` when an entry is stale or expired.
   - Inspection/debugging tools receive an `InspectionView` with `is_stale: true` and `is_actionable: false`, ensuring stale recommendations are never presented as valid next-step actions.
7. **Zero-Cost Execution Accounting**:
   - When a ranking pipeline run is wholly satisfied from cache, `ExecutionAccounting` reports `new_requests = 0` and `new_tokens = 0`, ensuring truthful cost tracking and preventing double-billing or false usage reporting.

## Persistent Store in `sr rank`

`sr rank` keeps validated responses in the qualified owner-only store described
in [storage foundation](storage-foundation.md), under the platform cache
directory (`$XDG_CACHE_HOME/sr` or `~/.cache/sr`).

- **When it is used:** only when the response-cache effect is enabled and the
  context carries a session identity. `--no-cache`, `--no-persist` and
  `--dry-run` never open or create the store. A context without a session never
  shares responses across invocations.
- **Keying:** fingerprints use the store's random key. The namespace binds the
  workspace, session, branch, context epoch, harness and store generation.
  Request fingerprints bind the exact request bytes, candidates, endpoint,
  model alias and policy versions.
- **Pairing:** a cached wide answer is served only when it needs no rerank (low
  need) or when the rerank answer for its exact shortlist is cached too.
  Otherwise the pair is refreshed together, or the run is `cache-miss` when no
  send is allowed. `--offline` can therefore serve a complete cached pair.
- **Accounting:** a served pair reports zero new requests, attempts and tokens.
- **Recording:** fresh answers are recorded after local evaluation, with receipt
  time as wall-clock milliseconds and the ten-minute TTL.
- **Single flight:** on a miss, one process per exact request (namespace plus
  wide request fingerprint) sends. Its lease and response rows share the
  qualified `cache.sqlite3`. Ownership, generation, completion status, and
  expiry are checked inside the response-write transaction, preventing a
  superseded leader from overwriting a successor's response. Another process
  waits within its remaining budget and can consume the exact recorded pair
  with zero new usage. A missing or partial pair requires reacquiring leadership
  before sending; an active follower cannot bypass its leader near timeout.
  No transaction spans HTTP. Completion is a later bounded metadata operation;
  a crash can leave an incomplete lease until expiry. `--no-cache` and
  `--no-persist` disable cross-process sharing.
- **Failure:** an unusable optional store permits uncached ranking. Recording
  failures and unconfirmed lease completion preserve valid answers with bounded
  warnings. Confirmed lease supersession withholds the result. Incompatible
  older caches are preserved without automatic migration or deletion.

## Verification

The contract is verified in `tests/response_cache_contract.rs` (8 tests, 0 failures):
- `per_stage_isolation`: confirms Wide and Rerank responses cannot be cross-retrieved.
- `shortlist_sensitivity_causes_rerank_miss_when_candidates_change`: verifies that changing candidate set $M$ from 3 to 2 causes a clean Stage 2 cache miss.
- `truthful_freshness_and_no_ttl_renewal_on_read`: verifies exact elapsed age calculation, absence of read-renewal, and expiration at $t_0 + 601\text{s}$.
- `wall_clock_rollback_detected_and_rejected`: verifies that backward clock movement produces `FreshnessStatus::ClockRollback`.
- `revision_mismatch_and_unversioned_alias_rules`: confirms model/revision changes invalidate cache entries, and unversioned aliases reject cached-wide/fresh-rerank mixtures.
- `zero_cost_accounting_for_wholly_cached_executions`: confirms 0 new requests and 0 new tokens for cached runs.
- `stale_inspection_non_actionable_and_no_hook_fallback`: confirms expired entries yield `None` for live hooks and `is_actionable: false` for inspection views.
- `namespace_isolation_and_eviction`: verifies multi-session isolation and targeted namespace eviction without cross-session contamination.
