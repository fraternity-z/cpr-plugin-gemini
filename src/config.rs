//! 校验实例配置并生成仅匹配指定 Gemini 模型的上游声明

use std::collections::BTreeSet;

use gateway_plugin_sdk::call::{
    host::AuthCredential,
    upstream_adapter::{
        BuiltinProvider, UpstreamAdapterDeclaration, UpstreamAdapterRegistration,
        UpstreamAdapterRequest, UpstreamPath, UpstreamPathPurpose, UpstreamTransport,
    },
};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::error::{Error, Result};

pub(crate) const ADAPTER_ID: &str = "gemini-api";

#[derive(Clone, Copy, Default, Deserialize)]
pub(crate) enum AuthMode {
    #[default]
    #[serde(rename = "x-goog-api-key")]
    ApiKey,
    #[serde(rename = "bearer")]
    Bearer,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Config {
    pub base_url: String,
    pub models: Vec<String>,
    pub auth_mode: AuthMode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: "https://generativelanguage.googleapis.com/v1beta/".into(),
            models: vec!["gemini-2.5-flash".into(), "gemini-2.5-pro".into()],
            auth_mode: AuthMode::default(),
        }
    }
}

impl Config {
    pub fn parse(value: Value) -> Result<Self> {
        let mut config: Self = serde_json::from_value(value)
            .map_err(|_| Error::invalid("Invalid Gemini plugin configuration"))?;
        config.base_url = normalize_base_url(&config.base_url)?;
        let mut seen = BTreeSet::new();
        if config.models.is_empty()
            || config.models.len() > 64
            || config
                .models
                .iter()
                .any(|model| !valid_model(model) || !seen.insert(model))
        {
            return Err(Error::invalid(
                "Configure 1 to 64 unique model IDs without paths or wildcards",
            ));
        }
        Ok(config)
    }

    pub fn registration(&self) -> UpstreamAdapterRegistration {
        UpstreamAdapterRegistration {
            adapters: vec![UpstreamAdapterDeclaration {
                id: ADAPTER_ID.into(),
                provider: BuiltinProvider::OpenAi,
                base_url: self.base_url.clone(),
                paths: self
                    .models
                    .iter()
                    .map(|model| UpstreamPath {
                        path: inference_path(model),
                        purpose: UpstreamPathPurpose::Inference,
                    })
                    .collect(),
                authentication_kinds: vec!["api_key".into()],
                transport: UpstreamTransport::HttpSse,
                protocol: "openai".into(),
                models: self.models.clone(),
            }],
        }
    }

    pub fn validate_request(&self, request: &UpstreamAdapterRequest) -> Result<()> {
        if request.adapter_id != ADAPTER_ID
            || request.provider != BuiltinProvider::OpenAi
            || request.protocol != "openai"
            || !self.models.contains(&request.upstream_model)
        {
            return Err(Error::invalid(
                "Request does not match the Gemini adapter configuration",
            ));
        }
        if request.continuation.is_some() {
            return Err(Error::unsupported(
                "Gemini requires full conversation history; native continuation is not supported",
            ));
        }
        Ok(())
    }

    pub fn headers(
        &self,
        request: &UpstreamAdapterRequest,
        credential: AuthCredential,
    ) -> Result<Vec<(String, String)>> {
        if credential.account_id != request.account_id
            || credential.provider_id != "openai"
            || credential.credential_revision != request.credential_revision
            || credential.facts.authentication_kind != "api_key"
        {
            return Err(Error::invalid(
                "The selected Gemini API Key account or credential revision has changed",
            ));
        }
        let base_url = credential
            .facts
            .material
            .get("base_url")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::invalid("The selected account must be a Gemini URL + Key account")
            })?;
        if normalize_base_url(base_url)? != self.base_url {
            return Err(Error::invalid(
                "The selected account Base URL must match the Gemini plugin Base URL; use a dedicated account group",
            ));
        }
        let key = credential
            .facts
            .material
            .get("api_key")
            .and_then(Value::as_str)
            .filter(|key| {
                !key.is_empty()
                    && key.len() <= 16 * 1024
                    && key.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .ok_or_else(|| Error::invalid("The selected account has no valid Gemini API Key"))?;
        let mut headers = vec![
            ("content-type".into(), "application/json".into()),
            ("accept".into(), "text/event-stream".into()),
        ];
        match self.auth_mode {
            AuthMode::ApiKey => headers.extend([
                ("authorization".into(), String::new()),
                ("x-goog-api-key".into(), key.into()),
            ]),
            AuthMode::Bearer => headers.push(("authorization".into(), format!("Bearer {key}"))),
        }
        Ok(headers)
    }
}

pub(crate) fn inference_path(model: &str) -> String {
    format!("models/{model}:streamGenerateContent")
}

fn valid_model(model: &str) -> bool {
    model.len() <= 128
        && model
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn normalize_base_url(input: &str) -> Result<String> {
    if input.len() > 2048 || input.chars().any(char::is_control) {
        return Err(Error::invalid("Invalid Gemini API Base URL"));
    }
    let mut url = Url::parse(input).map_err(|_| Error::invalid("Invalid Gemini API Base URL"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::invalid(
            "Gemini Base URL requires HTTPS, no credentials, query or fragment; HTTP is only allowed on loopback",
        ));
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url.into())
}
