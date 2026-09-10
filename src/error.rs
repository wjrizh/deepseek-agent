//! 统一错误类型。所有模块返回 `Result<T, AgentError>`。

use thiserror::Error;

pub type Result<T> = std::result::Result<T, AgentError>;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("HTTP 请求失败: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON 解析失败: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("认证失败: {0}")]
    Auth(String),

    #[error("API 返回错误 (code={code}): {msg}")]
    Api { code: i64, msg: String },

    #[error("PoW 计算失败: {0}")]
    Pow(String),

    #[error("WASM 错误: {0}")]
    Wasm(String),

    #[error("配置错误: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}
