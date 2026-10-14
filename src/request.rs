//! 将 Responses 请求转换为 Gemini 内容生成请求

use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};

use crate::{
    error::{Error, Result},
    replay::Replay,
    tools::{self, Tools, optional_namespace, string},
};

pub(crate) struct PreparedRequest {
    pub body: Value,
    pub stream: bool,
    pub tools: Tools,
    pub response_fields: Map<String, Value>,
}

struct Call {
    name: String,
    id: Option<String>,
    custom: bool,
    answered: bool,
}

pub(crate) fn translate(input: Value, model: &str) -> Result<PreparedRequest> {
    let object = input
        .as_object()
        .ok_or_else(|| Error::invalid("Responses request must be a JSON object"))?;
    validate_options(object)?;
    let stream = boolean(object, "stream", false)?;
    let tools = Tools::parse(object.get("tools"))?;
    if !tools.is_empty() && !boolean(object, "parallel_tool_calls", true)? {
        return Err(Error::unsupported(
            "Gemini cannot guarantee parallel_tool_calls=false",
        ));
    }
    let items = match object.get("input") {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({"role":"user", "content":text})]
        }
        Some(Value::Array(items)) if !items.is_empty() && items.len() <= 4096 => items.clone(),
        _ => {
            return Err(Error::invalid(
                "input must be non-empty text or an array of at most 4096 items",
            ));
        }
    };
    tools::check_unique_ids(
        items
            .iter()
            .filter(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("function_call" | "custom_tool_call")
                )
            })
            .filter_map(|item| item["call_id"].as_str()),
    )?;
    let replay = Replay::parse(&items, model)?;
    let mut contents = Vec::new();
    let mut system = Vec::new();
    let mut calls = BTreeMap::<String, Call>::new();
    if let Some(instructions) = object.get("instructions").filter(|value| !value.is_null()) {
        let instructions = instructions
            .as_str()
            .ok_or_else(|| Error::invalid("instructions must be text"))?;
        if !instructions.is_empty() {
            system.push(json!({"text":instructions}));
        }
    }
    for item in &items {
        if !item.is_object() {
            return Err(Error::invalid("Each input item must be an object"));
        }
        match item
            .get("type")
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| Error::invalid("Input item type must be text"))
            })
            .transpose()?
            .unwrap_or("message")
        {
            "message" => {
                let role = string(item, "role")?;
                let mut parts = content_parts(&item["content"])?;
                match role {
                    "system" | "developer" => {
                        if parts.iter().any(|part| !part["text"].is_string()) {
                            return Err(Error::unsupported(
                                "Gemini system instructions support text only",
                            ));
                        }
                        system.extend(parts);
                    }
                    "user" => append(&mut contents, "user", parts),
                    "assistant" => {
                        if let Some(saved) = item
                            .get("id")
                            .and_then(Value::as_str)
                            .and_then(|id| replay.get(&format!("message:{id}")))
                        {
                            if text(saved, false)? != text(&parts, false)?
                                || saved.iter().any(|part| {
                                    part.get("functionCall").is_some() || part["thought"] == true
                                })
                            {
                                return Err(Error::invalid(
                                    "Assistant content no longer matches its Gemini signature",
                                ));
                            }
                            parts = saved.to_vec();
                        }
                        append(&mut contents, "model", parts);
                    }
                    _ => return Err(Error::unsupported("Unsupported message role")),
                }
            }
            "reasoning" => {
                if let Some(saved) = item
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| replay.get(&format!("reasoning:{id}")))
                {
                    if saved.iter().any(|part| {
                        part.get("functionCall").is_some()
                            || (part.get("text").is_some() && part["thought"] != true)
                    }) {
                        return Err(Error::invalid("Invalid signed reasoning item"));
                    }
                    append(&mut contents, "model", saved.to_vec());
                }
            }
            "function_call" | "custom_tool_call" => {
                let custom = item["type"] == "custom_tool_call";
                let name = tools::alias(string(item, "name")?, optional_namespace(item)?);
                let call_id = string(item, "call_id")?;
                if call_id.len() > 512 || call_id.chars().any(char::is_control) {
                    return Err(Error::invalid("Invalid tool call ID"));
                }
                let arguments = if custom {
                    json!({"input": item["input"].as_str().ok_or_else(|| Error::invalid("Custom tool input must be text"))?})
                } else {
                    let value: Value =
                        serde_json::from_str(string(item, "arguments")?).map_err(|_| {
                            Error::invalid("Function arguments must contain valid JSON")
                        })?;
                    if !value.is_object() {
                        return Err(Error::invalid("Function arguments must be a JSON object"));
                    }
                    value
                };
                let (parts, id) = if let Some(saved) = replay.get(&format!("call:{call_id}")) {
                    let functions: Vec<_> = saved
                        .iter()
                        .filter_map(|part| part.get("functionCall"))
                        .collect();
                    if functions.len() != 1
                        || functions[0]["name"] != name
                        || functions[0].get("args").unwrap_or(&json!({})) != &arguments
                        || saved
                            .iter()
                            .any(|part| part.get("text").is_some() || part["thought"] == true)
                    {
                        return Err(Error::invalid(
                            "Tool call no longer matches its Gemini signature",
                        ));
                    }
                    let id = functions[0]
                        .get("id")
                        .filter(|value| !value.is_null())
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::to_owned)
                                .ok_or_else(|| Error::invalid("Invalid signed function call ID"))
                        })
                        .transpose()?;
                    (saved.to_vec(), id)
                } else {
                    (
                        vec![json!({"functionCall":{"name":name, "args":arguments, "id":call_id}})],
                        Some(call_id.into()),
                    )
                };
                calls.insert(
                    call_id.into(),
                    Call {
                        name,
                        id,
                        custom,
                        answered: false,
                    },
                );
                append(&mut contents, "model", parts);
            }
            "function_call_output" | "custom_tool_call_output" => {
                let call_id = string(item, "call_id")?;
                let call = calls.get_mut(call_id).ok_or_else(|| {
                    Error::invalid(
                        "Tool output requires its preceding call in the full input history",
                    )
                })?;
                if call.answered || call.custom != (item["type"] == "custom_tool_call_output") {
                    return Err(Error::invalid("Duplicate or mismatched tool output"));
                }
                let output = match &item["output"] {
                    Value::String(output) => output.clone(),
                    Value::Array(_) => text(&content_parts(&item["output"])?, false)?,
                    _ => {
                        return Err(Error::invalid(
                            "Tool output must be text or text content parts",
                        ));
                    }
                };
                let mut response = json!({"name":call.name, "response":{"output":output}});
                if let Some(id) = &call.id {
                    response["id"] = json!(id);
                }
                call.answered = true;
                append(
                    &mut contents,
                    "user",
                    vec![json!({"functionResponse":response})],
                );
            }
            _ => {
                return Err(Error::unsupported(
                    "Unsupported Responses input item; send full messages and tool history",
                ));
            }
        }
    }
    if contents.is_empty() || calls.values().any(|call| !call.answered) {
        return Err(Error::invalid(
            "Conversation needs content and a result for every historical tool call",
        ));
    }
    let mut body = json!({"contents":contents});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts":system});
    }
    if let Some(declarations) = tools.declarations() {
        body["tools"] = declarations;
    }
    if let Some(choice) = tools.tool_choice(object.get("tool_choice"))? {
        body["toolConfig"] = choice;
    }
    let generation = generation_config(object, model)?;
    if !generation.is_empty() {
        body["generationConfig"] = Value::Object(generation);
    }
    let response_fields = [
        "instructions",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "max_output_tokens",
        "temperature",
        "top_p",
        "reasoning",
        "text",
        "metadata",
    ]
    .into_iter()
    .filter_map(|key| object.get(key).map(|value| (key.into(), value.clone())))
    .collect();
    Ok(PreparedRequest {
        body,
        stream,
        tools,
        response_fields,
    })
}

