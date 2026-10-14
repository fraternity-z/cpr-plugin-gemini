//! 验证 Gemini 文本、工具、签名、用量与 Responses 终态的转换

use gateway_plugin_sdk::call::model::{CanonicalEvent, ExecutionEvent, FinishReason, WirePayload};
use serde_json::{Value, json};

use crate::{request::translate, response::Response};

fn response(stream: bool, tools: Value) -> Response {
    let prepared = translate(
        json!({"input":"Hello", "stream":stream, "tools":tools}),
        "gemini-2.5-flash",
    )
    .unwrap();
    Response::new(
        "gemini-2.5-flash".into(),
        stream,
        prepared.tools,
        prepared.response_fields,
    )
}

fn chunk(text: &str) -> Value {
    json!({"candidates":[{"index":0, "content":{"role":"model", "parts":[{"text":text}]}}]})
}

fn wire(events: &[ExecutionEvent]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|event| match &event.wire.as_ref()?.payload {
            WirePayload::Json { data, .. } => Some(data.clone()),
            WirePayload::RawJson { body } => Some(serde_json::from_slice(body).unwrap()),
            _ => None,
        })
        .collect()
}

#[test]
fn streams_deltas_and_a_single_completed_response_with_monotonic_sequence() {
    let mut response = response(true, json!([]));
    let mut events = response.consume(chunk("Hello ")).unwrap();
    events.extend(response.consume(chunk("world")).unwrap());
    events.extend(response.consume(json!({"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":3,"cachedContentTokenCount":4,"totalTokenCount":15}})).unwrap());
    assert!(
        !wire(&events)
            .iter()
            .any(|event| event["type"] == "response.completed")
    );
    events.extend(response.finish().unwrap());
    let values = wire(&events);
    for (index, value) in values.iter().enumerate() {
        assert_eq!(value["sequence_number"], index as u64);
    }
    let final_response = &values.last().unwrap()["response"];
    assert_eq!(
        final_response["output"][0]["content"][0]["text"],
        "Hello world"
    );
    assert_eq!(final_response["usage"]["output_tokens"], 5);
    assert_eq!(
        final_response["usage"]["output_tokens_details"]["reasoning_tokens"],
        3
    );
    assert_eq!(
        events
            .iter()
            .flat_map(|event| &event.facts)
            .filter(|fact| matches!(fact, CanonicalEvent::Completed { .. }))
            .count(),
        1
    );
    assert!(response.finish().is_err());
}

#[test]
fn non_streaming_provides_the_terminal_event_required_by_the_host_collector() {
    let mut response = response(false, json!([]));
    response
        .consume(json!({"candidates":[{"finishReason":"STOP"}]}))
        .unwrap();
    let events = response.finish().unwrap();
    assert!(
        matches!(&events.last().unwrap().wire.as_ref().unwrap().payload,
        WirePayload::Json { event:Some(name), data, .. } if name == "response.completed" && data["response"]["object"] == "response")
    );
}

#[test]
fn non_streaming_has_only_one_json_wire_but_preserves_canonical_facts() {
    let mut response = response(false, json!([]));
    let mut events = response.consume(chunk("Hello")).unwrap();
    events.extend(
        response
            .consume(json!({"candidates":[{"finishReason":"STOP"}]}))
            .unwrap(),
    );
    events.extend(response.finish().unwrap());
    let values = wire(&events);
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["type"], "response.completed");
    assert_eq!(values[0]["response"]["object"], "response");
    assert_eq!(values[0]["response"]["status"], "completed");
    assert!(values[0]["response"]["usage"].is_null());
    assert!(
        events
            .iter()
            .flat_map(|event| &event.facts)
            .any(|fact| matches!(fact, CanonicalEvent::TextDelta { text, .. } if text == "Hello"))
    );
}

