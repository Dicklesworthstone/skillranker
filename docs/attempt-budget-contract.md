# Attempt Budget, Admission, and Cost Receipts Contract

**Owner Task:** `sr-roadmap-l1i.2.8`  
**Boundary ID:** `p1_admission_budget`  
**Phase:** P1  
**Module:** `skillranker::jev::admission`  
**Tests:** `tests/transport_contract.rs::attempt_budget_admission`

---

## 1. Purpose and Architecture

Under the SkillRanker contract (I03, `sr-roadmap-l1i.2.8`), fresh calls to TypeSafe's Jev inference service are strictly bounded:
- A four-attempt cap per invocation is enforced across retries and stages.
- Every HTTP attempt must pass through a **single admission seam** before bytes reach the network.
- Single-use permits guarantee that no attempt is sent without authorization or deadline verification.
- Provider token usage is accounted as either **known** (returned from a valid response) or **unknown** (in-flight attempts that failed, timed out, or were cancelled after transmission).
- Unknown provider cost is **never counted as zero**.

```text
Invocation Started (EntryClock captured)
         │
         ▼
admit(Wide, endpoint)
  ├─► Insufficient remaining deadline?  ──► Refuse: InsufficientDeadline (exit 6)
  ├─► Attempts exhausted (>= 4)?       ──► Refuse: AttemptsExhausted (exit 4)
  ├─► Logical requests exhausted (>=2)? ──► Refuse: LogicalRequestsExhausted (exit 4)
  └─► Admitted: Issue AttemptPermit(attempt_id, Stage::Wide)
         │
         ├─► Discard before send? ──► Consumes slot, 0 unknown tokens
         │
         ▼
      mark_sent() ──► Bytes on wire
         │
         ├─► Valid 200 Response ──► record_response(usage): +known_tokens
         └─► Timeout / Error    ──► record_terminal_failure(): +unknown_usage_attempt
         │
         ▼
admit(Rerank, endpoint)
  ├─► Wide not completed?      ──► Refuse: StageOrderingViolation (exit 2)
  ├─► Budget exhausted?        ──► Refuse: AttemptsExhausted (exit 4, Wide NOT promoted)
  └─► Admitted: Issue AttemptPermit(attempt_id, Stage::Rerank)
         │
         ▼
Final CostReceipt (admitted, sent, completed, known tokens, unknown usage markers)
```

---

## 2. Limits and Invariants

| Parameter | Default | Contract |
|---|---|---|
| `DEFAULT_LOGICAL_REQUESTS` | `2` | Stage 1 Wide + Stage 2 Rerank |
| `DEFAULT_HTTP_ATTEMPTS` | `4` | Total attempts across initial tries and retries |
| `DEFAULT_MIN_ATTEMPT_RESERVE_MS` | `50 ms` | Minimum remaining time before cleanup reserve to admit an attempt |
| `DEFAULT_GUARD_GENERATION` | `1` | Invocation-local guard generation |

### Invariants:
1. **Single Admission Seam:** All HTTP attempts (stages, retries, future breaker probes, evaluation batches) must acquire an [`AttemptPermit`] before transmission. Hidden automatic retries inside HTTP clients are prohibited.
2. **Single-Use Permits:** Each permit binds an [`AttemptId`], target origin, guard generation, monotonic deadline, and stage. A permit is consumed at most once; calling `mark_sent()` twice is rejected.
3. **Known vs. Unknown Usage:**
   - Completed responses accumulate verified `Usage { input_tokens, output_tokens }`.
   - Attempts failed or timed out after `mark_sent()` increment `unknown_usage_attempts` and set `has_unknown_usage = true`.
   - Unknown provider cost is never reported as zero.
4. **Discard Before Send:** When an attempt is discarded prior to network transmission, it consumes an attempt slot but incurs zero unknown token cost.
5. **Stage Progression and No Promotion on Exhaustion:** Rerank admission requires Wide to have completed successfully. If attempts are exhausted before Rerank, Rerank is refused and the Wide winner is **never** promoted as a rerank decision.
6. **Exact Cache Zero-Cost:** Exact cache hits bypass provider admission entirely and produce a [`CostReceipt::zero_cost_cache_hit`] reporting 0 admitted, 0 sent, 0 tokens, and `cache_served = true`.
7. **Operational Refusal Mappings:**
   - Budget exhaustion maps to `ErrorKind::RequestBudget`, `CliExit::Provider` (exit 4).
   - Deadline insufficiency maps to `ErrorKind::Timeout`, `CliExit::Timeout` (exit 6).
   - Stage order violations map to `ErrorKind::InvalidUsage`, `CliExit::Usage` (exit 2).
   - Provider breaker cooldown maps to `ErrorKind::ProviderCooldown`, `CliExit::Provider` (exit 4).

---

## 3. Public Interface

```rust
pub enum RankingStage {
    Wide,
    Rerank,
    Probe,
    Evaluation,
}

pub struct AttemptId(String);

pub struct AttemptBudget { ... }

pub struct AttemptPermit {
    pub fn mark_sent(self) -> Result<SentAttempt, AdmissionError>;
    pub fn discard_before_send(self, reason: impl Into<String>) -> DiscardedAttempt;
}

pub struct CostReceipt {
    pub admitted_attempts: u32,
    pub sent_attempts: u32,
    pub completed_attempts: u32,
    pub known_usage: Usage,
    pub unknown_usage_attempts: u32,
    pub has_unknown_usage: bool,
    pub cache_served: bool,
}

pub struct AttemptAdmission {
    pub fn default_invocation(clock: EntryClock, invocation_id: impl Into<String>) -> Self;
    pub fn admit(&mut self, stage: RankingStage, endpoint: &CanonicalOrigin) -> Result<AttemptPermit, AdmissionRefusal>;
    pub fn record_sent(&mut self, sent: &SentAttempt) -> Result<(), AdmissionError>;
    pub fn record_response(&mut self, sent: &SentAttempt, usage: Usage) -> Result<(), AdmissionError>;
    pub fn record_terminal_failure(&mut self, sent: &SentAttempt, reason: &str) -> Result<(), AdmissionError>;
    pub fn record_discard(&mut self, discarded: &DiscardedAttempt) -> Result<(), AdmissionError>;
    pub fn record_cache_hit(&mut self);
    pub fn receipt(&self) -> &CostReceipt;
    pub fn can_attempt_rerank(&self) -> bool;
}
```
