//! Provider selection through the real resolver and the pipeline's endpoint seam.
//! Synthetic credentials only; constructing a client never sends a request.

use skillranker::config::{
    ConfigLayer, ConfigProblem, ConfigSources, IssueKey, ManagedPolicyMutation, PolicyBoundary,
    PolicyField, Provider, PublicationKind, RawValue, ResolvedConfig, Revalidation, SettingKey,
    ValueSource,
};
use skillranker::identity::ContentHash;
use skillranker::jev::client::JevClient;
use skillranker::jev::endpoint::{CLOUDFLARE_API_ORIGIN, EndpointError};
use skillranker::jev::{CanonicalOrigin, EndpointConfig, OriginScopedCredential};
use skillranker::privacy::{CredentialStatus, EffectFlags, EffectPolicy, NetworkConsent};
use std::ffi::OsString;

const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
const OTHER_ACCOUNT: &str = "fedcba9876543210fedcba9876543210";
const TYPESAFE_TOKEN: &str = "synthetic-typesafe-selected-token";
const CLOUDFLARE_TOKEN: &str = "synthetic-cloudflare-selected-token";

fn var(name: &str, value: &str) -> (OsString, OsString) {
    (name.into(), value.into())
}

fn setting(name: &str, value: &str) -> (String, RawValue) {
    (name.into(), RawValue::String(value.into()))
}

fn inputs(provider: &str) -> ConfigSources {
    ConfigSources {
        trusted_user: vec![setting("provider.kind", provider)],
        environment: vec![
            var("TYPESAFE_API_KEY", TYPESAFE_TOKEN),
            var("CLOUDFLARE_API_TOKEN", CLOUDFLARE_TOKEN),
            var("CLOUDFLARE_ACCOUNT_ID", ACCOUNT),
        ],
        ..Default::default()
    }
}

fn resolve(sources: ConfigSources) -> ResolvedConfig {
    ResolvedConfig::resolve(sources, 1).unwrap()
}

fn effects() -> EffectPolicy {
    EffectPolicy::from_flags(EffectFlags::default()).unwrap()
}

fn endpoint(config: &ResolvedConfig) -> Result<EndpointConfig, EndpointError> {
    // This is the existing rank pipeline's exact endpoint construction seam.
    match config.effective().endpoint() {
        Some(route) => EndpointConfig::from_override(route),
        None => Ok(EndpointConfig::production()),
    }
}

#[test]
fn typesafe_defaults_and_fingerprints_are_not_changed_by_unused_cloudflare_inputs() {
    let empty = resolve(ConfigSources::default());
    let configured = resolve(inputs("typesafe"));
    assert_eq!(configured.effective().provider(), Provider::TypeSafe);
    assert_eq!(configured.effective().model().as_str(), "jev-latest");
    assert_eq!(configured.effective().active_model(), "jev-latest");
    assert!(configured.effective().endpoint().is_none());
    assert_eq!(
        empty.effective().policy_fingerprint(),
        configured.effective().policy_fingerprint(),
    );
    assert_eq!(
        endpoint(&configured).unwrap().target_url().as_str(),
        "https://api.typesafe.ai/v1/systemone"
    );
}

#[test]
fn trusted_cloudflare_selection_reaches_the_existing_client_constructor() {
    let config = resolve(inputs("cloudflare"));
    assert_eq!(config.effective().provider(), Provider::Cloudflare);
    assert_eq!(config.effective().model().as_str(), "typesafe/jev");
    assert_eq!(config.effective().active_model(), "typesafe/jev");
    assert_eq!(
        config.source(SettingKey::ProviderModel),
        ValueSource::Single(ConfigLayer::BuiltIn)
    );
    let route = endpoint(&config).unwrap();
    assert_eq!(route.origin().as_str(), CLOUDFLARE_API_ORIGIN);
    let target = format!("{CLOUDFLARE_API_ORIGIN}/client/v4/accounts/{ACCOUNT}/ai/run");
    assert_eq!(route.target_url().as_str(), target);
    let client = JevClient::new(route).unwrap();
    assert_eq!(client.target_url(), target);
    let credential =
        OriginScopedCredential::bind(config.credential().unwrap().clone(), client.origin())
            .unwrap();
    assert_eq!(
        credential
            .authorization_header_for(client.origin())
            .unwrap(),
        format!("Bearer {CLOUDFLARE_TOKEN}")
    );
    assert!(
        credential
            .authorization_header_for(&CanonicalOrigin::production())
            .is_err()
    );
}

