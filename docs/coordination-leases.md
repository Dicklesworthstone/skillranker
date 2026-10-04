# Exact response sharing with fenced leases

The rank pipeline owns single-flight selection and follower waiting.
`CacheStore` provides the only persistent response and lease boundary:
the qualified, owner-only `cache.sqlite3`. The transaction primitives in
`src/cache/coordination.rs` accept that store's existing connection; they
cannot open another database or store response bodies.

The unused `SingleFlightCoordinator`, `MemoryCoordinator`,
`SqliteResponseCache`, standalone SQLite opener and alternate
`sr_response_cache` writer were retired under `sr-shnb`. Their source and
test files are retained with production primitives and production-store tests.
No old store is deleted or silently migrated.

## Production flow

```text
exact session/request namespace
  -> complete, coherent cache pair?
       yes -> revalidate current local eligibility and publication policy
       no  -> acquire bounded lease in cache.sqlite3
                leader   -> provider wide/rerank outside any transaction
                            -> validate and revalidate
                            -> publish response pair + completion in one transaction
                follower -> bounded settlement reads + exact cache-pair lookup
                            -> revalidate its own eligibility and publication policy
```

A cache-missing completed lease can be reacquired with a new fencing generation.
A concurrent refresh follows the active owner rather than preempting it.
A follower reaching its work deadline sends no provider attempt and returns
unavailable. Optional storage failures have distinct warnings; they do not
establish that an active leader exists.

## Invariants

- A keyed BLAKE3 coordination key binds the full request namespace and fingerprint.
  Equal redacted text alone does not permit sharing across sessions or branches.
- Ownership requires the random owner token, fencing generation, current store
  incarnation/generation, an uncompleted lease and exclusive expiry. Expired,
  superseded or already completed owners cannot replace the authoritative pair.
- Lease rows contain bounded ownership metadata only. Response bodies live in
  `sr_cache_response`; there is no hidden alternate response store.
- `publish_evaluation` writes both stages and lease completion in one short
  `BEGIN IMMEDIATE` transaction. A failed row or completion rolls the mutation
  back. Network work and follower waiting never hold a write transaction.
- `--no-cache`, `--no-persist` and dry-run disable cache access before opening
  storage. Response sharing is unavailable without the exact response cache.
- Only the owner incurs each provider attempt. Consumers report zero *new*
  provider usage. An owner's missing response or missing usage is unknown,
  never an invented zero-token success. Decisions and exposure records remain
  local to each consumer.
- SQLite busy waits and follower polling share the invocation's remaining
  deadline. Lease timestamps and generations are checked before mutation;
  clock reversal, malformed metadata and exhausted generations fail closed.
- Database commit and stdout are separate effects. These contracts do not
  claim exactly-once delivery or qualify a later executable/platform revision.

## Coverage after retirement

| Retired harness contract | Production coverage |
| --- | --- |
| Competing processes; both stages; owner-only attempts; subsequent offline reuse | `tests/real_rank_coordination.rs::ordinary_two_consumer_success_incurs_one_pair_and_subsequent_exact_offline_reuse`; `tests/coordination_contract.rs::qualified_store_delivers_both_stages_across_processes` |
| Follower deadline; optional acquisition/completion contention | Follower, unacquirable-lease and busy-completion cases in `tests/real_rank_coordination.rs` |
| Cache loss; forced refresh follows an active owner | Completed-lease/absent-pair case in `tests/real_rank_coordination.rs`; qualified refresh case in `tests/coordination_contract.rs` |
| Stalled/expired owner cannot overwrite successor; store mismatch | `tests/cache_publication_fence.rs`; stale-owner CLI case in `tests/real_rank_coordination.rs`; wrong-store case in `tests/cache_atomic_publication.rs` |
| Body-before-completion, single-use completion, failed write rollback | Pair/reopen, completed-publisher and second-row failure cases in `tests/cache_atomic_publication.rs`; completion/rollback/reopen unit tests |
| Namespace isolation; metadata contains no bodies | Namespace and metadata cases in `tests/coordination_contract.rs`; `tests/cache_namespace_isolation.rs` |
| Current exclusions; offline exact hit/miss; disabled persistence; stale entries | `tests/cache_policy_revocation.rs`; live exclusion, offline pair, disabled persistence and late-rerank cases in `tests/rank_acceptance.rs` |
| Expiry boundary, clock reversal, overflow and malformed metadata | `src/cache/coordination/integrity_tests.rs` exercises the production transaction primitives with controlled timestamps and deliberately permissive malformed-row fixtures |
| Engine, private paths, bounded locks, cancellation and quota | `tests/storage_contract.rs` exercises the production opener |

Storage fixture bytes prove storage behavior, not Jev answer validity or relevance.
The real CLI tests use an independent loopback TLS fixture with synthetic
answers; public provider and native platform qualification remain separate gates.
These references map coverage; executed results require a source-bound receipt.
See [storage foundation](storage-foundation.md) for schema, quotas and publication.
