//! 将 Gemini 增量内容转换为 Responses 事件及独立的宿主执行事实

use std::{
    collections::BTreeSet,
    time::{SystemTime, UNIX_EPOCH},
};

use gateway_plugin_sdk::call::model::{
    CanonicalEvent, ContentKind, ExecutionEvent, FinishReason, WireEvent, WirePayload,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{
    error::{Error, Result},
    replay::{self, ReplayItem},
    tools::{ToolKind, Tools},
    usage::Meter,
};

const MAX_OUTPUT_BYTES: usize = 256 * 1024;

struct Item {
    value: Value,
    parts: Vec<Value>,
    kind: ContentKind,
}

pub(crate) struct Response {
    id: String,
    model: String,
    created_at: u64,
    stream: bool,
    tools: Tools,
    fields: Map<String, Value>,
    sequence: u64,
    started: bool,
    finished: bool,
    finish_reason: Option<FinishReason>,
    items: Vec<Item>,
    call_ids: BTreeSet<String>,
    size: usize,
    meter: Meter,
}

impl Response {
    pub fn new(model: String, stream: bool, tools: Tools, fields: Map<String, Value>) -> Self {
        Self {
            id: id("resp_gemini"),
            model,
            created_at: now(),
            stream,
            tools,
            fields,
            sequence: 0,
            started: false,
            finished: false,
            finish_reason: None,
            items: Vec::new(),
            call_ids: BTreeSet::new(),
            size: 0,
            meter: Meter::default(),
        }
    }

    pub fn consume(&mut self, value: Value) -> Result<Vec<ExecutionEvent>> {
        if self.finished || !value.is_object() {
            return Err(Error::protocol(
                "Invalid Gemini response or data after completion",
            ));
        }
        if value.get("error").is_some() {
            return Err(Error::protocol(
                "Gemini returned an error inside its content stream",
            ));
        }
        let candidates = match value.get("candidates") {
            Some(value) => value
                .as_array()
                .ok_or_else(|| Error::protocol("Invalid Gemini candidates"))?
                .as_slice(),
            None => &[],
        };
        if candidates.len() > 1 {
            return Err(Error::protocol("Gemini returned more than one candidate"));
        }
        let blocked = value
            .get("promptFeedback")
            .and_then(|feedback| feedback.get("blockReason"))
            .and_then(Value::as_str)
            .is_some_and(|reason| reason != "BLOCK_REASON_UNSPECIFIED" && !reason.is_empty());
        if candidates.is_empty() && !blocked && value.get("usageMetadata").is_none() {
            return Err(Error::protocol(
                "Gemini returned neither candidates, usage nor a blocking reason",
            ));
        }
        let mut events = Vec::new();
        self.start(&mut events);
        if blocked {
            if self.finish_reason.is_some() {
                return Err(Error::protocol("Conflicting Gemini completion reasons"));
            }
            self.finish_reason = Some(FinishReason::ContentFilter);
        }
        if let Some(candidate) = candidates.first() {
            if !candidate.is_object()
                || candidate
                    .get("index")
                    .is_some_and(|index| index.as_u64() != Some(0))
            {
                return Err(Error::protocol("Invalid Gemini candidate index"));
            }
            if let Some(content) = candidate.get("content") {
                if !content.is_object() {
                    return Err(Error::protocol("Invalid Gemini candidate content"));
                }
                if let Some(role) = content.get("role")
                    && role != "model"
                {
                    return Err(Error::protocol("Gemini candidate has an invalid role"));
                }
                if let Some(parts) = content.get("parts") {
                    let parts = parts
                        .as_array()
                        .ok_or_else(|| Error::protocol("Invalid Gemini content parts"))?;
                    if self.finish_reason.is_some() && !parts.is_empty() {
                        return Err(Error::protocol(
                            "Gemini sent content after its finish reason",
                        ));
                    }
                    for part in parts {
                        self.part(part, &mut events)?;
                    }
                }
            }
            if let Some(reason) = candidate.get("finishReason") {
                let reason = reason
                    .as_str()
                    .ok_or_else(|| Error::protocol("Invalid Gemini finish reason"))?;
                let reason = match reason {
                    "FINISH_REASON_UNSPECIFIED" => None,
                    "STOP" => Some(if self.call_ids.is_empty() {
                        FinishReason::Stop
                    } else {
                        FinishReason::ToolCall
                    }),
                    "MAX_TOKENS" => Some(FinishReason::Length),
                    "SAFETY"
                    | "RECITATION"
                    | "BLOCKLIST"
                    | "PROHIBITED_CONTENT"
                    | "SPII"
                    | "IMAGE_SAFETY"
                    | "IMAGE_PROHIBITED_CONTENT" => Some(FinishReason::ContentFilter),
                    _ => {
                        return Err(Error::protocol(
                            "Gemini ended with an unsupported or invalid completion reason",
                        ));
                    }
                };
                if let Some(reason) = reason {
                    if self
                        .finish_reason
                        .is_some_and(|previous| previous != reason)
                    {
                        return Err(Error::protocol("Conflicting Gemini completion reasons"));
                    }
                    self.finish_reason = Some(reason);
                }
            }
        }
        if let Some(usage) = value.get("usageMetadata") {
            let usage = self.meter.update(usage)?;
            events.push(ExecutionEvent::canonical(CanonicalEvent::Usage { usage }));
        }
        Ok(events)
    }

    fn start(&mut self, events: &mut Vec<ExecutionEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        self.emit(
            "response.created",
            json!({"response":self.snapshot("in_progress")}),
            vec![CanonicalEvent::Started {
                id: self.id.clone(),
                model: Some(self.model.clone()),
            }],
            events,
        );
        self.emit(
            "response.in_progress",
            json!({"response":self.snapshot("in_progress")}),
            vec![],
            events,
        );
    }

    fn part(&mut self, part: &Value, events: &mut Vec<ExecutionEvent>) -> Result<()> {
        let object = part
            .as_object()
            .ok_or_else(|| Error::protocol("Invalid Gemini content part"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "text" | "thought" | "thoughtSignature" | "functionCall"
            )
        }) || object
            .get("thought")
            .is_some_and(|value| !value.is_boolean())
            || object
                .get("thoughtSignature")
                .is_some_and(|value| !value.is_string())
        {
            return Err(Error::protocol(
                "Gemini returned unsupported output content",
            ));
        }
        self.size = self
            .size
            .checked_add(
                serde_json::to_vec(part)
                    .map_err(|_| Error::protocol("Invalid Gemini output"))?
                    .len(),
            )
            .filter(|size| *size <= MAX_OUTPUT_BYTES)
            .ok_or_else(|| Error::protocol("Gemini output exceeds the 256 KiB response limit"))?;
        if let Some(function) = object.get("functionCall") {
            if object.contains_key("text") || part["thought"] == true {
                return Err(Error::protocol("Invalid mixed Gemini function part"));
            }
            self.function(function, part, events)
        } else if let Some(text) = object.get("text") {
            let text = text
                .as_str()
                .ok_or_else(|| Error::protocol("Invalid Gemini text part"))?;
            let thought = part["thought"] == true;
            self.text(text, thought, part, events)
        } else if part.get("thoughtSignature").is_some() {
            self.items
                .last_mut()
                .ok_or_else(|| Error::protocol("Gemini signature has no preceding content"))?
                .parts
                .push(part.clone());
            Ok(())
        } else {
            Err(Error::protocol("Gemini returned an empty content part"))
        }
    }

    fn text(
        &mut self,
        delta: &str,
        thought: bool,
        part: &Value,
        events: &mut Vec<ExecutionEvent>,
    ) -> Result<()> {
        let kind = if thought {
            ContentKind::Reasoning
        } else {
            ContentKind::Text
        };
        let index = if self.items.last().is_some_and(|item| item.kind == kind) {
            self.items.len() - 1
        } else {
            if delta.is_empty() && !replay::signed(std::slice::from_ref(part)) {
                return Ok(());
            }
            let value = if thought {
                json!({"id":id("rs"), "type":"reasoning", "summary":[], "encrypted_content":null})
            } else {
                json!({"id":id("msg"), "type":"message", "role":"assistant", "status":"in_progress", "content":[]})
            };
            self.add_item(value, kind, events)?
        };
        let item_id = self.items[index].value["id"].clone();
        let key = if thought { "summary" } else { "content" };
        if self.items[index].value[key]
            .as_array()
            .is_some_and(Vec::is_empty)
        {
            let content = if thought {
                json!({"type":"summary_text", "text":""})
            } else {
                json!({"type":"output_text", "text":"", "annotations":[], "logprobs":[]})
            };
            self.items[index].value[key] = json!([content]);
            let mut fields = json!({"item_id":item_id, "output_index":index, "part":content});
            fields[if thought {
                "summary_index"
            } else {
                "content_index"
            }] = json!(0);
            self.emit(
                if thought {
                    "response.reasoning_summary_part.added"
                } else {
                    "response.content_part.added"
                },
                fields,
                vec![],
                events,
            );
        }
        if let Value::String(text) = &mut self.items[index].value[key][0]["text"] {
            text.push_str(delta);
        }
        self.items[index].parts.push(part.clone());
        if !delta.is_empty() {
            let mut fields = json!({"item_id":item_id, "output_index":index, "delta":delta});
            fields[if thought {
                "summary_index"
            } else {
                "content_index"
            }] = json!(0);
            if !thought {
                fields["logprobs"] = json!([]);
            }
            let fact = if thought {
                CanonicalEvent::ReasoningDelta {
                    index: index as u32,
                    text: delta.into(),
                }
            } else {
                CanonicalEvent::TextDelta {
                    index: index as u32,
                    text: delta.into(),
                }
            };
            self.emit(
                if thought {
                    "response.reasoning_summary_text.delta"
                } else {
                    "response.output_text.delta"
                },
                fields,
                vec![fact],
                events,
            );
        }
        Ok(())
    }

    fn function(
        &mut self,
        function: &Value,
        part: &Value,
        events: &mut Vec<ExecutionEvent>,
    ) -> Result<()> {
        let function = function
            .as_object()
            .ok_or_else(|| Error::protocol("Invalid Gemini function call"))?;
        if function
            .keys()
            .any(|key| !matches!(key.as_str(), "name" | "args" | "id"))
        {
            return Err(Error::protocol(
                "Streaming partial Gemini function arguments are not supported",
            ));
        }
        let alias = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::protocol("Gemini function call has no name"))?;
        let tool = self
            .tools
            .get(alias)
            .cloned()
            .ok_or_else(|| Error::protocol("Gemini called a tool that was not declared"))?;
        let native_arguments = function.get("args").cloned().unwrap_or_else(|| json!({}));
        if !native_arguments.is_object() {
            return Err(Error::protocol(
                "Gemini function arguments must be an object",
            ));
        }
        let custom = tool.kind == ToolKind::Custom;
        let arguments = if custom {
            if native_arguments
                .as_object()
                .is_none_or(|object| object.len() != 1)
            {
                return Err(Error::protocol("Invalid custom tool arguments"));
            }
            native_arguments["input"]
                .as_str()
                .ok_or_else(|| Error::protocol("Gemini custom tool input must be text"))?
                .to_owned()
        } else {
            serde_json::to_string(&native_arguments)
                .map_err(|_| Error::protocol("Invalid Gemini function arguments"))?
        };
        let call_id = match function.get("id").filter(|value| !value.is_null()) {
            Some(value) => value
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
                .ok_or_else(|| Error::protocol("Invalid Gemini function ID"))?
                .to_owned(),
            None => id("call"),
        };
        if !self.call_ids.insert(call_id.clone()) {
            return Err(Error::protocol("Gemini reused a function call ID"));
        }
        let key = if custom { "input" } else { "arguments" };
        let mut value = if tool.kind == ToolKind::Search {
            json!({"id":id("tsc"), "type":"tool_search_call", "status":"in_progress", "call_id":call_id, "execution":"client", "arguments":{}})
        } else {
            let mut value = json!({"id":id(if custom { "ct" } else { "fc" }), "type":if custom { "custom_tool_call" } else { "function_call" }, "status":"in_progress", "call_id":call_id, "name":tool.name});
            value[key] = json!("");
            value
        };
        if let Some(namespace) = tool.namespace {
            value["namespace"] = json!(namespace);
        }
        let index = self.add_item(value, ContentKind::ToolCall, events)?;
        self.items[index].value[key] = if tool.kind == ToolKind::Search {
            native_arguments
        } else {
            json!(arguments)
        };
        self.items[index].parts.push(part.clone());
        let fact = CanonicalEvent::ToolCallDelta {
            index: index as u32,
            id: call_id,
            name: Some(tool.name),
            arguments: arguments.clone(),
        };
        if tool.kind == ToolKind::Search {
            events.push(ExecutionEvent::canonical(fact));
        } else {
            let fields = json!({"item_id":self.items[index].value["id"], "output_index":index, "delta":arguments});
            self.emit(
                if custom {
                    "response.custom_tool_call_input.delta"
                } else {
                    "response.function_call_arguments.delta"
                },
                fields,
                vec![fact],
                events,
            );
        }
        Ok(())
    }

    fn add_item(
        &mut self,
        value: Value,
        kind: ContentKind,
        events: &mut Vec<ExecutionEvent>,
    ) -> Result<usize> {
        let index = self.items.len();
        if index >= 1024 {
            return Err(Error::protocol("Gemini returned too many output items"));
        }
        self.emit(
            "response.output_item.added",
            json!({"output_index":index, "item":value}),
            vec![CanonicalEvent::ContentAdded {
                index: index as u32,
                kind,
            }],
            events,
        );
        self.items.push(Item {
            value,
            parts: Vec::new(),
            kind,
        });
        Ok(index)
    }

    pub fn finish(&mut self) -> Result<Vec<ExecutionEvent>> {
        if self.finished || !self.started {
            return Err(Error::protocol("Gemini stream ended without a response"));
        }
        let reason = self
            .finish_reason
            .ok_or_else(|| Error::protocol("Gemini stream ended before a finish reason"))?;
        let incomplete = matches!(reason, FinishReason::Length | FinishReason::ContentFilter);
        let mut events = Vec::new();
        let has_signature = self.items.iter().any(|item| replay::signed(&item.parts));
        let signed = self
            .items
            .iter()
            .filter(|item| has_signature && !item.parts.is_empty())
            .map(|item| ReplayItem {
                key: match item.kind {
                    ContentKind::ToolCall => format!(
                        "call:{}",
                        item.value["call_id"].as_str().unwrap_or_default()
                    ),
                    ContentKind::Reasoning => format!(
                        "reasoning:{}",
                        item.value["id"].as_str().unwrap_or_default()
                    ),
                    _ => format!("message:{}", item.value["id"].as_str().unwrap_or_default()),
                },
                parts: item.parts.clone(),
            })
            .collect();
        if let Some(encoded) = replay::encode(&self.model, signed)? {
            let index = if let Some(index) = self
                .items
                .iter()
                .position(|item| item.kind == ContentKind::Reasoning)
            {
                index
            } else {
                self.add_item(json!({"id":id("rs"), "type":"reasoning", "summary":[], "encrypted_content":null}), ContentKind::Reasoning, &mut events)?
            };
            self.items[index].value["encrypted_content"] = json!(encoded);
        }
        for index in 0..self.items.len() {
            let kind = self.items[index].kind;
            if kind != ContentKind::Reasoning {
                self.items[index].value["status"] =
                    json!(if incomplete && kind == ContentKind::Text {
                        "incomplete"
                    } else {
                        "completed"
                    });
            }
            let value = self.items[index].value.clone();
            let item_id = &value["id"];
            if matches!(kind, ContentKind::Text | ContentKind::Reasoning) {
                let thought = kind == ContentKind::Reasoning;
                let part = &value[if thought { "summary" } else { "content" }][0];
                if !part.is_null() {
                    let mut fields =
                        json!({"item_id":item_id, "output_index":index, "text":part["text"]});
                    fields[if thought {
                        "summary_index"
                    } else {
                        "content_index"
                    }] = json!(0);
                    if !thought {
                        fields["logprobs"] = json!([]);
                    }
                    self.emit(
                        if thought {
                            "response.reasoning_summary_text.done"
                        } else {
                            "response.output_text.done"
                        },
                        fields,
                        vec![],
                        &mut events,
                    );
                    let mut fields = json!({"item_id":item_id, "output_index":index, "part":part});
                    fields[if thought {
                        "summary_index"
                    } else {
                        "content_index"
                    }] = json!(0);
                    self.emit(
                        if thought {
                            "response.reasoning_summary_part.done"
                        } else {
                            "response.content_part.done"
                        },
                        fields,
                        vec![],
                        &mut events,
                    );
                }
            } else if value["type"] != "tool_search_call" {
                let custom = value["type"] == "custom_tool_call";
                let key = if custom { "input" } else { "arguments" };
                let mut fields = json!({"item_id":item_id, "output_index":index});
                fields[key] = value[key].clone();
                self.emit(
                    if custom {
                        "response.custom_tool_call_input.done"
                    } else {
                        "response.function_call_arguments.done"
                    },
                    fields,
                    vec![],
                    &mut events,
                );
            }
            self.emit(
                "response.output_item.done",
                json!({"output_index":index, "item":value}),
                vec![],
                &mut events,
            );
        }
        self.finished = true;
        let status = if incomplete {
            "incomplete"
        } else {
            "completed"
        };
        let response = self.snapshot(status);
        let fact = CanonicalEvent::Completed {
            id: self.id.clone(),
            model: Some(self.model.clone()),
            reason,
        };
        self.emit(
            if incomplete {
                "response.incomplete"
            } else {
                "response.completed"
            },
            json!({"response":response}),
            vec![fact],
            &mut events,
        );
        Ok(events)
    }

    fn snapshot(&self, status: &str) -> Value {
        let mut value = Value::Object(self.fields.clone());
        let object = value.as_object_mut().expect("response object");
        object.extend(json!({
            "id":self.id, "object":"response", "created_at":self.created_at,
            "completed_at":self.finished.then(now), "status":status, "model":self.model,
            "output":if self.finished { self.items.iter().map(|item| item.value.clone()).collect::<Vec<_>>() } else { vec![] },
            "error":null, "incomplete_details":match self.finish_reason {
                Some(FinishReason::Length) => json!({"reason":"max_output_tokens"}),
                Some(FinishReason::ContentFilter) => json!({"reason":"content_filter"}),
                _ => Value::Null,
            },
            "usage":if self.finished { self.meter.json() } else { Value::Null },
            "store":false, "background":false, "previous_response_id":null,
        }).as_object().expect("response metadata").clone());
        value
    }

    fn emit(
        &mut self,
        name: &str,
        mut data: Value,
        facts: Vec<CanonicalEvent>,
        events: &mut Vec<ExecutionEvent>,
    ) {
        if !self.stream && facts.is_empty() {
            return;
        }
        let wire = if self.stream || self.finished {
            data["type"] = json!(name);
            data["sequence_number"] = json!(self.sequence);
            self.sequence += 1;
            Some(WireEvent {
                protocol: "openai".into(),
                payload: WirePayload::Json {
                    event: Some(name.into()),
                    data,
                    id: None,
                    retry: None,
                    raw_sse: None,
                },
            })
        } else {
            None
        };
        events.push(ExecutionEvent {
            facts,
            wire,
            host: None,
        });
    }
}

fn id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
