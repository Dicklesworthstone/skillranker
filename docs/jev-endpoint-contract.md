# TypeSafe Endpoint Canonicalization and Credential Routing Contract

## 1. Scope and Mission

TypeSafe.ai's Jev is the essential ranking engine for SkillRanker (`sr`).
All recommendations for live agent turns originate from Jev evaluations.
This document defines the strict origin canonicalization, credential routing,
redirect prohibition, and proxy safety contracts governing all network interactions
with TypeSafe.ai endpoints.

## 2. Base Origin Identity and Canonicalization

The `TYPESAFE_ENDPOINT` setting is strictly a **base origin**, not a full target path.

### 2.1 Scheme Invariants
- Production endpoints **must** use `https://`.
- Insecure `http://` is strictly prohibited for non-loopback hosts and returns `EndpointError::InsecureScheme`.
- Insecure `http://` is permitted **only** for loopback addresses in local test/development harnesses:
  - IPv4 loopback: `127.0.0.0/8` (e.g. `127.0.0.1`).
  - IPv6 loopback: `::1` (bracketed `[::1]`).
  - Named loopback: `localhost`.
- Unsupported schemes (`ftp://`, `ws://`, `file://`, etc.) return `EndpointError::UnsupportedScheme`.
- Scheme names are matched case-insensitively (`HTTPS://` and `https://` are equivalent).

### 2.2 Canonical Form and Normalization Rules
Equivalent spellings of the same base origin are normalized to an identical canonical string
representation (`CanonicalOrigin::as_str()` and `origin_key()`). This guarantees that equivalent
endpoints share the exact same cache namespace and attempt-budget scope:
1. **Scheme**: Lowercase (`https` or `http`).
2. **Host**: Lowercase ASCII or RFC 3492 Punycode IDN (`xn--...`).
   Trailing root dots (e.g. `api.typesafe.ai.`) are stripped.
   Raw non-ASCII Unicode characters in hostnames are rejected with `EndpointError::NonAsciiHostForbidden`
   to eliminate UTS-46 / NFC case-mapping ambiguity and DNS-splitting attacks.
3. **Port**: Explicit default ports (`443` for HTTPS, `80` for HTTP) are omitted.
   Non-default ports (e.g. `8443`) are preserved in canonical output.
   Ports must be strictly numeric without leading `+` or leading `0`, within `1..=65535`.
4. **Path**: Trailing slashes are stripped. Non-root paths are prohibited.

### 2.3 Strict Host and IP Syntax Enforcement
- **IPv6 Literals**: Must be bracketed (e.g. `[::1]`, `[::1]:8080`) per RFC 3986 section 3.2.2.
  Unbracketed IPv6 literals (e.g. `::1`, `::1:8080`) are rejected with `EndpointError::UnbracketedIpv6Forbidden`.
- **Numeric IPv4 Aliases**: Decimal integer aliases (e.g. `2130706433`), hex addresses (`0x7f.0.0.1`),
  octal notation (`0177.0.0.1`), and leading zeros in octets (`127.0.0.01`) are rejected with
  `EndpointError::NumericIpv4AliasForbidden`. Only canonical 4-octet dotted-decimal IPv4 is permitted.

### 2.4 Default Origin
The default production origin is:
```text
https://api.typesafe.ai
```

## 3. Target URL Resolution and One-Time Joining

SkillRanker queries Jev via the SystemOne API.
The target evaluation endpoint is formed by appending `/v1/systemone` **exactly once**
to the canonical base origin:
```text
https://api.typesafe.ai/v1/systemone
```

### Prohibited Path Constructions
- Supplying an origin that already contains `/v1/systemone` or `/v1` returns `EndpointError::DuplicatePathJoin`.
- Supplying any subpath (e.g. `https://api.typesafe.ai/eval`) returns `EndpointError::NonRootPathForbidden`.

## 4. Rejection of Userinfo, Queries, and Fragments Without Input Echo

To prevent token leakage and injection attacks:
- **Userinfo** (`https://user:pass@host`): Strictly forbidden. Credentials must never be passed in the URL.
  Returns `EndpointError::UserinfoForbidden`.
- **Query strings** (`https://host/?key=val`): Forbidden in base origins.
  Returns `EndpointError::QueryForbidden`.
- **Fragments** (`https://host/#frag`): Forbidden in base origins.
  Returns `EndpointError::FragmentForbidden`.

### Zero Untrusted Input Echo in Diagnostics
All `EndpointError` and `CredentialRoutingError` variants are non-echoing: they contain static descriptions
and never store or print input paths, ports, hostnames, query strings, or canaries.

## 5. Origin-Scoped Credential Routing

`TYPESAFE_API_KEY` is held in memory as an `ApiCredential` and bound to an authorized
origin via `OriginScopedCredential`:

1. **No Insecure Transmission**: Binding an `ApiCredential` to an unencrypted HTTP origin
   (including loopback HTTP) fails immediately with `CredentialRoutingError::InsecureHttpForbidden`.
   Loopback HTTP is strictly credential-free.
2. **Single-Origin Isolation**: An `OriginScopedCredential` emits an `Authorization: Bearer <token>`
   header **only** when the request target matches the bound canonical origin.
   Requests targeting any other origin or domain fail with `CredentialRoutingError::OriginMismatch`.
3. **Redacted Diagnostics**: `OriginScopedCredential` implements `Debug` and `Display`
   by emitting `<redacted>`, preventing leakage in logs or stack traces.

## 6. Strict Redirect Prohibition and Terminal Injection Defense

HTTP redirects (status codes `300..=399`) are strictly disabled by policy:
- `RedirectPolicy::validate_response_status` rejects any 3xx response with `RedirectError::RedirectForbidden`.
- Redirects to third-party or attacker servers are never followed.
- If a redirect `Location` header is logged in diagnostics, it is first passed through
  `sanitize_url_for_diagnostics`:
  - Passwords containing `@` are completely redacted by splitting at the *last* `@` in authority.
  - Queries without preceding `/` and fragments (`#`) are scrubbed.
  - Control characters (such as ESC `\x1b`, BEL `\x07`, `\r`, `\n`) are replaced with `?` to prevent terminal injection.
  - URLs are bounded to at most 256 characters, appending `...[TRUNCATED]` on overflow.

## 7. Ambient Proxy Safety

SkillRanker defaults to direct connections.
Ambient proxy environment variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `http_proxy`,
`https_proxy`, `all_proxy`, `NO_PROXY`, `no_proxy`) are not implicitly trusted.
When inspecting the environment, `ProxyPolicy` scrubs any userinfo, queries, or fragments
from detected proxy URLs before inclusion in receipts or diagnostics.
