//! Bind exact replay to the actual compiler, target and Cargo build controls.
#![forbid(unsafe_code)]
use std::{env, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let compiler = env::var_os("RUSTC").expect("Cargo must identify the compiler");
    let mut command = Command::new(compiler);
    command.arg("-vV").env_clear();
    // Cargo may point at a rustup proxy. Pass only its lookup environment,
    // never provider credentials, config, preload hooks or session state.
    for name in [
        "PATH",
        "HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "LD_LIBRARY_PATH",
    ] {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    let version = command
        .output()
        .expect("compiler version must be available");
    assert!(
        version.status.success() && version.stdout.len() <= 4096,
        "compiler profile unavailable"
    );
    let mut hash = blake3::Hasher::new();
    hash.update(b"sr.numeric-build.v1\0");
    hash.update(&version.stdout);
    for name in [
        "TARGET",
        "HOST",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "CARGO_CFG_TARGET_FEATURE",
        "CARGO_FEATURE_TUI",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "RUSTC_BOOTSTRAP",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
        hash.update(&(name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        let value = env::var(name).unwrap_or_default();
        hash.update(&(value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    let mut source = blake3::Hasher::new();
    source.update(b"sr.replay-computation-source.v1\0");
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        "src/scoring.rs",
        "src/eligibility.rs",
        "src/replay.rs",
        "src/replay/frozen.rs",
        "src/pipeline.rs",
        "src/jev/codec.rs",
        "src/jev/cloudflare_codec.rs",
        "src/jev/wide.rs",
        "src/jev/rerank.rs",
        "src/context/render.rs",
        "src/context/branch.rs",
        "src/privacy/redaction.rs",
    ] {
        println!("cargo:rerun-if-changed={name}");
        let bytes = std::fs::read(name).expect("replay computation source must be available");
        source.update(&(name.len() as u64).to_le_bytes());
        source.update(name.as_bytes());
        source.update(&(bytes.len() as u64).to_le_bytes());
        source.update(&bytes);
    }
    println!(
        "cargo:rustc-env=SKILLRANKER_REPLAY_COMPUTATION_DIGEST={}",
        source.finalize().to_hex()
    );
    println!(
        "cargo:rustc-env=SKILLRANKER_NUMERIC_BUILD_DIGEST={}",
        hash.finalize().to_hex()
    );
}
