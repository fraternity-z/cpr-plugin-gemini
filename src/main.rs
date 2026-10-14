//! 在标准输入输出上运行宿主 SDK 二进制 RPC 会话

use gateway_plugin_sdk::client::{PluginSession, SessionConfig};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?;
    let plugin = cpr_plugin_gemini::plugin(session.handshake().configuration.clone())?;
    session.run(plugin).await?;
    Ok(())
}
