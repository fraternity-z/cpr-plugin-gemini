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
fn accepts_codex_client_metadata_without_forwarding_or_echoing_it() {
    for metadata in [
        json!({}),
        json!({
            "x-codex-installation-id":"fixture-private-installation",
            "x-codex-turn-metadata":"fixture-private-turn-metadata",
            "ws_request_header_traceparent":"fixture-private-trace",
            "nested":{"value":"fixture-private-value"}
        }),
    ] {
        let result = translate(json!({"model":"gemini-4-argon","input":"Hello","stream":true,"store":false,"client_metadata":metadata}), "gemini-4-argon").unwrap();
        assert_eq!(result.body["contents"][0]["parts"][0]["text"], "Hello");
        assert!(result.body.get("client_metadata").is_none());
        assert!(!result.response_fields.contains_key("client_metadata"));
        assert!(!result.body.to_string().contains("fixture-private"));
        assert!(
            !serde_json::to_string(&result.response_fields)
                .unwrap()
                .contains("fixture-private")
        );
    }
}

#[test]
fn accepts_codex_and_standard_streaming_hints_without_sending_them_to_gemini() {
    for options in [
        json!({}),
        json!({"reasoning_summary_delivery":"sequential_cutoff"}),
        json!({"include_obfuscation":false}),
        json!({"include_usage":true}),
        json!({"include_usage":false}),
    ] {
        let result = translate(
            json!({"input":"Hello","stream":true,"stream_options":options}),
            "gemini-4-argon",
        )
        .unwrap();
        assert!(result.stream);
        assert!(result.body.get("stream_options").is_none());
        assert!(!result.response_fields.contains_key("stream_options"));
    }
}

