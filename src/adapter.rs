//! 通过宿主已选账号执行受管 HTTP，并按消费进度转换与释放上游响应

use std::collections::VecDeque;

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        host::{AuthCredential, AuthGetRequest},
        model::ExecutionEvent,
        upstream_adapter::{
            UpstreamAdapterEvent, UpstreamAdapterRequest, UpstreamFailure, UpstreamFailureKind,
            UpstreamHttpRequest,
        },
    },
    client::{
        Empty, HostHttpBody, PullResponseFuture, PullResponseStream, ResponseStream, SessionError,
        TypedCall, TypedReply,
    },
};
use serde_json::{Value, json};

use crate::{
    config::{Config, inference_path},
    error::Error,
    request,
    response::Response,
    sse,
};

pub(crate) async fn execute(
    config: &Config,
    call: TypedCall<UpstreamAdapterRequest>,
) -> Result<TypedReply<Empty>, PluginFault> {
    if let Err(error) = config.validate_request(&call.request) {
        return failed_reply(error.failure());
    }
    if call.payload.len() > 16 * 1024 * 1024 {
        return failed_reply(
            Error::invalid("Responses request exceeds the 16 MiB limit").failure(),
        );
    }
    let input: Value = match serde_json::from_slice(&call.payload) {
        Ok(value) => value,
        Err(_) => {
            return failed_reply(Error::invalid("Responses request is not valid JSON").failure());
        }
    };
    let prepared = match request::translate(input, &call.request.upstream_model) {
        Ok(value) => value,
        Err(error) => return failed_reply(error.failure()),
    };
    if serde_json::to_vec(&prepared.response_fields)
        .map_err(|_| encoding_fault())?
        .len()
        > 128 * 1024
    {
        return failed_reply(
            Error::invalid("Response echo fields exceed the 128 KiB limit").failure(),
        );
    }
    let auth = call
        .host
        .call(
            "host.auth.get",
            json!({}),
            serde_json::to_vec(&AuthGetRequest {
                account_id: call.request.account_id.clone(),
            })
            .map_err(|_| encoding_fault())?,
        )
        .await
        .map_err(SessionError::into_plugin_fault)?;
    if auth.result != json!({}) {
        return Err(encoding_fault());
    }
    let credential: AuthCredential =
        serde_json::from_slice(&auth.payload).map_err(|_| encoding_fault())?;
    let headers = match config.headers(&call.request, credential) {
        Ok(headers) => headers,
        Err(error) => return failed_reply(error.failure()),
    };
    let response = call
        .host
        .upstream_http(
            UpstreamHttpRequest {
                method: "POST".into(),
                path: inference_path(&call.request.upstream_model),
                headers,
                query: vec![("alt".into(), "sse".into())],
            },
            serde_json::to_vec(&prepared.body).map_err(|_| encoding_fault())?,
        )
        .await?;
    if response.status != 200 {
        let retry = retry_after(&response.headers);
        let error = response
            .body
            .collect(64 * 1024)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        return failed_reply(http_failure(response.status, error.as_ref(), retry));
    }
    let content_type = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str());
    if !content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
    }) {
        let mut body = response.body;
        let _ = body.close().await;
        return failed_reply(
            Error::protocol("Gemini streamGenerateContent did not return text/event-stream")
                .failure(),
        );
    }
    Ok(
        TypedReply::new(Empty {}).with_stream(ResponseStream::pull(Box::new(Stream {
            body: response.body,
            decoder: sse::Decoder::default(),
            response: Response::new(
                call.request.upstream_model,
                prepared.stream,
                prepared.tools,
                prepared.response_fields,
            ),
            pending: VecDeque::new(),
            ended: false,
        }))),
    )
}

struct Stream {
    body: HostHttpBody,
    decoder: sse::Decoder,
    response: Response,
    pending: VecDeque<Vec<u8>>,
    ended: bool,
}

