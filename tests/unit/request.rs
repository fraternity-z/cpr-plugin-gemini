//! 验证请求内容转换与不支持的 Responses 状态语义

use serde_json::json;

use crate::request::translate;

#[test]
fn converts_text_and_system_instructions() {
    let result = translate(
        json!({"model":"gemini-2.5-flash", "instructions":"Be concise", "input":"Hello", "stream":true}),
        "gemini-2.5-flash",
    ).unwrap();
    assert_eq!(
        result.body["systemInstruction"]["parts"],
        json!([{"text":"Be concise"}])
    );
    assert_eq!(
        result.body["contents"],
        json!([{"role":"user","parts":[{"text":"Hello"}]}])
    );
}

#[test]
fn rejects_previous_response_id_instead_of_losing_context() {
    assert!(
        translate(
            json!({"input":"Continue", "previous_response_id":"resp_previous"}),
            "gemini-2.5-flash",
        )
        .is_err()
    );
}

#[test]
fn rejects_non_string_input_type_instead_of_reinterpreting_it_as_a_message() {
    assert!(
        translate(
            json!({"input":[{"type":17,"role":"user","content":"hello"}]}),
            "gemini-2.5-flash"
        )
        .is_err()
    );
}

#[test]
fn rejects_missing_input() {
    assert!(translate(json!({"model":"gemini-2.5-flash"}), "gemini-2.5-flash").is_err());
}

#[test]
fn preserves_parallel_calls_and_function_results() {
    let result = translate(
        json!({"input":[
            {"role":"user","content":"Check two cities"},
            {"type":"function_call","call_id":"a","name":"weather","arguments":"{\"city\":\"A\"}"},
            {"type":"function_call","call_id":"b","name":"weather","arguments":"{\"city\":\"B\"}"},
            {"type":"function_call_output","call_id":"a","output":"sunny"},
            {"type":"function_call_output","call_id":"b","output":"rain"}
        ]}),
        "gemini-2.5-flash",
    )
    .unwrap();
    assert_eq!(result.body["contents"].as_array().unwrap().len(), 3);
    assert_eq!(
        result.body["contents"][1]["parts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        result.body["contents"][2]["parts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        result.body["contents"][2]["parts"][1]["functionResponse"]["name"],
        "weather"
    );
}

#[test]
fn rejects_orphan_and_duplicate_tool_results() {
    for input in [
        json!([{"type":"function_call_output","call_id":"missing","output":"x"}]),
        json!([
            {"type":"function_call","call_id":"a","name":"f","arguments":"{}"},
            {"type":"function_call_output","call_id":"a","output":"x"},
            {"type":"function_call_output","call_id":"a","output":"x"}
        ]),
    ] {
        assert!(translate(json!({"input":input}), "gemini-2.5-flash").is_err());
    }
}

#[test]
fn maps_generation_settings_and_json_schema() {
    let result = translate(json!({"input":"JSON please", "max_output_tokens":100, "temperature":0.3, "top_p":0.8,
        "reasoning":{"effort":"low","summary":"auto"},
        "text":{"format":{"type":"json_schema","name":"Result","schema":{"type":"object","properties":{"ok":{"type":"boolean"}}}}}
    }), "gemini-2.5-flash").unwrap();
    assert_eq!(result.body["generationConfig"]["maxOutputTokens"], 100);
    assert_eq!(
        result.body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        1024
    );
    assert_eq!(
        result.body["generationConfig"]["responseMimeType"],
        "application/json"
    );
}

#[test]
fn maps_base64_images_without_fetching_urls() {
    let result = translate(json!({"input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}]}]}), "gemini-2.5-flash").unwrap();
    assert_eq!(
        result.body["contents"][0]["parts"][0]["inlineData"]["mimeType"],
        "image/png"
    );
    assert!(translate(json!({"input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.com/image.png"}]}]}), "gemini-2.5-flash").is_err());
}

#[test]
fn namespace_aliases_are_stable_and_tool_choice_uses_the_alias() {
    let result = translate(json!({"input":"go", "tools":[{"type":"namespace","name":"files","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]}],
        "tool_choice":{"type":"function","namespace":"files","name":"read"}
    }), "gemini-2.5-flash").unwrap();
    let name = &result.body["tools"][0]["functionDeclarations"][0]["name"];
    assert_eq!(
        &result.body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
        name
    );
    assert_ne!(name, "read");
}

#[test]
fn rejects_unsupported_semantics_instead_of_ignoring_them() {
    for (key, value) in [
        ("store", json!(true)),
        ("background", json!(true)),
        ("truncation", json!("auto")),
        ("service_tier", json!("priority")),
        ("audio", json!({})),
        ("tools", json!([{"type":"web_search"}])),
    ] {
        let mut request = json!({"input":"Hello"});
        request[key] = value;
        assert!(translate(request, "gemini-2.5-flash").is_err(), "{key}");
    }
}
