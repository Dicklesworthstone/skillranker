//! TypeSafe Jev client, endpoint canonicalization, request/response codecs,
//! and admission policies.
//!
//! TypeSafe.ai's Jev is the essential ranking engine for SkillRanker.
//! This module provides pure base-origin canonicalization, origin-scoped credential
//! routing, redirect prohibition, and request/response validation.

pub mod codec;
pub mod endpoint;

pub use endpoint::{
    AMBIENT_PROXY_VARS, CanonicalOrigin, CredentialRoutingError, DEFAULT_TYPESAFE_ENDPOINT,
    EndpointConfig, EndpointError, OriginScopedCredential, ProxyPolicy, RedirectError,
    RedirectPolicy, SKILLRANKER_USER_AGENT, SYSTEMONE_PATH, Scheme, TargetUrl,
};
