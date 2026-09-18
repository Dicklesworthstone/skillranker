# Exact-Response Sharing with Fenced Bounded Leases

Implementation documentation for boundary `sr-roadmap-l1i.5.10`.

## Architectural Overview

SkillRanker coordinates concurrent provider requests across processes and threads using single-flight coordination with fenced, bounded leases.

```text
Incoming Stage Request (Key, Namespace, RequestFingerprint)
  -> CoordinationKey::compute(key, namespace, request_fingerprint)
       -> Cache Hit?
            YES -> Return Cached Response (new_requests: 0, new_tokens: 0)
            NO  -> Acquire Lease in Coordinator (Memory or SQLite)
                     Leading  -> Execute Provider Call (Attempt Owner)
                                 -> Complete with Fencing Generation Check
                                      Published  -> Put in Response Cache
                                      Superseded -> Drop Result (Quiet Fallback)
                     Following -> Bounded Wait within Invocation Deadline
                                 -> Leader Completed -> Read from Cache (0 new requests/tokens)
                                 -> Deadline Exceeded -> Quiet Fallback
```

## Lease & Fencing Invariants

1. **Coordination Key Isolation**:
   - `CoordinationKey` is a keyed BLAKE3 digest binding the local secret `CacheKey`, `CacheNamespace` (harness, workspace, session, branch, epoch, key generation), and the canonical `RequestFingerprint`.
   - Distinct turns with identical prompts and contexts in the same session share the single-flight lease.
   - Different sessions, branches, or event namespaces produce distinct coordination keys and never coalesce or block each other.
2. **Lease Leadership & Monotonic Fencing**:
   - The winner of a lease receives a unique 16-byte `OwnerToken` and `FencingGeneration(1)`.
   - If an active leader stalls past `lease_ttl_ms` without completing, a successor process acquires the lease with an incremented fencing generation (`generation + 1`).
   - If the old leader eventually finishes after lease expiration or successor acquisition, its publication attempt is strictly rejected with `PublishOutcome::Superseded`.
   - Superseded results are silently dropped as quiet fallback, guaranteeing that obsolete provider responses can never overwrite newer state.
3. **Follower Deadline Bounding**:
   - Follower processes wait only within their remaining invocation deadline.
   - If the leader does not complete before the deadline expires, the follower exits with `FollowerResolution::DeadlineExceeded` (quiet fallback), ensuring no hook or process hangs indefinitely.
4. **Short SQLite Transactions**:
   - In SQLite-backed coordination (`SqliteLeaseCoordinator`), atomic transactions (`BEGIN IMMEDIATE ... COMMIT`) are used only for short lease acquisitions and completions (< 1ms).
   - Write locks are **never** held across network HTTP requests or follower wait loops.
5. **Zero Hidden Response Bodies in Coordination State**:
   - The SQLite table `sr_coordination_leases` strictly stores:
     `coordination_key (BLOB[32])`, `owner_token (BLOB[16])`, `fencing_generation (INTEGER)`, `acquired_at_unix_ms (INTEGER)`, `expires_at_unix_ms (INTEGER)`, `attempt_id (TEXT)`, `is_completed (INTEGER)`.
   - Zero response bodies or advice texts are stored in coordination state.
   - When `--no-cache` is enabled, cross-process response sharing is disabled; followers cannot read response bodies, enforcing that coordination state is not an unauthorized secondary response store.
6. **Usage Accounting & Identity Separation**:
   - Only the leader process records newly incurred provider attempts and debits usage (`new_requests = 1`, `new_tokens = usage.total_tokens()`).
   - Followers and cache consumers incur `new_requests = 0` and `new_tokens = 0`, with zero duplicate attempt debits.
   - Missing or unknown owner usage remains 0 tokens and is never fabricated.
   - Distinct turns sharing a response keep separate invocation identities, event keys, eligibility evaluations, decision fingerprints, and exposure records.

## Verification

The contract is verified in `tests/coordination_contract.rs` (7 tests, 0 failures):
- `real_competing_processes_and_single_flight`: verifies concurrent OS subprocesses (`std::process::Command`) competing for a SQLite lease, where leader completes and follower observes completion with 0 new requests/tokens.
- `lease_expiry_and_reacquisition_with_fencing_bump`: verifies that an expired lease bumps `FencingGeneration` from 1 to 2 on reacquisition.
- `old_owner_late_completion_rejected_after_expiry`: verifies that an old leader completing after expiration is rejected with `PublishOutcome::Superseded`.
- `follower_deadline_causes_quiet_fallback`: verifies that a follower with a tight deadline returns `DeadlineExceeded` without hanging.
- `different_session_and_event_isolation`: verifies distinct sessions obtain independent coordination keys and leadership without crosstalk.
- `no_hidden_response_body_in_coordination_store`: inspects SQLite schema via `PRAGMA table_info` and verifies `--no-cache` policy refuses body sharing.
- `missing_owner_usage_remains_unknown_no_duplicate_attempt_charges`: verifies follower usage accounting and absence of duplicate charges.