fn append(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut().filter(|last| last["role"] == role) {
        last["parts"]
            .as_array_mut()
            .expect("constructed content parts")
            .extend(parts);
    } else {
        contents.push(json!({"role":role, "parts":parts}));
    }
}

fn content_parts(value: &Value) -> Result<Vec<Value>> {
    if let Some(text) = value.as_str() {
        return Ok(vec![json!({"text":text})]);
    }
    let parts = value
        .as_array()
        .ok_or_else(|| Error::invalid("Message content must be text or content parts"))?;
    parts.iter().map(|part| match part["type"].as_str() {
        Some("input_text" | "output_text") => Ok(json!({"text":part["text"].as_str().ok_or_else(|| Error::invalid("Content text must be a string"))?})),
        Some("refusal") => Ok(json!({"text":part["refusal"].as_str().ok_or_else(|| Error::invalid("Refusal must be text"))?})),
        Some("input_image") => inline_image(string(part, "image_url")?),
        _ => Err(Error::unsupported("Only text and base64 image content parts are supported")),
    }).collect()
}

fn inline_image(uri: &str) -> Result<Value> {
    let (mime, encoded) = uri.strip_prefix("data:").and_then(|uri| uri.split_once(";base64,"))
        .ok_or_else(|| Error::unsupported("Images must use data:image/...;base64 URLs; remote URLs and file IDs are not fetched"))?;
    if !matches!(
        mime,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) || encoded.is_empty()
        || encoded.len() > 16 * 1024 * 1024
        || STANDARD.decode(encoded).is_err()
    {
        return Err(Error::invalid(
            "Invalid base64 image or unsupported image MIME type",
        ));
    }
    Ok(json!({"inlineData":{"mimeType":mime, "data":encoded}}))
}

