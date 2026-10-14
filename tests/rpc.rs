//! 使用真实插件子进程和 SDK 帧验证注册、受管请求、流控、取消及失败回收

use std::{collections::VecDeque, process::Stdio, time::Duration};

use gateway_plugin_sdk::{
    CallContext, Frame, Handshake, Manifest, Message, PROTOCOL_VERSION, Stage,
    call::{
        model::{CanonicalEvent, WirePayload},
        upstream_adapter::{
            BuiltinProvider, UpstreamAdapterEvent, UpstreamAdapterRegistration,
            UpstreamAdapterRequest, UpstreamFailureKind, UpstreamHttpRequest,
        },
    },
    client::{read_frame, write_frame},
};
use serde_json::{Value, json};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const BASE: &str = "http://127.0.0.1:32123/v1beta/";
const MODEL: &str = "gemini-2.5-flash";

struct Peer {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
}

impl Peer {
    async fn start() -> Self {
        Self::start_with(json!({"baseUrl":BASE,"models":[MODEL]})).await
    }

    async fn start_with(configuration: Value) -> Self {
        let binary = std::env::var_os("CPR_PLUGIN_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_cpr-plugin-gemini").into());
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let mut peer = Self {
            child,
            input,
            output,
        };
        let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json")).unwrap();
        peer.control(Message::Hello {
            handshake: Handshake {
                protocol_version: PROTOCOL_VERSION,
                artifact_sha256: "a".repeat(64),
                plugin_id: manifest.plugin_id().unwrap(),
                instance_id: "fixture-instance".into(),
                generation: 1,
                incarnation: "fixture-incarnation".into(),
                configuration,
                contributes: manifest.contributes,
            },
        })
        .await;
        assert!(matches!(
            peer.receive().await.message,
            Message::Ready {
                protocol_version: PROTOCOL_VERSION,
                ..
            }
        ));
        peer
    }

    async fn control(&mut self, message: Message) {
        write_frame(&mut self.input, &Frame::control(message))
            .await
            .unwrap();
    }

    async fn receive(&mut self) -> Frame {
        tokio::time::timeout(Duration::from_secs(10), read_frame(&mut self.output))
            .await
            .expect("plugin response timeout")
            .unwrap()
    }

    async fn call(&mut self, id: u64, method: &str, stage: Stage, payload: Vec<u8>) {
        write_frame(
            &mut self.input,
            &Frame {
                message: Message::Call {
                    id,
                    method: method.into(),
                    params: json!({}),
                    context: CallContext {
                        call_id: id,
                        instance_id: "fixture-instance".into(),
                        generation: 1,
                        incarnation: "fixture-incarnation".into(),
                        stage,
                        timeout_ms: 10_000,
                        resource_stream: stage == Stage::Upstream,
                        resource_scope_id: format!("scope-{id}"),
                        request_id: None,
                        attempt_id: None,
                        account_id: Some("acct_fixture".into()),
                        credential_revision: Some(7),
                    },
                },
                payload,
            },
        )
        .await
        .unwrap();
    }

    async fn execute(&mut self, input: Value) {
        self.execute_model(MODEL, input).await;
    }

    async fn execute_model(&mut self, model: &str, input: Value) {
        let request = UpstreamAdapterRequest {
            adapter_id: "gemini-api".into(),
            provider: BuiltinProvider::OpenAi,
            upstream_model: model.into(),
            client_key_id: "key_fixture".into(),
            account_id: "acct_fixture".into(),
            credential_revision: 7,
            protocol: "openai".into(),
            client_transport: "http".into(),
            fast_mode: "default".into(),
            headers: vec![(
                "authorization".into(),
                b"Bearer fixture-client-secret".to_vec(),
            )],
            continuation: None,
        };
        self.call(
            1,
            "upstream_adapter.execute",
            Stage::Upstream,
            request.encode(serde_json::to_vec(&input).unwrap()).unwrap(),
        )
        .await;
        self.control(Message::Credit {
            id: 1,
            bytes: 1024 * 1024,
            frames: 1,
        })
        .await;
    }

    async fn reply(&mut self, id: u64, result: Value, payload: Vec<u8>) {
        write_frame(
            &mut self.input,
            &Frame {
                message: Message::Result { id, result },
                payload,
            },
        )
        .await
        .unwrap();
    }

    async fn shutdown(mut self) {
        self.control(Message::Shutdown).await;
        drop(self.input);
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .expect("plugin exit timeout")
            .unwrap();
        assert!(status.success());
    }
}

