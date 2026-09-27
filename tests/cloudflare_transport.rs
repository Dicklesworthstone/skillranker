//! Explicit live Cloudflare smoke test. The request contains synthetic data only.

#![cfg(unix)]

use asupersync::Cx;
use serde_json::json;
use skillranker::config::{ConfigSources, ResolvedConfig};
use skillranker::jev::OriginScopedCredential;
use skillranker::jev::cloudflare::CloudflareClient;
use skillranker::jev::codec::{Answer, Question, Request};
use skillranker::limits::DurationMillis;
use skillranker::privacy::{ConsentSource, NetworkConsent};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::ffi::OsString;

const CONSENT: NetworkConsent = NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag);

#[test]
fn cloudflare_configuration_selects_its_credential_and_default_model() {
    let config = ResolvedConfig::resolve(
        ConfigSources {
            environment: vec![
                (OsString::from("SR_PROVIDER"), OsString::from("cloudflare")),
                (
                    OsString::from("CLOUDFLARE_ACCOUNT_ID"),
                    OsString::from("0123456789abcdef0123456789abcdef"),
                ),
                (
                    OsString::from("CLOUDFLARE_API_TOKEN"),
                    OsString::from("synthetic-cloudflare-token"),
                ),
            ],
            ..Default::default()
        },
        1,
    )
    .unwrap();
    assert_eq!(config.effective().provider().as_str(), "cloudflare");
    assert_eq!(config.effective().active_model(), "typesafe/jev");
    assert!(config.credential().is_some());
}

#[test]
#[ignore = "live Cloudflare request: set SKILLRANKER_CLOUDFLARE_LIVE_CONSENT=1 explicitly"]
fn cloudflare_live_synthetic_probe() {
    assert_eq!(
        std::env::var("SKILLRANKER_CLOUDFLARE_LIVE_CONSENT").as_deref(),
        Ok("1"),
        "explicit live consent is required"
    );
    let account_id = std::env::var("CLOUDFLARE_ACCOUNT_ID").expect("Cloudflare account ID");
    let token = std::env::var("CLOUDFLARE_API_TOKEN").expect("Cloudflare API token");
    let config = ResolvedConfig::resolve(
        ConfigSources {
            environment: vec![
                (OsString::from("SR_PROVIDER"), OsString::from("cloudflare")),
                (OsString::from("SR_MODEL"), OsString::from("typesafe/jev")),
                (
                    OsString::from("CLOUDFLARE_ACCOUNT_ID"),
                    OsString::from(&account_id),
                ),
                (
                    OsString::from("CLOUDFLARE_API_TOKEN"),
                    OsString::from(&token),
                ),
            ],
            ..Default::default()
        },
        1,
    )
    .expect("Cloudflare configuration should resolve");
    let client = CloudflareClient::new(config.effective().cloudflare_account_id().unwrap())
        .expect("Cloudflare client should initialize");
    let credential =
        OriginScopedCredential::bind(config.credential().unwrap().clone(), client.origin())
            .expect("credential should bind to Cloudflare origin");
    let request = Request::new(
        config.effective().active_model().to_owned(),
        json!("synthetic Cloudflare smoke test; return a small score"),
        [(
            "fit".into(),
            Question::Noul {
                instructions: json!("Return a bounded numeric score for this synthetic probe."),
                criteria: None,
            },
        )],
    )
    .unwrap();
    let clock = EntryClock::capture_with(
        DurationMillis::new("test-total", 30_000, 30_000).unwrap(),
        DurationMillis::new("test-cleanup", 100, 30_000).unwrap(),
    )
    .unwrap();
    let host_clock = EntryClock::capture_with(
        DurationMillis::new("harness-total", 60_000, 120_000).unwrap(),
        DurationMillis::new("harness-cleanup", 5_000, 120_000).unwrap(),
    )
    .unwrap();
    let invocation = ProcessInvocation::from_clock(host_clock).unwrap();
    let cx: Cx = invocation.request_cx().unwrap();
    let response = invocation.runtime().block_on(client.send(
        &request,
        Some(&credential),
        CONSENT,
        &cx,
        &clock,
    ));
    let response = response.expect("Cloudflare should return a validated Jev response");
    assert!(matches!(response.answers["fit"], Answer::Noul(value) if (0.0..=1.0).contains(&value)));
    assert_eq!(response.requested_model, config.effective().active_model());
    assert!(response.returned_model.starts_with("jev-"));
    assert!(invocation.shutdown(), "owned runtime must shut down");
}
