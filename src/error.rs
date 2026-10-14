//! 对请求、上游协议与宿主回调错误进行不含敏感正文的分类

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::upstream_adapter::{UpstreamFailure, UpstreamFailureKind},
};

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub(crate) struct Error {
    pub kind: UpstreamFailureKind,
    pub message: &'static str,
}

impl Error {
    pub const fn invalid(message: &'static str) -> Self {
        Self {
            kind: UpstreamFailureKind::InvalidRequest,
            message,
        }
    }

    pub const fn unsupported(message: &'static str) -> Self {
        Self {
            kind: UpstreamFailureKind::Unsupported,
            message,
        }
    }

    pub const fn protocol(message: &'static str) -> Self {
        Self {
            kind: UpstreamFailureKind::Protocol,
            message,
        }
    }

    pub fn failure(self) -> UpstreamFailure {
        UpstreamFailure {
            kind: self.kind,
            status: None,
            retry_after_ms: None,
            message: self.message.into(),
            code: Some(
                match self.kind {
                    UpstreamFailureKind::InvalidRequest => "gemini_invalid_request",
                    UpstreamFailureKind::Unsupported => "gemini_unsupported",
                    _ => "gemini_protocol_error",
                }
                .into(),
            ),
        }
    }
}

impl From<Error> for PluginFault {
    fn from(error: Error) -> Self {
        Self::new(
            match error.kind {
                UpstreamFailureKind::InvalidRequest => ErrorCode::InvalidInput,
                UpstreamFailureKind::Unsupported => ErrorCode::Unsupported,
                _ => ErrorCode::Upstream,
            },
            error.message,
        )
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for Error {}
