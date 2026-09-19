# Jev retries and stage accounting

`jev::retry::RetrySession` performs sequential stage calls through a
`jev::client::JevTransport`: the real `JevClient`, or an in-memory test double
that has no origin and never receives a credential. The caller supplies the
process-entry `EntryClock`, scoped credential, attempt budget and trusted
authorization refresh. It retains one admission coordinator across wide and
rerank: by default two logical requests and four HTTP attempts in total. The
HTTP client still has automatic retries disabled. `sr rank` opens one session
per invocation, only once a send is due, and re-authorizes trusted policy before
every attempt. This boundary does not implement the future shared allowance,
batch scheduler, persistent endpoint cooldown, or circuit breaker.

The explicit transient allowlist is HTTP 429, 500, 502, 503, 504 and 529, DNS or
connect failure, and selected connection I/O failures. Authentication, input,
TLS verification, HTTP protocol, invalid answer, cancellation and deadline
failures stop immediately. A POST retry may repeat provider work and incur
another charge; there is no idempotency or billing-refund claim.

The first three backoff bases are 100, 200 and 400 ms, capped at 800 ms for larger
explicit budgets. Asupersync context entropy adds 0–50 ms of jitter. A valid
`Retry-After` raises that base; it never lowers it. Delta-seconds and HTTP-date
are parsed with bounded input using pinned `httpdate` 1.0.3, including its
supported legacy HTTP date formats. This follows the delay/date forms in
[RFC 9110 section 10.2.3](https://www.rfc-editor.org/rfc/rfc9110.html#name-retry-after).
The dependency maps two-digit legacy years to 1970–2069; IMF-fixdate has no
two-digit-year ambiguity. Duplicate, oversized or invalid headers stop retries
instead of silently ignoring a potentially significant cooldown.

Date delays are converted once at response receipt. Subsequent waiting uses
only monotonic time with sub-millisecond precision, preserves the entry deadline, and
checks cancellation at most every 10 ms. A delay that cannot fit together with
the minimum attempt reserve stops immediately; it is never shortened to fit.
Overflow means the delay cannot fit. Output and cleanup keep their original
reserve. These are cooperative runtime bounds, subject to the same blocking
OS/trust-store limitations as the transport.

The authorization callback runs for every attempt so the rank orchestrator can
refresh effective trusted policy. It must reject changed policy or unavailable
required enforcement state. A credential alone is never network consent. The
low-level client checks consent, origin binding and bounded request encoding
again before entering HTTP. Cross-process admission and final publication
revalidation remain separate downstream integration responsibilities.

Each admitted slot is charged once. The pre-HTTP callback marks possible send;
even a DNS or handshake failure then has conservative unknown usage because the
transport does not prove a zero-cost execution. A local refusal before HTTP
records a discard. A dropped in-flight future also finalizes unknown usage
synchronously. No response, malformed usage or terminal transport failure is
ever counted as a known free call. Every stage error carries the cumulative
receipt, including known wide tokens when rerank fails. A cache hit adds zero
new usage without erasing any prior costs.
If adding returned usage would overflow the invocation counters, accounting
refuses it, keeps the preceding exact totals and marks that attempt unknown.

Permit ownership is bound to the issuing coordinator, even if two invocations
reuse the same label. Permits cannot be cloned; sends and completions must follow
admitted → sent → finished, with one active attempt at a time. Duplicate/foreign
completions and duplicate discards fail without changing counts. A new successful
evaluation followed by another evaluation consumes another logical request;
retries within an unfinished stage do not.

Each successful stage retains requested and returned model IDs, wall times,
and monotonic elapsed times. Different requested or returned identities invalidate
the pair while retaining both calls' costs. Matching returned names are explicitly
`MatchingUnpinnedIdentifiers`: the service's unversioned alias does not prove
an atomic or immutable model snapshot. Raw model IDs, bodies, keys and local
invocation IDs are omitted from error formatting.

`tests/jev_retry.rs` exercises the production session over real loopback TLS
using `tests/fixtures/jev-tls/retry_server.py`, the existing synthetic CA and a
synthetic bearer canary. The server independently counts connections and records
bounded counts/timings, while the Rust tests assert usage and refusal behavior.
Pure tests cover exact deadline edges, date parsing, overflow, jitter bounds,
classification, duplicate completion and cross-coordinator ownership. The suite
uses no live Jev service, personal input or maintainer credential.

Run with `RCH_REQUIRE_REMOTE=1 rch exec -- cargo test --locked --test jev_retry
--test transport_contract --test jev_transport -- --nocapture`. Executed commands,
source receipts and results belong in the live bead; this document specifies the
boundary rather than certifying an unexecuted check.
