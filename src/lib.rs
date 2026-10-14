//! Gemini 插件的协议转换与 SDK 处理器入口

mod adapter;
mod config;
mod error;
mod replay;
mod request;
mod response;
mod sse;
mod tools;
mod usage;

use std::sync::Arc;

use gateway_plugin_sdk::client::{ComposedPlugin, PluginBuilder, TypedReply, methods};
use serde_json::Value;

pub fn plugin(configuration: Value) -> Result<ComposedPlugin, Box<dyn std::error::Error>> {
    let config = Arc::new(config::Config::parse(configuration)?);
    let registration = config.registration();
    Ok(PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .on(methods::UPSTREAM_ADAPTER_REGISTER, move |_| {
            let registration = registration.clone();
            async move { Ok(TypedReply::new(registration)) }
        })?
        .on(methods::UPSTREAM_ADAPTER_EXECUTE, move |call| {
            let config = Arc::clone(&config);
            async move { adapter::execute(&config, call).await }
        })?
        .build()?)
}

#[cfg(test)]
#[path = "../tests/unit/mod.rs"]
mod tests;
