//! 将 Gemini 累计 token 快照投影为宿主用量和 Responses 用量

use gateway_plugin_sdk::call::model::Usage;
use serde_json::{Value, json};

use crate::error::{Error, Result};

#[derive(Default)]
pub(crate) struct Meter {
    pub usage: Usage,
    candidates: Option<u64>,
}

impl Meter {
    pub fn update(&mut self, value: &Value) -> Result<Usage> {
        if !value.is_object() {
            return Err(Error::protocol("Invalid Gemini usage metadata"));
        }
        for (key, target) in [
            ("promptTokenCount", &mut self.usage.input_tokens),
            ("candidatesTokenCount", &mut self.candidates),
            ("thoughtsTokenCount", &mut self.usage.reasoning_tokens),
            ("cachedContentTokenCount", &mut self.usage.cached_tokens),
            ("totalTokenCount", &mut self.usage.total_tokens),
        ] {
            if let Some(value) = value.get(key) {
                *target = Some(
                    value
                        .as_u64()
                        .filter(|count| *count <= i64::MAX as u64)
                        .ok_or_else(|| Error::protocol("Invalid Gemini token count"))?,
                );
            }
        }
        if self.candidates.is_some() || self.usage.reasoning_tokens.is_some() {
            self.usage.output_tokens = Some(
                self.candidates
                    .unwrap_or_default()
                    .checked_add(self.usage.reasoning_tokens.unwrap_or_default())
                    .filter(|count| *count <= i64::MAX as u64)
                    .ok_or_else(|| Error::protocol("Gemini token count overflow"))?,
            );
        }
        if let (Some(cached), Some(input)) = (self.usage.cached_tokens, self.usage.input_tokens)
            && cached > input
        {
            return Err(Error::protocol("Gemini cached tokens exceed input tokens"));
        }
        Ok(self.usage.clone())
    }

    pub fn json(&self) -> Value {
        let (Some(input), Some(output)) = (self.usage.input_tokens, self.usage.output_tokens)
        else {
            return Value::Null;
        };
        let Some(total) = self.usage.total_tokens.or_else(|| {
            input
                .checked_add(output)
                .filter(|count| *count <= i64::MAX as u64)
        }) else {
            return Value::Null;
        };
        json!({
            "input_tokens":input,
            "input_tokens_details":self.usage.cached_tokens.map(|cached| json!({"cached_tokens":cached})),
            "output_tokens":output,
            "output_tokens_details":self.usage.reasoning_tokens.map(|reasoning| json!({"reasoning_tokens":reasoning})),
            "total_tokens":total,
        })
    }
}
