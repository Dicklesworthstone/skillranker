# Identity contracts (P0)

The library defines local identity, normalized context and roster record types.
These types do not implement native adapters, filesystem discovery, ranking,
provider requests, persistence or delivery. The input example in
[`tests/fixtures/normalized-context.v1.json`](../tests/fixtures/normalized-context.v1.json)
is synthetic and deliberately lacks durable attribution.

`SessionIdentity` separates source provenance, workspace, session, agent, branch
and context epoch. A durable namespace requires every dimension. Native source
provenance includes adapter and version; normalized provenance includes producer,
declared harness and schema version. A normalized envelope never becomes native
evidence by claiming a native harness name. Its workspace is independently
resolved by the trusted caller; the declared path grants no filesystem access.
Missing attribution produces an invocation-local namespace using a fresh nonce
supplied by the entry boundary. Neither equal prompt text nor a wall-clock value
alone provides a safe nonce.

Raw local namespace containers intentionally have no generic serialization
implementation. Future private persistence codecs and public receipts must each
declare their fields. Identifier Debug formatting hides its value. Normalized
context is serializable for explicit local input interchange, **not** for provider
payloads or routine logs. Text and local paths also have private Debug output.
Option IDs, invocation names and display names hide their values in Debug too;
terminal sanitization alone does not remove private content. Explicit local
serialization remains lossless. Raw Serde decoding errors can contain input text;
the production normalized parser maps them to fixed public diagnostics instead.

Opaque identifiers are nonempty and at most 512 UTF-8 bytes. They reject
whitespace, controls and bidi controls. This bound does not replace outer reader
byte, nesting or record limits. Derived struct deserializers reject repeated and
unknown named fields. `validate_definitions` separately rejects repeated event
IDs and repeated supplied-load definitions. A current request may reference an
existing event; equal text in two different events remains two turns. Use
`skillranker::context::parse_normalized_context(&[u8]) -> Result<NormalizedContext, ContextError>`
for production input decoding. It checks the 1 MiB (1,048,576-byte) limit before
decoding, enforces JSON depth 64, and rejects duplicate keys at every object level
before they can be discarded by a generic JSON map. It rejects malformed JSON,
trailing input, unknown fields and invalid field values, then validates schema
version 1 and event/supplied-load definitions before returning the context.
`ContextError` exposes fixed diagnostics without rejected input text.

The parser is a pure, bounded local-byte boundary, not a filesystem or stdin
reader. It does not perform source selection, filesystem authorization, context
windowing, native ingestion or provider serialization. Parsed paths and identities
remain declarations; normalized identity grants no native authority.

Unknown event IDs remain null and cannot prove a causal association or load observation.
`SessionIdentity::event_key` requires both complete session attribution and a
concrete event ID before constructing a `DurableEventKey`; it confers no observed
load status. Adapters still have to establish that the event belongs to that
session and branch.
Parent references may point outside a bounded history; later adapters must retain
that gap instead of fabricating the missing record.

`SuppliedLoadClaim` always reports supplied provenance. It cannot deserialize an
observed-origin field. `LoadObservation` is a separate adapter-produced local type;
its state and source/rendered hashes keep attempted, observed, unavailable and
censored knowledge distinct. None of these types alone proves successful native
loading or authorizes a durable observation write.

`SkillId::from_source` uses domain-separated, length-framed BLAKE3 over source and
logical key. Editing skill content changes its independent content hash, not its
stable ID. Invocation name, sanitized display name, source aliases and local load
target remain separate. A syntactically accepted invocation name is still subject
to the selected adapter's grammar and verified visibility. Restrictions can
resolve to agent, manual-only or forbidden; later eligibility code must also
check shadowing and ambiguous visibility. Option IDs are distinct request-local
handles. Both skill and option constructors reserve `__none__`; the sentinel has
its own enum variant.

The focused tests cover attribution separation, harness/producer isolation,
null unknowns, duplicate definitions and nested keys, repeated references,
content versions, aliases, invocation restrictions, Unicode byte limits, private
Debug output and non-UTF-8 local paths. They do not claim native harness or live
provider conformance.
