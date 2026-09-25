use skillranker::config::*;
use skillranker::context::parse_normalized_context;
use skillranker::identity::ContentHash;
use skillranker::output::{CliExit, ErrorKind};
use skillranker::privacy::*;
use std::collections::BTreeSet;
use std::ffi::OsString;

/// Synthetic canary, never a real credential; diagnostics must not contain it.
const CANARY: &str = "canary-credential-0123456789";

fn s(text: &str) -> RawValue {
    RawValue::String(text.to_owned())
}

fn list(items: &[&str]) -> RawValue {
    RawValue::StringList(items.iter().map(|item| (*item).to_owned()).collect())
}

fn entry(path: &str, value: RawValue) -> (String, RawValue) {
    (path.to_owned(), value)
}

fn var(name: &str, value: &str) -> (OsString, OsString) {
    (name.into(), value.into())
}

fn effects(flags: EffectFlags) -> EffectPolicy {
    EffectPolicy::from_flags(flags).unwrap()
}

fn resolve(sources: ConfigSources) -> Result<ResolvedConfig, ConfigErrors> {
    ResolvedConfig::resolve(sources, 1)
}

fn user(entries: Vec<(String, RawValue)>) -> ConfigSources {
    ConfigSources {
        trusted_user: entries,
        ..ConfigSources::default()
    }
}

fn known(key: SettingKey) -> IssueKey {
    IssueKey::Known(key)
}

fn assert_private(text: &str, secrets: &[&str]) {
    for secret in secrets {
        assert!(
            !text.contains(secret),
            "diagnostic leaked {secret:?}: {text}"
        );
    }
}

#[test]
fn registry_is_complete_ordered_and_internally_consistent() {
    assert_eq!(KEY_SPECS.len(), SettingKey::ALL.len());
    let mut paths = BTreeSet::new();
    let mut env_names = BTreeSet::new();
    let mut flags = BTreeSet::new();
    for (index, key) in SettingKey::ALL.iter().copied().enumerate() {
        let spec = key.spec();
        assert_eq!(KEY_SPECS[index].key, key, "registry order");
        assert_eq!(SettingKey::from_path(spec.path), Some(key));
        assert!(paths.insert(spec.path), "duplicate path {}", spec.path);
        assert_eq!(
            spec.environment.is_some(),
            spec.environment_rule != LayerRule::Forbidden,
            "{}: environment binding and rule disagree",
            spec.path
        );
        assert_eq!(
            spec.cli_flag.is_some(),
            spec.cli != LayerRule::Forbidden,
            "{}: flag binding and rule disagree",
            spec.path
        );
        if let Some(name) = spec.environment {
            assert!(env_names.insert(name), "duplicate env {name}");
            assert_eq!(SettingKey::from_environment_name(name), Some(key));
            assert!(name.starts_with("SR_") || name.starts_with("TYPESAFE_"));
        }
        if let Some(flag) = spec.cli_flag {
            assert!(flags.insert(flag), "duplicate flag {flag}");
        }
        assert_eq!(spec.rule(ConfigLayer::BuiltIn), LayerRule::Forbidden);
        match spec.sensitivity {
            Sensitivity::Reserved => {
                for layer in [
                    ConfigLayer::TrustedUser,
                    ConfigLayer::Project,
                    ConfigLayer::Environment,
                    ConfigLayer::Cli,
                ] {
                    assert_eq!(spec.rule(layer), LayerRule::Forbidden, "{}", spec.path);
                }
            }
            Sensitivity::NetworkConsent
            | Sensitivity::Credential
            | Sensitivity::Routing
            | Sensitivity::TranscriptAccess
            | Sensitivity::AdviceInjection => {
                assert_eq!(spec.project, LayerRule::Forbidden, "{}", spec.path)
            }
            Sensitivity::DisclosureVolume => {
                assert_eq!(spec.project, LayerRule::RestrictOnly, "{}", spec.path)
            }
            Sensitivity::Ordinary | Sensitivity::SkillRootAccess => {}
        }
        if spec.managed_policy {
            assert_eq!(spec.sensitivity, Sensitivity::Ordinary, "{}", spec.path);
            assert_eq!(spec.trusted_user, LayerRule::Allowed, "{}", spec.path);
        }
    }
}

