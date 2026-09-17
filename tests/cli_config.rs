//! Real binary checks of local configuration trust and stream boundaries.
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-cli-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
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
        cmd.output().unwrap()
    }
    fn project(&self, bytes: &[u8]) {
        std::fs::write(self.root.join("workspace/.sr/config.toml"), bytes).unwrap();
    }
}

#[test]
fn doctor_config_resolves_layers_without_disclosing_credentials() {
    let f = Fixture::new();
    std::fs::write(
        f.root.join("user/sr/config.toml"),
        "[ranking]\ntop=2\nshortlist=8\n",
    )
    .unwrap();
    f.project(b"[ranking]\ntop=3\n");
    let output = f.run(
        &["doctor", "--config", "--json", "--top", "5"],
        &[("SR_TOP", "4"), ("TYPESAFE_API_KEY", "privatecanary")],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["settings"]["ranking.top"]["value"], 5);
    assert_eq!(
        report["settings"]["ranking.top"]["sources"],
        serde_json::json!(["cli"])
    );
    assert_eq!(
        report["settings"]["typesafe.api_key"]["value"]["present"],
        true
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("privatecanary"));
    assert!(output.stderr.is_empty());
    assert!(!f.root.join("home").exists());
    let output = f.run(&["doctor", "--config"], &[("SR_TOP", "4")]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["settings"]["ranking.top"]["value"], 4);
    let output = f.run(&["doctor", "--config"], &[]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["settings"]["ranking.top"]["value"], 3);
}

#[test]
fn malformed_and_forbidden_configuration_never_echoes_private_input() {
    let f = Fixture::new();
    for bytes in [
        b"[network]\nenabled=true\n".as_slice(),
        b"[ranking]\ntop=1\ntop=2\n",
        b"[ranking]\ngate=nan\n",
        b"[ranking]\ntop=99\n",
        b"[privatecanary]\nsecret=\"privatecanary\"\n",
        b"\xffprivatecanary",
        b"[ranking]\ntop=\"privatecanary\"\n",
    ] {
        f.project(bytes);
        let output = f.run(&["doctor", "--config", "--json"], &[]);
        assert_eq!(output.status.code(), Some(2));
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["error"]["kind"], "invalid-configuration");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("privatecanary"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("privatecanary"));
    }
    f.project(&vec![b' '; 256 * 1024 + 1]);
    assert_eq!(f.run(&["doctor", "--config"], &[]).status.code(), Some(2));
}

#[test]
fn help_and_usage_do_not_read_configuration() {
    let f = Fixture::new();
    f.project(b"privatecanary invalid config");
    for args in [&["--help"][..], &["--version"], &["doctor", "--help"]] {
        let output = f.run(args, &[("SR_PRIVATECANARY", "privatecanary")]);
        assert_eq!(output.status.code(), Some(0));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("privatecanary"));
    }
    for args in [
        &["doctor", "--config", "--json", "--table"][..],
        &["doctor", "--config", "--top", "1", "--top", "2"],
        &["doctor", "--privatecanary"],
    ] {
        let output = f.run(args, &[]);
        assert_eq!(output.status.code(), Some(2));
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["error"]["kind"], "invalid-usage");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("privatecanary"));
    }
}

#[cfg(unix)]
#[test]
fn project_symlink_escape_is_refused() {
    let f = Fixture::new();
    std::fs::write(f.root.join("outside.toml"), "[ranking]\ntop=1\n").unwrap();
    std::os::unix::fs::symlink(
        f.root.join("outside.toml"),
        f.root.join("workspace/.sr/config.toml"),
    )
    .unwrap();
    assert_eq!(f.run(&["doctor", "--config"], &[]).status.code(), Some(2));
}

/// Positive effect observer: the sentinel write proves the observer detects a
/// real filesystem mutation; the inspected boundary must leave every probe
/// absent across success, failure and help paths.
#[test]
fn local_inspection_creates_no_effects_and_observer_detects_them() {
    let f = Fixture::new();
    let canary = f.root.join("workspace/effect-probe");
    let ledger_probe = f.root.join("user/.local/state");
    std::fs::write(
        f.root.join("user/sr/config.toml"),
        b"[ranking]\ntop=2\n[roster]\nroots=[\"skills\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(f.root.join("workspace/skills")).unwrap();
    let observes = |path: &std::path::Path| path.exists();
    // Positive control first: a deliberately writing child must be caught.
    let mut writer = Command::new("sh");
    writer
        .env_clear()
        .current_dir(f.root.join("workspace"))
        .args(["-c", "echo marker > effect-probe"]);
    let control = writer.output().unwrap();
    assert!(control.status.success());
    assert!(observes(&canary), "positive control must be observable");
    std::fs::remove_file(&canary).unwrap();
    for (args, env) in [
        (&["doctor", "--config", "--json"][..], Vec::new()),
        (&["doctor", "--config"][..], vec![("SR_TOP", "7")]),
        (&["--help"][..], Vec::new()),
        (
            &["doctor", "--config", "--json"][..],
            vec![("SR_TOP", "999")],
        ),
    ] {
        let output = f.run(args, &env);
        let code = output.status.code();
        assert!(
            matches!(code, Some(0) | Some(2)),
            "unexpected exit {code:?}"
        );
        assert!(!observes(&canary), "no workspace writes permitted");
        assert!(!observes(&ledger_probe), "no persistent state creation");
        // No child execution: a process spawned by sr would inherit no TTY; the
        // canary above proves the observer catches filesystem effects.
    }
}
