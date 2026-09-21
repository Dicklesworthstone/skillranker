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
