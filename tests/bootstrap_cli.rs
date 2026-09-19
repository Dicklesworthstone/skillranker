use std::process::Command;

#[test]
fn version_reports_package_version() {
    let version = Command::new(env!("CARGO_BIN_EXE_sr"))
        .arg("--version")
        .env_clear()
        .output()
        .unwrap();
    assert!(version.status.success());
    assert_eq!(
        version.stdout,
        concat!("sr ", env!("CARGO_PKG_VERSION"), "\n").as_bytes()
    );
    assert!(version.stderr.is_empty());
}

#[test]
fn unsupported_commands_fail_without_echoing_private_arguments() {
    for args in [
        vec!["rank", "private-canary"],
        vec!["--help", "private-canary"],
        vec!["hook", "private-canary"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .args(args)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["decision"], "unavailable");
        assert_eq!(report["error"]["code"], 2);
        assert_eq!(report["error"]["kind"], "invalid-usage");
        assert_eq!(report["error"]["retryable"], false);
        assert!(!String::from_utf8_lossy(&result.stdout).contains("private-canary"));
        assert!(result.stderr.is_empty());
    }
}

/// Bare `sr` is `sr rank` since P4 (it was a usage error in the P0 bootstrap
/// build). Without a session source it fails with a typed envelope, not usage.
#[test]
fn bare_sr_ranks_instead_of_reporting_usage() {
    let result = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .output()
        .unwrap();
    assert_ne!(result.status.code(), Some(0));
    assert_ne!(result.status.code(), Some(2));
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["decision"], "unavailable");
    assert_ne!(report["error"]["kind"], "invalid-usage");
    assert!(result.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_are_usage_errors_without_panicking() {
    use std::os::unix::ffi::OsStringExt;
    let result = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["decision"], "unavailable");
    assert_eq!(report["error"]["code"], 2);
    assert_eq!(report["error"]["kind"], "invalid-usage");
    assert!(result.stderr.is_empty());
}