fn text(parts: &[Value], thoughts: bool) -> Result<String> {
    let mut result = String::new();
    for part in parts {
        if part
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            != thoughts
        {
            continue;
        }
        if let Some(value) = part.get("text") {
            result.push_str(
                value
                    .as_str()
                    .ok_or_else(|| Error::invalid("Invalid text content"))?,
            );
        } else if part.get("thoughtSignature").is_none() {
            return Err(Error::unsupported(
                "Multimodal tool results and signed non-text messages are not supported",
            ));
        }
    }
    Ok(result)
}

fn boolean(object: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    object
        .get(key)
        .filter(|value| !value.is_null())
        .map_or(Ok(default), |value| {
            value
                .as_bool()
                .ok_or_else(|| Error::invalid("A boolean request option has an invalid type"))
        })
}

fn validate_options(object: &Map<String, Value>) -> Result<()> {
    if object
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
        || object
            .get("conversation")
            .is_some_and(|value| !value.is_null())
    {
        return Err(Error::unsupported(
            "Gemini is stateless: omit previous_response_id/conversation and send full input history",
        ));
    }
    if boolean(object, "store", false)? || boolean(object, "background", false)? {
        return Err(Error::unsupported(
            "Stored and background Responses are not supported; use store=false and background=false",
        ));
    }
    if object
        .get("truncation")
        .is_some_and(|value| !value.is_null() && value != "disabled")
    {
        return Err(Error::unsupported(
            "Automatic context truncation is not supported",
        ));
    }
    if object
        .get("service_tier")
        .is_some_and(|value| !value.is_null() && value != "auto" && value != "default")
    {
        return Err(Error::unsupported(
            "OpenAI service tiers cannot be applied to Gemini",
        ));
    }
    let supported = [
        "model",
        "input",
        "instructions",
        "stream",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "max_output_tokens",
        "temperature",
        "top_p",
        "reasoning",
        "text",
        "store",
        "background",
        "previous_response_id",
        "conversation",
        "truncation",
        "service_tier",
        "metadata",
        "include",
        "user",
        "safety_identifier",
        "prompt_cache_key",
        "prompt_cache_retention",
    ];
    if object.keys().any(|key| !supported.contains(&key.as_str())) {
        return Err(Error::unsupported(
            "The request contains an unsupported Responses option",
        ));
    }
    if let Some(include) = object.get("include").filter(|value| !value.is_null()) {
        let include = include
            .as_array()
            .ok_or_else(|| Error::invalid("include must be an array"))?;
        if include
            .iter()
            .any(|value| value != "reasoning.encrypted_content")
        {
            return Err(Error::unsupported(
                "Only reasoning.encrypted_content is supported in include",
            ));
        }
    }
    Ok(())
}

