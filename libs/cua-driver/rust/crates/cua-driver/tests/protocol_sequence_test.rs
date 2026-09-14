//! Focused protocol checks for the bounded window sequence composite.
//! No valid action is sent to a native application by these tests.

use cua_driver_testkit::RawDriver;
use serde_json::{json, Value};

fn driver() -> RawDriver {
    let mut driver = RawDriver::spawn_with_env(&[
        ("CUA_DRIVER_PERMISSION_MODE", "unrestricted"),
        ("CUA_DRIVER_DANGEROUSLY_BYPASS_APPROVALS", "1"),
    ])
    .expect("build the driver before running sequence protocol tests");
    driver.send(&json!({
        "jsonrpc":"2.0", "id":1, "method":"initialize",
        "params":{"protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"sequence-protocol-test", "version":"1"}}
    }));
    let response = driver.recv();
    assert!(
        response["result"]["protocolVersion"].is_string(),
        "{response}"
    );
    driver
}

fn no_images(response: &Value) {
    assert!(response["result"]["content"]
        .as_array()
        .is_none_or(|blocks| blocks.iter().all(|block| block["type"] != "image")));
}

#[test]
fn sequence_is_registered_with_closed_flat_input_and_composite_output() {
    let mut driver = driver();
    driver.send(&json!({"jsonrpc":"2.0", "id":2, "method":"tools/list"}));
    let response = driver.recv();
    let tool = response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "run_sequence")
        .expect("run_sequence must be registered by the production runtime");
    let input = &tool["inputSchema"];
    assert_eq!(input["additionalProperties"], false);
    let steps = &input["properties"]["steps"];
    assert_eq!(steps["minItems"], 1);
    assert_eq!(steps["maxItems"], 8);
    let step = &steps["items"];
    assert_eq!(step["additionalProperties"], false);
    assert!(step.get("oneOf").is_none());
    let arguments = &step["properties"]["arguments"];
    assert_eq!(arguments["additionalProperties"], false);
    let properties = arguments["properties"].as_object().unwrap();
    assert_eq!(properties.len(), 4);
    for key in ["x", "y", "element_token", "text"] {
        assert!(properties.contains_key(key));
    }
    assert_eq!(step["properties"]["timeout_ms"]["default"], 1000);
    assert_eq!(step["properties"]["timeout_ms"]["maximum"], 10_000);
    assert_eq!(step["properties"]["stable_samples"]["maximum"], 5);
    assert_eq!(tool["outputSchema"]["type"], "object");
    let success = &tool["outputSchema"]["anyOf"][0];
    for key in ["status", "stopped_at", "stop_reason", "elapsed_ms", "steps"] {
        assert!(
            success["properties"][key].is_object(),
            "missing {key}: {success}"
        );
    }
    assert!(!cua_driver_contract::is_action_result_tool("run_sequence"));
    assert_ne!(tool["risk"]["class"], "unclassified");
}

fn first_step() -> Value {
    json!({"tool":"click", "arguments":{"x":10,"y":20},
        "expect":[{"window":{"exists":true}}], "timeout_ms":0})
}

#[test]
fn malformed_later_steps_are_protocol_errors_before_native_dispatch() {
    let mut driver = driver();
    let cases = [
        json!({"tool":"type_text", "arguments":{"text":"unused","session":"other"},
            "expect":[{"window":{"exists":true}}]}),
        json!({"tool":"click", "arguments":{"x":10,"y":20,"delivery_mode":"foreground"},
            "expect":[{"window":{"exists":true}}]}),
        json!({"tool":"click", "arguments":{"x":10,"y":20,"_session_id":"forged"},
            "expect":[{"window":{"exists":true}}]}),
        json!({"tool":"type_text", "arguments":{"text":"unused"},
            "expect":[{"window":{"exists":true,"unreviewed":true}}]}),
        json!({"tool":"type_text", "arguments":{"text":"unused"},
            "expect":[{"window":{"exists":true}}], "timeout_ms":0,"stable_samples":2}),
        json!({"tool":"click", "arguments":{"x":10},
            "expect":[{"window":{"exists":true}}]}),
    ];
    for (index, later) in cases.into_iter().enumerate() {
        driver.send(
            &json!({"jsonrpc":"2.0", "id":index+2, "method":"tools/call",
            "params":{"name":"run_sequence", "arguments":{
                "pid":2_147_483_647_u64,"window_id":1,"steps":[first_step(),later]}}}),
        );
        let response = driver.recv();
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(
            response
                .pointer("/result/structuredContent/status")
                .is_none(),
            "{response}"
        );
        let text = response["result"]["content"].to_string();
        assert!(
            !text.contains("Unknown tool"),
            "executor was not exercised: {response}"
        );
        assert!(
            text.contains("unknown field")
                || text.contains("step")
                || text.contains("stable_samples"),
            "expected request validation, not a native failure: {response}"
        );
        no_images(&response);
    }
}
