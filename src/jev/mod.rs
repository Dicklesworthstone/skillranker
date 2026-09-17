//! TypeSafe Jev client, endpoint canonicalization, request/response codecs,
//! and admission policies.
//!
//! TypeSafe.ai's Jev is the essential ranking engine for SkillRanker.
//! This module provides pure base-origin canonicalization, origin-scoped credential
//! routing, redirect prohibition, and request/response validation.

pub mod endpoint;
pub mod codec;

pub use endpoint::{
    CanonicalOrigin, CredentialRoutingError, EndpointConfig, EndpointError,
    OriginScopedCredential, ProxyPolicy, RedirectError, RedirectPolicy, Scheme,
    TargetUrl, DEFAULT_TYPESAFE_ENDPOINT, SKILLRANKER_USER_AGENT, SYSTEMONE_PATH,
};
