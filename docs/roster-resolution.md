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
and partial coverage. An unreadable root, walk limits, or an unsupported layout
(whose callable name is unknowable) withhold invocation authority globally.
For per-file metadata or read errors where the callable name is known from the
supported `<name>/SKILL.md` layout, authority is withheld specifically for the
invocation names the failed candidates could claim (preventing shadowed winners),
continuing past individual file errors so remaining valid records stay inspectable
and advisory. Declared unenumerated sources remain separately disclosed by discovery;
a caller's verified visibility assertion must cover the actual session before
using any advice. This is an adapter API, not proof of conformance with a running
Claude installation.

Limits are 10,000 inputs, 256 KiB per file, and 32 MiB cumulative read/parse bytes.
Cancellation and the invocation deadline are checked around filesystem work and
through resolution. Blocking filesystem syscalls remain cooperative; they are
not forcibly interruptible. This capture does not replace mandatory pre-publication
roster/content revalidation, which is a separate integration boundary.

Tests in `tests/roster_resolution.rs` exercise real files, hard links, content
rewrites, collisions, restrictions, local option maps, and privacy-safe debug
output. They establish local resolution behavior, not live Jev or hook readiness.
