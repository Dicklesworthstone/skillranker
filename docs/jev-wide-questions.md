# Wide questions and gate policy (P4)

`src/jev/wide.rs` builds the first Jev request over every admitted candidate and
interprets its answers. Explicit references are resolved locally before this
stage; an empty admission never reaches it.

## Request

| Key | Type | Question |
| --- | --- | --- |
| `which` | Choice | Which available skill would most help the next step? `__none__` means no listed skill adds useful guidance. |
| `gate::specialized_method` | Noul | Would the next step benefit from a specialized method, reference, or procedure? |
| `gate::material_help` | Noul | Would consulting a relevant skill materially improve correctness or execution here? |
| `gate::context_suffices` | Noul, inverted | Is the existing context sufficient without consulting any additional skill? |
| `phase` | Choice | planning, implementing, debugging, testing, reviewing, releasing, conversing, or other |
| `stuck` | Optional Noul | Is there evidence of repeated failure? Off by default |

`which` has one option per admitted candidate, at most 254, plus the typed
sentinel, for 255 in total. Options use request-local handles (`o000`, …) from
the roster's option map, never skill IDs or paths. Each option description is
the callable name, then `: ` and a redacted excerpt of the description of at
most 160 Unicode scalars. Names and descriptions are redacted before
truncation. The rendered context payload is the request `state`. The request
is serialized once and capped at 96 KiB (`MAX_REQUEST_BYTES`). The exact bytes
are inspected for secret-shaped text before they may be sent, and an assembled
request that still contains any is refused (`Privacy`).

Over budget, trimming follows a fixed order. First the oldest recent messages
go, one at a time; the latest request is always kept. Then the description
excerpts shrink, through 160, 80 and 40 scalars to the name alone. Admitted
candidates and the sentinel are never dropped. If even that cannot fit, the
build fails with `RequestTooLarge`, whose decision reason is
`request-too-large` and whose output kind is `oversized-input`. That is never
reported as a relevance abstention. `trimming()` reports the messages dropped,
the excerpt cap used, the omitted description scalars and the redaction count,
for dry runs.

## Gate and shortlist

`needs_skill = (specialized_method + material_help + (1 - context_suffices)) / 3`.
It is a heuristic over correlated answers, not a calibrated probability. Below
the gate (default 0.30), the decision is `abstain / low-need` and no rerank
runs. Otherwise the best `M_effective` real candidates are shortlisted by raw
wide probability, with ties falling to the stable skill ID. The sentinel never
takes a slot, and a winning sentinel alone does not end the pass: the detailed
rerank can rescue a lookalike or poorly described skill. The sentinel's raw
probability, the choice confidence and the phase distribution are kept as
evidence.

`Sizes::new(K, M)` validates `1 <= K <= M <= 32` (defaults K = 5, M = 8)
before any adaptation. Then `M_effective = min(M, admitted)` and
`K_effective = min(K, M_effective)`, so a single Quill hit shortlists and
returns one.

## Verification

`tests/jev_wide.rs` builds real rosters from files. It covers:
- no request for 0 or 255 candidates;
- the exact reviewed bytes of the one-candidate request (`tests/fixtures/jev-wide-one.json`);
- the pinned length and digest of the 254-candidate request, with `stuck` on;
- defaults with one hit;
- the low-gate skip, with a lower-gate twin;
- sentinel rescue;
- the M bound with stable ties;
- size and gate validation before clamping;
- deterministic trimming that drops the oldest context first, keeps every candidate and the latest request, and refuses an unfittable request;
- redaction across the excerpt boundary, and refusal of a secret in the state.