struct Fixture {
    status: u16,
    base_url: String,
    model: String,
    chunks: VecDeque<Vec<u8>>,
    outbound: Vec<Value>,
    closed: usize,
    reads: usize,
    auth_reads: usize,
    retry_after: Option<String>,
}

impl Fixture {
    fn success(body: Vec<u8>, size: usize) -> Self {
        Self {
            status: 200,
            base_url: BASE.into(),
            model: MODEL.into(),
            chunks: body.chunks(size).map(<[u8]>::to_vec).collect(),
            outbound: vec![],
            closed: 0,
            reads: 0,
            auth_reads: 0,
            retry_after: None,
        }
    }

    async fn callback(&mut self, peer: &mut Peer, frame: Frame) {
        let Message::Callback {
            id,
            parent_id,
            method,
            params,
        } = frame.message
        else {
            panic!("expected callback")
        };
        assert_eq!(parent_id, 1);
        match method.as_str() {
            "host.auth.get" => {
                self.auth_reads += 1;
                assert_eq!(params, json!({}));
                assert_eq!(
                    serde_json::from_slice::<Value>(&frame.payload).unwrap(),
                    json!({"account_id":"acct_fixture"})
                );
                let credential = json!({
                    "account_id":"acct_fixture", "provider_id":"openai", "credential_revision":7,
                    "facts":{
                        "name":"Gemini fixture", "authentication_kind":"api_key",
                        "material":{"schema_version":1,"installation_id":"fixture-installation","base_url":self.base_url,"api_key":"fixture-google-secret","transport":"http_sse"},
                        "email":null,"upstream_user_id":null,"upstream_account_id":null,"plan_type":null,
                        "has_refresh_token":false,"access_token_expires_at_ms":null,"next_refresh_at_ms":null
                    }
                });
                peer.reply(id, json!({}), serde_json::to_vec(&credential).unwrap())
                    .await;
            }
            "host.upstream.http.do_stream" => {
                let request: UpstreamHttpRequest = serde_json::from_value(params).unwrap();
                assert_eq!(request.method, "POST");
                assert_eq!(
                    request.path,
                    format!("models/{}:streamGenerateContent", self.model)
                );
                assert_eq!(request.query, vec![("alt".into(), "sse".into())]);
                assert!(
                    request
                        .headers
                        .contains(&("x-goog-api-key".into(), "fixture-google-secret".into()))
                );
                assert!(
                    request
                        .headers
                        .contains(&("authorization".into(), String::new()))
                );
                assert!(
                    !request
                        .headers
                        .iter()
                        .any(|(_, value)| value.contains("fixture-client-secret"))
                );
                self.outbound
                    .push(serde_json::from_slice(&frame.payload).unwrap());
                let mut headers = vec![(
                    "content-type",
                    if self.status == 200 {
                        "text/event-stream; charset=utf-8"
                    } else {
                        "application/json"
                    },
                )];
                if let Some(retry_after) = &self.retry_after {
                    headers.push(("retry-after", retry_after));
                }
                peer.reply(
                    id,
                    json!({"status":self.status,"headers":headers,"stream":"fixture-body"}),
                    vec![],
                )
                .await;
            }
            "host.upstream.http.stream_read" => {
                assert_eq!(params["stream"], "fixture-body");
                self.reads += 1;
                if let Some(chunk) = self.chunks.pop_front() {
                    assert!(chunk.len() <= params["maximum_bytes"].as_u64().unwrap() as usize);
                    peer.reply(id, json!({"eof":false}), chunk).await;
                } else {
                    peer.reply(id, json!({"eof":true}), vec![]).await;
                }
            }
            "host.upstream.http.stream_close" => {
                assert_eq!(params["stream"], "fixture-body");
                self.closed += 1;
                peer.reply(id, json!({}), vec![]).await;
            }
            _ => panic!("unexpected callback {method}"),
        }
    }
}

