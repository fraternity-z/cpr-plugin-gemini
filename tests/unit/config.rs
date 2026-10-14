//! 验证插件声明、账号版本、目标地址隔离与错误脱敏

use gateway_plugin_sdk::{
    Manifest,
    call::{
        host::{AuthCredential, CredentialFacts},
        upstream_adapter::{BuiltinProvider, UpstreamAdapterRequest, UpstreamFailureKind},
    },
};
use serde_json::{Value, json};

use crate::{adapter::http_failure, config::Config};

fn request() -> UpstreamAdapterRequest {
    UpstreamAdapterRequest {
        adapter_id: "gemini-api".into(),
        provider: BuiltinProvider::OpenAi,
        upstream_model: "gemini-2.5-flash".into(),
        client_key_id: "key_test".into(),
        account_id: "acct_gemini".into(),
        credential_revision: 2,
        protocol: "openai".into(),
        client_transport: "http".into(),
        fast_mode: "default".into(),
        headers: vec![],
        continuation: None,
    }
}

fn credential(base_url: &str) -> AuthCredential {
    AuthCredential {
        account_id:"acct_gemini".into(), provider_id:"openai".into(), credential_revision:2,
        facts:CredentialFacts {
            name:"Gemini fixture".into(), authentication_kind:"api_key".into(),
            material:json!({"schema_version":1,"installation_id":"fixture","api_key":"fixture-secret-key","base_url":base_url,"transport":"http_sse"}).as_object().unwrap().clone(),
            email:None, upstream_user_id:None, upstream_account_id:None, plan_type:None, has_refresh_token:false,
            access_token_expires_at_ms:None, next_refresh_at_ms:None,
        },
    }
}

#[test]
fn author_manifest_and_handlers_match_sdk_v2() {
    let manifest = Manifest::from_author_slice(include_bytes!("../../plugin.json")).unwrap();
    assert_eq!(manifest.plugin_id().unwrap(), "cpr-community.gemini");
    assert!(crate::plugin(json!({})).is_ok());
    let registration = Config::parse(json!({})).unwrap().registration();
    assert_eq!(registration.adapters.len(), 1);
    assert_eq!(
        registration.adapters[0].authentication_kinds,
        vec!["api_key"]
    );
    assert!(!registration.adapters[0].models.is_empty());
    assert!(
        !registration.adapters[0]
            .models
            .contains(&"gpt-5.3-codex".into())
    );
}

#[test]
fn selected_key_is_sent_only_as_google_header_and_default_bearer_is_overridden() {
    let config = Config::parse(json!({})).unwrap();
    let headers = config
        .headers(
            &request(),
            credential("https://generativelanguage.googleapis.com/v1beta"),
        )
        .unwrap();
    assert!(headers.contains(&("x-goog-api-key".into(), "fixture-secret-key".into())));
    assert!(headers.contains(&("authorization".into(), String::new())));
}

#[test]
fn ordinary_openai_credentials_and_rotated_keys_are_rejected_before_network_access() {
    let config = Config::parse(json!({})).unwrap();
    let error = config
        .headers(&request(), credential("https://api.openai.com/v1/"))
        .err()
        .unwrap();
    assert!(!error.to_string().contains("fixture-secret-key"));
    let mut rotated = credential(&config.base_url);
    rotated.credential_revision = 3;
    assert!(config.headers(&request(), rotated).is_err());
    let mut other = credential(&config.base_url);
    other.account_id = "acct_other".into();
    assert!(config.headers(&request(), other).is_err());
}

#[test]
fn configuration_rejects_wildcards_credentials_in_urls_and_non_tls_remote_hosts() {
    for value in [
        json!({"models":[]}),
        json!({"models":["gemini-*"]}),
        json!({"models":["gemini-x","gemini-x"]}),
        json!({"models":["."]}),
        json!({"models":["gemini-x/../../elsewhere"]}),
        json!({"baseUrl":"http://example.com/v1beta/"}),
        json!({"baseUrl":"https://user:secret@example.com/"}),
        json!({"baseUrl":"https://example.com/?key=secret"}),
        json!({"baseUrl":"https://example.com/#fragment"}),
    ] {
        assert!(Config::parse(value).is_err());
    }
    assert!(Config::parse(json!({"baseUrl":"http://127.0.0.1:8080/v1beta/"})).is_ok());
}

#[test]
fn third_party_urls_model_ids_and_bearer_auth_are_configurable() {
    let base = "https://gateway.example.com/native/google/v1beta/";
    let config =
        Config::parse(json!({"baseUrl":base,"models":["team-flash"],"authMode":"bearer"})).unwrap();
    let registration = config.registration();
    assert_eq!(registration.adapters[0].base_url, base);
    assert_eq!(
        registration.adapters[0].paths[0].path,
        "models/team-flash:streamGenerateContent"
    );
    let mut request = request();
    request.upstream_model = "team-flash".into();
    config.validate_request(&request).unwrap();
    let headers = config.headers(&request, credential(base)).unwrap();
    assert!(headers.contains(&("authorization".into(), "Bearer fixture-secret-key".into())));
    assert!(!headers.iter().any(|(name, _)| name == "x-goog-api-key"));
}

#[test]
fn third_party_google_header_mode_does_not_require_a_google_domain() {
    let base = "https://gateway.example.com/custom-prefix/v1beta/";
    let config = Config::parse(json!({"baseUrl":base,"authMode":"x-goog-api-key"})).unwrap();
    let headers = config.headers(&request(), credential(base)).unwrap();
    assert!(headers.contains(&("x-goog-api-key".into(), "fixture-secret-key".into())));
    assert_eq!(config.registration().adapters[0].base_url, base);
}

#[test]
fn upstream_errors_keep_status_and_backoff_but_not_raw_messages() {
    let body = json!({"error":{"message":"invalid fixture-secret-key", "details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"1.5s"}]}});
    let error = http_failure(429, Some(&body), None);
    assert_eq!(error.kind, UpstreamFailureKind::RateLimited);
    assert_eq!(error.retry_after_ms, Some(1500));
    assert_eq!(error.status, Some(429));
    let serialized: Value = serde_json::to_value(&error).unwrap();
    assert!(!serialized.to_string().contains("fixture-secret-key"));
    assert_eq!(
        http_failure(403, None, None).kind,
        UpstreamFailureKind::PermissionDenied
    );
    assert!(http_failure(302, None, None).status.is_none());
}
