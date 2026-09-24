//! Real binary checks of `sr doctor` readiness and the `--config` policy
//! fingerprint, in isolated homes with a cleared environment.
use serde_json::Value;
use skillranker::config::{ConfigSources, RawValue, ResolvedConfig};
use skillranker::readiness::{
    TransportEvidence, TransportIdentity, TransportState, assess_transport,
};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CANARY: &str = "synthetic-doctor-canary-key";

struct Home {
    root: PathBuf,
}

impl Home {
    // Intentionally retained: repository policy forbids automatic tree deletion.
    fn new() -> Self {
        // Trees are retained, so a reused PID must never reuse an old tree.
        let root = std::env::temp_dir().join(format!(
            "sr-doctor-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        for dir in ["workspace/.claude/skills", "home/.claude/skills", "user/sr"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self { root }
    }
    fn skill(&self, name: &str) {
        let dir = self.root.join("workspace/.claude/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: {name} skill.\n---\nBody.\n"),
        )
        .unwrap();
    }
    fn user_config(&self, text: &str) {
        std::fs::write(self.root.join("user/sr/config.toml"), text).unwrap();
    }
    fn project_config(&self, text: &str) {
        std::fs::create_dir_all(self.root.join("workspace/.sr")).unwrap();
        std::fs::write(self.root.join("workspace/.sr/config.toml"), text).unwrap();
    }
    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sr"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("user"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .current_dir(self.root.join("workspace"))
            .args(args);
        for (name, value) in environment {
            command.env(name, value);
        }
        command.output().unwrap()
    }
    fn doctor(&self, args: &[&str], environment: &[(&str, &str)]) -> Value {
        let output = self.run(args, environment);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains(CANARY), "the key is never echoed");
        // Doctor is local: no state, cache or data directory appears.
        for dir in ["data", "cache", "state"] {
            assert!(!self.root.join(dir).exists(), "doctor created {dir}");
        }
        serde_json::from_str(&text).unwrap()
    }
}

fn steps(report: &Value) -> Vec<String> {
    report["next_steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["check"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn an_empty_isolated_home_reports_each_missing_prerequisite() {
    let home = Home::new();
    let report = home.doctor(&["doctor", "--json"], &[]);
    assert_eq!(report["command"], "doctor");
    assert_eq!(report["scope"], "local-readiness");
    let checks = &report["checks"];
    assert_eq!(checks["configuration"]["state"], "valid");
    // Doctor selects no session, so it claims nothing about input readiness.
    assert_eq!(checks["input"]["state"], "not-evaluated");
    assert_eq!(checks["roster"]["state"], "empty");
    assert_eq!(checks["credential"]["state"], "absent");
    assert_eq!(checks["network"]["state"], "not-authorized");
    assert_eq!(checks["transport"]["state"], "untested");
    // The ledger is optional and absent from this build; it blocks nothing.
    assert_eq!(checks["ledger"]["state"], "not-available");
    assert_eq!(checks["ledger"]["blocks_ranking"], false);
    assert_eq!(checks["hook"]["mode"], "shadow");
    assert_eq!(
        checks["hook"]["mode_sources"],
        serde_json::json!(["built-in"])
    );
    // One concrete step per failed check, and local commands stay listed.
    assert_eq!(steps(&report), vec!["roster", "credential", "network"]);
    for check in ["roster", "credential", "network"] {
        assert!(checks[check]["next_step"].as_str().unwrap().len() > 10);
    }
    assert!(
        checks["credential"]["next_step"]
            .as_str()
            .unwrap()
            .contains("https://console.typesafe.ai")
    );
    assert!(report["local_commands"].as_array().unwrap().len() >= 2);
}

#[test]
fn a_present_key_is_presence_not_authentication() {
    let home = Home::new();
    home.skill("alpha");
    home.user_config("[network]\nenabled = true\n");
    let report = home.doctor(&["doctor", "--json"], &[("TYPESAFE_API_KEY", CANARY)]);
    let checks = &report["checks"];
    assert_eq!(checks["credential"]["state"], "present");
    assert_eq!(checks["credential"]["verified"], false);
    // No request was made, so nothing about the connection is known.
    assert_eq!(checks["transport"]["state"], "untested");
    assert_eq!(checks["network"]["state"], "authorized");
    assert_eq!(checks["network"]["source"], "trusted-user");
    assert!(!steps(&report).contains(&"credential".to_owned()));
    assert!(!steps(&report).contains(&"network".to_owned()));
}

#[test]
fn network_consent_follows_flags_and_trusted_settings() {
    let home = Home::new();
    let flag = home.doctor(&["doctor", "--json", "--allow-network"], &[]);
    assert_eq!(flag["checks"]["network"]["state"], "authorized");
    assert_eq!(flag["checks"]["network"]["source"], "allow-network-flag");
    let offline = home.doctor(&["doctor", "--json", "--offline"], &[]);
    assert_eq!(offline["checks"]["network"]["state"], "blocked");
    assert_eq!(offline["checks"]["network"]["by"], "offline");
    home.user_config("[network]\nenabled = true\n");
    let blocked = home.doctor(&["doctor", "--json", "--offline"], &[]);
    assert_eq!(blocked["checks"]["network"]["state"], "blocked");
    let conflict = home.run(&["doctor", "--offline", "--allow-network"], &[]);
    assert_eq!(conflict.status.code(), Some(2));
}

#[test]
fn a_roster_rank_cannot_advise_from_is_not_called_ready() {
    // Every skill is manual-only: rank would have nothing to suggest.
    let home = Home::new();
    for name in ["alpha", "beta"] {
        let dir = home.root.join("workspace/.claude/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\ndescription: {name} skill.\ndisable-model-invocation: true\n---\nBody.\n"
            ),
        )
        .unwrap();
    }
    let report = home.doctor(&["doctor", "--json"], &[]);
    let roster = &report["checks"]["roster"];
    assert_eq!(roster["skills"], 2);
    assert_eq!(roster["advisory"], 0);
    assert_eq!(roster["state"], "no-advisory-candidates");
}

