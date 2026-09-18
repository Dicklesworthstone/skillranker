# Publication revalidation (P2)

`src/roster/revalidation.rs` checks, before a live advisory decision, explicit
result or no-match claim is published, that the roster evidence the decision
used still holds. Wiring the check into publication belongs to the rank
pipeline.

## Dependencies

A decision depends on more than the skills it returns. `capture` records:

- the membership, precedence (visibility, including the shadowing winner) and
  effective restrictions of every binding in the resolved scope;
- the content hash of every file named as decision content. The caller names
  all indexed or wide candidates, the whole shortlist including candidates
  scoring removed, and any explicit targets;
- records excluded while resolving, and the scope's `partial` flag;
- the causes that already made part of the scope unobservable;
- the capture time on the invocation clock.

So a changed wide candidate outside the shortlist, a new overflow candidate, a
new shadowing source, or a restriction change on a filtered shortlist candidate
each invalidates the decision, even when every returned skill is unchanged.
Changes that are not decision dependencies do not: for example, the body of a
manual-only skill that was not decision content and whose restrictions did not
change. An explicit result names its targets as content; the whole-scope
precedence covers the names that resolve them.

## Revalidation

No adapter here offers a trusted generation that covers every dependency, so
revalidation always re-enumerates. `revalidate_claude` builds a fresh plan,
re-opening the roots by path so that a replaced root is noticed, which reusing
the old descriptors would miss. It then re-resolves within the invocation
deadline and compares digests. Size and modification time are never used.

| Outcome | Meaning | Output kind |
| --- | --- | --- |
| `Validated { captured_at, validated_at }` | Every dependency matches | Publish |
| `Changed` | A dependency differs | `unavailable / roster-changed` |
| `Incomplete` | A root, directory or entry became unreadable, or a walk limit was hit, beyond what the capture already had | `unavailable / incomplete-roster` |
| `Deadline` | The invocation deadline ran out | `timeout` (exit 6) |
| `Cancelled` | The invocation was cancelled | Signal path |

Revalidation uses the caller's clock and never extends it, and it returns no
alternative candidate, so no runner-up can be substituted. A partial scope, for
example one with plugin and managed sources that are not enumerated, still
validates positively when the evidence it used is unchanged. It remains
explicitly scoped rather than being treated as complete. A scan is an
observation, not a freeze: the harness still validates its own later load.

## Verification

`tests/roster_revalidation.rs` mutates real Claude project and personal trees
between capture and revalidation. It covers:
- an unchanged partial scope, with a non-dependency edit twin;
- a changed wide candidate outside the shortlist;
- a new overflow candidate;
- a new personal shadow;
- a restriction change on a removed shortlist candidate, through trusted overrides with unchanged bytes;
- explicit target content and name precedence;
- an unreadable root reported as incomplete rather than changed;
- an exhausted deadline that is not extended, and cancellation;
- undeclared dependencies.
