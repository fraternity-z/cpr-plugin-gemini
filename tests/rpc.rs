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
        let request = UpstreamAdapterRequest {
            adapter_id: "gemini-api".into(),
            provider: BuiltinProvider::OpenAi,
            upstream_model: MODEL.into(),
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
    chunks: VecDeque<Vec<u8>>,
    outbound: Vec<Value>,
    closed: usize,
    reads: usize,
    retry_after: Option<String>,
}

impl Fixture {
    fn success(body: Vec<u8>, size: usize) -> Self {
        Self {
            status: 200,
            base_url: BASE.into(),
            chunks: body.chunks(size).map(<[u8]>::to_vec).collect(),
            outbound: vec![],
            closed: 0,
            reads: 0,
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
                    format!("models/{MODEL}:streamGenerateContent")
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
    let mut peer = Peer::start().await;
    peer.execute(input).await;
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
    assert_eq!(
        registration.adapters[0].paths[0].path,
        "models/team-flash:streamGenerateContent"
    );
    peer.shutdown().await;
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
