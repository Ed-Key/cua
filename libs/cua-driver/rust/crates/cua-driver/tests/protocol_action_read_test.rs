//! Run only in the authorized macOS test VM. No valid native action is sent.
#![cfg(target_os = "macos")]
use cua_driver_testkit::RawDriver;
use serde_json::json;

fn driver() -> RawDriver {
    let mut d = RawDriver::spawn_with_env(&[
        ("CUA_DRIVER_PERMISSION_MODE", "unrestricted"),
        ("CUA_DRIVER_DANGEROUSLY_BYPASS_APPROVALS", "1"),
    ])
    .unwrap();
    d.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"action-read-protocol-test","version":"1"}}}));
    assert!(d.recv()["result"]["protocolVersion"].is_string());
    d
}
#[test]
fn action_read_live_schema_is_the_mac_contract_and_not_an_action_result() {
    let mut d = driver();
    d.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let response = d.recv();
    let tool = response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "act_and_read")
        .expect("registered product tool");
    let contract = cua_driver_contract::tool_contract("act_and_read").unwrap();
    assert_eq!(tool["inputSchema"], contract.input_schema);
    assert_eq!(
        tool["outputSchema"],
        cua_driver_contract::advertised_output_schema(contract.success_output_schema.unwrap())
    );
    assert!(!cua_driver_contract::is_action_result_tool("act_and_read"));
    assert_ne!(tool["risk"]["class"], "unclassified");
}
#[test]
fn action_read_invalid_observation_is_rejected_before_native_lookup() {
    let mut d = driver();
    for (i, observe) in [
        json!({"query_context":true}),
        json!({"max_elements":0}),
        json!({"pid":1}),
    ]
    .into_iter()
    .enumerate()
    {
        d.send(&json!({"jsonrpc":"2.0","id":i+2,"method":"tools/call","params":{
            "name":"act_and_read","arguments":{"pid":2147483647,"window_id":1,"action":"click","element_token":"never-dispatched","observe":observe}}}));
        let response = d.recv();
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(
            response["result"]["content"]
                .to_string()
                .contains("invalid arguments"),
            "{response}"
        );
        assert!(response
            .pointer("/result/structuredContent/action")
            .is_none());
    }
}