#[test]
fn environment_provider_overrides_trusted_selection_before_defaulting_the_model() {
    let mut sources = inputs("cloudflare");
    sources.environment.push(var("SR_PROVIDER", "typesafe"));
    let config = resolve(sources);
    assert_eq!(config.effective().provider(), Provider::TypeSafe);
    assert_eq!(config.effective().model().as_str(), "jev-latest");
    assert!(config.effective().endpoint().is_none());
    assert_eq!(
        config
            .credential()
            .unwrap()
            .expose_for_authorization_header(),
        TYPESAFE_TOKEN
    );
    assert_eq!(
        config.source(SettingKey::Provider),
        ValueSource::Single(ConfigLayer::Environment)
    );
}

#[test]
fn explicit_native_model_is_preserved_and_foreign_models_are_rejected() {
    let mut sources = inputs("cloudflare");
    sources.environment.push(var("SR_MODEL", "typesafe/jev"));
    let config = resolve(sources);
    assert_eq!(config.effective().model().as_str(), "typesafe/jev");
    assert_eq!(
        config.source(SettingKey::ProviderModel),
        ValueSource::Single(ConfigLayer::Environment)
    );
    for model in [
        "jev-latest",
        "jev-other",
        "@cf/other/model",
        "typesafe/jev?private=1",
    ] {
        let mut sources = inputs("cloudflare");
        sources.environment.push(var("SR_MODEL", model));
        let error = ResolvedConfig::resolve(sources, 1).unwrap_err();
        assert!(error.contains(
            ConfigLayer::Environment,
            &IssueKey::Known(SettingKey::ProviderModel),
            ConfigProblem::Conflict(SettingKey::Provider)
        ));
        assert!(!format!("{error} {error:?}").contains(model));
    }
}

#[test]
fn an_explicit_typesafe_model_is_not_silently_rewritten_on_a_provider_switch() {
    let mut sources = inputs("typesafe");
    sources
        .trusted_user
        .push(setting("provider.model", "jev-pinned-test"));
    let before = resolve(sources);
    assert_eq!(before.effective().model().as_str(), "jev-pinned-test");
    let error = before
        .reresolve_files(
            vec![
                setting("provider.kind", "cloudflare"),
                setting("provider.model", "jev-pinned-test"),
            ],
            vec![],
            2,
        )
        .unwrap_err();
    assert!(error.contains(
        ConfigLayer::TrustedUser,
        &IssueKey::Known(SettingKey::ProviderModel),
        ConfigProblem::Conflict(SettingKey::Provider)
    ));
}

#[test]
fn account_case_is_canonical_but_distinct_accounts_have_distinct_request_targets() {
    let lower = resolve(inputs("cloudflare"));
    let mut sources = inputs("cloudflare");
    sources.environment[2] = var("CLOUDFLARE_ACCOUNT_ID", &ACCOUNT.to_ascii_uppercase());
    let upper = resolve(sources.clone());
    assert_eq!(upper.effective().cloudflare_account_id(), Some(ACCOUNT));
    assert_eq!(
        lower.effective().policy_fingerprint(),
        upper.effective().policy_fingerprint()
    );
    assert_eq!(endpoint(&lower).unwrap(), endpoint(&upper).unwrap());
    sources.environment[2] = var("CLOUDFLARE_ACCOUNT_ID", OTHER_ACCOUNT);
    let other = resolve(sources);
    assert_ne!(
        lower.effective().policy_fingerprint(),
        other.effective().policy_fingerprint()
    );
    assert_ne!(
        endpoint(&lower).unwrap().target_url(),
        endpoint(&other).unwrap().target_url()
    );
    // The durable allowance remains per origin, not a new bucket per account.
    assert_eq!(
        endpoint(&lower).unwrap().origin(),
        endpoint(&other).unwrap().origin()
    );
}

#[test]
fn malformed_accounts_fail_without_leaking_values() {
    for value in [
        "",
        "short",
        "../private-account",
        "0123456789abcdef0123456789abcdeg",
        "0123456789abcdef0123456789abcdef?",
        "１２３４５６７８９０abcdef",
    ] {
        let mut sources = inputs("cloudflare");
        sources.environment[2] = var("CLOUDFLARE_ACCOUNT_ID", value);
        let error = ResolvedConfig::resolve(sources, 1).unwrap_err();
        assert!(error.contains(
            ConfigLayer::Environment,
            &IssueKey::Known(SettingKey::CloudflareAccountId),
            ConfigProblem::InvalidValue
        ));
        if !value.is_empty() {
            assert!(!format!("{error} {error:?}").contains(value));
        }
    }
}