#[test]
fn accepts_a_full_codex_style_argon_request() {
    let result = translate(json!({
        "model":"gemini-4-argon", "instructions":"Be concise", "input":[{"role":"user","content":[{"type":"input_text","text":"Hello"}]}],
        "tools":[], "tool_choice":"auto", "parallel_tool_calls":true,
        "reasoning":{"effort":"low","summary":"auto","context":"auto"}, "store":false, "stream":true,
        "stream_options":{"reasoning_summary_delivery":"sequential_cutoff","include_obfuscation":false},
        "include":["reasoning.encrypted_content"], "prompt_cache_key":"fixture-session",
        "text":{"verbosity":"medium"}, "client_metadata":{"x-codex-turn-metadata":"fixture-private-trace"}
    }), "gemini-4-argon").unwrap();
    assert_eq!(
        result.body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "LOW"
    );
    assert_eq!(
        result.body["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
    assert!(result.body.get("client_metadata").is_none());
    assert!(result.body.get("stream_options").is_none());
}

#[test]
fn accepts_client_tool_search_without_treating_it_as_a_hosted_tool() {
    let tools = json!([
        {"type":"namespace","name":"functions","tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}]},
        {"type":"custom","name":"apply_patch","format":{"type":"text"}},
        {"type":"tool_search","execution":"client","description":"Find deferred tools","parameters":{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query","limit"],"additionalProperties":false}}
    ]);
    let result = translate(json!({
        "model":"gemini-4-argon","input":[{"role":"user","content":[{"type":"input_text","text":"Find a file reader"}]}],
        "tools":tools,"tool_choice":"auto","parallel_tool_calls":true,"store":false,"stream":true,
        "reasoning":{"effort":"low","summary":"auto","context":"auto"},
        "include":["reasoning.encrypted_content"],"stream_options":{"reasoning_summary_delivery":"sequential_cutoff"},
        "client_metadata":{"x-codex-turn-metadata":"fixture-private-trace"}
    }), "gemini-4-argon").unwrap();
    let declarations = result.body["tools"][0]["functionDeclarations"]
        .as_array()
        .unwrap();
    assert_eq!(declarations.len(), 3);
    assert_eq!(declarations[2]["name"], "cpr_tool_search");
    assert_eq!(
        declarations[2]["parametersJsonSchema"],
        tools[2]["parameters"]
    );
    assert_eq!(result.response_fields["tools"], tools);
    assert!(!result.body.to_string().contains("fixture-private-trace"));
}

#[test]
fn null_optional_fields_do_not_enable_unsupported_capabilities() {
    let result = translate(json!({"input":"Hello", "client_metadata":null, "stream_options":null,
        "context_management":null, "access_programs":null, "max_tool_calls":null, "top_logprobs":null, "future_option":null,
        "text":{"format":null}, "reasoning":{"effort":null,"summary":null,"context":null}
    }), "gemini-4-argon").unwrap();
    assert_eq!(
        result.body,
        json!({"contents":[{"role":"user","parts":[{"text":"Hello"}]}]})
    );
}

#[test]
fn invalid_metadata_and_stream_options_are_not_silently_accepted() {
    for (key, value) in [
        ("client_metadata", json!("fixture-private-value")),
        ("stream_options", json!([])),
        (
            "stream_options",
            json!({"include_usage":"fixture-private-value"}),
        ),
        (
            "stream_options",
            json!({"reasoning_summary_delivery":"fixture-private-value"}),
        ),
        ("stream_options", json!({"include_obfuscation":true})),
        (
            "stream_options",
            json!({"fixture_private_field":"fixture-private-value"}),
        ),
    ] {
        let mut request = json!({"input":"Hello","stream":true});
        request[key] = value;
        let error = translate(request, "gemini-4-argon")
            .err()
            .expect("option must be rejected");
        assert!(!error.to_string().contains("fixture-private-value"));
    }
}

#[test]
fn identifies_known_unsupported_options_without_disclosing_values() {
    for (key, value) in [
        (
            "context_management",
            json!([{"type":"compaction","compact_threshold":1000,"value":"fixture-private-value"}]),
        ),
        ("access_programs", json!({"cyber":"fixture-private-value"})),
        ("max_tool_calls", json!(4)),
    ] {
        let mut request = json!({"input":"Hello"});
        request[key] = value;
        let error = translate(request, "gemini-4-argon")
            .err()
            .expect("capability must be rejected");
        assert!(
            error.to_string().contains(key),
            "missing option name for {key}"
        );
        assert!(!error.to_string().contains("fixture-private-value"));
    }
    let error = translate(
        json!({"input":"Hello","fixture_private_field_name":"fixture-private-value"}),
        "gemini-4-argon",
    )
    .err()
    .unwrap();
    assert!(!error.to_string().contains("fixture_private_field_name"));
    assert!(!error.to_string().contains("fixture-private-value"));
}

#[test]
fn rejects_explicit_openai_reasoning_context_selection() {
    for context in ["current_turn", "all_turns"] {
        let error = translate(
            json!({"input":"Hello","reasoning":{"effort":"low","context":context}}),
            "gemini-4-argon",
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("reasoning.context"));
    }
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
fn client_tool_search_history_loads_namespaced_and_custom_tools() {
    let loaded = json!([{"type":"namespace","name":"files","tools":[
        {"type":"function","name":"read","defer_loading":true,"parameters":{"type":"object","properties":{"path":{"type":"string"}}}},
        {"type":"custom","name":"apply_patch","format":{"type":"text"}}
    ]}]);
    let result = translate(json!({"input":[
        {"role":"user","content":"Find file tools"},
        {"type":"tool_search_call","call_id":"search-1","execution":"client","arguments":{"query":"file tools"}},
        {"type":"tool_search_output","call_id":"search-1","execution":"client","status":"completed","tools":loaded},
        {"type":"function_call","call_id":"read-1","namespace":"files","name":"read","arguments":"{\"path\":\"example.txt\"}"},
        {"type":"custom_tool_call","call_id":"patch-1","namespace":"files","name":"apply_patch","input":"patch text"},
        {"type":"function_call_output","call_id":"read-1","output":"file text"},
        {"type":"custom_tool_call_output","call_id":"patch-1","output":"applied"}
    ],"tool_choice":{"type":"custom","namespace":"files","name":"apply_patch"}}), "gemini-4-argon").unwrap();
    let contents = &result.body["contents"];
    let declarations = result.body["tools"][0]["functionDeclarations"]
        .as_array()
        .unwrap();
    assert_eq!(declarations.len(), 2);
    assert_eq!(
        contents[1]["parts"][0]["functionCall"],
        json!({"name":"cpr_tool_search","id":"search-1","args":{"query":"file tools"}})
    );
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"],
        json!({"name":"cpr_tool_search","id":"search-1","response":{"tools":loaded}})
    );
    assert_eq!(
        contents[3]["parts"][0]["functionCall"]["name"],
        declarations[0]["name"]
    );
    assert_eq!(
        contents[3]["parts"][1]["functionCall"]["name"],
        declarations[1]["name"]
    );
    assert_eq!(
        contents[3]["parts"][1]["functionCall"]["args"],
        json!({"input":"patch text"})
    );
    assert_eq!(
        contents[4]["parts"][1]["functionResponse"]["response"],
        json!({"output":"applied"})
    );
    assert_eq!(
        result.body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
        declarations[1]["name"]
    );
}

#[test]
fn additional_tools_support_responses_lite_without_duplicate_declarations() {
    let tools = json!([
        {"type":"function","name":"read","parameters":{"type":"object"}},
        {"type":"tool_search","execution":"client","description":"Find tools","parameters":{"type":"object"}}
    ]);
    let mut input = json!({"input":[
        {"type":"additional_tools","role":"developer","tools":tools},
        {"role":"user","content":"Hello"},
        {"type":"additional_tools","role":"developer","tools":tools}
    ],"tool_choice":{"type":"tool_search"}});
    let result = translate(input.clone(), "gemini-4-argon").unwrap();
    assert_eq!(
        result.body["contents"],
        json!([{"role":"user","parts":[{"text":"Hello"}]}])
    );
    assert_eq!(
        result.body["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        result.body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
        json!(["cpr_tool_search"])
    );
    input["parallel_tool_calls"] = json!(false);
    assert!(translate(input.clone(), "gemini-4-argon").is_err());
    input["parallel_tool_calls"] = json!(true);
    input["input"][2]["tools"][0]["parameters"] =
        json!({"type":"object","properties":{"path":{"type":"string"}}});
    assert!(translate(input, "gemini-4-argon").is_err());
}

#[test]
fn loaded_tools_obey_the_combined_limit_and_do_not_override_existing_tools() {
    let tools: Vec<_> = (0..128)
        .map(|index| json!({"type":"function","name":format!("tool_{index}")}))
        .collect();
    let mut input = json!({"input":[
        {"role":"user","content":"Hello"},
        {"type":"additional_tools","role":"developer","tools":[tools[0].clone()]}
    ],"tools":tools});
    let result = translate(input.clone(), "gemini-4-argon").unwrap();
    assert_eq!(
        result.body["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .len(),
        128
    );
    input["input"][1]["tools"] = json!([{"type":"function","name":"one_too_many"}]);
    assert!(translate(input, "gemini-4-argon").is_err());
    for extra in [
        json!({"type":"custom","name":"read"}),
        json!({"type":"function","name":"read","parameters":{"type":"object","required":["path"]}}),
    ] {
        assert!(translate(json!({"input":[{"role":"user","content":"Hello"},{"type":"additional_tools","role":"developer","tools":[extra]}],
            "tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]
        }), "gemini-4-argon").is_err());
    }
}

#[test]
fn client_tool_search_has_a_separate_identity_from_regular_functions() {
    let tools = json!([
        {"type":"function","name":"tool_search"},
        {"type":"function","name":"cpr_tool_search"},
        {"type":"tool_search","execution":"client","description":"Find tools","parameters":{"type":"object"}}
    ]);
    for (selector, index) in [
        (json!({"type":"function","name":"tool_search"}), 0),
        (json!({"type":"function","name":"cpr_tool_search"}), 1),
        (json!({"type":"tool_search"}), 2),
    ] {
        let result = translate(
            json!({"input":"Hello","tools":tools,"tool_choice":selector}),
            "gemini-4-argon",
        )
        .unwrap();
        let declarations = &result.body["tools"][0]["functionDeclarations"];
        assert_eq!(declarations[0]["name"], "tool_search");
        assert_ne!(declarations[1]["name"], "cpr_tool_search");
        assert_eq!(declarations[2]["name"], "cpr_tool_search");
        assert_eq!(
            result.body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
            declarations[index]["name"]
        );
    }
    assert!(translate(json!({"input":"Hello","tools":tools,"tool_choice":{"type":"custom","name":"tool_search"}}), "gemini-4-argon").is_err());
}

#[test]
fn rejects_invalid_client_tool_search_histories() {
    let call = json!({"type":"tool_search_call","call_id":"search-1","execution":"client","arguments":{"query":"file tools"}});
    let output = json!({"type":"tool_search_output","call_id":"search-1","execution":"client","status":"completed","tools":[]});
    for history in [
        vec![output.clone()],
        vec![call.clone()],
        vec![call.clone(), output.clone(), output.clone()],
        vec![call.clone(), call.clone(), output.clone()],
        vec![
            call.clone(),
            json!({"type":"function_call_output","call_id":"search-1","output":"wrong kind"}),
        ],
        vec![
            json!({"type":"function_call","name":"tool_search","call_id":"search-1","arguments":"{}"}),
            output.clone(),
        ],
    ] {
        let mut input = vec![json!({"role":"user","content":"Hello"})];
        input.extend(history);
        assert!(translate(json!({"input":input}), "gemini-4-argon").is_err());
    }
    for (index, key, value) in [
        (1, "execution", json!("server")),
        (1, "arguments", json!("fixture-private-value")),
        (1, "arguments", json!(null)),
        (1, "call_id", json!(null)),
        (2, "execution", json!("server")),
        (2, "execution", json!(null)),
        (2, "status", json!("in_progress")),
        (2, "tools", json!(null)),
        (2, "tools", json!([{"type":"web_search"}])),
    ] {
        let mut input = json!({"input":[{"role":"user","content":"Hello"},call,output]});
        input["input"][index][key] = value;
        let error = translate(input, "gemini-4-argon")
            .err()
            .expect("invalid history must fail");
        assert!(!error.to_string().contains("fixture-private-value"));
    }
}

#[test]
fn rejects_malformed_tools_and_server_executed_tool_search() {
    for tool in [
        json!(null),
        json!(7),
        json!("fixture-private-tool"),
        json!({}),
        json!({"type":true}),
        json!({"type":"tool_search"}),
        json!({"type":"tool_search","execution":"server"}),
        json!({"type":"tool_search","execution":"client","parameters":{"type":"object"}}),
        json!({"type":"tool_search","execution":"client","description":"Find tools","parameters":null}),
        json!({"type":"namespace","name":"files","tools":[{"type":"tool_search","execution":"client","description":"Find tools","parameters":{"type":"object"}}]}),
    ] {
        let error = translate(json!({"input":"Hello","tools":[tool]}), "gemini-4-argon")
            .err()
            .expect("invalid tool must fail");
        assert!(!error.to_string().contains("fixture-private-tool"));
    }
}

#[test]
fn omits_optional_web_search_without_discarding_codex_tools() {
    let client_tools = json!([
        {"type":"namespace","name":"functions","tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"command":{"type":"string"}}}}]},
        {"type":"custom","name":"apply_patch","format":{"type":"text"}},
        {"type":"tool_search","execution":"client","description":"Find tools","parameters":{"type":"object","properties":{"query":{"type":"string"}}}}
    ]);
    for kind in [
        "web_search",
        "web_search_preview",
        "web_search_2025_08_26",
        "web_search_preview_2025_03_11",
    ] {
        let mut request = json!({
            "model":"gemini-4-argon","input":[{"role":"user","content":[{"type":"input_text","text":"Edit a file"}]}],
            "tools":client_tools,"tool_choice":"auto","parallel_tool_calls":true,"store":false,"stream":true,
            "reasoning":{"effort":"low","summary":"auto","context":"auto"},
            "stream_options":{"reasoning_summary_delivery":"sequential_cutoff","include_obfuscation":false},
            "include":["reasoning.encrypted_content","web_search_call.action.sources"],
            "client_metadata":{"x-codex-turn-metadata":"fixture-private-trace"}
        });
        request["tools"].as_array_mut().unwrap().insert(
            1,
            json!({
                "type":kind,"external_web_access":false,"search_context_size":"medium",
                "user_location":{"type":"approximate","city":"fixture-private-city"},
                "filters":{"allowed_domains":["fixture-private-domain.example"]}
            }),
        );
        let result = translate(request, "gemini-4-argon").unwrap();
        let declarations = result.body["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap();
        assert_eq!(declarations.len(), 3);
        assert_eq!(declarations[1]["name"], "apply_patch");
        assert_eq!(declarations[2]["name"], "cpr_tool_search");
        assert_eq!(result.response_fields["tools"], client_tools);
        assert!(!result.body.to_string().contains("fixture-private"));
        assert!(
            !serde_json::to_string(&result.response_fields)
                .unwrap()
                .contains("fixture-private")
        );
        assert_eq!(
            result.body["toolConfig"]["functionCallingConfig"]["mode"],
            "AUTO"
        );
    }
}

#[test]
fn omits_web_search_only_requests_without_enabling_any_native_tool() {
    for choice in [
        None,
        Some(json!(null)),
        Some(json!("auto")),
        Some(json!("none")),
    ] {
        let mut request = json!({
            "input":"Hello","tools":[{"type":"web_search"},{"type":"web_search_preview"}],
            "parallel_tool_calls":false,"include":["web_search_call.action.sources"]
        });
        if let Some(choice) = choice {
            request["tool_choice"] = choice;
        }
        let result = translate(request, "gemini-2.5-flash").unwrap();
        assert_eq!(
            result.body,
            json!({"contents":[{"role":"user","parts":[{"text":"Hello"}]}]})
        );
        assert_eq!(result.response_fields["tools"], json!([]));
        assert!(result.tools.is_empty());
    }
}

#[test]
fn optional_web_search_preserves_required_and_selected_client_tools() {
    let tools = json!([
        {"type":"web_search"},
        {"type":"function","name":"read_file","parameters":{"type":"object"}},
        {"type":"custom","name":"apply_patch"},
        {"type":"tool_search","execution":"client","description":"Find tools","parameters":{"type":"object"}}
    ]);
    for (choice, selected) in [
        (json!("required"), None),
        (
            json!({"type":"function","name":"read_file"}),
            Some("read_file"),
        ),
        (
            json!({"type":"custom","name":"apply_patch"}),
            Some("apply_patch"),
        ),
        (json!({"type":"tool_search"}), Some("cpr_tool_search")),
    ] {
        let result = translate(
            json!({"input":"Edit a file","tools":tools,"tool_choice":choice}),
            "gemini-4-argon",
        )
        .unwrap();
        let config = &result.body["toolConfig"]["functionCallingConfig"];
        assert_eq!(config["mode"], "ANY");
        if let Some(selected) = selected {
            assert_eq!(config["allowedFunctionNames"], json!([selected]));
        } else {
            assert!(config.get("allowedFunctionNames").is_none());
        }
        assert_eq!(result.response_fields["tool_choice"], choice);
        assert_eq!(result.response_fields["tools"].as_array().unwrap().len(), 3);
    }
    let result = translate(json!({
        "input":[{"role":"user","content":"Read a file"},{"type":"additional_tools","role":"developer","tools":[{"type":"function","name":"read_file"}]}],
        "tools":[{"type":"web_search"}],"tool_choice":"required"
    }), "gemini-4-argon").unwrap();
    assert_eq!(
        result.body["tools"][0]["functionDeclarations"][0]["name"],
        "read_file"
    );
    assert_eq!(
        result.body["toolConfig"]["functionCallingConfig"]["mode"],
        "ANY"
    );
}

#[test]
fn rejects_forced_web_search_instead_of_pretending_to_search() {
    for kind in [
        "web_search",
        "web_search_preview",
        "web_search_2025_08_26",
        "web_search_preview_2025_03_11",
    ] {
        for tools in [
            json!([]),
            json!([{"type":kind},{"type":"function","name":"read_file"}]),
        ] {
            let error = translate(
                json!({
                    "input":"fixture-private-message","tools":tools,
                    "tool_choice":{"type":kind,"value":"fixture-private-choice"}
                }),
                "gemini-4-argon",
            )
            .err()
            .expect("forced web search must fail");
            assert_eq!(
                error.kind,
                gateway_plugin_sdk::call::upstream_adapter::UpstreamFailureKind::Unsupported
            );
            assert!(error.to_string().contains("web_search"));
            assert!(error.to_string().contains("tool_choice"));
            assert!(!error.to_string().contains("fixture-private"));
        }
        let error = translate(
            json!({"input":"Search","tools":[{"type":kind}],"tool_choice":"required"}),
            "gemini-4-argon",
        )
        .err()
        .expect("required web search must fail");
        assert_eq!(
            error.kind,
            gateway_plugin_sdk::call::upstream_adapter::UpstreamFailureKind::Unsupported
        );
        assert!(error.to_string().contains("web_search"));
        assert!(error.to_string().contains("tool_choice"));
    }
}

#[test]
fn web_search_compatibility_does_not_match_client_tool_names_or_unknown_types() {
    let client_tools = json!([
        {"type":"function","name":"web_search"},
        {"type":"custom","name":"web_search_preview"},
        {"type":"namespace","name":"web_search","tools":[{"type":"function","name":"search"}]}
    ]);
    let mut request = json!({"input":"Use a client tool","tools":client_tools,"tool_choice":{"type":"function","name":"web_search"}});
    request["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"web_search"}));
    let result = translate(request, "gemini-4-argon").unwrap();
    assert_eq!(result.response_fields["tools"], client_tools);
    assert_eq!(
        result.body["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        result.body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
        json!(["web_search"])
    );
    for tool in [
        json!({"type":"web_search_fixture-private-type"}),
        json!({"type":"namespace","name":"files","tools":[{"type":"web_search"}]}),
    ] {
        let error = translate(
            json!({"input":"Hello","tools":[{"type":"web_search"},tool]}),
            "gemini-4-argon",
        )
        .err()
        .expect("only known top-level web search declarations may be omitted");
        assert!(!error.to_string().contains("fixture-private"));
    }
}

#[test]
fn optional_web_search_does_not_relax_other_options_or_drop_search_history() {
    for (key, value) in [
        (
            "include",
            json!(["web_search_call.action.sources", "fixture-private-include"]),
        ),
        ("include", json!("web_search_call.action.sources")),
        ("store", json!(true)),
        ("tool_choice", json!({"type":"fixture-private-selector"})),
        (
            "input",
            json!([{"role":"user","content":"Hello"},{"type":"web_search_call","id":"ws_fixture","status":"completed","action":{"type":"search","query":"fixture-private-query"}}]),
        ),
        (
            "input",
            json!([{"role":"user","content":"Hello"},{"type":"additional_tools","role":"developer","tools":[{"type":"web_search"}]}]),
        ),
    ] {
        let mut request = json!({"input":"Hello","tools":[{"type":"web_search"}]});
        request[key] = value;
        let error = translate(request, "gemini-4-argon")
            .err()
            .expect("unsupported semantics must still fail");
        assert!(!error.to_string().contains("fixture-private"));
    }
    assert!(
        translate(
            json!({"input":"Hello","include":["web_search_call.action.sources"]}),
            "gemini-4-argon"
        )
        .is_err()
    );
}

#[test]
fn names_known_unsupported_tools_without_disclosing_custom_types_or_values() {
    for kind in [
        "file_search",
        "code_interpreter",
        "image_generation",
        "mcp",
        "local_shell",
        "shell",
        "computer",
        "computer_use_preview",
    ] {
        let error = translate(json!({"input":"fixture-private-message","tools":[{"type":kind,"description":"fixture-private-description"}]}), "gemini-4-argon").err().unwrap();
        assert!(error.to_string().contains(kind));
        assert!(!error.to_string().contains("fixture-private"));
    }
    let error = translate(json!({"input":"Hello","tools":[{"type":"fixture-private-type","name":"fixture-private-name"}]}), "gemini-4-argon").err().unwrap();
    assert!(!error.to_string().contains("fixture-private"));
}

#[test]
fn rejects_unsupported_semantics_instead_of_ignoring_them() {
    for (key, value) in [
        ("store", json!(true)),
        ("background", json!(true)),
        ("truncation", json!("auto")),
        ("service_tier", json!("priority")),
        ("audio", json!({})),
        ("tools", json!([{"type":"file_search"}])),
    ] {
        let mut request = json!({"input":"Hello"});
        request[key] = value;
        assert!(translate(request, "gemini-2.5-flash").is_err(), "{key}");
    }
}