#[test]
fn defaults_match_the_documented_contract_and_grant_nothing() {
    let config = resolve(ConfigSources::default()).unwrap();
    let e = config.effective();
    assert_eq!((e.top(), e.shortlist()), (5, 8));
    assert_eq!((e.gate(), e.fits()), (0.30, 0.30));
    assert_eq!((e.w_fit(), e.w_prior(), e.w_phase()), (1.0, 0.0, 0.0));
    assert_eq!(e.timeout_ms(), 3_000);
    assert_eq!((e.messages(), e.budget_chars()), (12, 12_000));
    assert_eq!(e.model().as_str(), "jev-latest");
    assert_eq!(e.hook_mode(), HookMode::Shadow);
    assert_eq!(e.notification_turns(), NotificationTurns::Skip);
    assert_eq!(e.context_profile(), ContextProfile::Standard);
    assert!(!e.no_tools());
    assert!(!e.trusted_user_network_enabled());
    assert!(e.endpoint().is_none());
    assert!(e.transcript_roots().is_empty());
    assert!(e.exclude_skills().is_empty());
    assert!(e.roster_roots().is_empty());
    assert_eq!(config.credential_status(), CredentialStatus::Absent);
    assert_eq!(
        config.network_consent(effects(EffectFlags::default())),
        NetworkConsent::NotAuthorized
    );
    for key in SettingKey::ALL {
        assert_eq!(
            config.source(*key),
            ValueSource::Single(ConfigLayer::BuiltIn)
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    Accepted,
    Forbidden,
    Reserved,
    UnknownEnvironmentName,
}

/// Written independently of `KEY_SPECS`: a registry edit that loosens trust
/// must also change this table to pass.
#[test]
fn security_sensitive_keys_follow_an_independent_layer_matrix() {
    use Expect::{Accepted as A, Forbidden as F, Reserved as R, UnknownEnvironmentName as U};
    #[rustfmt::skip]
    let matrix: &[(&str, &str, RawValue, &str, [Expect; 4])] = &[
        // path                      env name               file or CLI value            env text               user/project/env/cli
        ("network.enabled",          "SR_NETWORK_ENABLED",  RawValue::Bool(true),        "true",                [A, F, U, F]),
        ("network.proxy",            "SR_NETWORK_PROXY",    s("https://proxy.example"),  "x",                   [R, R, U, R]),
        ("typesafe.api_key",         "TYPESAFE_API_KEY",    s(CANARY),                   CANARY,                [F, F, A, F]),
        ("typesafe.endpoint",        "TYPESAFE_ENDPOINT",   s("https://api.example"),    "https://api.example", [F, F, A, F]),
        ("provider.model",           "SR_MODEL",            s("jev-other"),              "jev-other",           [A, F, A, F]),
        ("hook.mode",                "SR_HOOK_MODE",        s("shadow"),                 "advisory",            [A, F, U, A]),
        ("hook.notification_turns",  "SR_NOTIFICATION_TURNS", s("skip"),                 "skip",                [A, A, U, F]),
        ("context.profile",          "SR_CONTEXT_PROFILE",  s("minimal"),                "minimal",             [A, A, U, A]),
        ("context.no_tools",         "SR_NO_TOOLS",         RawValue::Bool(true),        "true",                [A, A, U, A]),
        ("context.messages",         "SR_MESSAGES",         RawValue::Integer(6),        "6",                   [A, A, A, A]),
        ("context.budget_chars",     "SR_BUDGET_CHARS",     RawValue::Integer(4_000),    "4000",                [A, A, A, A]),
        ("context.transcript_roots", "SR_TRANSCRIPT_ROOTS", list(&["/transcripts"]),     "/t",                  [A, F, U, F]),
        ("roster.roots",             "SR_ROSTER_ROOTS",     list(&["skills"]),           "skills",              [A, A, U, F]),
        ("ranking.timeout_ms",       "SR_TIMEOUT_MS",       RawValue::Integer(2_500),    "2500",                [A, F, A, A]),
        ("privacy.redaction",        "SR_REDACTION",        RawValue::Bool(false),       "false",               [R, R, U, R]),
        ("privacy.raw_retention",    "SR_RAW_RETENTION",    RawValue::Bool(true),        "true",                [R, R, U, R]),
    ];
    let covered: BTreeSet<&str> = matrix.iter().map(|row| row.0).collect();
    for spec in &KEY_SPECS {
        if spec.sensitivity != Sensitivity::Ordinary {
            assert!(
                covered.contains(spec.path),
                "{} missing from matrix",
                spec.path
            );
        }
    }

    for (path, env_name, value, env_text, expected) in matrix {
        let key = SettingKey::from_path(path).unwrap();
        let attempts = [
            (
                ConfigLayer::TrustedUser,
                user(vec![entry(path, value.clone())]),
            ),
            (
                ConfigLayer::Project,
                ConfigSources {
                    project: vec![entry(path, value.clone())],
                    ..ConfigSources::default()
                },
            ),
            (
                ConfigLayer::Environment,
                ConfigSources {
                    environment: vec![var(env_name, env_text)],
                    ..ConfigSources::default()
                },
            ),
            (
                ConfigLayer::Cli,
                ConfigSources {
                    cli: vec![entry(path, value.clone())],
                    ..ConfigSources::default()
                },
            ),
        ];
        for ((layer, sources), expect) in attempts.into_iter().zip(expected) {
            let result = resolve(sources);
            let case = format!("{path} at {}", layer.as_str());
            match expect {
                Expect::Accepted => {
                    let config = result.expect(&case);
                    assert_private(&format!("{config:?}"), &[CANARY]);
                    if *path != "typesafe.api_key" {
                        let source = config.source(key);
                        assert!(
                            matches!(&source, ValueSource::Single(l) if *l == layer)
                                || matches!(&source, ValueSource::Union(ls) if ls.contains(&layer)),
                            "{case}: {source:?}"
                        );
                    }
                }
                Expect::Forbidden | Expect::Reserved | Expect::UnknownEnvironmentName => {
                    let err = result.expect_err(&case);
                    let (issue_key, problem) = match expect {
                        Expect::Forbidden => (known(key), ConfigProblem::ForbiddenInLayer),
                        Expect::Reserved => (known(key), ConfigProblem::ReservedSetting),
                        _ => (IssueKey::Unknown, ConfigProblem::UnknownKey),
                    };
                    assert!(err.contains(layer, &issue_key, problem), "{case}: {err}");
                    assert_private(&format!("{err} {err:?}"), &[CANARY, "proxy.example"]);
                }
            }
        }
    }
}

#[test]
fn project_cannot_widen_trusted_disclosure_or_advice_authority() {
    let widened = |sources: ConfigSources, layer, key| {
        let err = resolve(sources).unwrap_err();
        assert!(
            err.contains(layer, &known(key), ConfigProblem::WidensLowerLayer),
            "{err}"
        );
    };
    widened(
        ConfigSources {
            trusted_user: vec![entry("context.profile", s("minimal"))],
            project: vec![entry("context.profile", s("standard"))],
            ..ConfigSources::default()
        },
        ConfigLayer::Project,
        SettingKey::ContextProfile,
    );
    widened(
        ConfigSources {
            trusted_user: vec![entry("context.no_tools", RawValue::Bool(true))],
            project: vec![entry("context.no_tools", RawValue::Bool(false))],
            ..ConfigSources::default()
        },
        ConfigLayer::Project,
        SettingKey::ContextNoTools,
    );
    widened(
        ConfigSources {
            trusted_user: vec![entry("context.messages", RawValue::Integer(4))],
            project: vec![entry("context.messages", RawValue::Integer(8))],
            ..ConfigSources::default()
        },
        ConfigLayer::Project,
        SettingKey::ContextMessages,
    );
    widened(
        ConfigSources {
            trusted_user: vec![entry("context.budget_chars", RawValue::Integer(2_000))],
            project: vec![entry("context.budget_chars", RawValue::Integer(2_001))],
            ..ConfigSources::default()
        },
        ConfigLayer::Project,
        SettingKey::ContextBudgetChars,
    );
    // A project cannot opt the user into spending on notification turns
    // (sr-sif6); the trusted user can.
    widened(
        ConfigSources {
            project: vec![entry("hook.notification_turns", s("rank"))],
            ..ConfigSources::default()
        },
        ConfigLayer::Project,
        SettingKey::HookNotificationTurns,
    );
    let opted_in = resolve(ConfigSources {
        trusted_user: vec![entry("hook.notification_turns", s("rank"))],
        ..ConfigSources::default()
    })
    .unwrap();
    assert_eq!(
        opted_in.effective().notification_turns(),
        NotificationTurns::Rank
    );
    widened(
        ConfigSources {
            cli: vec![entry("hook.mode", s("advisory"))],
            ..ConfigSources::default()
        },
        ConfigLayer::Cli,
        SettingKey::HookMode,
    );

    // Honest counterparts: narrowing, no-op equality and explicit trusted overrides.
    let narrowed = resolve(ConfigSources {
        trusted_user: vec![entry("context.messages", RawValue::Integer(4))],
        project: vec![
            entry("context.messages", RawValue::Integer(3)),
            entry("context.profile", s("standard")),
            entry("context.no_tools", RawValue::Bool(true)),
        ],
        ..ConfigSources::default()
    })
    .unwrap();
    assert_eq!(narrowed.effective().messages(), 3);
    assert_eq!(
        narrowed.effective().context_profile(),
        ContextProfile::Standard
    );
    assert!(narrowed.effective().no_tools());

    let trusted_widening = resolve(ConfigSources {
        trusted_user: vec![
            entry("context.profile", s("minimal")),
            entry("context.messages", RawValue::Integer(4)),
            entry("hook.mode", s("advisory")),
        ],
        project: vec![entry("context.messages", RawValue::Integer(2))],
        environment: vec![var("SR_MESSAGES", "10")],
        cli: vec![
            entry("context.profile", s("standard")),
            entry("hook.mode", s("shadow")),
        ],
    })
    .unwrap();
    let e = trusted_widening.effective();
    assert_eq!(e.context_profile(), ContextProfile::Standard);
    assert_eq!(e.messages(), 10);
    assert_eq!(e.hook_mode(), HookMode::Shadow);
    assert_eq!(
        trusted_widening.source(SettingKey::ContextMessages),
        ValueSource::Single(ConfigLayer::Environment)
    );
}

#[test]
fn project_roots_stay_inside_the_workspace_and_lists_only_grow() {
    let root_error = |value: &str| {
        let err = resolve(ConfigSources {
            project: vec![entry("roster.roots", list(&[value]))],
            ..ConfigSources::default()
        })
        .unwrap_err();
        err.issues()[0].problem
    };
    use ConfigProblem::InvalidRoot;
    assert_eq!(
        root_error("/etc/skills"),
        InvalidRoot(RootError::AbsoluteNotAllowed)
    );
    assert_eq!(root_error(".."), InvalidRoot(RootError::EscapesWorkspace));
    assert_eq!(
        root_error("a/../../b"),
        InvalidRoot(RootError::EscapesWorkspace)
    );
    assert_eq!(root_error(""), InvalidRoot(RootError::Empty));
    assert_eq!(
        root_error("skills\u{1b}[2J"),
        InvalidRoot(RootError::InvalidCharacter)
    );

    let config = resolve(ConfigSources {
        trusted_user: vec![
            entry("roster.roots", list(&["/opt/team skills", "local"])),
            entry("ranking.exclude_skills", list(&["noisy-skill"])),
        ],
        project: vec![
            entry("roster.roots", list(&["./skills/../skills", "local"])),
            entry(
                "ranking.exclude_skills",
                list(&["legacy-skill", "noisy-skill"]),
            ),
        ],
        ..ConfigSources::default()
    })
    .unwrap();
    let roots = config.effective().roster_roots();
    assert_eq!(roots.len(), 3, "union deduplicates normalized roots");
    assert!(
        matches!(&roots[0], SkillRoot::TrustedAbsolute(r) if r.as_path() == std::path::Path::new("/opt/team skills"))
    );
    assert!(
        matches!(&roots[1], SkillRoot::WorkspaceRelative(r) if r.as_path() == std::path::Path::new("local"))
    );
    assert!(
        matches!(&roots[2], SkillRoot::WorkspaceRelative(r) if r.as_path() == std::path::Path::new("skills"))
    );
    let excluded: Vec<&str> = config
        .effective()
        .exclude_skills()
        .iter()
        .map(SkillReference::as_str)
        .collect();
    assert_eq!(excluded, ["noisy-skill", "legacy-skill"]);
    assert_eq!(
        config.source(SettingKey::RankingExcludeSkills),
        ValueSource::Union([ConfigLayer::TrustedUser, ConfigLayer::Project].into())
    );
    assert_private(&format!("{config:?}"), &["team skills", "noisy-skill"]);

    // Only a trusted user can grant an outside root, and never with '..'.
    let err = resolve(user(vec![entry(
        "context.transcript_roots",
        list(&["/home/x/../y"]),
    )]))
    .unwrap_err();
    assert_eq!(
        err.issues()[0].problem,
        InvalidRoot(RootError::ParentComponent)
    );
    let err = resolve(user(vec![entry(
        "context.transcript_roots",
        list(&["relative"]),
    )]))
    .unwrap_err();
    assert_eq!(
        err.issues()[0].problem,
        InvalidRoot(RootError::RelativeNotAllowed)
    );
}

#[test]
fn workspace_relative_roots_normalize_lexically() {
    for (input, expected) in [
        (".", ""),
        ("skills", "skills"),
        ("a/./b/", "a/b"),
        ("a/../b", "b"),
        ("a/b/../..", ""),
        ("dir with space/x", "dir with space/x"),
    ] {
        let root = WorkspaceRelativeRoot::parse(input).unwrap();
        assert_eq!(root.as_path(), std::path::Path::new(expected), "{input}");
        assert_eq!(format!("{root:?}"), "WorkspaceRelativeRoot(<private>)");
    }
    for (input, err) in [
        ("../x", RootError::EscapesWorkspace),
        ("a/../../x", RootError::EscapesWorkspace),
        ("/abs", RootError::AbsoluteNotAllowed),
        ("x\u{202e}y", RootError::InvalidCharacter),
        ("x\ty", RootError::InvalidCharacter),
    ] {
        assert_eq!(WorkspaceRelativeRoot::parse(input), Err(err), "{input}");
    }
    let too_long = "a".repeat(MAX_ROOT_BYTES + 1);
    assert_eq!(
        WorkspaceRelativeRoot::parse(&too_long),
        Err(RootError::TooLong)
    );
    let absolute = TrustedAbsoluteRoot::parse("/private/user/skills/").unwrap();
    assert_eq!(
        absolute.as_path(),
        std::path::Path::new("/private/user/skills")
    );
    assert_eq!(format!("{absolute:?}"), "TrustedAbsoluteRoot(<private>)");
}

#[test]
fn an_api_key_alone_grants_no_network_consent() {
    let key_only = resolve(ConfigSources {
        environment: vec![var("TYPESAFE_API_KEY", CANARY), var("PATH", "/usr/bin")],
        ..ConfigSources::default()
    })
    .unwrap();
    assert_eq!(
        key_only.credential_status(),
        CredentialStatus::PresentFromEnvironment
    );
    assert_eq!(
        key_only
            .credential()
            .unwrap()
            .expose_for_authorization_header(),
        CANARY
    );
    let consent = key_only.network_consent(effects(EffectFlags::default()));
    assert_eq!(consent, NetworkConsent::NotAuthorized);
    assert_eq!(
        admit_provider_attempt(consent, key_only.credential_status()),
        Err(ProviderAdmissionRefusal::NetworkNotAuthorized)
    );

    // A project claim of consent is an error, not an ignored or honored setting.
    let err = resolve(ConfigSources {
        project: vec![entry("network.enabled", RawValue::Bool(true))],
        environment: vec![var("TYPESAFE_API_KEY", CANARY)],
        ..ConfigSources::default()
    })
    .unwrap_err();
    assert!(err.contains(
        ConfigLayer::Project,
        &known(SettingKey::NetworkEnabled),
        ConfigProblem::ForbiddenInLayer
    ));

    // Honest successes: trusted user consent and the explicit flag.
    let trusted = resolve(ConfigSources {
        trusted_user: vec![entry("network.enabled", RawValue::Bool(true))],
        environment: vec![var("TYPESAFE_API_KEY", CANARY)],
        ..ConfigSources::default()
    })
    .unwrap();
    let consent = trusted.network_consent(effects(EffectFlags::default()));
    assert_eq!(
        admit_provider_attempt(consent, trusted.credential_status()),
        Ok(ConsentSource::TrustedUserConfig)
    );
    let flagged = EffectFlags {
        allow_network: true,
        ..EffectFlags::default()
    };
    assert_eq!(
        admit_provider_attempt(
            key_only.network_consent(effects(flagged)),
            key_only.credential_status()
        ),
        Ok(ConsentSource::AllowNetworkFlag)
    );

    // Consent without a key, and blocking modes that outrank trusted consent.
    let no_key = resolve(user(vec![entry("network.enabled", RawValue::Bool(true))])).unwrap();
    assert_eq!(
        admit_provider_attempt(
            no_key.network_consent(effects(EffectFlags::default())),
            no_key.credential_status()
        ),
        Err(ProviderAdmissionRefusal::MissingCredential)
    );
    for (flags, block, refusal) in [
        (
            EffectFlags {
                offline: true,
                ..EffectFlags::default()
            },
            NetworkBlock::Offline,
            ProviderAdmissionRefusal::Offline,
        ),
        (
            EffectFlags {
                dry_run: true,
                ..EffectFlags::default()
            },
            NetworkBlock::DryRun,
            ProviderAdmissionRefusal::DryRun,
        ),
    ] {
        let consent = trusted.network_consent(effects(flags));
        assert_eq!(consent, NetworkConsent::Blocked(block));
        assert_eq!(
            admit_provider_attempt(consent, trusted.credential_status()),
            Err(refusal)
        );
    }

    // Empty means absent; malformed keys fail without echoing their value.
    let empty = resolve(ConfigSources {
        environment: vec![var("TYPESAFE_API_KEY", "")],
        ..ConfigSources::default()
    })
    .unwrap();
    assert_eq!(empty.credential_status(), CredentialStatus::Absent);
    let newline = format!("{CANARY}\n");
    let err = resolve(ConfigSources {
        environment: vec![var("TYPESAFE_API_KEY", &newline)],
        ..ConfigSources::default()
    })
    .unwrap_err();
    assert!(err.contains(
        ConfigLayer::Environment,
        &known(SettingKey::TypesafeApiKey),
        ConfigProblem::InvalidValue
    ));
    let duplicate = resolve(ConfigSources {
        environment: vec![
            var("TYPESAFE_API_KEY", "bad\u{7}"),
            var("TYPESAFE_API_KEY", CANARY),
        ],
        ..ConfigSources::default()
    })
    .unwrap_err();
    assert!(duplicate.contains(
        ConfigLayer::Environment,
        &known(SettingKey::TypesafeApiKey),
        ConfigProblem::DuplicateKey
    ));

    let receipt = trusted.receipt(effects(EffectFlags::default()));
    assert_eq!(
        receipt.credential(),
        CredentialStatus::PresentFromEnvironment
    );
    for text in [
        format!("{trusted:?}"),
        format!("{receipt:?}"),
        format!("{:?}", trusted.credential()),
        format!("{err} {err:?} {duplicate}"),
    ] {
        assert_private(&text, &[CANARY, "canary"]);
    }
}

#[test]
fn normalized_input_cannot_carry_configuration_authority() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/normalized-context.v1.json")).unwrap();
    parse_normalized_context(&serde_json::to_vec(&fixture).unwrap()).unwrap();
    for (field, value) in [
        ("allow_network", serde_json::json!(true)),
        ("network", serde_json::json!({"enabled": true})),
        ("hook", serde_json::json!({"mode": "advisory"})),
        ("context_profile", serde_json::json!("standard")),
        ("transcript_roots", serde_json::json!(["/"])),
        ("roster", serde_json::json!({"roots": ["/"]})),
        ("typesafe_api_key", serde_json::json!(CANARY)),
        ("endpoint", serde_json::json!("https://attacker.example")),
    ] {
        let mut forged = fixture.clone();
        forged[field] = value;
        assert!(
            parse_normalized_context(&serde_json::to_vec(&forged).unwrap()).is_err(),
            "normalized input accepted authority field {field}"
        );
    }
}

#[test]
fn duplicate_unknown_nonfinite_and_out_of_range_values_fail_together() {
    let err = resolve(ConfigSources {
        trusted_user: vec![
            entry("ranking.gate", RawValue::Float(f64::NAN)),
            entry("ranking.fits", RawValue::Float(f64::INFINITY)),
            entry("ranking.top", RawValue::Integer(0)),
            entry("ranking.top", RawValue::Integer(3)),
            entry("ranking.gaet", RawValue::Float(0.5)),
            entry("ranking.w_prior", RawValue::Float(0.51)),
            entry("ranking.shortlist", RawValue::Float(8.0)),
            entry("hook.mode", s("Advisory")),
            entry("network.enabled", s("true")),
        ],
        project: vec![
            entry("network.enabled", RawValue::Bool(true)),
            entry("network.enabled", RawValue::Bool(true)),
        ],
        environment: vec![
            var("SR_GATE", "NaN"),
            var("SR_FITS", "inf"),
            var("SR_ALLOW_NETWORK", "1"),
            var("SR_TOP", " 5"),
            var("SR_SHORTLIST", "+8"),
            var("TYPESAFE_OTHER", "ignored"),
            var("HOME", "/home/private"),
        ],
        cli: vec![entry("ranking.exclude_skills", list(&["a"]))],
    })
    .unwrap_err();
    use ConfigLayer::{Cli, Environment as Env, Project, TrustedUser as User};
    use ConfigProblem as P;
    use SettingKey as K;
    let expected = [
        (User, known(K::RankingGate), P::NonFinite),
        (User, known(K::RankingFits), P::NonFinite),
        (User, known(K::RankingTop), P::OutOfRange),
        (User, known(K::RankingTop), P::DuplicateKey),
        (User, IssueKey::Unknown, P::UnknownKey),
        (User, known(K::RankingWPrior), P::OutOfRange),
        (User, known(K::RankingShortlist), P::WrongType),
        (User, known(K::HookMode), P::InvalidValue),
        (User, known(K::NetworkEnabled), P::WrongType),
        (Project, known(K::NetworkEnabled), P::ForbiddenInLayer),
        (Project, known(K::NetworkEnabled), P::DuplicateKey),
        (Env, known(K::RankingGate), P::NonFinite),
        (Env, known(K::RankingFits), P::NonFinite),
        (Env, IssueKey::Unknown, P::UnknownKey),
        (Env, known(K::RankingTop), P::WrongType),
        (Env, known(K::RankingShortlist), P::WrongType),
        (Cli, known(K::RankingExcludeSkills), P::ForbiddenInLayer),
    ];
    for (layer, key, problem) in &expected {
        assert!(
            err.contains(*layer, key, *problem),
            "missing {layer:?} {key:?} {problem:?}: {err}"
        );
    }
    assert_eq!(err.issues().len(), expected.len(), "{err}");
    assert_private(&format!("{err} {err:?}"), &["/home/private", "ignored"]);

    // Honest counterparts accepted by the same validators.
    let ok = resolve(ConfigSources {
        trusted_user: vec![
            entry("ranking.w_fit", RawValue::Integer(2)),
            entry("ranking.gate", RawValue::Float(-0.0)),
        ],
        environment: vec![var("SR_FITS", "1e-1"), var("SR_TOP", "007")],
        ..ConfigSources::default()
    })
    .unwrap();
    assert_eq!(ok.effective().w_fit(), 2.0);
    assert_eq!(ok.effective().fits(), 0.1);
    assert_eq!(ok.effective().top(), 7);

    let huge_integer = resolve(user(vec![entry(
        "ranking.w_fit",
        RawValue::Integer(i64::MAX),
    )]))
    .unwrap_err();
    assert_eq!(huge_integer.issues()[0].problem, P::OutOfRange);
}

#[test]
fn list_layer_and_diagnostic_bounds_are_enforced() {
    let first_problem = |sources| resolve(sources).unwrap_err().issues()[0].problem;
    assert_eq!(
        first_problem(user(vec![entry(
            "ranking.exclude_skills",
            list(&["a", "a"])
        )])),
        ConfigProblem::DuplicateItem
    );
    assert_eq!(
        first_problem(user(vec![entry(
            "ranking.exclude_skills",
            list(&["has space"])
        )])),
        ConfigProblem::InvalidValue
    );
    let many: Vec<String> = (0..=MAX_LIST_ITEMS).map(|i| format!("skill-{i}")).collect();
    assert_eq!(
        first_problem(user(vec![entry(
            "ranking.exclude_skills",
            RawValue::StringList(many)
        )])),
        ConfigProblem::TooManyItems
    );
    let within: Vec<String> = (0..MAX_LIST_ITEMS).map(|i| format!("skill-{i}")).collect();
    resolve(user(vec![entry(
        "ranking.exclude_skills",
        RawValue::StringList(within),
    )]))
    .unwrap();
    assert_eq!(
        first_problem(user(vec![entry(
            "provider.model",
            s(&"m".repeat(MAX_MODEL_BYTES + 1))
        )])),
        ConfigProblem::ValueTooLong
    );

    let entries: Vec<_> = (0..=MAX_LAYER_ENTRIES)
        .map(|_| entry("ranking.top", RawValue::Integer(1)))
        .collect();
    let err = resolve(user(entries)).unwrap_err();
    assert_eq!(
        err.issues(),
        [ConfigIssue {
            layer: ConfigLayer::TrustedUser,
            key: IssueKey::Layer,
            problem: ConfigProblem::TooManyEntries
        }]
    );

    let unknown: Vec<_> = (0..MAX_REPORTED_ISSUES + 8)
        .map(|i| entry(&format!("unknown.key_{i}"), RawValue::Bool(true)))
        .collect();
    let err = resolve(user(unknown)).unwrap_err();
    assert_eq!(
        (err.issues().len(), err.omitted()),
        (MAX_REPORTED_ISSUES, 8)
    );
    assert!(err.to_string().ends_with("; 8 more"));

    let unprintable =
        resolve(user(vec![entry("bad\u{1b}[31mkey", RawValue::Bool(true))])).unwrap_err();
    assert_eq!(unprintable.issues()[0].key, IssueKey::Unknown);
    assert!(unprintable.to_string().contains("<unknown key>"));
    assert!(!unprintable.to_string().contains('\u{1b}'));
}

#[cfg(unix)]
#[test]
fn non_utf8_environment_is_rejected_only_in_the_strict_namespace() {
    use std::os::unix::ffi::OsStringExt;
    let err = resolve(ConfigSources {
        environment: vec![
            (OsString::from_vec(b"SR_\xff".to_vec()), "1".into()),
            ("SR_MODEL".into(), OsString::from_vec(b"jev-\xff".to_vec())),
            (OsString::from_vec(b"OTHER_\xff".to_vec()), "1".into()),
        ],
        ..ConfigSources::default()
    })
    .unwrap_err();
    assert!(err.contains(
        ConfigLayer::Environment,
        &IssueKey::Unknown,
        ConfigProblem::NonUtf8
    ));
    assert!(err.contains(
        ConfigLayer::Environment,
        &known(SettingKey::ProviderModel),
        ConfigProblem::NonUtf8
    ));
    assert_eq!(err.issues().len(), 2);
}

#[test]
fn precedence_is_defaults_user_project_environment_cli() {
    type Layers = (Option<i64>, Option<i64>, Option<&'static str>, Option<i64>);
    let cases: [(Layers, u32, ConfigLayer); 6] = [
        ((None, None, None, None), 5, ConfigLayer::BuiltIn),
        ((Some(4), None, None, None), 4, ConfigLayer::TrustedUser),
        ((Some(4), Some(3), None, None), 3, ConfigLayer::Project),
        (
            (Some(4), Some(3), Some("2"), None),
            2,
            ConfigLayer::Environment,
        ),
        ((Some(4), Some(3), Some("2"), Some(1)), 1, ConfigLayer::Cli),
        ((None, Some(3), None, Some(6)), 6, ConfigLayer::Cli),
    ];
    for ((u, p, e, c), expected, layer) in cases {
        let top = |v: Option<i64>| {
            v.map(|v| vec![entry("ranking.top", RawValue::Integer(v))])
                .unwrap_or_default()
        };
        let config = resolve(ConfigSources {
            trusted_user: top(u),
            project: top(p),
            environment: e.map(|v| vec![var("SR_TOP", v)]).unwrap_or_default(),
            cli: top(c),
        })
        .unwrap();
        assert_eq!(config.effective().top(), expected);
        assert_eq!(
            config.source(SettingKey::RankingTop),
            ValueSource::Single(layer)
        );
    }
}

#[test]
fn rank_sizes_are_cross_checked_after_merge() {
    let err = resolve(ConfigSources {
        trusted_user: vec![entry("ranking.top", RawValue::Integer(6))],
        project: vec![entry("ranking.shortlist", RawValue::Integer(4))],
        ..ConfigSources::default()
    })
    .unwrap_err();
    assert!(err.contains(
        ConfigLayer::Project,
        &known(SettingKey::RankingTop),
        ConfigProblem::Conflict(SettingKey::RankingShortlist)
    ));
    for (top, shortlist) in [(8, 8), (1, 1), (32, 32), (1, 32)] {
        let config = resolve(user(vec![
            entry("ranking.top", RawValue::Integer(top)),
            entry("ranking.shortlist", RawValue::Integer(shortlist)),
        ]))
        .unwrap();
        assert_eq!(
            (config.effective().top(), config.effective().shortlist()),
            (top as u32, shortlist as u32)
        );
    }
    let err = resolve(user(vec![entry("ranking.top", RawValue::Integer(33))])).unwrap_err();
    assert_eq!(err.issues()[0].problem, ConfigProblem::OutOfRange);
}

#[test]
fn effect_flags_cover_every_combination() {
    let mut valid = 0;
    for bits in 0u8..128 {
        let bit = |n: u8| bits & (1 << n) != 0;
        let flags = EffectFlags {
            offline: bit(0),
            allow_network: bit(1),
            dry_run: bit(2),
            no_cache: bit(3),
            no_ledger: bit(4),
            no_persist: bit(5),
            save_case: bit(6),
        };
        let mut expected = Vec::new();
        if flags.offline && flags.allow_network {
            expected.push(FlagConflict::OfflineWithAllowNetwork);
        }
        if flags.dry_run && flags.allow_network {
            expected.push(FlagConflict::DryRunWithAllowNetwork);
        }
        if flags.save_case && flags.dry_run {
            expected.push(FlagConflict::SaveCaseWithDryRun);
        }
        if flags.save_case && flags.no_persist {
            expected.push(FlagConflict::SaveCaseWithNoPersist);
        }
        match EffectPolicy::from_flags(flags) {
            Err(conflicts) => assert_eq!(conflicts, expected, "{flags:?}"),
            Ok(policy) => {
                assert!(expected.is_empty(), "{flags:?}");
                valid += 1;
                let stateless = flags.dry_run || flags.no_persist;
                let disabled = |access| matches!(access, StoreAccess::Disabled(_));
                assert_eq!(
                    disabled(policy.response_cache()),
                    stateless || flags.no_cache
                );
                assert_eq!(disabled(policy.ledger()), stateless || flags.no_ledger);
                assert_eq!(disabled(policy.persistent_runtime_state()), stateless);
                assert_eq!(policy.case_capture(), flags.save_case);
                assert_eq!(
                    policy.network_block(),
                    if flags.offline {
                        Some(NetworkBlock::Offline)
                    } else if flags.dry_run {
                        Some(NetworkBlock::DryRun)
                    } else {
                        None
                    }
                );
            }
        }
    }
    assert_eq!(valid, 52, "valid combinations");
    assert_eq!(
        FlagConflict::OfflineWithAllowNetwork.to_string(),
        "--offline conflicts with --allow-network"
    );
    let policy = effects(EffectFlags {
        dry_run: true,
        no_cache: true,
        no_persist: true,
        ..EffectFlags::default()
    });
    assert_eq!(
        policy.response_cache(),
        StoreAccess::Disabled(Restriction::DryRun)
    );
    let policy = effects(EffectFlags {
        no_ledger: true,
        offline: true,
        ..EffectFlags::default()
    });
    assert_eq!(
        policy.ledger(),
        StoreAccess::Disabled(Restriction::NoLedger)
    );
    assert_eq!(
        policy.response_cache(),
        StoreAccess::Enabled,
        "offline may read cache"
    );
}

#[test]
fn receipts_revalidate_only_their_boundary_dependencies() {
    use PolicyBoundary::{CliPublication, HookPublication, ProviderAdmission};
    use PublicationKind::{Advisory, Explicit};
    let base_user = vec![entry("network.enabled", RawValue::Bool(true))];
    let config = ResolvedConfig::resolve(
        ConfigSources {
            trusted_user: base_user.clone(),
            environment: vec![var("TYPESAFE_API_KEY", CANARY)],
            ..ConfigSources::default()
        },
        1,
    )
    .unwrap();
    let receipt = config.receipt(effects(EffectFlags::default()));
    assert_eq!(receipt.schema_version(), CONFIG_SCHEMA_VERSION);
    assert_eq!(receipt.generation(), 1);
    let all = [
        ProviderAdmission,
        CliPublication(Advisory),
        CliPublication(Explicit),
        HookPublication(Advisory),
        HookPublication(Explicit),
    ];
    let check = |user_entries: Vec<(String, RawValue)>,
                 project: Vec<(String, RawValue)>,
                 expected: [Revalidation; 5]| {
        for (boundary, expected) in all.into_iter().zip(expected) {
            assert_eq!(
                config.revalidate(&receipt, user_entries.clone(), project.clone(), 2, boundary),
                expected,
                "{boundary:?}"
            );
        }
    };
    let same = || Revalidation::Unchanged;
    let changed = |fields: &[PolicyField]| Revalidation::Superseded(fields.to_vec());
    let with_base = |extra: (String, RawValue)| {
        let mut entries = base_user.clone();
        entries.push(extra);
        entries
    };

    // An identical re-read, and a change to the invocation deadline, supersede nothing.
    check(
        base_user.clone(),
        vec![],
        [same(), same(), same(), same(), same()],
    );
    check(
        with_base(entry("ranking.timeout_ms", RawValue::Integer(2_000))),
        vec![],
        [same(), same(), same(), same(), same()],
    );
    // Revoked consent blocks the next HTTP attempt, not local publication.
    check(
        vec![entry("network.enabled", RawValue::Bool(false))],
        vec![],
        [
            changed(&[PolicyField::NetworkConsent]),
            same(),
            same(),
            same(),
            same(),
        ],
    );
    // Eligibility changes supersede advisory output only.
    check(
        with_base(entry("ranking.gate", RawValue::Float(0.5))),
        vec![],
        [
            same(),
            changed(&[PolicyField::Gate]),
            same(),
            changed(&[PolicyField::Gate]),
            same(),
        ],
    );
    check(
        base_user.clone(),
        vec![entry("ranking.exclude_skills", list(&["skill-x"]))],
        [
            changed(&[PolicyField::ExcludeSkills]),
            changed(&[PolicyField::ExcludeSkills]),
            same(),
            changed(&[PolicyField::ExcludeSkills]),
            same(),
        ],
    );
    // Hook mode matters only to hook publication; disclosure only to admission.
    check(
        with_base(entry("hook.mode", s("advisory"))),
        vec![],
        [
            same(),
            same(),
            same(),
            changed(&[PolicyField::HookMode]),
            changed(&[PolicyField::HookMode]),
        ],
    );
    check(
        with_base(entry("context.profile", s("minimal"))),
        vec![],
        [
            changed(&[PolicyField::ContextProfile]),
            same(),
            same(),
            same(),
            same(),
        ],
    );
    check(
        base_user.clone(),
        vec![entry("roster.roots", list(&["skills"]))],
        [
            changed(&[PolicyField::RosterRoots]),
            changed(&[PolicyField::RosterRoots]),
            changed(&[PolicyField::RosterRoots]),
            changed(&[PolicyField::RosterRoots]),
            changed(&[PolicyField::RosterRoots]),
        ],
    );
    // Invalid current configuration fails closed at every boundary.
    let invalid = || Revalidation::InvalidConfiguration;
    check(
        with_base(entry("ranking.gate", RawValue::Float(f64::NAN))),
        vec![],
        [invalid(), invalid(), invalid(), invalid(), invalid()],
    );
    check(
        base_user.clone(),
        vec![entry("network.enabled", RawValue::Bool(true))],
        [invalid(), invalid(), invalid(), invalid(), invalid()],
    );
    for boundary in all {
        assert!(!boundary.dependencies().is_empty());
    }
}

#[test]
fn fixed_invocation_layers_hide_file_edits_without_claiming_revocation() {
    let flagged = effects(EffectFlags {
        allow_network: true,
        ..EffectFlags::default()
    });
    let config = resolve(ConfigSources {
        environment: vec![
            var("TYPESAFE_API_KEY", CANARY),
            var("SR_MODEL", "jev-pinned"),
        ],
        cli: vec![entry("ranking.top", RawValue::Integer(3))],
        ..ConfigSources::default()
    })
    .unwrap();
    let receipt = config.receipt(flagged);
    assert_eq!(
        receipt.network_consent(),
        NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag)
    );
    let edits = vec![
        entry("network.enabled", RawValue::Bool(false)),
        entry("ranking.top", RawValue::Integer(4)),
        entry("provider.model", s("jev-other")),
    ];
    for boundary in [
        PolicyBoundary::ProviderAdmission,
        PolicyBoundary::CliPublication(PublicationKind::Advisory),
    ] {
        assert_eq!(
            config.revalidate(&receipt, edits.clone(), vec![], 2, boundary),
            Revalidation::Unchanged,
            "{boundary:?}"
        );
    }
    let reread = config.reresolve_files(edits, vec![], 2).unwrap();
    assert_eq!(reread.effective().top(), 3);
    assert_eq!(
        reread.source(SettingKey::RankingTop),
        ValueSource::Single(ConfigLayer::Cli)
    );
    assert_eq!(reread.effective().model().as_str(), "jev-pinned");
    assert_eq!(
        reread.credential_status(),
        CredentialStatus::PresentFromEnvironment
    );
    assert_eq!(reread.receipt(flagged).generation(), 2);
}

#[test]
fn managed_mutations_touch_only_ranking_policy_fields() {
    let base = ContentHash::from_bytes(b"trusted user config bytes");
    let mutation = ManagedPolicyMutation::new(
        base.clone(),
        vec![
            entry("ranking.w_prior", RawValue::Float(0.2)),
            entry("ranking.gate", RawValue::Float(0.4)),
        ],
    )
    .unwrap();
    assert_eq!(mutation.base_digest(), &base);
    assert_eq!(
        mutation.keys().collect::<Vec<_>>(),
        [SettingKey::RankingGate, SettingKey::RankingWPrior]
    );
    for path in [
        "network.enabled",
        "typesafe.api_key",
        "typesafe.endpoint",
        "provider.model",
        "hook.mode",
        "context.profile",
        "context.transcript_roots",
        "roster.roots",
        "ranking.exclude_skills",
        "ranking.top",
        "privacy.redaction",
    ] {
        let key = SettingKey::from_path(path).unwrap();
        let err = ManagedPolicyMutation::new(base.clone(), vec![entry(path, RawValue::Bool(true))])
            .unwrap_err();
        assert!(
            err.contains(
                ConfigLayer::TrustedUser,
                &known(key),
                ConfigProblem::NotManagedPolicy
            ),
            "{path}: {err}"
        );
    }
    let err = ManagedPolicyMutation::new(
        base,
        vec![
            entry("ranking.gate", RawValue::Float(1.5)),
            entry("ranking.gate", RawValue::Float(0.5)),
            entry("ranking.gates", RawValue::Float(0.5)),
        ],
    )
    .unwrap_err();
    assert!(err.contains(
        ConfigLayer::TrustedUser,
        &known(SettingKey::RankingGate),
        ConfigProblem::OutOfRange
    ));
    assert!(err.contains(
        ConfigLayer::TrustedUser,
        &known(SettingKey::RankingGate),
        ConfigProblem::DuplicateKey
    ));
    assert!(err.contains(
        ConfigLayer::TrustedUser,
        &IssueKey::Unknown,
        ConfigProblem::UnknownKey
    ));
}

#[test]
fn raw_inputs_and_overrides_have_private_debug_output() {
    let sources = ConfigSources {
        trusted_user: vec![entry("provider.model", s("private-model-name"))],
        environment: vec![
            var("TYPESAFE_API_KEY", CANARY),
            var(
                "TYPESAFE_ENDPOINT",
                "https://user:canary-password@api.example",
            ),
        ],
        ..ConfigSources::default()
    };
    assert_private(
        &format!("{sources:?}"),
        &[CANARY, "canary-password", "private-model-name"],
    );
    assert_eq!(format!("{:?}", s(CANARY)), "RawValue::string(<private>)");
    let config = resolve(sources).unwrap();
    let endpoint = config.effective().endpoint().unwrap();
    assert_eq!(
        endpoint.as_str(),
        "https://user:canary-password@api.example"
    );
    assert_eq!(format!("{endpoint:?}"), "EndpointOverride(<private>)");
    assert_private(
        &format!(
            "{config:?} {:?}",
            config.receipt(effects(EffectFlags::default()))
        ),
        &["canary-password", "user:"],
    );
    for refusal in [
        ProviderAdmissionRefusal::Offline,
        ProviderAdmissionRefusal::DryRun,
        ProviderAdmissionRefusal::NetworkNotAuthorized,
        ProviderAdmissionRefusal::MissingCredential,
    ] {
        assert_private(&refusal.to_string(), &[CANARY]);
    }
}

#[test]
fn unknown_configuration_keys_never_echo_private_input() {
    let err = resolve(ConfigSources {
        trusted_user: vec![entry(CANARY, s("unused"))],
        project: vec![entry(CANARY, RawValue::Bool(true))],
        environment: vec![var(&format!("SR_{CANARY}"), "unused")],
        cli: vec![entry(CANARY, RawValue::Integer(1))],
    })
    .unwrap_err();
    for layer in [
        ConfigLayer::TrustedUser,
        ConfigLayer::Project,
        ConfigLayer::Environment,
        ConfigLayer::Cli,
    ] {
        assert!(
            err.issues().iter().any(|issue| {
                issue.layer == layer && issue.problem == ConfigProblem::UnknownKey
            })
        );
    }
    assert_private(&format!("{err} {err:?} {:?}", err.issues()), &[CANARY]);
    let valid = resolve(user(vec![entry("ranking.gate", RawValue::Float(0.4))])).unwrap();
    assert_eq!(valid.effective().gate(), 0.4);
}

#[test]
fn failures_map_onto_output_error_kinds_and_exit_codes() {
    let err = resolve(ConfigSources {
        project: vec![entry("network.enabled", RawValue::Bool(true))],
        ..ConfigSources::default()
    })
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidConfiguration);
    assert_eq!(err.kind().exit_code(), CliExit::Usage);

    let conflicts = EffectPolicy::from_flags(EffectFlags {
        offline: true,
        allow_network: true,
        ..EffectFlags::default()
    })
    .unwrap_err();
    assert_eq!(conflicts[0].kind(), ErrorKind::InvalidUsage);
    assert_eq!(conflicts[0].kind().exit_code(), CliExit::Usage);

    for (refusal, kind, exit, wire) in [
        (
            ProviderAdmissionRefusal::Offline,
            ErrorKind::CacheMiss,
            CliExit::CacheMiss,
            "cache-miss",
        ),
        (
            ProviderAdmissionRefusal::DryRun,
            ErrorKind::NetworkDenied,
            CliExit::Privacy,
            "network-denied",
        ),
        (
            ProviderAdmissionRefusal::NetworkNotAuthorized,
            ErrorKind::NetworkDenied,
            CliExit::Privacy,
            "network-denied",
        ),
        (
            ProviderAdmissionRefusal::MissingCredential,
            ErrorKind::CredentialAbsent,
            CliExit::Provider,
            "credential-absent",
        ),
    ] {
        assert_eq!(refusal.kind(), kind, "{refusal:?}");
        assert_eq!(kind.exit_code(), exit, "{refusal:?}");
        assert_eq!(kind.as_str(), wire, "{refusal:?}");
    }
    assert_eq!(
        [
            CliExit::Usage,
            CliExit::Provider,
            CliExit::Privacy,
            CliExit::CacheMiss
        ]
        .map(|e| e as u8),
        [2, 4, 8, 11]
    );
}