async fn run(input: Value, fixture: &mut Fixture) -> Vec<UpstreamAdapterEvent> {
    let mut peer = Peer::start_with(json!({"baseUrl":BASE,"models":[&fixture.model]})).await;
    peer.execute_model(&fixture.model, input).await;
    let mut events = Vec::new();
    let mut sequence = 0;
    loop {
        let frame = peer.receive().await;
        match &frame.message {
            Message::Callback { .. } => fixture.callback(&mut peer, frame).await,
            Message::Result { id: 1, result } => assert_eq!(result, &json!({})),
            Message::Stream {
                id: 1,
                sequence: current,
            } => {
                assert_eq!(*current, sequence);
                sequence += 1;
                events.push(UpstreamAdapterEvent::decode(&frame.payload).unwrap());
                peer.control(Message::Credit {
                    id: 1,
                    bytes: frame.payload.len() as u32,
                    frames: 1,
                })
                .await;
            }
            Message::End { id: 1, error: None } => break,
            _ => panic!("unexpected plugin frame: {:?}", frame.message),
        }
    }
    peer.shutdown().await;
    events
}

fn sse(values: &[Value]) -> Vec<u8> {
    values
        .iter()
        .map(|value| format!("data: {value}\r\n\r\n"))
        .collect::<String>()
        .into_bytes()
}

fn completed(events: &[UpstreamAdapterEvent]) -> usize {
    events
        .iter()
        .flat_map(|event| &event.event.facts)
        .filter(|fact| matches!(fact, CanonicalEvent::Completed { .. }))
        .count()
}

fn terminal_response(events: &[UpstreamAdapterEvent]) -> &Value {
    assert!(events.iter().all(|event| event.failure.is_none()));
    assert_eq!(completed(events), 1);
    let WirePayload::Json {
        data,
        event: Some(name),
        ..
    } = &events.last().unwrap().event.wire.as_ref().unwrap().payload
    else {
        panic!("missing terminal Responses event")
    };
    assert_eq!(name, "response.completed");
    assert_eq!(data["response"]["status"], "completed");
    &data["response"]
}

#[tokio::test]
async fn binary_registers_only_the_configured_gemini_models() {
    let mut peer = Peer::start().await;
    peer.call(1, "plugin.register", Stage::Registration, vec![])
        .await;
    assert!(matches!(
        peer.receive().await.message,
        Message::Result { id: 1, .. }
    ));
    peer.call(3, "upstream_adapter.register", Stage::Registration, vec![])
        .await;
    let frame = peer.receive().await;
    let registration: UpstreamAdapterRegistration = serde_json::from_slice(&frame.payload).unwrap();
    assert_eq!(registration.adapters[0].models, vec![MODEL]);
    assert_eq!(registration.adapters[0].base_url, BASE);
    assert_eq!(
        registration.adapters[0].authentication_kinds,
        vec!["api_key"]
    );
    peer.shutdown().await;
}

#[tokio::test]
async fn binary_converts_fragmented_sse_and_json_responses() {
    for stream in [true, false] {
        let mut fixture = Fixture::success(
            sse(&[
                json!({"candidates":[{"content":{"parts":[{"text":"你好"}]}}]}),
                json!({"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5}}),
            ]),
            13,
        );
        let events = run(
            json!({"model":MODEL,"input":"hello","stream":stream}),
            &mut fixture,
        )
        .await;
        assert_eq!(completed(&events), 1);
        assert!(events.iter().all(|event| event.failure.is_none()));
        assert_eq!(fixture.outbound.len(), 1);
        assert_eq!(
            fixture.outbound[0]["contents"][0]["parts"][0]["text"],
            "hello"
        );
        assert!(fixture.outbound[0].get("input").is_none());
        let terminal = &events.last().unwrap().event.wire.as_ref().unwrap().payload;
        let response = match terminal {
            WirePayload::Json {
                data,
                event: Some(name),
                ..
            } if name == "response.completed" => data["response"].clone(),
            _ => panic!("missing terminal Responses event required by the host collector"),
        };
        if !stream {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event.wire.is_some())
                    .count(),
                1
            );
        }
        assert_eq!(response["output"][0]["content"][0]["text"], "你好");
        assert_eq!(response["usage"]["total_tokens"], 5);
    }
}

