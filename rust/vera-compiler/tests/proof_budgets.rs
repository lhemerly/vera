//! Explicit proof costs fail closed and stay independent of declared state size.
use vera_compiler::{
    parse_source,
    semantic::analyze,
    verifier::{verify_transition, VerificationResult, VerifierConfig},
};

fn verify(source: &str, config: &VerifierConfig) -> VerificationResult {
    let program = analyze(parse_source(source).unwrap()).unwrap();
    verify_transition(&program, "go", config)
}

fn resource_failure(result: VerificationResult, limit: &str) {
    assert!(!result.ok());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == "D_VERIFY_RESOURCE" && d.message.contains(limit)),
        "{}",
        result.summary()
    );
    assert!(result.witness.is_none());
    assert!(result.counterexample.is_none());
    assert!(
        result.smt_safety.is_empty(),
        "partial proofs must never be exported"
    );
}

const PURE: &str = "transition go(x:u8) reads [] writes [] ensure same:eq(x,x); body {commit;}";

#[test]
fn expression_budget_is_explicit_and_independent_of_state_budget() {
    for state_limit in [0, 4096, usize::MAX] {
        resource_failure(
            verify(
                PURE,
                &VerifierConfig {
                    max_expression_steps: Some(1),
                    max_state_cells: Some(state_limit),
                    ..VerifierConfig::default()
                },
            ),
            "max_expression_steps",
        );
    }
    let result = verify(
        PURE,
        &VerifierConfig {
            max_expression_steps: Some(32),
            max_state_cells: Some(0),
            ..VerifierConfig::default()
        },
    );
    assert!(result.ok(), "{}", result.summary());
}

#[test]
fn zero_and_small_script_budgets_fail_closed() {
    for max_smt_bytes in [0, 1, 32, 128] {
        resource_failure(
            verify(
                PURE,
                &VerifierConfig {
                    max_smt_bytes: Some(max_smt_bytes),
                    ..VerifierConfig::default()
                },
            ),
            "max_smt_bytes",
        );
    }
    let result = verify(
        PURE,
        &VerifierConfig {
            max_smt_bytes: Some(4096),
            ..VerifierConfig::default()
        },
    );
    assert!(result.ok(), "{}", result.summary());
    for script in [&result.smt_safety, &result.smt_panic, &result.smt_liveness] {
        assert!(script.len() <= 4096);
    }
}

#[test]
fn exhausted_budget_does_not_hide_a_real_violation() {
    let source = PURE.replace("eq(x,x)", "eq(x,0)");
    resource_failure(
        verify(
            &source,
            &VerifierConfig {
                max_expression_steps: Some(1),
                ..VerifierConfig::default()
            },
        ),
        "max_expression_steps",
    );
    let result = verify(&source, &VerifierConfig::default());
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    assert!(result.counterexample.is_some());
}

#[test]
fn bounded_folds_charge_actual_symbolic_expansion() {
    let source = "transition go() reads [] writes [] body {let %n:u64=fold_bounded<32>(a:u64=0,i in [0..31]) {yield a;};commit;}";
    resource_failure(
        verify(
            source,
            &VerifierConfig {
                max_expression_steps: Some(16),
                ..VerifierConfig::default()
            },
        ),
        "max_expression_steps",
    );
    let result = verify(
        source,
        &VerifierConfig {
            max_expression_steps: Some(256),
            ..VerifierConfig::default()
        },
    );
    assert!(result.ok(), "{}", result.summary());
}

#[test]
fn row_local_contract_encoding_scales_linearly_not_pairwise() {
    let mut sizes = Vec::new();
    for capacity in [8, 16, 32] {
        let source = format!("table T capacity {capacity} {{key id:id64<T>;col code:u8 [unique];col v:u8;}} invariant bounded {{forall k in T:le(T[k].v,10);}} transition go(k:id64<T>) reads [] writes [T[k].v] require live:table_exists(T,k); body {{table_set(T,k,v,4);commit;}}");
        let result = verify(&source, &VerifierConfig::default());
        assert!(result.ok(), "{}", result.summary());
        sizes.push(result.smt_safety.len());
    }
    // Structural size, not fragile wall-clock thresholds; each capacity doubles.
    assert!(sizes[1] <= sizes[0] * 3, "{sizes:?}");
    assert!(sizes[2] <= sizes[1] * 3, "{sizes:?}");
}

