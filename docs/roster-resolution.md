# Roster identity and invocation resolution

`roster::resolution` connects bounded authorized reads and metadata parsing to
local skill identities, invocation eligibility, and request-local option IDs.
It performs no network calls, substitutions, skill execution, or persistence.

`SkillEntry::from_read` accepts a trusted adapter's `BindingSpec` and an
`authorized_read::BoundedRead`. Metadata and content hash come from the same
bytes. The adapter supplies the callable name, source, logical identity,
visibility contract, comparable priority, and effective restriction ceiling.
Frontmatter cannot supply those assertions or create callable aliases.

`ResolvedRoster::resolve` deduplicates device/inode identities. It retains each
source binding and its ID, invocation, visibility, and restrictions. Inconsistent
hashes for the same physical file reject the snapshot. The smallest binding ID
selects a deterministic representative record; consumers use the returned
**binding** for invocation decisions, not the representative's name. Content
changes do not alter a binding ID. Changing the logical source or key does.

A callable-name collision needs a shared verified contract and known priority.
A unique highest-priority physical file wins; tied or unknown precedence is
ambiguous. A forbidden or manual-only winner never promotes a lower-priority
file. Multiple winning bindings for the same callable intersect restrictions.
Distinct callable aliases retain their own effective restrictions.

`exact_name` and `exact_id` return typed references or missing, ambiguous,
shadowed, unverified, and forbidden outcomes. Manual-only references contain an
ID and invocation name, not a file-reading instruction. The user-invocable flag
alone does not disable agent advice. Natural-language directive parsing and CLI
aggregation belong to the explicit-requirements boundary, not this module.

`OptionMap::new` takes eligible binding IDs from the complete roster or later
retrieval. It rejects ineligible bindings and duplicate physical targets, sorts
by canonical ID, and assigns `o000` through `o253`. Only that map resolves a
provider selection; names, paths, foreign IDs, and excluded aliases cannot gain
authority through a response. `__none__` is separate. An empty map is allowed.
These records are local data; outgoing text still requires privacy redaction.

## Claude directory adapter

`resolve_claude_plan` reads only direct project/personal `<directory>/SKILL.md`
entries through authorized roots. Its logical key hashes the native declared
path, retaining stable identity across content edits and file replacement at
that path while distinguishing workspaces. Moving a skill changes that key.

The adapter follows the documented personal-over-project precedence and uses
the directory for the callable name; frontmatter `name` supplies display text.
It excludes the reserved `synced` directory. Effective settings are trusted,
restrict-only inputs. See the [Claude skill contract](https://code.claude.com/docs/en/skills).

Nested, plugin, managed, synced, and legacy-command discovery are not implemented
here. Unsupported layouts and parsing/read failures produce bounded diagnostics
and partial coverage. An unsupported layout has no callable name under this
adapter's direct-layout contract: it stays excluded without revoking authority
from unrelated valid skills. Ordinary notes and marker files do not become skill
candidates. Partial coverage is never evidence that no skill exists.

### Discovery gaps and per-name authority

A skipped directory link or an exhausted walk/byte bound does **not** automatically
make every resolved entry unverified. Nor is a readable singleton automatically a
winner: an omitted definition in another root could shadow it. When discovery has
such a gap, resolution checks the exact `<name>/SKILL.md` slot for every observed
name in every supported root. A slot is accounted for only when it is absent and
has no captured binding, or it matches the physical identity of the binding
already read and parsed at that **source and declared path**. An unseen file is
not admitted by this check, even if it aliases a file observed elsewhere.

These probes use the pinned authorized root descriptors and never descend
symlinked skill directories. Unknown/unreadable slots, directory links, dangling
file links, and changed or unobserved files withhold only their invocation names.
An entirely unreadable supported root still withholds every name because none of
its slots can be proved absent. A valid file symlink already admitted by the
authorized reader can retain its binding; a skipped link does not acquire one.
Manual-only/forbidden winners, precedence, deduplication, and option-map checks
are unchanged. A proof never upgrades an adapter's unverified visibility.

The original discovery diagnostics and partial-coverage flag remain present,
including `symlinked-directory-skipped`, `entry-limit`, and `byte-limit`. They
remain source-level and path-free. Proofs are local, metadata-only, deterministic
by invocation name, and capped at 10,000 name/root probes per resolution pass.
Exhaustion adds per-candidate `Limit` diagnostics and withholds only names whose
proof did not finish; completed names retain their authority. This separate
verification bound does not increase the 10,000-entry discovery ceiling, read
skipped skill contents, or continue the stopped enumeration.

Per-file metadata/read failures and exhaustion of the cumulative parse allowance
also withhold the failed candidates' known names rather than revoking previously
resolved unrelated entries. Declared unenumerated sources remain separately
disclosed by discovery; a caller's verified visibility assertion must cover the
actual session before using any advice. This is an adapter API, not proof of
conformance with a running Claude installation. Explicit roster import retains
its separate all-or-nothing validation contract and does not gain directory-link
support from this change.

Limits are 10,000 inputs, 256 KiB per file, and 32 MiB cumulative read/parse bytes.
Cancellation and the invocation deadline are checked around filesystem work,
between name/root probes, and through resolution. Blocking filesystem syscalls
remain cooperative; they are not forcibly interruptible. These metadata probes
do not replace mandatory pre-publication roster/content revalidation. That
boundary builds a fresh plan and repeats resolution, so a newly hidden competitor
can invalidate advice even when the number of discovery diagnostics is unchanged.
No scan freezes files for a later harness load.

Tests in `tests/roster_resolution.rs` exercise real files, hard links, content
rewrites, collisions, restrictions, local option maps, and privacy-safe debug
output. `tests/roster_discovery_gaps.rs` adds the issue #4 symlink regression,
actual discovery/parse ceilings, unseen competing names, and publication checks.
The resolver's internal proof tests cover missing/changed slots, dangling links,
and deterministic proof-budget exhaustion. These establish local contracts, not
live Jev or hook readiness.