#[test]
fn roster_readiness_reports_counts_and_unverified_visibility() {
    let home = Home::new();
    home.skill("alpha");
    home.skill("beta");
    let report = home.doctor(&["doctor", "--json"], &[]);
    let roster = &report["checks"]["roster"];
    assert_eq!(roster["skills"], 2);
    // Doctor mirrors rank: both skills are rankable under the provisional
    // Claude contract, and their visibility is still reported unverified.
    assert_eq!(roster["advisory"], 2);
    assert_eq!(roster["unverified"], 2);
    assert_eq!(roster["visibility"], "unverified");
    assert_eq!(roster["state"], "ready");
    assert_eq!(roster["partial"], true);
    assert!(roster["snapshot"].as_str().unwrap().len() == 64);
    assert!(roster["source_causes"].get("root-unreadable").is_none());
    // An unreadable root is reported, not treated as an empty roster.
    std::fs::rename(
        home.root.join("home/.claude/skills"),
        home.root.join("home/.claude/skills-moved"),
    )
    .unwrap();
    std::fs::write(home.root.join("home/.claude/skills"), "not a directory").unwrap();
    let partial = home.doctor(&["doctor", "--json"], &[]);
    let roster = &partial["checks"]["roster"];
    assert_eq!(roster["skills"], 2);
    assert_eq!(roster["source_causes"]["root-unreadable"], 1);
    // A malformed skill is an excluded record with a stable cause.
    let broken = home.root.join("workspace/.claude/skills/broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(
        broken.join("SKILL.md"),
        "---\ndescription: [unclosed\n---\nBody.\n",
    )
    .unwrap();
    let excluded = home.doctor(&["doctor", "--json"], &[]);
    assert_eq!(excluded["checks"]["roster"]["skills"], 2);
    assert_eq!(
        excluded["checks"]["roster"]["record_causes"]
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_u64().unwrap())
            .sum::<u64>(),
        1
    );
}

#[test]
fn invalid_configuration_has_no_fingerprint_and_no_readiness_report() {
    let home = Home::new();
    home.skill("alpha");
    home.project_config("[network]\nenabled = true\n");
    for args in [&["doctor", "--json"][..], &["doctor", "--config", "--json"]] {
        let output = home.run(args, &[]);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["kind"], "invalid-configuration");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(!text.contains("policy_fingerprint"));
        assert!(!text.contains("checks"));
    }
}

#[test]
fn the_policy_fingerprint_tracks_effective_values_only() {
    let home = Home::new();
    let fingerprint = |args: &[&str], env: &[(&str, &str)]| {
        let report = home.doctor(args, env);
        report["policy_fingerprint"].as_str().unwrap().to_owned()
    };
    let base = fingerprint(&["doctor", "--config", "--json"], &[]);
    assert_eq!(base.len(), 64);
    // The readiness report carries the same fingerprint.
    let readiness = home.doctor(&["doctor", "--json"], &[]);
    assert_eq!(
        readiness["checks"]["configuration"]["policy_fingerprint"],
        base
    );
    // A different value changes it; the same value from another layer does not.
    let changed = fingerprint(&["doctor", "--config", "--json", "--top", "3"], &[]);
    assert_ne!(changed, base);
    home.user_config("[ranking]\ntop = 3\n");
    assert_eq!(fingerprint(&["doctor", "--config", "--json"], &[]), changed);
    // Set-valued lists are canonical: the same members in another layer
    // order are the same policy, and a different member is not.
    home.user_config("[ranking]\ntop = 3\nexclude_skills = [\"alpha\"]\n");
    home.project_config("[ranking]\nexclude_skills = [\"beta\"]\n");
    let forward = fingerprint(&["doctor", "--config", "--json"], &[]);
    home.user_config("[ranking]\ntop = 3\nexclude_skills = [\"beta\"]\n");
    home.project_config("[ranking]\nexclude_skills = [\"alpha\"]\n");
    assert_eq!(fingerprint(&["doctor", "--config", "--json"], &[]), forward);
    home.project_config("[ranking]\nexclude_skills = [\"gamma\"]\n");
    assert_ne!(fingerprint(&["doctor", "--config", "--json"], &[]), forward);
    home.project_config("");
    home.user_config("[ranking]\ntop = 3\n");
    // The credential never enters it.
    assert_eq!(
        fingerprint(
            &["doctor", "--config", "--json"],
            &[("TYPESAFE_API_KEY", CANARY)]
        ),
        changed
    );
}