#[test]
fn round_trips_signed_function_calls_without_losing_the_original_part() {
    let tools = json!([{"type":"function", "name":"weather", "parameters":{"type":"object","properties":{"city":{"type":"string"}}}}]);
    let part = json!({"functionCall":{"name":"weather","args":{"city":"北京"},"id":"google_call"},"thoughtSignature":"opaque-signature"});
    let mut response = response(true, tools.clone());
    response
        .consume(json!({"candidates":[{"content":{"parts":[part.clone()]},"finishReason":"STOP"}]}))
        .unwrap();
    let values = wire(&response.finish().unwrap());
    let mut history = vec![json!({"role":"user","content":"weather?"})];
    history.extend(
        values.last().unwrap()["response"]["output"]
            .as_array()
            .unwrap()
            .clone(),
    );
    history.push(json!({"type":"function_call_output","call_id":"google_call","output":"sunny"}));
    let next = translate(json!({"input":history, "tools":tools}), "gemini-2.5-flash").unwrap();
    assert_eq!(next.body["contents"][1]["parts"][0], part);
    assert_eq!(
        next.body["contents"][2]["parts"][0]["functionResponse"]["id"],
        "google_call"
    );
    assert_eq!(
        next.body["contents"][2]["parts"][0]["functionResponse"]["name"],
        "weather"
    );
}

#[test]
fn round_trips_signed_text_without_joining_signed_parts() {
    let mut response = response(false, json!([]));
    let parts =
        json!([{"text":"a","thoughtSignature":"sig-a"},{"text":"b","thoughtSignature":"sig-b"}]);
    response
        .consume(json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]}))
        .unwrap();
    let final_response = &wire(&response.finish().unwrap())[0]["response"];
    let mut input = vec![json!({"role":"user","content":"say ab"})];
    input.extend(final_response["output"].as_array().unwrap().clone());
    input.push(json!({"role":"user","content":"continue"}));
    let next = translate(json!({"input":input}), "gemini-2.5-flash").unwrap();
    assert_eq!(next.body["contents"][1]["parts"], parts);
}

#[test]
fn parallel_signed_turn_preserves_all_parts_not_just_signed_ones() {
    let tools = json!([{"type":"function","name":"first"},{"type":"function","name":"second"}]);
    let parts = json!([
        {"functionCall":{"name":"first","args":{}},"thoughtSignature":"turn-signature"},
        {"functionCall":{"name":"second","args":{}}}
    ]);
    let mut response = response(false, tools.clone());
    response
        .consume(json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]}))
        .unwrap();
    let result = wire(&response.finish().unwrap()).remove(0)["response"].clone();
    let output = result["output"].as_array().unwrap();
    let mut input = vec![json!({"role":"user","content":"both tools"})];
    input.extend(output.clone());
    for call in output.iter().filter(|item| item["type"] == "function_call") {
        input.push(json!({"type":"function_call_output","call_id":call["call_id"],"output":"OK"}));
    }
    let next = translate(json!({"input":input,"tools":tools}), "gemini-2.5-flash").unwrap();
    assert_eq!(next.body["contents"][1]["parts"], parts);
}

#[test]
fn signed_parameterless_calls_keep_an_omitted_args_field() {
    let mut response = response(false, json!([{"type":"function", "name":"status"}]));
    let part = json!({"functionCall":{"name":"status"},"thoughtSignature":"sig"});
    response
        .consume(json!({"candidates":[{"content":{"parts":[part.clone()]},"finishReason":"STOP"}]}))
        .unwrap();
    let result = wire(&response.finish().unwrap()).remove(0)["response"].clone();
    let mut input = vec![json!({"role":"user","content":"status?"})];
    input.extend(result["output"].as_array().unwrap().clone());
    let call_id = result["output"][0]["call_id"].clone();
    input.push(json!({"type":"function_call_output","call_id":call_id,"output":"OK"}));
    let next = translate(json!({"input":input}), "gemini-2.5-flash").unwrap();
    assert_eq!(next.body["contents"][1]["parts"][0], part);
    assert!(
        next.body["contents"][2]["parts"][0]["functionResponse"]
            .get("id")
            .is_none()
    );
}

#[test]
fn signed_thoughts_and_text_preserve_order_and_reject_tampering() {
    let parts = json!([{"text":"Summary","thought":true,"thoughtSignature":"thought-sig"},{"text":"Answer","thoughtSignature":"answer-sig"}]);
    let mut response = response(false, json!([]));
    response
        .consume(json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]}))
        .unwrap();
    let result = wire(&response.finish().unwrap()).remove(0)["response"].clone();
    let mut input = vec![json!({"role":"user","content":"question"})];
    input.extend(result["output"].as_array().unwrap().clone());
    input.push(json!({"role":"user","content":"continue"}));
    let next = translate(json!({"input":input}), "gemini-2.5-flash").unwrap();
    assert_eq!(next.body["contents"][1]["parts"], parts);
    assert!(translate(json!({"input":input}), "gemini-2.5-pro").is_err());
    input[2]["content"][0]["text"] = json!("tampered");
    assert!(translate(json!({"input":input}), "gemini-2.5-flash").is_err());
}

