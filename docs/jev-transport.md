# Authenticated Jev HTTPS transport

`jev::client::JevClient` connects the typed Jev codec to the pinned Asupersync
HTTP/TLS stack. It posts to the canonical origin's `/v1/systemone` path using
an origin-scoped TypeSafe credential. Network consent is required independently
of credential presence. The caller supplies redacted request state and reserves
the invocation-wide attempt allowance before calling this single-attempt seam.

The client checks consent, credential routing and the entry deadline before
connection establishment. The remaining work window bounds DNS, connection,
handshake and body receipt through Asupersync's per-request timeout. Cancellation
and deadline are checked again after receipt and JSON validation. A late result
cannot become a timely ranking result.
These are cooperative deadlines; blocking OS and trust-store calls retain the
owning runtime's documented limitations.
An owned 10 ms timer also checks cancellation while a response body is stalled;
the same HTTP future is polled throughout and is dropped on cancellation. This
addresses the pinned client's lack of a cancellation wakeup during that read.

The default Cargo features select native trust roots. Explicit additional roots
are accepted only through the trusted library constructor, with count/size bounds;
there is no accept-all verifier. This client requires HTTPS. It does not expose
the endpoint parser's separate credential-free HTTP development exception.

Requests are capped by the existing 96 KiB codec. Response bodies are bounded
to 2 MiB in the HTTP reader and again by the codec. The client requests identity
encoding and rejects compressed responses rather than inflating them. Only JSON
success responses are decoded; duplicate content-type/encoding headers, redirects,
non-success status codes and invalid answer maps fail explicitly.

Automatic redirects, retries, ambient proxies and cookies are disabled. Each
exchange requests `Connection: close`; no idle connection is retained between
calls. Retry/backoff and process-level signal handling belong to the owning
invocation, not a hidden second transport or background worker.

Errors contain closed kinds and bounded HTTP status codes. They never retain
library error strings, response bodies, request text, credentials or endpoint
spellings. Once the HTTP client has been entered, `http_attempt_started` is true
even for failure: the caller cannot infer zero provider cost. This flag is not a
claim that bytes reached Jev. Successful responses retain exact returned usage
through the typed codec.

`tests/jev_transport.rs` exercises actual loopback TLS sockets through a Python
standard-library TLS server, including authenticated POST, chain and hostname
rejection, pre-connect policy checks, response limits, cancellation and teardown.
The checked-in CA/certificate/key are public synthetic test material trusted only
when explicitly added by these tests. They are not machine or service credentials.
The CA signing key is not included. The separately gated live TypeSafe test sends
only a synthetic probe and requires an explicit invocation with the user's key;
local TLS results alone do not establish live provider compatibility.

The [API reference](https://docs.typesafe.ai/api),
[model reference](https://docs.typesafe.ai/models), and
[state guide](https://docs.typesafe.ai/concepts/state), checked on 2026-09-17,
do not specify maximum model context, criteria length, question count, or
response bytes. The 96 KiB/2 MiB bounds above are application safety limits,
not claims about provider capacity. A successful synthetic probe establishes
that request's compatibility only; it does not qualify the maximum-size ranking
request or turn an alias into an immutable model revision.

Run the ordinary socket suite with `rch exec -- cargo test --locked --test
jev_transport`. To exercise public DNS, native roots and the actual provider,
run the resulting test binary on a machine with an explicitly supplied
`TYPESAFE_API_KEY`, selecting
`--ignored --exact actual_typesafe_https_accepts_synthetic_typed_request`.
Do not forward the maintainer's key to compilation workers. The live case sends
one small synthetic request and retains normal certificate verification.

Transport support does not itself expose `sr rank`, choose skills, install hooks,
authorize raw disclosure, or claim native harness conformance.