#[test]
fn incomplete_native_setup_never_falls_back_to_the_typesafe_endpoint() {
    let mut sources = inputs("cloudflare");
    sources
        .environment
        .retain(|(name, _)| name != "CLOUDFLARE_ACCOUNT_ID");
    let config = resolve(sources);
    assert!(config.effective().endpoint().is_some());
    assert_eq!(config.effective().model().as_str(), "typesafe/jev");
    assert_eq!(
        endpoint(&config),
        Err(EndpointError::InvalidCloudflareAccount)
    );
    assert_eq!(
        CanonicalOrigin::from_override(config.effective().endpoint().unwrap())
            .unwrap()
            .as_str(),
        CLOUDFLARE_API_ORIGIN
    );
}

#[test]
fn another_providers_token_does_not_satisfy_native_credential_readiness() {
    let mut sources = inputs("cloudflare");
    sources
        .environment
        .retain(|(name, _)| name != "CLOUDFLARE_API_TOKEN");
    let config = resolve(sources);
    assert!(config.credential().is_none());
    assert_eq!(config.credential_status(), CredentialStatus::Absent);
    assert_eq!(
        endpoint(&config).unwrap().origin().as_str(),
        CLOUDFLARE_API_ORIGIN
    );
}

#[test]
fn native_tokens_are_bounded_validated_and_kept_out_of_diagnostics() {
    for token in [
        "bad token".to_owned(),
        "bad\nheader".to_owned(),
        "x".repeat(4097),
    ] {
        let mut sources = inputs("cloudflare");
        sources.environment[1] = var("CLOUDFLARE_API_TOKEN", &token);
        let error = ResolvedConfig::resolve(sources, 1).unwrap_err();
        assert!(!format!("{error} {error:?}").contains(&token));
    }
    let config = resolve(inputs("cloudflare"));
    let route = endpoint(&config).unwrap();
    let text = format!("{config:?} {:?} {route:?}", config.receipt(effects()));
    for private in [ACCOUNT, TYPESAFE_TOKEN, CLOUDFLARE_TOKEN] {
        assert!(!text.contains(private));
    }
}

#[test]
fn empty_native_token_is_absent_not_a_typesafe_fallback() {
    let mut sources = inputs("cloudflare");
    sources.environment[1] = var("CLOUDFLARE_API_TOKEN", "");
    let config = resolve(sources);
    assert!(config.credential().is_none());
    assert_eq!(config.credential_status(), CredentialStatus::Absent);
}

#[test]
fn project_configuration_cannot_select_routes_accounts_or_credentials() {
    for (key, value) in [
        ("provider.kind", "cloudflare"),
        ("cloudflare.account_id", ACCOUNT),
        ("cloudflare.api_token", CLOUDFLARE_TOKEN),
    ] {
        let mut sources = inputs("typesafe");
        sources.project.push(setting(key, value));
        let error = ResolvedConfig::resolve(sources, 1).unwrap_err();
        assert!(error.contains(
            ConfigLayer::Project,
            &IssueKey::Known(SettingKey::from_path(key).unwrap()),
            ConfigProblem::ForbiddenInLayer
        ));
    }
}

#[test]
fn account_and_token_remain_environment_only_even_in_trusted_user_config() {
    for (key, value) in [
        ("cloudflare.account_id", ACCOUNT),
        ("cloudflare.api_token", CLOUDFLARE_TOKEN),
    ] {
        let mut sources = inputs("cloudflare");
        sources.trusted_user.push(setting(key, value));
        let error = ResolvedConfig::resolve(sources, 1).unwrap_err();
        assert!(error.contains(
            ConfigLayer::TrustedUser,
            &IssueKey::Known(SettingKey::from_path(key).unwrap()),
            ConfigProblem::ForbiddenInLayer
        ));
    }
}

#[test]
fn native_selection_cannot_be_redirected_by_a_typesafe_override() {
    let mut sources = inputs("cloudflare");
    sources
        .environment
        .push(var("TYPESAFE_ENDPOINT", "https://other.example"));
    let config = resolve(sources);
    assert_eq!(
        endpoint(&config).unwrap().origin().as_str(),
        CLOUDFLARE_API_ORIGIN
    );
    let unchanged = resolve(inputs("cloudflare"));
    assert_eq!(
        config.effective().policy_fingerprint(),
        unchanged.effective().policy_fingerprint()
    );
}