#[test]
fn rejects_malformed_content_and_unexpected_tool_calls() {
    assert!(
        response(true, json!([]))
            .consume(json!({"candidates":[{"content":"bad","finishReason":"STOP"}]}))
            .is_err()
    );
    assert!(response(true, json!([])).consume(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"undeclared","args":{}}}]}}]})).is_err());
    assert!(response(true, json!([])).consume(json!({"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"AA=="}}]}}]})).is_err());
    assert!(
        response(true, json!([]))
            .consume(json!({"usageMetadata":{"promptTokenCount":-1}}))
            .is_err()
    );
}

#[test]
fn blocks_and_late_usage_are_not_mistaken_for_truncated_success() {
    let mut blocked = response(false, json!([]));
    blocked
        .consume(json!({"promptFeedback":{"blockReason":"SAFETY"}}))
        .unwrap();
    assert_eq!(
        wire(&blocked.finish().unwrap())[0]["response"]["status"],
        "incomplete"
    );
    let mut late = response(false, json!([]));
    late.consume(json!({"candidates":[{"finishReason":"STOP"}]}))
        .unwrap();
    late.consume(json!({"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":0,"totalTokenCount":1}})).unwrap();
    assert_eq!(
        wire(&late.finish().unwrap())[0]["response"]["usage"]["total_tokens"],
        1
    );
}

#[test]
fn maps_length_and_safety_to_incomplete_instead_of_success() {
    for (reason, expected) in [
        ("MAX_TOKENS", "max_output_tokens"),
        ("SAFETY", "content_filter"),
    ] {
        let mut response = response(true, json!([]));
        response
            .consume(json!({"candidates":[{"finishReason":reason}]}))
            .unwrap();
        let values = wire(&response.finish().unwrap());
        assert_eq!(values.last().unwrap()["type"], "response.incomplete");
        assert_eq!(
            values.last().unwrap()["response"]["incomplete_details"]["reason"],
            expected
        );
    }
}

#[test]
fn refuses_a_truncated_stream_or_invalid_finish_reason() {
    let mut response = response(true, json!([]));
    response.consume(chunk("partial")).unwrap();
    assert!(response.finish().is_err());
    assert!(
        response
            .consume(json!({"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}))
            .is_err()
    );
}

#[test]
fn cumulative_usage_is_not_added_across_chunks() {
    let mut response = response(true, json!([]));
    response.consume(json!({"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1,"thoughtsTokenCount":2}})).unwrap();
    response.consume(json!({"candidates":[{"finishReason":"STOP"}], "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"thoughtsTokenCount":2,"totalTokenCount":15}})).unwrap();
    let values = wire(&response.finish().unwrap());
    assert_eq!(
        values.last().unwrap()["response"]["usage"]["output_tokens"],
        5
    );
    assert_eq!(
        values.last().unwrap()["response"]["usage"]["input_tokens"],
        10
    );
}

#[test]
fn custom_tools_are_returned_as_custom_tool_calls() {
    let mut response = response(true, json!([{"type":"custom", "name":"apply_patch"}]));
    let events = response.consume(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"apply_patch","args":{"input":"patch text"}}}]},"finishReason":"STOP"}]})).unwrap();
    assert!(wire(&events).iter().any(|event| event["type"]
        == "response.custom_tool_call_input.delta"
        && event["delta"] == "patch text"));
    let events = response.finish().unwrap();
    let values = wire(&events);
    assert_eq!(
        values.last().unwrap()["response"]["output"][0]["type"],
        "custom_tool_call"
    );
    assert!(
        events
            .iter()
            .flat_map(|event| &event.facts)
            .any(|fact| matches!(
                fact,
                CanonicalEvent::Completed {
                    reason: FinishReason::ToolCall,
                    ..
                }
            ))
    );
}
