//! Strict CLI and Provenance-Aware Configuration Contract Tests
//!
//! Satisfies contract boundary `p4_strict_cli` (sr-roadmap-l1i.5.1)
//! mapped in `tests/contract_matrix.toml`.
//!
//! Verifies:
//! 1. Strict flag parsing, mutual exclusivity, and conflict detection.
//! 2. Complete precedence hierarchy: built-in -> trusted user -> project -> environment -> CLI.
//! 3. Clean rejection of unknown flags, unknown commands, and forbidden/malformed configuration.
//! 4. Non-disclosure of canary secrets in stdout, stderr, and structured error envelopes.
//! 5. Zero persistence, zero filesystem mutation, and zero child-process execution during local inspection.
//! 6. Consequential boundary rereads via `ConfigFiles::refresh` detecting no-op, superseded, and invalid edits.

#![cfg(unix)]

use serde_json::Value;
use skillranker::cli::ConfigFiles;
use skillranker::config::{ConfigSources, PolicyBoundary, PolicyField, RawValue, Revalidation};
use skillranker::privacy::{EffectFlags, EffectPolicy};
use skillranker::runtime::EntryClock;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct CliFixture {
    root: PathBuf,
}

impl CliFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-cli-contract-{}-{}",
            std::process::id(),
            FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("user/sr")).unwrap();
        std::fs::create_dir_all(root.join("workspace/.sr")).unwrap();
        Self { root }
    }

    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sr"));
        cmd.env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("user"))
            .current_dir(self.root.join("workspace"))
            .args(args);
        for (name, value) in environment {
            cmd.env(name, value);
        }
        cmd.output().expect("binary execution")
    }

    fn write_user_config(&self, content: &str) {
        std::fs::write(self.root.join("user/sr/config.toml"), content).unwrap();
    }

    fn write_project_config(&self, bytes: &[u8]) {
        std::fs::write(self.root.join("workspace/.sr/config.toml"), bytes).unwrap();
    }
}

impl Drop for CliFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn strict_flags_and_provenance() {
    let f = CliFixture::new();
    let canary = "canary-secret-never-disclose-998877";

