//! Correctness-gated batch workloads for Lisp threads and worker isolates.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use neomacs_melpa_test_support::MelpaSandbox;
use serde::{Deserialize, Serialize};

use crate::harness::{
    CorrectnessMismatch, EditorProvenance, HostProvenance, Measurement, MetricName, MetricUnit,
    PreparedScenario, PreparedWorkload, RunRequest, SCENARIO_RESULT_SCHEMA_VERSION, ScenarioId,
    ScenarioOutcome, ScenarioStatus, collect_editor_provenance, collect_host_provenance,
    deserialize_optional_error, mismatch, prepare_gui_runtime_directory, scenario_outcome,
    sha256_file,
};

const FIXTURES: &[(ScenarioId, &str)] = &[
    (ScenarioId::ThreadsNumericLoop, "threads-numeric-loop.el"),
    (ScenarioId::ThreadsJsonParse, "threads-json-parse.el"),
    (ScenarioId::ThreadsStress, "threads-stress.el"),
];

pub(crate) fn prepare(
    workspace_root: &Path,
    request: &RunRequest,
    run_directory: &Path,
) -> Result<PreparedScenario, String> {
    let sandbox = MelpaSandbox::new(&format!("perf-{}", request.scenario))?;
    let editor = collect_editor_provenance(request.editor(), &sandbox)?;
    let fixture_name = FIXTURES
        .iter()
        .find_map(|(scenario, name)| (*scenario == request.scenario).then_some(*name))
        .ok_or_else(|| format!("{} is not a threads scenario", request.scenario))?;
    let source = workspace_root
        .join("crates/neomacs-perf/fixtures")
        .join(fixture_name);
    if !source.is_file() {
        return Err(format!("missing performance fixture {}", source.display()));
    }
    let fixture = run_directory.join(fixture_name);
    fs::copy(&source, &fixture).map_err(|error| {
        format!(
            "failed to copy performance fixture {}: {error}",
            source.display()
        )
    })?;
    let provenance = run_directory.join("input-provenance.json");
    let manifest = ThreadsInputProvenance {
        editor,
        host: collect_host_provenance(request.machine_policy()),
        workload_source: format!("crates/neomacs-perf/fixtures/{fixture_name}"),
        workload_source_sha256: sha256_file(&source)?,
        threading_mode: request
            .benchmark_environment()
            .get("NEOVM_THREADS")
            .map_or_else(
                || "sim".to_owned(),
                |value| value.to_string_lossy().into_owned(),
            ),
        passthrough_environment: request
            .benchmark_environment()
            .into_iter()
            .map(|(name, value)| (name, value.to_string_lossy().into_owned()))
            .collect(),
    };
    fs::write(
        &provenance,
        serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("failed to serialize input provenance: {error}"))?,
    )
    .map_err(|error| format!("failed to write input provenance: {error}"))?;
    Ok(PreparedScenario {
        fixture,
        provenance,
        result: run_directory.join("scenario-result.json"),
        sentinel: run_directory.join("completed"),
        terminal_bytes: run_directory.join("terminal.ansi"),
        gui_app_log: run_directory.join("gui-app.log"),
        gui_weston_log: run_directory.join("weston.log"),
        gui_runtime_directory: prepare_gui_runtime_directory(workspace_root)?,
        sandbox,
        workload: PreparedWorkload::Threads,
    })
}

#[derive(Serialize)]
struct ThreadsInputProvenance {
    editor: EditorProvenance,
    host: HostProvenance,
    workload_source: String,
    workload_source_sha256: String,
    threading_mode: String,
    passthrough_environment: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "ThreadsResultWire")]
pub(crate) struct ThreadsResult {
    schema_version: u32,
    scenario: ScenarioId,
    outcome: ScenarioOutcome,
    threads: u32,
    iterations: Option<u32>,
    serial_us: Option<u64>,
    threads_us: Option<u64>,
    isolates_us: Option<u64>,
    threads_speedup: Option<f64>,
    isolates_speedup: Option<f64>,
    alist_serial_us: Option<u64>,
    alist_threads_us: Option<u64>,
    alist_isolates_us: Option<u64>,
    alist_threads_speedup: Option<f64>,
    alist_isolates_speedup: Option<f64>,
    equal_ok: Option<bool>,
    threading_mode: Option<String>,
    seconds: Option<u32>,
    iterations_total: Option<u64>,
    invariants_ok: Option<bool>,
    gcs_done_delta: Option<u64>,
}