impl PullResponseStream for Stream {
    fn next(&mut self) -> PullResponseFuture<'_> {
        Box::pin(async move {
            loop {
                if let Some(bytes) = self.pending.pop_front() {
                    return Some(Ok(bytes));
                }
                if self.ended {
                    return None;
                }
                match self.body.read().await {
                    Ok(Some(bytes)) => {
                        let values = match self.decoder.push(&bytes) {
                            Ok(values) => values,
                            Err(error) => {
                                if let Err(fault) = self.fail(error.failure()).await {
                                    return Some(Err(fault));
                                }
                                continue;
                            }
                        };
                        for value in values {
                            if value.get("error").is_some() {
                                let status = value["error"]["code"]
                                    .as_u64()
                                    .filter(|status| (400..=599).contains(status))
                                    .unwrap_or(502)
                                    as u16;
                                if let Err(fault) =
                                    self.fail(http_failure(status, Some(&value), None)).await
                                {
                                    return Some(Err(fault));
                                }
                                break;
                            }
                            let events = self.response.consume(value).and_then(encode_events);
                            match events {
                                Ok(events) => self.pending.extend(events),
                                Err(error) => {
                                    if let Err(fault) = self.fail(error.failure()).await {
                                        return Some(Err(fault));
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        let events = self
                            .decoder
                            .finish()
                            .and_then(|()| self.response.finish())
                            .and_then(encode_events);
                        match events {
                            Ok(events) => {
                                self.pending.extend(events);
                                self.ended = true;
                            }
                            Err(error) => {
                                if let Err(fault) = self.fail(error.failure()).await {
                                    return Some(Err(fault));
                                }
                            }
                        }
                    }
                    Err(fault) => {
                        self.ended = true;
                        let _ = self.body.close().await;
                        return Some(Err(fault));
                    }
                }
            }
        })
    }
}

impl Stream {
    async fn fail(&mut self, failure: UpstreamFailure) -> Result<(), PluginFault> {
        self.ended = true;
        let _ = self.body.close().await;
        self.pending.push_back(encode_failure(failure)?);
        Ok(())
    }
}

fn encode_events(events: Vec<ExecutionEvent>) -> crate::error::Result<Vec<Vec<u8>>> {
    events
        .into_iter()
        .map(|event| {
            let encoded = UpstreamAdapterEvent::new(event).encode().map_err(|_| {
                Error::protocol("Converted response exceeds the plugin event limit")
            })?;
            if encoded.len() > 1024 * 1024 {
                return Err(Error::protocol(
                    "Converted response exceeds the 1 MiB event limit",
                ));
            }
            Ok(encoded)
        })
        .collect()
}

fn failed_reply(failure: UpstreamFailure) -> Result<TypedReply<Empty>, PluginFault> {
    Ok(TypedReply::new(Empty {})
        .with_stream(ResponseStream::from_chunks(vec![encode_failure(failure)?])))
}

fn encode_failure(failure: UpstreamFailure) -> Result<Vec<u8>, PluginFault> {
    let mut event = UpstreamAdapterEvent::new(ExecutionEvent::default());
    event.failure = Some(failure);
    event.encode().map_err(|_| encoding_fault())
}

pub(crate) fn http_failure(
    status: u16,
    error: Option<&Value>,
    retry: Option<u64>,
) -> UpstreamFailure {
    let (kind, message, code) = match status {
        400 | 422 => (
            UpstreamFailureKind::InvalidRequest,
            "Gemini rejected the request parameters",
            "gemini_invalid_request",
        ),
        401 => (
            UpstreamFailureKind::Unauthorized,
            "Gemini rejected the API Key",
            "gemini_unauthorized",
        ),
        403 => (
            UpstreamFailureKind::PermissionDenied,
            "Gemini denied access to this API Key or model",
            "gemini_permission_denied",
        ),
        404 => (
            UpstreamFailureKind::InvalidRequest,
            "Gemini model or API endpoint was not found",
            "gemini_not_found",
        ),
        408 | 504 => (
            UpstreamFailureKind::Timeout,
            "Gemini request timed out",
            "gemini_timeout",
        ),
        429 => (
            UpstreamFailureKind::RateLimited,
            "Gemini rate limit or quota was exceeded",
            "gemini_rate_limited",
        ),
        500..=599 => (
            UpstreamFailureKind::Unavailable,
            "Gemini is temporarily unavailable",
            "gemini_unavailable",
        ),
        _ => (
            UpstreamFailureKind::Protocol,
            "Gemini returned an unexpected HTTP status",
            "gemini_protocol_error",
        ),
    };
    let retry_after_ms = retry.or_else(|| {
        error?
            .get("error")?
            .get("details")?
            .as_array()?
            .iter()
            .find_map(|detail| {
                if detail["@type"] != "type.googleapis.com/google.rpc.RetryInfo" {
                    return None;
                }
                seconds(detail["retryDelay"].as_str()?.strip_suffix('s')?)
            })
    });
    UpstreamFailure {
        kind,
        status: (400..=599).contains(&status).then_some(status),
        retry_after_ms,
        message: message.into(),
        code: Some(code.into()),
    }
}

fn retry_after(headers: &[(String, String)]) -> Option<u64> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| seconds(value.trim()))
}

fn seconds(value: &str) -> Option<u64> {
    let seconds: f64 = value.parse().ok()?;
    (seconds.is_finite() && (0.0..=86400.0).contains(&seconds))
        .then(|| (seconds * 1000.0).ceil() as u64)
}

fn encoding_fault() -> PluginFault {
    PluginFault::new(ErrorCode::Fault, "Invalid Gemini plugin RPC encoding")
}