#[test]
fn an_origin_string_alone_does_not_select_the_native_protocol() {
    let mut sources = inputs("typesafe");
    sources
        .environment
        .push(var("TYPESAFE_ENDPOINT", CLOUDFLARE_API_ORIGIN));
    let config = resolve(sources);
    let client = JevClient::new(endpoint(&config).unwrap()).unwrap();
    assert_eq!(
        client.target_url(),
        format!("{CLOUDFLARE_API_ORIGIN}/v1/systemone")
    );
    assert_eq!(
        config
            .credential()
            .unwrap()
            .expose_for_authorization_header(),
        TYPESAFE_TOKEN
    );
}

#[test]
fn provider_changes_revoke_admission_and_advisory_publication_but_not_explicit_results() {
    let config = resolve(inputs("cloudflare"));
    let receipt = config.receipt(effects());
    let changed = config
        .reresolve_files(vec![setting("provider.kind", "typesafe")], vec![], 2)
        .unwrap();
    assert_eq!(
        changed
            .credential()
            .unwrap()
            .expose_for_authorization_header(),
        TYPESAFE_TOKEN
    );
    assert_eq!(changed.effective().model().as_str(), "jev-latest");
    for boundary in [
        PolicyBoundary::ProviderAdmission,
        PolicyBoundary::CliPublication(PublicationKind::Advisory),
        PolicyBoundary::HookPublication(PublicationKind::Advisory),
    ] {
        assert!(
            matches!(receipt.compare(&changed.receipt(effects()), boundary), Revalidation::Superseded(fields) if fields.contains(&PolicyField::Provider))
        );
    }
    assert_eq!(
        receipt.compare(
            &changed.receipt(effects()),
            PolicyBoundary::CliPublication(PublicationKind::Explicit)
        ),
        Revalidation::Unchanged
    );
}

#[test]
fn a_fixed_environment_provider_override_survives_mutable_file_edits() {
    let mut sources = inputs("typesafe");
    sources.environment.push(var("SR_PROVIDER", "cloudflare"));
    let config = resolve(sources);
    let changed = config
        .reresolve_files(vec![setting("provider.kind", "cloudflare")], vec![], 2)
        .unwrap();
    assert_eq!(
        config.receipt(effects()).compare(
            &changed.receipt(effects()),
            PolicyBoundary::ProviderAdmission
        ),
        Revalidation::Unchanged
    );
    assert_eq!(
        config.effective().policy_fingerprint(),
        changed.effective().policy_fingerprint()
    );
    assert_eq!(
        changed
            .credential()
            .unwrap()
            .expose_for_authorization_header(),
        CLOUDFLARE_TOKEN
    );
}

#[test]
fn selected_credentials_are_preserved_when_files_are_reresolved() {
    let config = resolve(inputs("typesafe"));
    let changed = config
        .reresolve_files(vec![setting("provider.kind", "cloudflare")], vec![], 2)
        .unwrap();
    assert_eq!(
        changed
            .credential()
            .unwrap()
            .expose_for_authorization_header(),
        CLOUDFLARE_TOKEN
    );
    assert_eq!(changed.effective().model().as_str(), "typesafe/jev");
    assert_eq!(
        endpoint(&changed).unwrap().target_url().as_str(),
        format!("{CLOUDFLARE_API_ORIGIN}/client/v4/accounts/{ACCOUNT}/ai/run")
    );
}

#[test]
fn provider_selection_and_credential_presence_do_not_grant_network_consent() {
    let config = resolve(inputs("cloudflare"));
    assert_eq!(
        config.network_consent(effects()),
        NetworkConsent::NotAuthorized
    );
    let offline = EffectPolicy::from_flags(EffectFlags {
        offline: true,
        ..Default::default()
    })
    .unwrap();
    assert!(matches!(
        config.network_consent(offline),
        NetworkConsent::Blocked(_)
    ));
    let dry = EffectPolicy::from_flags(EffectFlags {
        dry_run: true,
        ..Default::default()
    })
    .unwrap();
    assert!(matches!(
        config.network_consent(dry),
        NetworkConsent::Blocked(_)
    ));
}

#[test]
fn managed_policy_changes_cannot_enable_a_provider() {
    let error = ManagedPolicyMutation::new(
        ContentHash::from_bytes(b"synthetic"),
        vec![setting("provider.kind", "cloudflare")],
    )
    .unwrap_err();
    assert!(error.contains(
        ConfigLayer::TrustedUser,
        &IssueKey::Known(SettingKey::Provider),
        ConfigProblem::NotManagedPolicy
    ));
}
