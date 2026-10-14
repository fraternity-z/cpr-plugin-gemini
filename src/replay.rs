//! 通过 Responses reasoning 的不透明字段保留 Gemini 原始签名分段

use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

const PREFIX: &str = "cpr_gemini_v1:";
const MAX_REPLAY_BYTES: usize = 256 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplayItem {
    pub key: String,
    pub parts: Vec<Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    model: String,
    items: Vec<ReplayItem>,
}

#[derive(Default)]
pub(crate) struct Replay {
    items: BTreeMap<String, Vec<Value>>,
}

impl Replay {
    pub fn parse(input: &[Value], model: &str) -> Result<Self> {
        let mut replay = Self::default();
        for item in input.iter().filter(|item| item["type"] == "reasoning") {
            let Some(encoded) = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .and_then(|value| value.strip_prefix(PREFIX))
            else {
                continue;
            };
            if encoded.len() > MAX_REPLAY_BYTES * 2 {
                return Err(Error::invalid(
                    "Gemini signature state exceeds its size limit",
                ));
            }
            let data = STANDARD
                .decode(encoded)
                .map_err(|_| Error::invalid("Invalid Gemini signature state"))?;
            if data.len() > MAX_REPLAY_BYTES {
                return Err(Error::invalid(
                    "Gemini signature state exceeds its size limit",
                ));
            }
            let envelope: Envelope = serde_json::from_slice(&data)
                .map_err(|_| Error::invalid("Invalid Gemini signature state"))?;
            if envelope.version != 1 || envelope.model != model {
                return Err(Error::invalid(
                    "Gemini signature state belongs to a different model or version",
                ));
            }
            for entry in envelope.items {
                if entry.parts.is_empty()
                    || entry.key.len() > 600
                    || replay.items.len() >= 4096
                    || entry.parts.iter().any(|part| !valid_part(part))
                    || replay.items.insert(entry.key, entry.parts).is_some()
                {
                    return Err(Error::invalid(
                        "Invalid or duplicate Gemini signature state item",
                    ));
                }
            }
        }
        Ok(replay)
    }

    pub fn get(&self, key: &str) -> Option<&[Value]> {
        self.items.get(key).map(Vec::as_slice)
    }
}

pub(crate) fn encode(model: &str, items: Vec<ReplayItem>) -> Result<Option<String>> {
    if items.is_empty() {
        return Ok(None);
    }
    let envelope = Envelope {
        version: 1,
        model: model.into(),
        items,
    };
    let bytes = serde_json::to_vec(&envelope)
        .map_err(|_| Error::protocol("Could not encode Gemini signature state"))?;
    if bytes.len() > MAX_REPLAY_BYTES {
        return Err(Error::protocol(
            "Gemini signature state exceeds its size limit",
        ));
    }
    Ok(Some(format!("{PREFIX}{}", STANDARD.encode(bytes))))
}

pub(crate) fn signed(parts: &[Value]) -> bool {
    parts.iter().any(|part| {
        part.get("thoughtSignature")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    })
}

fn valid_part(part: &Value) -> bool {
    let Some(object) = part.as_object() else {
        return false;
    };
    !object.is_empty()
        && object.keys().all(|key| {
            matches!(
                key.as_str(),
                "text" | "thought" | "thoughtSignature" | "functionCall"
            )
        })
        && object.get("text").is_none_or(Value::is_string)
        && object.get("thought").is_none_or(Value::is_boolean)
        && object.get("thoughtSignature").is_none_or(Value::is_string)
        && object.get("functionCall").is_none_or(Value::is_object)
}
