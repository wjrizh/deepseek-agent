//! 全局配置。从环境变量 / 持久化文件加载。

use crate::error::{AgentError, Result};
use std::path::PathBuf;

/// DeepSeek API 常量
pub const BASE_URL: &str = "https://chat.deepseek.com";
pub const CLIENT_VERSION: &str = "2.4.0";
pub const CLIENT_BUNDLE_ID: &str = "com.deepseek.chat";

#[derive(Debug, Clone)]
pub struct Config {
    /// 基础 URL
    pub base_url: String,
    /// 请求超时（秒）
    pub timeout_secs: u64,
/// token 持久化文件路径
pub token_file: PathBuf,
/// 会话持久化文件路径
pub session_file: PathBuf,
/// PoW WASM 文件路径
pub pow_wasm: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            base_url: BASE_URL.to_string(),
            timeout_secs: 300,
token_file: home.join(".deepseek-agent").join("token.json"),
session_file: home.join(".deepseek-agent").join("session.json"),
pow_wasm: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/wasm/sha3.wasm")),
        }
    }
}

impl Config {
    /// 从环境变量覆盖默认值
    pub fn from_env() -> Result<Self> {
        let mut cfg = Self::default();
        if let Ok(url) = std::env::var("DS_BASE_URL") {
            cfg.base_url = url;
        }
        if let Ok(t) = std::env::var("DS_TIMEOUT") {
            cfg.timeout_secs = t
                .parse()
                .map_err(|_| AgentError::Config(format!("无效的 DS_TIMEOUT: {t}")))?;
        }
        if let Ok(p) = std::env::var("DS_TOKEN_FILE") {
            cfg.token_file = PathBuf::from(p);
        }
if let Ok(p) = std::env::var("DS_POW_WASM") {
    cfg.pow_wasm = PathBuf::from(p);
}
if let Ok(p) = std::env::var("DS_SESSION_FILE") {
    cfg.session_file = PathBuf::from(p);
}
        Ok(cfg)
    }
}
