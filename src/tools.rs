//! 保留 Responses 工具身份并将函数、自定义工具及命名空间映射为 Gemini 函数

use std::collections::BTreeSet;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

#[derive(Clone)]
pub(crate) struct Tool {
    pub name: String,
    pub namespace: Option<String>,
    pub alias: String,
    pub custom: bool,
    declaration: Value,
}

#[derive(Clone, Default)]
pub(crate) struct Tools {
    entries: Vec<Tool>,
}

impl Tools {
    pub fn parse(input: Option<&Value>) -> Result<Self> {
        let mut tools = Self::default();
        if let Some(input) = input.filter(|value| !value.is_null()) {
            for entry in input
                .as_array()
                .ok_or_else(|| Error::invalid("tools must be an array"))?
            {
                if entry["type"] == "namespace" {
                    let namespace = string(entry, "name")?;
                    let members = entry["tools"]
                        .as_array()
                        .ok_or_else(|| Error::invalid("A tool namespace requires a tools array"))?;
                    for member in members {
                        tools.add(member, Some(namespace))?;
                    }
                } else {
                    tools.add(entry, None)?;
                }
            }
        }
        Ok(tools)
    }

    fn add(&mut self, value: &Value, namespace: Option<&str>) -> Result<()> {
        if self.entries.len() >= 128 {
            return Err(Error::invalid(
                "At most 128 function or custom tools are supported",
            ));
        }
        let custom = match value["type"].as_str() {
            Some("function") => false,
            Some("custom") => true,
            _ => {
                return Err(Error::unsupported(
                    "Only function, custom and namespace tools are supported; hosted OpenAI tools are not available",
                ));
            }
        };
        let name = string(value, "name")?;
        if name.len() > 256
            || name.chars().any(char::is_control)
            || namespace.is_some_and(|namespace| {
                namespace.len() > 256 || namespace.chars().any(char::is_control)
            })
        {
            return Err(Error::invalid("Invalid tool name or namespace"));
        }
        let alias = alias(name, namespace);
        if self.entries.iter().any(|entry| entry.alias == alias) {
            return Err(Error::invalid(
                "Tool names must be unique within their namespace",
            ));
        }
        let description =
            value
                .get("description")
                .filter(|v| !v.is_null())
                .map_or(Ok(""), |v| {
                    v.as_str()
                        .ok_or_else(|| Error::invalid("Tool description must be text"))
                })?;
        let description = if alias == name {
            description.into()
        } else {
            format!(
                "{}{}\n{description}",
                namespace.map(|v| format!("{v}.")).unwrap_or_default(),
                name
            )
        };
        let mut declaration = json!({"name":alias, "description":description});
        declaration["parametersJsonSchema"] = if custom {
            let mut input_description =
                "The complete, unmodified input for this custom tool".to_owned();
            if let Some(format) = value.get("format").filter(|v| !v.is_null()) {
                match format["type"].as_str() {
                    Some("text") => {}
                    Some("grammar") => {
                        let syntax = string(format, "syntax")?;
                        let definition = string(format, "definition")?;
                        input_description
                            .push_str(&format!(". Follow this {syntax} grammar:\n{definition}"));
                    }
                    _ => return Err(Error::unsupported("Unsupported custom tool format")),
                }
            }
            json!({"type":"object", "properties":{"input":{"type":"string", "description":input_description}}, "required":["input"], "additionalProperties":false})
        } else {
            let schema = value
                .get("parameters")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| json!({"type":"object", "properties":{}}));
            if !schema.is_object() {
                return Err(Error::invalid(
                    "Function parameters must be a JSON Schema object",
                ));
            }
            schema
        };
        self.entries.push(Tool {
            name: name.into(),
            namespace: namespace.map(str::to_owned),
            alias,
            custom,
            declaration,
        });
        Ok(())
    }

    pub fn declarations(&self) -> Option<Value> {
        (!self.entries.is_empty()).then(|| json!([{"functionDeclarations":self.entries.iter().map(|entry| &entry.declaration).collect::<Vec<_>>()}]))
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.entries.iter().find(|entry| entry.alias == name)
    }

    pub fn tool_choice(&self, value: Option<&Value>) -> Result<Option<Value>> {
        let Some(value) = value.filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let (mode, allowed) = match value.as_str() {
            Some("auto") => ("AUTO", None),
            Some("none") => ("NONE", None),
            Some("required") => ("ANY", None),
            Some(_) => return Err(Error::unsupported("Unsupported tool_choice")),
            None => {
                if !matches!(value["type"].as_str(), Some("function" | "custom")) {
                    return Err(Error::unsupported("Unsupported tool_choice selector"));
                }
                let name = string(value, "name")?;
                let namespace = optional_namespace(value)?;
                let candidate = alias(name, namespace);
                if self.get(&candidate).is_none() {
                    return Err(Error::invalid("tool_choice must select a declared tool"));
                }
                ("ANY", Some(candidate))
            }
        };
        if self.entries.is_empty() {
            return if mode == "ANY" {
                Err(Error::invalid("tool_choice requires a declared tool"))
            } else {
                Ok(None)
            };
        }
        let mut config = json!({"functionCallingConfig":{"mode":mode}});
        if let Some(name) = allowed {
            config["functionCallingConfig"]["allowedFunctionNames"] = json!([name]);
        }
        Ok(Some(config))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub(crate) fn alias(name: &str, namespace: Option<&str>) -> String {
    if namespace.is_none()
        && !name.starts_with("cpr_")
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'.' | b'-'))
    {
        return name.into();
    }
    let mut digest = Sha256::new();
    digest.update(namespace.unwrap_or_default());
    digest.update([0]);
    digest.update(name);
    format!("cpr_{:x}", digest.finalize())
}

pub(crate) fn optional_namespace(value: &Value) -> Result<Option<&str>> {
    value
        .get("namespace")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| Error::invalid("Tool namespace must be non-empty text"))
        })
        .transpose()
}

pub(crate) fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::invalid("A required text field is missing or empty"))
}

pub(crate) fn check_unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = BTreeSet::new();
    if ids.into_iter().any(|id| !seen.insert(id)) {
        return Err(Error::invalid("Duplicate tool call ID"));
    }
    Ok(())
}
