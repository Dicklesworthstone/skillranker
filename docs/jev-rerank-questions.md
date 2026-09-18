# Detailed rerank questions (P4)

`src/jev/rerank.rs` builds the second Jev request over the actual shortlist
that the wide stage selected, and reads its answers into the estimates that
[local eligibility](eligibility.md) consumes.

## Request

| Key | Type | Question |
| --- | --- | --- |
| `rerank` | Choice | Compare the shortlisted skills for the next step, given the user's constraints. Any number, including none, may be suitable; `__none__` when none would help. |
| `fits::<option>` | Noul, one per real candidate | Would this described skill help the specific next step, given the user's constraints? Judged on its own. |

Options use the request-local handles of the shortlist's option map (`o000`, …).
The keys are opaque, so every question carries the skill's meaning itself:
- **Each `rerank` option** is a JSON object with the redacted name, a
  description excerpt of up to 1,000 scalars, and a body excerpt of up to 700.
- **Each `fits::` question** holds the fixed evaluator instructions followed by
  its own skill's name and description as JSON. It names no other skill.

The original "exactly one is right" framing is gone; every candidate may be
unsuitable.

Skill text is untrusted. It is redacted before truncation. The body excerpt is
cut from a lookahead window the parser keeps (700 + 1,024 scalars), so a
secret that starts inside the excerpt is recognized whole. The text appears
only as JSON-quoted data after the fixed instructions, which never change
whatever the skill says. The assembled request is inspected for secret-shaped
text before it may be sent.

Answers are decoded against the request. Foreign or missing options and
questions are rejected, and every value maps back through the local option
map, so a response can yield only shortlisted skills and numbers, never a
name, path, command or endpoint. `shortlist_id()` is a local digest of the
options, bindings and content. It stays on the machine and keys the stage for
caching and revalidation.

## Budget

The request is capped at 96 KiB. Over budget, the oldest recent messages go
first, and dropped context is never restored. Only once all of it is gone do
the excerpts shrink, in this order:
1. drop the body excerpt;
2. cut descriptions to 400 scalars;
3. cut descriptions to 160 scalars.

The shortlist, the sentinel and the latest request are never reduced. An
unfittable request is `RequestTooLarge`. The wide request follows the same
order.

## Verification

`tests/jev_rerank.rs` covers:
- opaque keys with meaningful per-candidate fit questions, and no local IDs or paths in the bytes;
- injected skill text confined to quoted data under byte-identical instructions;
- foreign options and missing fit questions rejected at decode;
- only local skills from a valid answer;
- redaction of a secret straddling the body cut, and refusal of a secret in the state;
- an oversized 32-candidate request trimmed in order, and an unfittable one refused;
- shortlist identity across membership and content;
- answers flowing into local eligibility with the per-candidate none check;
- empty and oversized shortlists.

`tests/jev_wide.rs` now also checks that wide excerpts shrink only after all
older context is gone.