fn generation_config(object: &Map<String, Value>, model: &str) -> Result<Map<String, Value>> {
    let mut result = Map::new();
    for (source, target, maximum) in [("temperature", "temperature", 2.0), ("top_p", "topP", 1.0)] {
        if let Some(value) = object.get(source).filter(|value| !value.is_null()) {
            if value
                .as_f64()
                .is_none_or(|number| !number.is_finite() || number < 0.0 || number > maximum)
            {
                return Err(Error::invalid(
                    "temperature or top_p is outside its supported range",
                ));
            }
            result.insert(target.into(), value.clone());
        }
    }
    if let Some(value) = object
        .get("max_output_tokens")
        .filter(|value| !value.is_null())
    {
        if value
            .as_u64()
            .is_none_or(|count| count == 0 || count > i32::MAX as u64)
        {
            return Err(Error::invalid(
                "max_output_tokens must be a positive integer",
            ));
        }
        result.insert("maxOutputTokens".into(), value.clone());
    }
    if let Some(reasoning) = object.get("reasoning").filter(|value| !value.is_null()) {
        let reasoning = reasoning
            .as_object()
            .ok_or_else(|| Error::invalid("reasoning must be an object"))?;
        let mut thinking = Map::new();
        if let Some(summary) = reasoning.get("summary").filter(|value| !value.is_null()) {
            if !matches!(summary.as_str(), Some("auto" | "concise" | "detailed")) {
                return Err(Error::invalid("Unsupported reasoning summary setting"));
            }
            thinking.insert("includeThoughts".into(), json!(true));
        }
        if let Some(effort) = reasoning.get("effort").filter(|value| !value.is_null()) {
            let effort = effort
                .as_str()
                .ok_or_else(|| Error::invalid("reasoning.effort must be text"))?;
            let (budget, level) = match effort {
                "none" => (0, "MINIMAL"),
                "minimal" => (512, "MINIMAL"),
                "low" => (1024, "LOW"),
                "medium" => (8192, "MEDIUM"),
                "high" | "xhigh" => (24576, "HIGH"),
                _ => return Err(Error::unsupported("Unsupported reasoning effort")),
            };
            if model.starts_with("gemini-2.5-") {
                if budget == 0 && model.starts_with("gemini-2.5-pro") {
                    return Err(Error::unsupported("Gemini 2.5 Pro cannot disable thinking"));
                }
                thinking.insert("thinkingBudget".into(), json!(budget));
            } else if model.starts_with("gemini-3") || model.starts_with("gemini-4") {
                if effort == "none" {
                    return Err(Error::unsupported(
                        "This Gemini model cannot disable thinking",
                    ));
                }
                thinking.insert("thinkingLevel".into(), json!(level));
            } else {
                return Err(Error::unsupported(
                    "No reasoning-effort mapping is defined for this Gemini model",
                ));
            }
        }
        if reasoning
            .keys()
            .any(|key| !matches!(key.as_str(), "effort" | "summary"))
        {
            return Err(Error::unsupported("Unsupported reasoning option"));
        }
        if !thinking.is_empty() {
            result.insert("thinkingConfig".into(), Value::Object(thinking));
        }
    }
    if let Some(text) = object.get("text").filter(|value| !value.is_null()) {
        if !text.is_object() {
            return Err(Error::invalid("text must be an object"));
        }
        if let Some(format) = text.get("format") {
            match format["type"].as_str() {
                Some("text") => {}
                Some("json_object") => {
                    result.insert("responseMimeType".into(), json!("application/json"));
                }
                Some("json_schema") => {
                    if !format["schema"].is_object() {
                        return Err(Error::invalid(
                            "text.format.schema must be a JSON Schema object",
                        ));
                    }
                    result.insert("responseMimeType".into(), json!("application/json"));
                    result.insert("responseJsonSchema".into(), format["schema"].clone());
                }
                _ => return Err(Error::unsupported("Unsupported text output format")),
            }
        }
    }
    Ok(result)
}