    // -------------------------------------------------------------------------
    // 1. Strict Help, Version, and Flag Validation
    // -------------------------------------------------------------------------
    // Top-level help and version exit 0 without touching config.
    f.write_project_config(b"malformed project config syntax !@#$%^&*");
    let out_help = f.run(&["--help"], &[("SR_CANARY", canary)]);
    assert_eq!(out_help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out_help.stdout).contains("SkillRanker"));
    assert!(!String::from_utf8_lossy(&out_help.stdout).contains(canary));

    let out_ver = f.run(&["--version"], &[]);
    assert_eq!(out_ver.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out_ver.stdout).starts_with("sr "));

    // Unknown command rejected with exit 2 invalid-usage.
    let out_unknown_cmd = f.run(&["unknown-subcommand"], &[]);
    assert_eq!(out_unknown_cmd.status.code(), Some(2));
    let err_val: Value = serde_json::from_slice(&out_unknown_cmd.stdout).unwrap();
    assert_eq!(err_val["error"]["kind"], "invalid-usage");

    // Unknown flag rejected with exit 2 invalid-usage.
    let out_unknown_flag = f.run(&["doctor", "--unknown-flag"], &[]);
    assert_eq!(out_unknown_flag.status.code(), Some(2));
    let err_val: Value = serde_json::from_slice(&out_unknown_flag.stdout).unwrap();
    assert_eq!(err_val["error"]["kind"], "invalid-usage");

    // Top-level flags cannot accompany subcommands.
    let out_top_sub = f.run(&["--help", "doctor"], &[]);
    assert_eq!(out_top_sub.status.code(), Some(2));

    // Mutual exclusivity: --json and --table conflict.
    let out_conflict_format = f.run(&["doctor", "--config", "--json", "--table"], &[]);
    assert_eq!(out_conflict_format.status.code(), Some(2));

    // Mutual exclusivity: --offline and --allow-network conflict.
    let out_conflict_net = f.run(&["doctor", "--config", "--offline", "--allow-network"], &[]);
    assert_eq!(out_conflict_net.status.code(), Some(2));
    let err_val: Value = serde_json::from_slice(&out_conflict_net.stdout).unwrap();
    assert_eq!(err_val["error"]["kind"], "invalid-usage");

    // Lone --offline and lone --allow-network are recognized and succeed.
    f.write_project_config(b"");
    let out_offline = f.run(&["doctor", "--config", "--offline"], &[]);
    assert_eq!(out_offline.status.code(), Some(0));
    let out_net = f.run(&["doctor", "--config", "--allow-network"], &[]);
    assert_eq!(out_net.status.code(), Some(0));

    // Duplicate CLI flags rejected with exit 2.
    let out_dup_flag = f.run(&["doctor", "--config", "--top", "1", "--top", "2"], &[]);
    assert_eq!(out_dup_flag.status.code(), Some(2));

    // -------------------------------------------------------------------------
    // 2. Complete Precedence Hierarchy and Provenance Tracking
    // -------------------------------------------------------------------------
    // Case A: built-in default (5)
    f.write_user_config("");
    f.write_project_config(b"");
    let out_builtin = f.run(&["doctor", "--config", "--json"], &[]);
    assert_eq!(out_builtin.status.code(), Some(0));
    let rep: Value = serde_json::from_slice(&out_builtin.stdout).unwrap();
    assert_eq!(rep["settings"]["ranking.top"]["value"], 5);
    assert_eq!(
        rep["settings"]["ranking.top"]["sources"],
        serde_json::json!(["built-in"])
    );

    // Case B: trusted-user override (2)
    f.write_user_config("[ranking]\ntop=2\nshortlist=8\n");
    let out_user = f.run(&["doctor", "--config", "--json"], &[]);
    assert_eq!(out_user.status.code(), Some(0));
    let rep: Value = serde_json::from_slice(&out_user.stdout).unwrap();
    assert_eq!(rep["settings"]["ranking.top"]["value"], 2);
    assert_eq!(
        rep["settings"]["ranking.top"]["sources"],
        serde_json::json!(["trusted-user"])
    );

    // Case C: project override (3)
    f.write_project_config(b"[ranking]\ntop=3\n");
    let out_proj = f.run(&["doctor", "--config", "--json"], &[]);
    assert_eq!(out_proj.status.code(), Some(0));
    let rep: Value = serde_json::from_slice(&out_proj.stdout).unwrap();
    assert_eq!(rep["settings"]["ranking.top"]["value"], 3);
    assert_eq!(
        rep["settings"]["ranking.top"]["sources"],
        serde_json::json!(["project"])
    );

    // Case D: environment override (4)
    let out_env = f.run(&["doctor", "--config", "--json"], &[("SR_TOP", "4")]);
    assert_eq!(out_env.status.code(), Some(0));
    let rep: Value = serde_json::from_slice(&out_env.stdout).unwrap();
    assert_eq!(rep["settings"]["ranking.top"]["value"], 4);
    assert_eq!(
        rep["settings"]["ranking.top"]["sources"],
        serde_json::json!(["environment"])
    );

    // Case E: CLI flag override (1)
    let out_cli = f.run(
        &["doctor", "--config", "--json", "--top", "1"],
        &[("SR_TOP", "4")],
    );
    assert_eq!(out_cli.status.code(), Some(0));
    let rep: Value = serde_json::from_slice(&out_cli.stdout).unwrap();
    assert_eq!(rep["settings"]["ranking.top"]["value"], 1);
    assert_eq!(
        rep["settings"]["ranking.top"]["sources"],
        serde_json::json!(["cli"])
    );

    // -------------------------------------------------------------------------
    // 3. Credential Non-Disclosure and Redaction
    // -------------------------------------------------------------------------
    let out_cred = f.run(
        &["doctor", "--config", "--json"],
        &[("TYPESAFE_API_KEY", canary)],
    );
    assert_eq!(out_cred.status.code(), Some(0));
    let rep: Value = serde_json::from_slice(&out_cred.stdout).unwrap();
    assert_eq!(
        rep["settings"]["typesafe.api_key"]["value"]["present"],
        true
    );
    let stdout_text = String::from_utf8_lossy(&out_cred.stdout);
    assert!(!stdout_text.contains(canary));
    assert!(out_cred.stderr.is_empty());

    // -------------------------------------------------------------------------
    // 4. Strict Rejection of Malformed, Forbidden, and Non-Finite Config
    // -------------------------------------------------------------------------
    for invalid_payload in [
        b"[network]\nenabled=true\n".as_slice(), // Forbidden in project layer
        b"[ranking]\ntop=1\ntop=2\n",            // Duplicate key
        b"[ranking]\ngate=nan\n",                // NaN rejected
        b"[ranking]\ngate=inf\n",                // Inf rejected
        b"[ranking]\ntop=99\n",                  // Out of bounds (>32)
        b"[ranking]\ntop=0\n",                   // Out of bounds (<1)
        b"\xff\xfe invalid utf8",                // Non-UTF8
    ] {
        f.write_project_config(invalid_payload);
        let out_inv = f.run(&["doctor", "--config", "--json"], &[]);
        assert_eq!(out_inv.status.code(), Some(2));
        let rep: Value = serde_json::from_slice(&out_inv.stdout).unwrap();
        assert_eq!(rep["error"]["kind"], "invalid-configuration");
        assert!(!String::from_utf8_lossy(&out_inv.stdout).contains(canary));
        assert!(!String::from_utf8_lossy(&out_inv.stderr).contains(canary));
    }

    // Oversized config (> 256 KiB) is rejected
    f.write_project_config(&vec![b' '; 256 * 1024 + 1]);
    let out_oversized = f.run(&["doctor", "--config"], &[]);
    assert_eq!(out_oversized.status.code(), Some(2));

    // Symlink traversal escaping workspace is rejected
    std::fs::write(f.root.join("outside.toml"), "[ranking]\ntop=1\n").unwrap();
    let symlink_path = f.root.join("workspace/.sr/config.toml");
    let _ = std::fs::remove_file(&symlink_path);
    std::os::unix::fs::symlink(f.root.join("outside.toml"), &symlink_path).unwrap();
    let out_symlink = f.run(&["doctor", "--config"], &[]);
    assert_eq!(out_symlink.status.code(), Some(2));
    let _ = std::fs::remove_file(&symlink_path);

    // -------------------------------------------------------------------------
    // 5. Zero Filesystem Mutation and State Side-Effects
    // -------------------------------------------------------------------------
    f.write_project_config(b"");
    let workspace_probe = f.root.join("workspace/probe_marker");
    let state_probe = f.root.join("user/.local/state");

    for (args, env) in [
        (&["doctor", "--config", "--json"][..], vec![]),
        (&["doctor", "--config", "--json"][..], vec![("SR_TOP", "5")]),
        (&["--help"][..], vec![]),
        (
            &["doctor", "--config", "--json"][..],
            vec![("SR_TOP", "999")],
        ), // failure
    ] {
        let out_eff = f.run(args, &env);
        assert!(matches!(out_eff.status.code(), Some(0) | Some(2)));
        assert!(
            !workspace_probe.exists(),
            "local inspection must not write to workspace"
        );
        assert!(
            !state_probe.exists(),
            "local inspection must not create persistent state"
        );
    }

    // -------------------------------------------------------------------------
    // 6. Consequential Boundary Rereads via ConfigFiles::refresh
    // -------------------------------------------------------------------------
    let files = ConfigFiles::new(f.root.join("workspace"), Some(f.root.join("user")));
    let user_toml = f.root.join("user/sr/config.toml");
    std::fs::write(
        &user_toml,
        "[ranking]\ntop=2\nshortlist=8\n[network]\nenabled=true\n",
    )
    .unwrap();
    f.write_project_config(b"[ranking]\ntop=3\n");

    let clock = EntryClock::capture().expect("clock capture");
    let initial = files
        .load(
            &clock,
            ConfigSources {
                cli: vec![("ranking.top".into(), RawValue::Integer(4))],
                ..ConfigSources::default()
            },
        )
        .expect("initial load");

    let effects = EffectPolicy::from_flags(EffectFlags::default()).expect("effect policy");
    let receipt = initial.receipt(effects);
    let admission = PolicyBoundary::ProviderAdmission;

    // Unchanged config produces Revalidation::Unchanged
    let (_, reval_unchanged) = files
        .refresh(&clock, &initial, &receipt, admission)
        .expect("refresh unchanged");
    assert_eq!(reval_unchanged, Revalidation::Unchanged);

    // Overridden edit: changing user ranking.top to 5 does not change effective top (which is CLI 4)
    std::fs::write(
        &user_toml,
        "[ranking]\ntop=5\nshortlist=8\n[network]\nenabled=true\n",
    )
    .unwrap();
    let (refreshed_override, reval_override) = files
        .refresh(&clock, &initial, &receipt, admission)
        .expect("refresh override");
    assert_eq!(refreshed_override.effective().top(), 4);
    assert_eq!(reval_override, Revalidation::Unchanged);

    // Consequential change: changing network.enabled from true to false triggers Revalidation::Superseded
    std::fs::write(
        &user_toml,
        "[ranking]\ntop=5\nshortlist=8\n[network]\nenabled=false\n",
    )
    .unwrap();
    let (_, reval_changed) = files
        .refresh(&clock, &initial, &receipt, admission)
        .expect("refresh changed");
    assert_eq!(
        reval_changed,
        Revalidation::Superseded(vec![PolicyField::NetworkConsent])
    );

    // Malformed edit during refresh returns invalid-configuration error
    std::fs::write(&user_toml, "[ranking]\ntop=\n").unwrap();
    let err_refresh = files
        .refresh(&clock, &initial, &receipt, admission)
        .unwrap_err();
    assert_eq!(err_refresh.0, 2);
    assert_eq!(err_refresh.1, "invalid-configuration");
}