#[tokio::test]
async fn binary_registration_preserves_a_third_party_path_prefix() {
    let base = "https://gateway.example.com/bridge/generative/v1beta/";
    let mut peer =
        Peer::start_with(json!({"baseUrl":base,"models":["team-flash"],"authMode":"bearer"})).await;
    peer.call(1, "upstream_adapter.register", Stage::Registration, vec![])
        .await;
    let frame = peer.receive().await;
    let registration: UpstreamAdapterRegistration = serde_json::from_slice(&frame.payload).unwrap();
    assert_eq!(registration.adapters[0].base_url, base);
    assert_eq!(registration.adapters[0].models, vec!["team-flash"]);
    assert_eq!(registration.adapters[0].paths.len(), 1);
    assert_eq!(registration.adapters[0].paths[0].path, "models");
    peer.shutdown().await;
}

#[tokio::test]
async fn binary_registration_respects_host_path_limits_for_every_model_count() {
    for count in [1, 16, 17, 64] {
        let models: Vec<_> = (0..count)
            .map(|index| format!("team-flash-{index}"))
            .collect();
        let mut peer = Peer::start_with(json!({"baseUrl":BASE,"models":models})).await;
        peer.call(1, "upstream_adapter.register", Stage::Registration, vec![])
            .await;
        let frame = peer.receive().await;
        let registration: UpstreamAdapterRegistration =
            serde_json::from_slice(&frame.payload).unwrap();
        assert_eq!(registration.adapters.len(), 1);
        assert_eq!(registration.adapters[0].models, models);
        assert_eq!(registration.adapters[0].paths.len(), 1);
        assert_eq!(registration.adapters[0].paths[0].path, "models");
        assert_eq!(
            serde_json::to_value(registration.adapters[0].paths[0].purpose).unwrap(),
            "inference"
        );
        peer.shutdown().await;
    }
}

#[tokio::test]
async fn binary_accepts_both_third_party_authentication_modes() {
    let base = "https://gateway.example.com/bridge/generative/v1beta/";
    for auth in ["x-goog-api-key", "bearer"] {
        let mut peer =
            Peer::start_with(json!({"baseUrl":base,"models":[MODEL],"authMode":auth})).await;
        peer.execute(json!({"input":"hello","stream":true})).await;
        let mut fixture = Fixture::success(
            sse(&[
                json!({"candidates":[{"content":{"parts":[{"text":"third party"}]},"finishReason":"STOP"}]}),
            ]),
            4096,
        );
        fixture.base_url = base.into();
        let mut outbound = 0;
        let mut events = Vec::new();
        loop {
            let frame = peer.receive().await;
            match &frame.message {
                Message::Callback {
                    id, method, params, ..
                } if method == "host.upstream.http.do_stream" => {
                    let request: UpstreamHttpRequest =
                        serde_json::from_value(params.clone()).unwrap();
                    let expected = if auth == "bearer" {
                        (
                            "authorization".into(),
                            "Bearer fixture-google-secret".into(),
                        )
                    } else {
                        ("x-goog-api-key".into(), "fixture-google-secret".into())
                    };
                    assert!(request.headers.contains(&expected));
                    assert_eq!(
                        request.path,
                        format!("models/{MODEL}:streamGenerateContent")
                    );
                    assert_eq!(request.query, vec![("alt".into(), "sse".into())]);
                    outbound += 1;
                    peer.reply(*id, json!({"status":200,"headers":[["content-type","text/event-stream"]],"stream":"fixture-body"}), vec![]).await;
                }
                Message::Callback { .. } => fixture.callback(&mut peer, frame).await,
                Message::Result { id: 1, .. } => {}
                Message::Stream { id: 1, .. } => {
                    events.push(UpstreamAdapterEvent::decode(&frame.payload).unwrap());
                    peer.control(Message::Credit {
                        id: 1,
                        bytes: frame.payload.len() as u32,
                        frames: 1,
                    })
                    .await;
                }
                Message::End { id: 1, error: None } => break,
                _ => panic!("unexpected third-party RPC frame"),
            }
        }
        assert_eq!(outbound, 1);
        assert_eq!(completed(&events), 1);
        assert!(events.iter().all(|event| event.failure.is_none()));
        peer.shutdown().await;
    }
}

