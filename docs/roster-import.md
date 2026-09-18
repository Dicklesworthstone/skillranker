# Explicit roster import (P2)

`src/roster/import.rs` validates an explicit `--roster FILE` inventory. An
explicit roster replaces candidate *enumeration*: discovery is not run, and
nothing on disk that the manifest omits is added. It does not replace
permissions or path validation. Every ID, path, digest and eligibility claim in
the manifest is untrusted until the selected adapter derives the same value from
the bytes it actually reads.

The library boundary is implemented and tested. Wiring `--roster` into
`sr rank` belongs to the rank orchestrator, which does not exist yet; this build
does not advertise the flag as an implemented command.

## Manifest schema

```json
{
  "schema": "sr.roster.v1",
  "harness": "claude_code",
  "mode": "authorized_files",
  "skills": [
    {
      "source": "claude_code.project",
      "path": "review/SKILL.md",
      "invocation": "review",
      "content_hash": "<64 lowercase hex BLAKE3 digest>",
      "agent_invocable": true,
      "user_invocable": true
    }
  ]
}
```

Every object rejects unknown and duplicate keys. There is no field for
visibility, precedence or priority: those come only from the adapter.
`harness` must equal the selected adapter's harness; it never selects one.

| Record field | `authorized_files` | `synthetic_text` |
| --- | --- | --- |
| `source`, `path` | Required | Rejected |
| `text` | Rejected | Required, at most 256 KiB |
| `invocation` | Optional claim | Required |
| `id`, `content_hash`, `agent_invocable`, `user_invocable` | Optional claims | Optional claims |

An omitted claim is derived; a present claim must equal the derived value.

## Authorized files

`import_authorized(bytes, plan, overrides, cx, clock)` takes the selected
adapter's `DiscoveryPlan` and the same trusted restrict-only overrides that
discovery uses. Only Claude Code has a verified layout, so any other adapter is
refused with `UnsupportedHarness`. For each record:

1. `source` must name a root the plan declares *and opened*. Other harnesses'
   sources, and Claude's plugin and managed sources (which this build does not
   enumerate), are `UnknownSource`.
2. `path` must be relative with plain components. Absolute paths, empty
   components, `.` and `..` are refused before any filesystem access.
3. The path must have Claude's `<name>/SKILL.md` layout, via the same helper
   discovery uses. The invocation name and the stable ID are derived exactly
   as discovery derives them, so an imported record and a discovered record for
   one file have equal IDs.
4. Two records resolving to one ID are a `DuplicateDefinition`.
5. A symlinked skill directory is refused, as discovery refuses to descend
   one. The file is then read through `AuthorizedRoots`, so a symlinked
   `SKILL.md` may resolve only into an authorized root. FIFOs, devices and
   oversized files are refused.
6. The digest of the bytes read must equal any `content_hash` claim. After
   collision resolution, the effective restrictions (frontmatter, trusted
   overrides and same-file folding) must equal any eligibility claim. A
   manifest can neither grant eligibility the file or the settings withhold nor
   hide a restriction.

Import is all-or-nothing: the first failing record rejects the manifest. A
successful import is a complete `ResolvedRoster` (`is_partial() == false`),
with the adapter's visibility and personal-over-project precedence.

## Synthetic text

`import_synthetic(bytes, harness)` accepts a `synthetic_text` manifest for
offline evaluation. Records are parsed with the same frontmatter parser, get
stable IDs from the `synthetic.text` namespace and their invocation name, and
must use unique names. The result is a `SyntheticRoster`. It has no load
target, no advisory view, no exact resolution and no conversion into a
`ResolvedRoster`, so synthetic input cannot become live hook advice.
`import_authorized` refuses a synthetic manifest with `ModeMismatch`, and the
reverse is also refused.

## Bounds

| Bound | Value | On overflow |
| --- | --- | --- |
| Manifest bytes | 32 MiB (`EXPLICIT_ROSTER_JSON_BYTES`) | `TooLarge`, before parsing |
| Records | 10,000 (`EXPLICIT_ROSTER_RECORDS`) | `TooManyRecords`, at record 10,001 |
| Nesting | Fixed schema depth 3, below the 64 ceiling | `Malformed` |
| One skill file or text | 256 KiB | Record `TooLarge` |
| Total skill bytes read | 32 MiB (`DISCOVERY_PARSED_BYTES`) | `ByteBudget` |

`read_roster_file` reads the manifest itself as a bounded regular file; any
symlink in it must stay under the file's own directory. Parsing is typed and
streaming: records are counted as they arrive rather than after the whole array
is built. Authorized import checks the invocation deadline and cancellation
before each record and read; synthetic import is pure CPU work over the
already-bounded manifest and takes no clock.

## Errors

`ImportError` carries a record index and a kind: never a path, name, digest,
link target, rejected key or content. `kind()` maps every failure to
`unusable-roster` (exit 5), a deadline to `timeout`, and returns `None` for
cancellation, which follows the signal path.

## Verification

`tests/roster_import.rs` builds real trees and covers:
- a verified inventory whose IDs equal discovery's, which keeps personal-over-project shadowing and never adds unlisted files;
- escaped paths (lexical, symlink target, symlinked directory), FIFOs and missing files, with a symlinked-file success twin;
- duplicate definitions and keys;
- false content, digest, invocation, ID and eligibility claims, including attempts to override frontmatter or trusted settings, with honest counterparts;
- harness, source and mode mismatches;
- malformed and deeply nested input;
- the byte and record ceilings at exactly the limit and one past it;
- synthetic evaluation-only import;
- diagnostics free of path and content canaries;
- bounded manifest file reads and cancellation.