#[test]
fn the_table_lists_every_check() {
    let home = Home::new();
    let output = home.run(&["doctor", "--table"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("CHECK\tSTATE\tNEXT STEP\n"));
    for check in [
        "configuration",
        "input",
        "roster",
        "credential",
        "network",
        "transport",
        "ledger",
        "hook",
    ] {
        assert!(
            text.lines()
                .any(|line| line.starts_with(&format!("{check}\t"))),
            "{check}"
        );
    }
}

fn config(entries: Vec<(&str, RawValue)>) -> ResolvedConfig {
    ResolvedConfig::resolve(
        ConfigSources {
            trusted_user: entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
            ..Default::default()
        },
        1,
    )
    .unwrap()
}

#[test]
fn historical_transport_evidence_is_invalidated_by_identity_changes() {
    const ORIGIN: &str = "https://api.typesafe.ai";
    let current = TransportIdentity::current(&config(vec![]), ORIGIN);
    let now = 1_800_000_000_000;
    let evidence = |identity: TransportIdentity, at: u64| TransportEvidence {
        checked_at_unix_ms: at,
        scope: "wide+rerank".into(),
        identity,
    };
    assert_eq!(
        assess_transport(None, &current, now),
        TransportState::Untested
    );
    let same = evidence(current.clone(), now - 60_000);
    assert_eq!(
        assess_transport(Some(&same), &current, now),
        TransportState::PreviouslyVerified {
            checked_at_unix_ms: now - 60_000,
            scope: "wide+rerank".into()
        }
    );
    let mut runtime = current.clone();
    runtime.runtime = "asupersync-0.4.0/native-roots".into();
    let mut version = current.clone();
    version.sr_version = "0.0.0-older".into();
    let endpoint = TransportIdentity::current(&config(vec![]), "https://staging.example.test");
    let model = TransportIdentity::current(
        &config(vec![(
            "provider.model",
            RawValue::String("jev-other".into()),
        )]),
        ORIGIN,
    );
    let timeout = TransportIdentity::current(
        &config(vec![("ranking.timeout_ms", RawValue::Integer(5_000))]),
        ORIGIN,
    );
    for (recorded, expected) in [
        (runtime, "runtime"),
        (version, "sr-version"),
        (endpoint, "endpoint"),
        (model, "model"),
        (timeout, "timeout"),
    ] {
        assert_eq!(
            assess_transport(Some(&evidence(recorded, now - 60_000)), &current, now),
            TransportState::Invalidated {
                changed: vec![expected]
            }
        );
    }
    // A check dated well in the future is not trusted.
    let future = evidence(current.clone(), now + 60 * 60 * 1000);
    assert_eq!(
        assess_transport(Some(&future), &current, now),
        TransportState::Invalidated {
            changed: vec!["clock"]
        }
    );
    // Records are strict: unknown fields are refused.
    let mut value = serde_json::to_value(&same).unwrap();
    value["verified_live"] = true.into();
    assert!(serde_json::from_value::<TransportEvidence>(value).is_err());
}

#[test]
fn ignored_ambient_proxies_are_named_without_their_values() {
    let home = Home::new();
    let secret_proxy = "http://user:hunter2@proxy.internal:3128";
    let report = home.doctor(
        &["doctor", "--json"],
        &[("HTTPS_PROXY", secret_proxy), ("no_proxy", "localhost")],
    );
    let transport = &report["checks"]["transport"];
    assert_eq!(transport["state"], "untested");
    assert_eq!(
        transport["ambient_proxy_ignored"],
        serde_json::json!(["HTTPS_PROXY", "no_proxy"])
    );
    assert!(steps(&report).contains(&"transport".to_owned()));
    let text = report.to_string();
    assert!(
        !text.contains("hunter2") && !text.contains("proxy.internal"),
        "{text}"
    );

    let quiet = Home::new().doctor(&["doctor", "--json"], &[]);
    assert!(
        quiet["checks"]["transport"]
            .get("ambient_proxy_ignored")
            .is_none()
    );
}
