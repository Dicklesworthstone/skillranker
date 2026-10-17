#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Phase P9 optional nine-row FrankenTUI inspection, watch-generation fencing,
//! non-TTY fallback refusal, and clean terminal recovery (sr-roadmap-l1i.10.7).
//!
//! Satisfies contract boundary `p9_frankentui` mapped in
//! `tests/contract_matrix.toml`.

use skillranker::experiments::{
    ExperimentError, MIN_WATCH_INTERVAL_MS, TuiCandidateRow, TuiState, run_tui_session,
};

#[test]
fn tui_rendering_and_recovery() {
    // 1. Non-TTY invocation without headless script is refused cleanly (`non-tty-fallback`).
    let non_tty = run_tui_session(false, None, 9);
    assert!(matches!(non_tty, Err(ExperimentError::NonTty(_))));

    // 2. Nine-row inline layout bounds, key handling (`1`-`5`, `r`, `w`, `e`, `q`),
    //    and clean terminal-state restoration (`terminal-state-restored`).
    let result = run_tui_session(true, Some("1ewrq"), 9).unwrap();
    assert_eq!(result["row_count"], 9);
    assert_eq!(result["max_rows"], 9);
    assert_eq!(result["terminal_state_restored"], true);
    assert_eq!(result["loader_executed"], false);
    assert_eq!(result["watch_enabled"], true);
    assert_eq!(result["show_evidence"], true);
    assert_eq!(result["active_generation"], 2);
    assert_eq!(
        result["selected_skill"]["skill_id"],
        "sk-rust-triage",
        "key '1' must select top locally resolved skill without executing a loader"
    );

    // 3. Small terminal height degrades gracefully to fewer rows (`max_rows.clamp(3, 9)`).
    let mut state = TuiState::sample_default();
    let compact = state.render_inline(4);
    assert_eq!(compact.len(), 4);

    // 4. Watch mode enforces >= 5,000 ms minimum interval and monotonic generation fencing.
    state.handle_key('w', 1_500);
    assert!(state.watch_enabled);
    let new_rows = vec![TuiCandidateRow {
        rank: 1,
        skill_id: "sk-updated".into(),
        invocation_name: "updated-skill".into(),
        rank_score: 0.91,
        fit: 0.90,
        source: "project".into(),
    }];
    // Too soon (< 5,000 ms since last_refresh_ms=1_000): coalesced / skipped.
    assert_eq!(
        state.apply_watch_update(2, 1_000 + MIN_WATCH_INTERVAL_MS - 1, new_rows.clone()),
        Ok(false)
    );
    // Stale generation (<= active_generation 1): rejected.
    assert_eq!(
        state.apply_watch_update(1, 1_000 + MIN_WATCH_INTERVAL_MS + 10, new_rows.clone()),
        Err("stale-generation-ignored")
    );
    // Valid generation after >= 5,000 ms: applied.
    assert_eq!(
        state.apply_watch_update(2, 1_000 + MIN_WATCH_INTERVAL_MS + 10, new_rows),
        Ok(true)
    );
    assert_eq!(state.active_generation, 2);
    assert_eq!(state.candidates[0].skill_id, "sk-updated");
}
