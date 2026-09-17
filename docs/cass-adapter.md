# Optional cass archive adapter

`context::cass` connects exact local archive selection to SkillRanker's owned
subprocess runner. It probes the installed producer, runs bounded `sessions` or
`export` commands, and returns private archive data with explicit cass provenance.
It does not index, start a daemon, contact Jev, install a hook, or select the
latest session implicitly. CLI dispatch is a separate integration boundary.

The caller supplies a trusted resolved executable, its independently qualified
binary digest, an absolute working directory, and an explicit database path.
Those values must come from trusted local configuration, never transcript text.
The adapter does not hash a potentially large executable inside the request.
The caller is responsible for keeping executable provenance current.

## Producer and process boundary

Only the observed cass 0.8.0 / API 1 / contract 1 combination is accepted. Its
capabilities must advertise JSON output, export, self-description, and the
required sessions/export arguments. Future versions require new qualification.
An unsupported producer fails before export; there is no format guessing.

Arguments are separate argv elements. Every child has an empty environment;
provider credentials, proxies, HOME, PATH, and loader settings are not inherited.
Output is capped at 256 KiB for capabilities and 8 MiB for sessions/export, with
16 KiB stderr. The existing runner owns deadline/cancellation, pipe draining,
process-group termination and direct-child reaping. Kernel spawn/I/O limitations
are the runner's documented limitations, not a hard real-time guarantee.

Offline, local-only, and dry-run requests fail before even the capability probe.
A successful earlier probe cannot bypass that check on subsequent operations.
Subprocess stderr, private paths, messages and malformed JSON never enter errors
or Debug output. Raw records are deliberately accessible only through the local
`native_value()` API and must not be logged or sent directly to a provider.

## Exact source and normalized projection

v1 accepts only cass's `local` source ID and passes `--source local` explicitly.
Other source IDs and hosted records are rejected. `ArchiveExport::provenance()`
uses `SourceProvenance::Cass`, never the native adapter or normalized-input
namespace. A listing result is a candidate, not a grant of filesystem authority
or proof that a session is live. The selecting caller must authorize the source.

The actual 0.8.0 JSON export preserves native message envelopes. The checked-in
synthetic input and exported output were obtained by running the installed cass
binary with an empty environment; they contain no user session or secret.
The ignored `actual_installed_cass_exports_synthetic_session` test additionally
runs the installed binary through this Rust adapter when `SR_TEST_CASS` points
to its absolute path. Recorded-shape tests alone are not live producer proof.

`normalized_events()` explicitly projects Claude-style message envelopes, flat
role/content records, and role/content records inside a payload. It preserves
source event and parent IDs, text, and known tool-call/result IDs. Tool blocks
without event IDs stay without event IDs; they do not receive invented source
identities. Unknown content is flagged incomplete; unsupported record shapes
fail explicitly. Native timestamps remain available in the raw records; this
projection does not invent Unix times. It does not establish an active branch.
Tool-result success is unknown unless an explicit result status supplies it.

No-tools processing belongs to SkillRanker. It removes tool-role messages and
tool blocks, projects only known text fields, and drops unknown payload fields;
it does not rely on cass's formatting switches to suppress tool data. Cass strips
skill injections by default, so load evidence remains unknown. Missing injection
text never means a skill was not loaded, and archive support never inherits
native hook conformance.

## Listing and resource limits

Session listing requests a positive limit up to 1,000. It preserves source IDs,
filters remote sources and nonmatching workspace paths, and reports rejected
counts. A full page or any rejected row marks the inventory incomplete. There
is no fake pagination cursor or claim that a recent-session page is exhaustive.
Caller-resolved canonical workspace spelling must be used for exact comparison.

JSON is duplicate-key rejecting and depth-bounded by the existing adapter parser.
Exports contain at most 10,000 records; normalization also caps expanded events
at 10,000. Oversized or malformed responses fail atomically. All child operations
share the invocation clock and reject results after the work window.
