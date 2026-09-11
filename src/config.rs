//! 全局配置。从环境变量 / 持久化文件加载。

use crate::error::{AgentError, Result};
use std::path::PathBuf;

/// DeepSeek API 常量
pub const BASE_URL: &str = "https://chat.deepseek.com";
pub const CLIENT_VERSION: &str = "2.4.0";
pub const CLIENT_BUNDLE_ID: &str = "com.deepseek.chat";
/// 浏览器 UA（与 Sec-Ch-Ua 保持一致，模拟 Windows Chrome 133）
pub const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36";
/// 页面版本号（X-App-Version），失败时回退
pub const APP_VERSION: &str = "20250226.1";

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
    /// 当前账号名（None = 单账号旧模式）
    pub account: Option<String>,
    /// 账号专属 Chromium profile 目录（None = 单账号默认 profile）
    pub browser_profile: Option<PathBuf>,
    /// 对话达到该 message 数时自动交接（生成报告 + 开新会话）
    pub compact_threshold: u64,
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
            account: None,
            browser_profile: None,
            compact_threshold: 300,
        }
    }
}

impl Config {
    /// 按账号构造（多账号模式）
    pub fn for_account(name: &str) -> Result<Self> {
        let mgr = crate::accounts::AccountManager::load_default()?;
        Ok(Self {
            account: Some(name.to_string()),
            token_file: mgr.token_file(name),
            session_file: mgr.session_file(name),
            browser_profile: Some(mgr.browser_profile(name)),
            ..Self::default()
        })
    }

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
        if let Ok(t) = std::env::var("DS_COMPACT_THRESHOLD") {
            cfg.compact_threshold = t
                .parse()
                .map_err(|_| AgentError::Config(format!("无效的 DS_COMPACT_THRESHOLD: {t}")))?;
        }
        Ok(cfg)
    }
}