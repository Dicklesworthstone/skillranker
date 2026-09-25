# Harness root discovery (P2)

`src/roster/discovery.rs` enumerates the skill files a *selected* harness could
load right now. It does not parse frontmatter (`sr-roadmap-l1i.3.3`), resolve
identities or collisions (`.3.5`), or revalidate a decision's dependencies
(`.3.11`). Reads of candidate bytes go through the authorized-read primitive
(`.3.1`), which repeats every containment check.

## Only declared roots, never a union

A `DiscoveryPlan` carries one `HarnessId` and the roots that harness's adapter
declares. Building a Claude Code plan yields the documented project root
(`<workspace>/.claude/skills`) and user root (`<home>/.claude/skills`) and
nothing else: no `.codex/skills`, no `./skills`, no `/mnt/skills`, and no
ancestor directory, because the plan is given explicit paths rather than a
search. Each root is opened once into an `AuthorizedRoot`, so the descriptor,
not the name, anchors the walk.

`claude_code_plan` additionally declares Claude's plugin and managed sources as
**not enumerated by this build**. That keeps the result honestly partial instead
of implying a complete inventory of what Claude can load.

| Outcome | Result | Partial? |
| --- | --- | --- |
| Optional root absent | `RootMissing` | No, this is normal |
| Configured root unreadable or not a directory | `RootUnreadable` | Yes |
| Documented source this build does not enumerate | `SourceNotEnumerated` | Yes |
| Directory unreadable mid-walk | `DirectoryUnreadable` | Yes |
| Symlinked directory | `SymlinkedDirectorySkipped` | Yes |
| Depth bound reached | `DepthLimitReached` | Yes |
| Entry or byte ceiling reached | `EntryLimitReached` / `ByteLimitReached` | Yes |

A partial pass can never support a global "no skill exists" claim.

## Bounded, descriptor-based enumeration

Every directory is listed through its own descriptor, and children are opened
relative to it with `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`. Symlinked
directories are not descended: a link that replaces a directory mid-walk is
refused rather than followed, which also means cycles cannot occur. Entry types
come from `readdir` where the filesystem supplies them and from
`fstatat(AT_SYMLINK_NOFOLLOW)` where it does not.

A directory below a root that holds the skill file is one skill, so its other
subdirectories (references, scripts, assets) are not descended: they are that
skill's own files, never further skills. Roots and container directories
without a skill file are still descended, so a nested skill file there is still
a candidate. On a measured personal root this walked 278 of 1,281 directories.

`DiscoveryLimits::defaults()` are the plan's documented ceilings: 10,000 entries
examined and 32 MiB of candidate bytes (`DISCOVERY_FILES` and
`DISCOVERY_PARSED_BYTES`), plus `MAX_ROOT_DEPTH` (8) directory levels. Reaching
any ceiling stops enumeration, records the diagnostic and marks the pass
partial. Tests drive the same code paths with small injected limits and separately
assert that the defaults equal the documented values.

## Candidates

A candidate names one file whose name equals the root's declared skill file
(`SKILL.md` for Claude). It carries its source, kind, priority, visibility, the
path relative to its root, the local path, `(device, inode)` identity and size.
Candidates are ordered by priority descending, then source, then relative path.
Claude's documented precedence is enterprise > personal > project, so user
(personal) roots precede project roots deterministically.

A symlinked *skill file* is a candidate, flagged `via_symlink`, because the
authorized read still decides whether its target lies inside an authorized root.
A symlinked *directory* is skipped, as above.

Visibility is whatever the adapter declared: a generic configured root with no
load contract stays `Unverified`, and discovery never upgrades it.

## Deliberate limits

- Unix only, like the authorized-read primitive.
- Plugin, managed and legacy sources are disclosed rather than guessed.
- A pass is an observation at a moment, not a freeze; the harness still
  validates its own later load.
- Diagnostics name a `SourceId` only. No path, entry name, link target or file
  content appears in them.

## Verification

`tests/roster_discovery.rs` builds real trees and covers: documented Claude
roots only with no cross-harness union and no ancestor traversal; personal (user)
before project precedence; unenumerated sources forcing partial; missing (normal) versus
unreadable (partial) roots; entry, byte and depth ceilings, each with a
generous-bound success twin; symlinked directory skipped while a symlinked skill
file is flagged and shares its target's inode; only the declared file name
matching, with a second declared name finding a disjoint set and invalid names
rejected; generic roots keeping `Unverified` visibility; and diagnostics
containing no path or content canary.
