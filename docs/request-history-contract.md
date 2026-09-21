# Active request history

Task-anchor recovery and directive extraction select history before interpreting
user prose. A sibling's `/use-skill` or `/exclude-skill` is not a requirement of
the active request, even when it appears later in the input vector.

`context::anchor::history::select` is the local selection boundary. Parent-linked
histories use the existing qualified branch resolver. The current request is the
leaf when its scoped event is present. A prompt not yet recorded may use a unique
leaf in the same scope; an ambiguous graph, cycle, conflicting definition, or a
current ID present only in a foreign scope is refused, not flattened or sorted by
time. Missing ancestry remains incomplete and never imports a foreign parent.

Flat normalized histories without parent links retain their existing supplied
sequence order, but only in the envelope's exact agent and branch namespace.
Unknown/unassigned scope is not a wildcard. Selection ends before the current
request event when present. Conflicting definitions are refused and identical
redelivery is deduplicated within that scope. This compatibility path is not a
claim that timestamps can reconstruct a native fork or establish loaded state.

The authoritative current prompt is interpreted separately, once. A stale copy
of its identified event cannot introduce a contradictory directive or serve as
the antecedent for `continue`. Distinct turns with equal text are not deduplicated.
All selected historical user directives are still inspected before message
windowing; active task boundaries still stop continuation recovery. Existing
conflict, exclusion, and all-or-nothing explicit-resolution rules are unchanged.

This selection grants no network, read, load, or persistence authority and does
not modify the input envelope. `tests/task_history_scope.rs` checks parent order,
sibling directives and boundaries, qualified flat histories, current-prompt
authority, ambiguous/malformed graphs, and the actual offline CLI explicit path.

## Provider disclosure

Both public render entry points use the same selected history before redaction,
argument summarization, essential-tool checks, and message/scalar windowing.
Sibling volume cannot evict active context from the window, and sibling bodies
cannot enter the provider request. The existing payload implementation is retained
unchanged in the private `render::payload` module; public types and helper paths
remain re-exported from `context::render`.

Excluded history and tool records are counted in their disclosure categories,
without exposing names, scope IDs, arguments, or bodies. Counts include exclusions
before the profile's own omissions, without double counting. Excluding a known
sibling is not a gap in the active history. Missing active ancestry is a gap:
its payload and receipt are marked partial (never overriding insufficient), and
the receipt byte count is recomputed from the final inspected payload.

`tests/render_history_scope.rs` checks both APIs and both disclosure profiles,
permuted input, foreign reused IDs, receipt totals, incomplete/ambiguous history,
and the actual production dry-run request. These tests do not establish live
provider quality, a new native adapter contract, or new load evidence.

## Session evidence is also provider input

Both rendering APIs prepare a separate, bounded provider view of session state.
Loaded-reference names and summaries, the loaded-state label, and explicit
exclusion strings are sanitized and fully redacted before any truncation. The
caller's local loaded records and exact exclusions are never modified. Redacted
names are display prose only, not a replacement for local invocation authority.
The caller's redactor (including its entropy setting) remains in force, followed
by inspection of the final serialized context.

Session input is preflighted without copying bodies: at most 10,000 combined
reference/exclusion records and 1 MiB of aggregate UTF-8 field bytes. Session
output has a separate 4,096-scalar allowance, shared across its label, exclusion
strings, reference names, and summaries. The existing 12,000-scalar request/history
allowance is unchanged, as is the final 96 KiB serialized-request ceiling shared
with project signals, roster options, and questions. These are upper bounds, not
a promise every combination fits the final wire limit.

The evidence-state label and all exclusions reserve space first. They remain
whole after redaction; inability to fit them is an explicit context error, not a
silent removal of a user constraint. Optional references use the remainder in
input order, up to 32 records and 700 scalars per summary. Names are never
truncated into other names. A reference with an empty or unrepresentable name is
omitted without preventing a later bounded reference from fitting. Reference
summary truncation occurs only after full-field redaction and includes a visible
omission marker when the remaining space permits it.

The session-state receipt counts included/omitted records, truncated summary
fields, and actual scanner redactions. Redactions count fields scanned during
selection, including a scanned name later omitted for lack of space; skipped
bodies are not claimed as scanned. All totals are recomputed after integrating
session and history accounting. Omitting/truncating reference evidence marks
context partial, never upgrading an insufficient context. `disclosed_scalars`
retains its existing request/history meaning; `disclosed_bytes` covers the final
whole payload, including the session view.

`tests/render_session_state.rs` exercises both production APIs and the receipt
verifier with secrets in every session field, Unicode/exact-bound allocation,
mandatory-constraint overflow, optional omissions, source limits, both profiles,
and immutable local inputs. No additional provider call, discovery, or persistence
is performed to build this view.

## Tool labels are transcript text

Tool names are redacted in the selected provider view before message windowing,
not copied verbatim until the final payload inspector rejects a secret-bearing
name. The complete label is scanned before it is shortened to at most 128 Unicode
scalars, with a head/tail omission marker. Aggregate label input is bounded to
1 MiB. Long labels cannot consume the entire message budget and displace the
active task or tool result. Names used for local dispatch and load attribution
remain unchanged; a shortened provider label creates no callable authority.

Name redactions and truncations contribute to the existing tool-event receipt
category and all totals. A truncated label marks the context partial without
overriding insufficient context. `--no-tools`, the minimal profile, and scope
selection happen before label processing: excluded labels cannot trigger a
privacy refusal, and their contents are not claimed as inspected. The final
serialized payload inspection remains mandatory.

`tests/render_tool_labels.rs` includes both API/receipt checks and an isolated
CLI dry-run that requires a real provider preview with a redacted tool label and
preserved result, plus the no-tools counterpart. No live Jev request is made.