#[tokio::test]
async fn codex_argon_metadata_and_streaming_hints_reach_the_native_endpoint_safely() {
    let model = "gemini-4-argon";
    let mut fixture = Fixture::success(
        sse(&[
            json!({"candidates":[{"content":{"parts":[{"text":"OK"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}}),
        ]),
        13,
    );
    fixture.model = model.into();
    let events = run(json!({
        "model":model,"instructions":"Be concise","input":[{"role":"user","content":[{"type":"input_text","text":"Hello"}]}],
        "tools":[],"tool_choice":"auto","parallel_tool_calls":true,"store":false,"stream":true,
        "reasoning":{"effort":"low","summary":"auto","context":"auto"},
        "stream_options":{"reasoning_summary_delivery":"sequential_cutoff","include_obfuscation":false,"include_usage":true},
        "include":["reasoning.encrypted_content"],"text":{"verbosity":"medium"},
        "client_metadata":{"x-codex-installation-id":"fixture-private-installation","x-codex-turn-metadata":"fixture-private-trace"},
        "max_tool_calls":null,"context_management":null
    }), &mut fixture).await;
    assert!(events.iter().all(|event| event.failure.is_none()));
    assert_eq!(completed(&events), 1);
    assert_eq!(fixture.outbound.len(), 1);
    assert_eq!(
        fixture.outbound[0]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "LOW"
    );
    assert_eq!(
        fixture.outbound[0]["contents"][0]["parts"][0]["text"],
        "Hello"
    );
    assert!(fixture.outbound[0].get("client_metadata").is_none());
    assert!(fixture.outbound[0].get("stream_options").is_none());
    assert!(!fixture.outbound[0].to_string().contains("fixture-private"));
    let WirePayload::Json { data, .. } =
        &events.last().unwrap().event.wire.as_ref().unwrap().payload
    else {
        panic!("expected Responses terminal event")
    };
    assert_eq!(data["response"]["model"], model);
    assert_eq!(data["response"]["status"], "completed");
    assert_eq!(data["response"]["output"][0]["content"][0]["text"], "OK");
    assert!(!data.to_string().contains("fixture-private"));
}

#[tokio::test]
async fn client_tool_search_with_optional_web_search_round_trips_three_turns_through_the_binary() {
    let model = "gemini-4-argon";
    let search_part = json!({"functionCall":{"name":"cpr_tool_search","id":"search-1","args":{"query":"file reader","limit":1}},"thoughtSignature":"fixture-search-signature"});
    let read_part = json!({"functionCall":{"name":"read_file","id":"read-1","args":{"path":"example.txt"}},"thoughtSignature":"fixture-read-signature"});
    let discovered = json!([{"type":"function","name":"read_file","defer_loading":true,"parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}]);
    for stream in [true, false] {
        let mut input = json!({
            "model":model,"instructions":"Use client tools","input":[{"role":"user","content":[{"type":"input_text","text":"Read example.txt"}]}],
            "tools":[
                {"type":"namespace","name":"functions","tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"command":{"type":"string"}}}}]},
                {"type":"custom","name":"apply_patch","format":{"type":"text"}},
                {"type":"tool_search","execution":"client","description":"Find deferred tools","parameters":{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query"]}},
                {"type":"web_search","external_web_access":false,"user_location":{"type":"approximate","city":"fixture-private-city"}}
            ],
            "tool_choice":"auto","parallel_tool_calls":true,"store":false,"stream":stream,
            "reasoning":{"effort":"low","summary":"auto","context":"auto"},"include":["reasoning.encrypted_content","web_search_call.action.sources"],
            "stream_options":if stream { json!({"reasoning_summary_delivery":"sequential_cutoff","include_obfuscation":false}) } else { Value::Null },
            "client_metadata":{"x-codex-turn-metadata":"fixture-private-trace"}
        });
        let mut search_fixture = Fixture::success(
            sse(&[
                json!({"candidates":[{"content":{"parts":[search_part.clone()]},"finishReason":"STOP"}]}),
            ]),
            11,
        );
        search_fixture.model = model.into();
        let events = run(input.clone(), &mut search_fixture).await;
        let response = terminal_response(&events);
        assert_eq!(response["tools"].as_array().unwrap().len(), 3);
        assert_eq!(response["output"][0]["type"], "tool_search_call");
        assert_eq!(response["output"][0]["execution"], "client");
        assert_eq!(
            response["output"][0]["arguments"],
            search_part["functionCall"]["args"]
        );
        assert_eq!(search_fixture.auth_reads, 1);
        assert_eq!(search_fixture.outbound.len(), 1);
        assert_eq!(
            search_fixture.outbound[0]["tools"][0]["functionDeclarations"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert!(!response.to_string().contains("fixture-private"));
        let history = input["input"].as_array_mut().unwrap();
        history.extend(response["output"].as_array().unwrap().clone());
        history.push(json!({"type":"tool_search_output","call_id":"search-1","execution":"client","status":"completed","tools":discovered}));

        let mut read_fixture = Fixture::success(
            sse(&[
                json!({"candidates":[{"content":{"parts":[read_part.clone()]},"finishReason":"STOP"}]}),
            ]),
            13,
        );
        read_fixture.model = model.into();
        let events = run(input.clone(), &mut read_fixture).await;
        let response = terminal_response(&events);
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["name"], "read_file");
        assert_eq!(response["output"][0]["call_id"], "read-1");
        assert_eq!(
            read_fixture.outbound[0]["contents"][1]["parts"][0],
            search_part
        );
        assert_eq!(
            read_fixture.outbound[0]["contents"][2]["parts"][0]["functionResponse"]["response"]["tools"],
            discovered
        );
        let declarations = read_fixture.outbound[0]["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap();
        assert_eq!(declarations.len(), 4);
        assert_eq!(declarations[3]["name"], "read_file");
        assert_eq!(
            declarations[3]["parametersJsonSchema"],
            discovered[0]["parameters"]
        );
        let history = input["input"].as_array_mut().unwrap();
        history.extend(response["output"].as_array().unwrap().clone());
        history.push(json!({"type":"function_call_output","call_id":"read-1","output":"Fixture file contents"}));

        let mut final_fixture = Fixture::success(
            sse(&[
                json!({"candidates":[{"content":{"parts":[{"text":"Fixture file contents"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"totalTokenCount":13}}),
            ]),
            17,
        );
        final_fixture.model = model.into();
        let events = run(input, &mut final_fixture).await;
        let response = terminal_response(&events);
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            "Fixture file contents"
        );
        assert_eq!(response["usage"]["total_tokens"], 13);
        assert_eq!(
            final_fixture.outbound[0]["contents"][3]["parts"][0],
            read_part
        );
        assert_eq!(
            final_fixture.outbound[0]["contents"][4]["parts"][0]["functionResponse"],
            json!({"id":"read-1","name":"read_file","response":{"output":"Fixture file contents"}})
        );
        for fixture in [&search_fixture, &read_fixture, &final_fixture] {
            assert_eq!(fixture.auth_reads, 1);
            assert_eq!(fixture.outbound.len(), 1);
            assert_eq!(fixture.closed, 0);
            assert!(fixture.chunks.is_empty());
            assert!(!fixture.outbound[0].to_string().contains("fixture-private"));
        }
    }
}

#[tokio::test]
async fn optional_web_search_reaches_the_native_endpoint_without_a_search_switch() {
    for stream in [true, false] {
        for kind in [
            "web_search",
            "web_search_preview",
            "web_search_2025_08_26",
            "web_search_preview_2025_03_11",
        ] {
            let mut fixture = Fixture::success(
                sse(&[
                    json!({"candidates":[{"content":{"parts":[{"text":"OK"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}}),
                ]),
                13,
            );
            fixture.model = "gemini-4-argon".into();
            let events = run(json!({
                "model":fixture.model,"input":[{"role":"user","content":[{"type":"input_text","text":"Hello"}]}],
                "tools":[{"type":kind,"external_web_access":false,"user_location":{"type":"approximate","city":"fixture-private-city"}}],
                "tool_choice":"auto","parallel_tool_calls":true,"store":false,"stream":stream,
                "include":["reasoning.encrypted_content","web_search_call.action.sources"],
                "stream_options":if stream { json!({"reasoning_summary_delivery":"sequential_cutoff"}) } else { Value::Null },
                "client_metadata":{"x-codex-turn-metadata":"fixture-private-trace"}
            }), &mut fixture).await;
            let response = terminal_response(&events);
            assert_eq!(response["tools"], json!([]));
            assert_eq!(response["output"].as_array().unwrap().len(), 1);
            assert_eq!(response["output"][0]["type"], "message");
            assert_eq!(response["output"][0]["content"][0]["text"], "OK");
            assert_eq!(response["usage"]["total_tokens"], 4);
            assert_eq!(fixture.auth_reads, 1);
            assert_eq!(fixture.outbound.len(), 1);
            assert!(fixture.outbound[0].get("tools").is_none());
            assert!(fixture.outbound[0].get("toolConfig").is_none());
            assert!(!fixture.outbound[0].to_string().contains("fixture-private"));
            assert!(!response.to_string().contains("fixture-private"));
        }
    }
}

#[tokio::test]
async fn forced_web_search_fails_before_credentials_or_upstream_access() {
    for kind in [
        "web_search",
        "web_search_preview",
        "web_search_2025_08_26",
        "web_search_preview_2025_03_11",
    ] {
        for choice in [json!("required"), json!({"type":kind})] {
            let mut fixture = Fixture::success(vec![], 1);
            let events = run(json!({
                "input":"fixture-private-message","tools":[{"type":kind}],"tool_choice":choice,
                "stream":true,"store":false,"include":["reasoning.encrypted_content","web_search_call.action.sources"],
                "stream_options":{"reasoning_summary_delivery":"sequential_cutoff"},
                "client_metadata":{"x-codex-turn-metadata":"fixture-private-trace"}
            }), &mut fixture).await;
            assert_eq!(events.len(), 1);
            let failure = events[0].failure.as_ref().unwrap();
            assert_eq!(failure.kind, UpstreamFailureKind::Unsupported);
            assert!(failure.message.contains("web_search"));
            assert!(failure.message.contains("tool_choice"));
            assert!(!failure.message.contains("fixture-private"));
            assert_eq!(fixture.auth_reads, 0);
            assert!(fixture.outbound.is_empty());
            assert_eq!(completed(&events), 0);
        }
    }
}

#[tokio::test]
async fn unsupported_tool_types_are_named_before_credentials_or_upstream_access() {
    for (tool, name) in [
        (
            json!({"type":"tool_search","execution":"server"}),
            "tool_search",
        ),
        (
            json!({"type":"file_search","vector_store_ids":["fixture-private-store"]}),
            "file_search",
        ),
        (json!({"type":"image_generation"}), "image_generation"),
        (
            json!({"type":"fixture-private-type","name":"fixture-private-name"}),
            "Unsupported tool type",
        ),
    ] {
        let mut fixture = Fixture::success(vec![], 1);
        let events = run(
            json!({
                "input":"fixture-private-message","tools":[tool],"stream":true,"store":false,
                "client_metadata":{"x-codex-turn-metadata":"fixture-private-trace"},
                "stream_options":{"reasoning_summary_delivery":"sequential_cutoff"}
            }),
            &mut fixture,
        )
        .await;
        assert_eq!(events.len(), 1);
        let failure = events[0].failure.as_ref().unwrap();
        assert_eq!(failure.kind, UpstreamFailureKind::Unsupported);
        assert_eq!(failure.code.as_deref(), Some("gemini_unsupported"));
        assert!(failure.message.contains(name));
        assert!(!failure.message.contains("fixture-private"));
        assert_eq!(fixture.auth_reads, 0);
        assert!(fixture.outbound.is_empty());
        assert_eq!(completed(&events), 0);
    }
}

#[tokio::test]
async fn unsupported_stateful_options_are_named_and_never_sent_upstream() {
    for (key, value) in [
        (
            "context_management",
            json!([{"type":"compaction","compact_threshold":1000}]),
        ),
        ("access_programs", json!({"cyber":"fixture-private-value"})),
        ("max_tool_calls", json!(4)),
    ] {
        let mut fixture = Fixture::success(vec![], 1);
        let mut input =
            json!({"input":"Hello","client_metadata":{"private":"fixture-private-value"}});
        input[key] = value;
        let events = run(input, &mut fixture).await;
        let failure = events[0].failure.as_ref().expect("option must be rejected");
        assert_eq!(failure.kind, UpstreamFailureKind::Unsupported);
        assert!(failure.message.contains(key));
        assert!(!failure.message.contains("fixture-private-value"));
        assert!(fixture.outbound.is_empty());
        assert_eq!(completed(&events), 0);
    }
}

#[tokio::test]
async fn wrong_account_never_reaches_the_upstream() {
    let mut fixture = Fixture::success(vec![], 1);
    fixture.base_url = "https://api.openai.com/v1/".into();
    let events = run(json!({"input":"hello","stream":true}), &mut fixture).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].failure.as_ref().unwrap().kind,
        UpstreamFailureKind::InvalidRequest
    );
    assert!(fixture.outbound.is_empty());
    assert_eq!(fixture.reads, 0);
}

#[tokio::test]
async fn invalid_requests_do_not_read_credentials_or_call_the_upstream() {
    let mut fixture = Fixture::success(vec![], 1);
    let events = run(
        json!({"input":"continue","previous_response_id":"resp_old"}),
        &mut fixture,
    )
    .await;
    assert_eq!(
        events[0].failure.as_ref().unwrap().kind,
        UpstreamFailureKind::Unsupported
    );
    assert!(fixture.outbound.is_empty());
}

#[tokio::test]
async fn google_errors_preserve_backoff_without_exposing_the_key() {
    let mut fixture = Fixture::success(
        br#"{"error":{"message":"fixture-google-secret"}}"#.to_vec(),
        16,
    );
    fixture.status = 429;
    fixture.retry_after = Some("2".into());
    let events = run(json!({"input":"hello","stream":true}), &mut fixture).await;
    assert_eq!(completed(&events), 0);
    let failure = events.last().unwrap().failure.as_ref().unwrap();
    assert_eq!(failure.status, Some(429));
    assert_eq!(failure.retry_after_ms, Some(2000));
    assert!(
        !serde_json::to_string(failure)
            .unwrap()
            .contains("fixture-google-secret")
    );
}

#[tokio::test]
async fn malformed_sse_closes_the_body_and_cannot_report_completion() {
    let mut fixture = Fixture::success(b"data: not-json\n\nmore bytes".to_vec(), 18);
    let events = run(json!({"input":"hello","stream":true}), &mut fixture).await;
    assert_eq!(fixture.closed, 1);
    assert_eq!(completed(&events), 0);
    assert_eq!(
        events.last().unwrap().failure.as_ref().unwrap().kind,
        UpstreamFailureKind::Protocol
    );
}

#[tokio::test]
async fn eof_without_a_finish_reason_reports_truncation() {
    let mut fixture = Fixture::success(
        sse(&[json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]})]),
        100,
    );
    let events = run(json!({"input":"hello","stream":true}), &mut fixture).await;
    assert_eq!(completed(&events), 0);
    assert_eq!(
        events.last().unwrap().failure.as_ref().unwrap().kind,
        UpstreamFailureKind::Protocol
    );
}

#[tokio::test]
async fn cancellation_interrupts_an_inflight_host_read_and_keeps_the_plugin_usable() {
    let mut peer = Peer::start().await;
    peer.execute(json!({"input":"hello","stream":true})).await;
    let mut fixture = Fixture::success(vec![], 1);
    let pending = loop {
        let frame = peer.receive().await;
        match &frame.message {
            Message::Callback { id, method, .. } if method == "host.upstream.http.stream_read" => {
                break *id;
            }
            Message::Callback { .. } => fixture.callback(&mut peer, frame).await,
            Message::Result { id: 1, .. } => {}
            _ => panic!("unexpected frame before cancellation"),
        }
    };
    peer.control(Message::Cancel { id: 1 }).await;
    assert!(matches!(
        peer.receive().await.message,
        Message::Cancelled { id: 1 }
    ));
    peer.reply(pending, json!({"eof":true}), vec![]).await;
    peer.call(3, "upstream_adapter.register", Stage::Registration, vec![])
        .await;
    assert!(matches!(
        peer.receive().await.message,
        Message::Result { id: 3, .. }
    ));
    peer.shutdown().await;
}

#[tokio::test]
async fn sdk_credit_stops_output_until_the_host_consumes_it() {
    let mut peer = Peer::start().await;
    peer.execute(json!({"input":"hello","stream":true})).await;
    let mut fixture = Fixture::success(
        sse(&[
            json!({"candidates":[{"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}]}),
        ]),
        4096,
    );
    loop {
        let frame = peer.receive().await;
        match &frame.message {
            Message::Callback { .. } => fixture.callback(&mut peer, frame).await,
            Message::Result { id: 1, .. } => {}
            Message::Stream { id: 1, .. } => break,
            _ => panic!("unexpected frame before credit exhaustion"),
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut peer.output))
            .await
            .is_err()
    );
    assert_eq!(fixture.reads, 1);
    peer.control(Message::Cancel { id: 1 }).await;
    assert!(matches!(
        peer.receive().await.message,
        Message::Cancelled { id: 1 }
    ));
    peer.shutdown().await;
}