#[test]
fn cli_accepts_and_enforces_independent_proof_budget_options() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_vera"))
        .args([
            "verify",
            "-",
            "--max-expression-steps",
            "1",
            "--max-smt-bytes",
            "4096",
            "--max-effect-events",
            "0",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(PURE.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("max_expression_steps"), "{text}");
}

#[test]
fn json_reports_effective_limits_and_observed_encoding_sizes() {
    let source = "table T capacity 8 {key id:id64<T>;col v:u8;} effect E(v:u8); transition go() reads [] writes [] ensure count:eq(effect_count(),2); body {emit E(1);emit E(2);commit;}";
    let config = VerifierConfig {
        max_state_cells: Some(24),
        max_expression_steps: Some(256),
        max_smt_bytes: Some(8192),
        max_effect_events: Some(2),
        ..VerifierConfig::default()
    };
    let result = verify(source, &config);
    assert!(result.ok(), "{}", result.summary());
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["config"], serde_json::to_value(&config).unwrap());
    let metrics = result.encoding_metrics.as_ref().unwrap();
    assert_eq!(metrics.state_cells, 24);
    assert_eq!(metrics.effect_events, 2);
    assert!(metrics.expression_steps > 0);
    assert!(metrics.expression_steps <= config.max_expression_steps.unwrap());
    assert!(metrics.retained_smt_bytes <= metrics.max_query_bytes);
    assert!(metrics.max_query_bytes <= config.max_smt_bytes.unwrap());
    assert_eq!(
        metrics.max_query_bytes,
        [&result.smt_safety, &result.smt_panic, &result.smt_liveness]
            .into_iter()
            .map(|script| script.len())
            .max()
            .unwrap()
    );
    assert_eq!(json["encoding_metrics"]["state_cells"], 24);
    assert_eq!(json["encoding_metrics"]["effect_events"], 2);
}

#[test]
fn failed_encoding_reports_limits_without_shrinking_declared_capacity() {
    let source = "table T capacity 8 {key id:id64<T>;col v:u8;} transition go() reads [] writes [] body {commit;}";
    let program = analyze(parse_source(source).unwrap()).unwrap();
    let mut config = VerifierConfig {
        max_state_cells: Some(23),
        ..VerifierConfig::default()
    };
    let result = verify_transition(&program, "go", &config);
    assert!(!result.ok());
    assert!(result.encoding_metrics.is_none());
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["config"]["max_state_cells"], 23);
    assert!(json["encoding_metrics"].is_null());
    assert!(result
        .diagnostics
        .iter()
        .any(|d| d.message.contains("requires 24 cells")));
    assert_eq!(program.tables()[0].capacity, 8);
    config.max_state_cells = Some(24);
    let result = verify_transition(&program, "go", &config);
    assert!(result.ok(), "{}", result.summary());
    assert_eq!(result.encoding_metrics.unwrap().state_cells, 24);
    assert_eq!(program.tables()[0].capacity, 8);
}

#[test]
fn unknown_transition_preserves_configuration_without_metrics() {
    let program = analyze(parse_source(PURE).unwrap()).unwrap();
    let config = VerifierConfig {
        max_smt_bytes: Some(12345),
        ..VerifierConfig::default()
    };
    let result = verify_transition(&program, "missing", &config);
    assert!(!result.ok());
    assert!(result.encoding_metrics.is_none());
    assert_eq!(result.config.max_smt_bytes, Some(12345));
    assert_eq!(
        serde_json::to_value(&result).unwrap()["config"],
        serde_json::to_value(&config).unwrap()
    );
}

#[test]
fn default_configuration_has_no_policy_limits_or_solver_timeout() {
    let config = VerifierConfig::default();
    let json = serde_json::to_value(&config).unwrap();
    assert!(json
        .as_object()
        .unwrap()
        .values()
        .all(serde_json::Value::is_null));
    let result = verify(PURE, &config);
    assert!(result.ok(), "{}", result.summary());
    for script in [&result.smt_safety, &result.smt_panic, &result.smt_liveness] {
        assert!(
            !script.contains(":timeout"),
            "default solver must have no timeout option"
        );
        assert!(
            !script.contains(":rlimit"),
            "default solver must have no resource option"
        );
    }
}

#[test]
fn default_verification_accepts_a_model_above_the_removed_cell_cap() {
    let source = "table T capacity 2048 {key id:id64<T>;col v:u8;} transition go() reads [] writes [] body {commit;}";
    let result = verify(source, &VerifierConfig::default());
    assert!(result.ok(), "{}", result.summary());
    assert_eq!(result.encoding_metrics.unwrap().state_cells, 6144);
    assert!(result.config.max_state_cells.is_none());
    assert_eq!(
        result.witness.unwrap().pre_state.tables[0].occupied.len(),
        2048
    );
}