impl ThreadsResult {
    #[cfg(test)]
    pub(crate) fn elapsed_us(&self) -> u64 {
        self.threads_us.unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadsResultWire {
    schema_version: u32,
    scenario: ScenarioId,
    status: ScenarioStatus,
    threads: u32,
    #[serde(default)]
    iterations: Option<u32>,
    #[serde(default)]
    serial_us: Option<u64>,
    #[serde(default)]
    threads_us: Option<u64>,
    #[serde(default)]
    isolates_us: Option<u64>,
    #[serde(default)]
    threads_speedup: Option<f64>,
    #[serde(default)]
    isolates_speedup: Option<f64>,
    #[serde(default)]
    alist_serial_us: Option<u64>,
    #[serde(default)]
    alist_threads_us: Option<u64>,
    #[serde(default)]
    alist_isolates_us: Option<u64>,
    #[serde(default)]
    alist_threads_speedup: Option<f64>,
    #[serde(default)]
    alist_isolates_speedup: Option<f64>,
    #[serde(default)]
    equal_ok: Option<bool>,
    #[serde(default)]
    threading_mode: Option<String>,
    #[serde(default)]
    shared_heap_stats: Option<serde_json::Value>,
    #[serde(default)]
    seconds: Option<u32>,
    #[serde(default)]
    iterations_total: Option<u64>,
    #[serde(default)]
    invariants_ok: Option<bool>,
    #[serde(default)]
    gcs_done_delta: Option<u64>,
    #[serde(deserialize_with = "deserialize_optional_error", rename = "error")]
    error: Option<String>,
}

impl TryFrom<ThreadsResultWire> for ThreadsResult {
    type Error = String;

    fn try_from(wire: ThreadsResultWire) -> Result<Self, Self::Error> {
        let outcome = scenario_outcome(wire.status, wire.error)?;
        let _shared_heap_stats = wire.shared_heap_stats;
        Ok(Self {
            schema_version: wire.schema_version,
            scenario: wire.scenario,
            outcome,
            threads: wire.threads,
            iterations: wire.iterations,
            serial_us: wire.serial_us,
            threads_us: wire.threads_us,
            isolates_us: wire.isolates_us,
            threads_speedup: wire.threads_speedup,
            isolates_speedup: wire.isolates_speedup,
            alist_serial_us: wire.alist_serial_us,
            alist_threads_us: wire.alist_threads_us,
            alist_isolates_us: wire.alist_isolates_us,
            alist_threads_speedup: wire.alist_threads_speedup,
            alist_isolates_speedup: wire.alist_isolates_speedup,
            equal_ok: wire.equal_ok,
            threading_mode: wire.threading_mode,
            seconds: wire.seconds,
            iterations_total: wire.iterations_total,
            invariants_ok: wire.invariants_ok,
            gcs_done_delta: wire.gcs_done_delta,
        })
    }
}

pub(crate) fn validate(request: &RunRequest, result: &ThreadsResult) -> Vec<CorrectnessMismatch> {
    let mut mismatches = Vec::new();
    mismatch(
        &mut mismatches,
        "scenario-result-schema",
        SCENARIO_RESULT_SCHEMA_VERSION,
        result.schema_version,
    );
    mismatch(
        &mut mismatches,
        "scenario-id",
        request.scenario,
        result.scenario,
    );
    mismatch(
        &mut mismatches,
        "scenario-outcome",
        &ScenarioOutcome::Ok,
        &result.outcome,
    );
    if result.threads == 0 {
        mismatch(&mut mismatches, "threads-positive", true, false);
    }
    match result.scenario {
        ScenarioId::ThreadsNumericLoop | ScenarioId::ThreadsJsonParse => {
            if result.threading_mode.as_deref().is_none_or(str::is_empty) {
                mismatch(&mut mismatches, "threading-mode-present", true, false);
            }
            mismatch(
                &mut mismatches,
                "iterations",
                request.iterations().get(),
                result.iterations.unwrap_or_default(),
            );
            if result.equal_ok != Some(true) {
                mismatch(&mut mismatches, "parallel-results-equal", true, false);
            }
            for (name, value) in [
                ("serial-us-positive", result.serial_us),
                ("threads-us-positive", result.threads_us),
            ] {
                if value.is_none_or(|value| value == 0) {
                    mismatch(&mut mismatches, name, true, false);
                }
            }
            if result
                .threads_speedup
                .is_none_or(|value| !value.is_finite() || value <= 0.0)
            {
                mismatch(&mut mismatches, "threads-speedup-positive", true, false);
            }
            if let Some(speedup) = result.threads_speedup {
                let expected = result.serial_us.unwrap_or_default() as f64
                    / result.threads_us.unwrap_or(1) as f64;
                if (speedup - expected).abs() > expected.max(1.0) * 1e-9 {
                    mismatch(
                        &mut mismatches,
                        "threads-speedup-consistent",
                        expected,
                        speedup,
                    );
                }
            }
            if let Some(value) = result.isolates_us {
                if value == 0
                    || result
                        .isolates_speedup
                        .is_none_or(|speedup| !speedup.is_finite() || speedup <= 0.0)
                {
                    mismatch(
                        &mut mismatches,
                        "isolates-measurement-positive",
                        true,
                        false,
                    );
                }
            }
            for (name, value) in [
                ("alist-serial-us-positive", result.alist_serial_us),
                ("alist-threads-us-positive", result.alist_threads_us),
            ] {
                if value.is_some_and(|value| value == 0) {
                    mismatch(&mut mismatches, name, true, false);
                }
            }
        }
        ScenarioId::ThreadsStress => {
            mismatch(
                &mut mismatches,
                "stress-seconds",
                request.iterations().get(),
                result.seconds.unwrap_or_default(),
            );
            if result.invariants_ok != Some(true) {
                mismatch(&mut mismatches, "stress-invariants", true, false);
            }
            if result.seconds.is_none_or(|seconds| seconds == 0) {
                mismatch(&mut mismatches, "stress-seconds-positive", true, false);
            }
            if result
                .iterations_total
                .is_none_or(|iterations| iterations == 0)
            {
                mismatch(&mut mismatches, "stress-iterations-positive", true, false);
            }
        }
        _ => mismatch(&mut mismatches, "threads-scenario-id", true, false),
    }
    mismatches
}

pub(crate) fn measurements(result: &ThreadsResult) -> Vec<Measurement> {
    let mut values = Vec::new();
    let mut push = |name, value: Option<f64>, unit| {
        if let Some(value) = value {
            values.push(Measurement { name, value, unit });
        }
    };
    push(
        MetricName::ThreadCount,
        Some(f64::from(result.threads)),
        MetricUnit::Count,
    );
    push(
        MetricName::SerialElapsedWallTime,
        result.serial_us.map(|v| v as f64),
        MetricUnit::Microseconds,
    );
    push(
        MetricName::ThreadsElapsedWallTime,
        result.threads_us.map(|v| v as f64),
        MetricUnit::Microseconds,
    );
    push(
        MetricName::IsolatesElapsedWallTime,
        result.isolates_us.map(|v| v as f64),
        MetricUnit::Microseconds,
    );
    push(
        MetricName::ThreadSpeedup,
        result.threads_speedup,
        MetricUnit::Ratio,
    );
    push(
        MetricName::IsolatesSpeedup,
        result.isolates_speedup,
        MetricUnit::Ratio,
    );
    push(
        MetricName::AlistThreadSpeedup,
        result.alist_threads_speedup,
        MetricUnit::Ratio,
    );
    push(
        MetricName::AlistIsolatesSpeedup,
        result.alist_isolates_speedup,
        MetricUnit::Ratio,
    );
    push(
        MetricName::AlistSerialElapsedWallTime,
        result.alist_serial_us.map(|v| v as f64),
        MetricUnit::Microseconds,
    );
    push(
        MetricName::AlistThreadsElapsedWallTime,
        result.alist_threads_us.map(|v| v as f64),
        MetricUnit::Microseconds,
    );
    push(
        MetricName::AlistIsolatesElapsedWallTime,
        result.alist_isolates_us.map(|v| v as f64),
        MetricUnit::Microseconds,
    );
    push(
        MetricName::ThreadIterations,
        result.iterations_total.map(|v| v as f64),
        MetricUnit::Count,
    );
    push(
        MetricName::GarbageCollections,
        result.gcs_done_delta.map(|v| v as f64),
        MetricUnit::Count,
    );
    // `elapsed_wall_us` is the parallel Lisp-thread region, the primary metric
    // for these rows. Main-thread-only counters miss the worker execution and
    // therefore cannot measure its total instruction and cycle cost.
    values
}

#[cfg(test)]
#[path = "tests/threads_test.rs"]
mod tests;
