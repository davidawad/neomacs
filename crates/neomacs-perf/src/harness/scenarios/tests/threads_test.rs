use std::num::NonZeroU32;
use std::path::PathBuf;
use std::str::FromStr;

use crate::scenario;

use super::*;

fn request() -> RunRequest {
    RunRequest::new(
        ScenarioId::ThreadsNumericLoop,
        PathBuf::from("neomacs"),
        NonZeroU32::new(300).expect("test iteration count is positive"),
    )
}

fn result_json() -> String {
    r#"{"schema_version":1,"scenario":"threads-numeric-loop","status":"ok","threads":2,"iterations":300,"serial_us":100,"threads_us":90,"isolates_us":null,"threads_speedup":1.1111111111111112,"isolates_speedup":null,"equal_ok":true,"threading_mode":"sim","error":null}"#.to_owned()
}

#[test]
fn result_validation_rejects_equal_false() {
    let raw = result_json().replace("\"equal_ok\":true", "\"equal_ok\":false");
    let result: ThreadsResult = serde_json::from_str(&raw).expect("well-formed result");
    let mismatches = validate(&request(), &result);
    assert!(
        mismatches
            .iter()
            .any(|mismatch| mismatch.invariant == "parallel-results-equal"),
        "equal_ok=false must invalidate timing output"
    );
}

#[test]
fn result_deserialization_rejects_missing_threads() {
    let raw = result_json().replace("\"threads\":2,", "");
    assert!(
        serde_json::from_str::<ThreadsResult>(&raw).is_err(),
        "the required threads field must not default to zero"
    );
}

#[test]
fn scenario_ids_round_trip_through_closed_enum() {
    for (name, expected) in [
        ("threads-numeric-loop", ScenarioId::ThreadsNumericLoop),
        ("threads-json-parse", ScenarioId::ThreadsJsonParse),
        ("threads-stress", ScenarioId::ThreadsStress),
    ] {
        let parsed = ScenarioId::from_str(name).expect("known scenario id");
        assert_eq!(parsed, expected);
        assert_eq!(parsed.as_str(), name);
        assert_eq!(
            serde_json::to_string(&parsed).expect("serialize id"),
            format!("\"{name}\"")
        );
        assert_eq!(scenario(parsed).id, parsed);
    }
    assert!(ScenarioId::from_str("threads-unknown").is_err());
}
