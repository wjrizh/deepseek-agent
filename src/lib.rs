//! DeepSeek Agent —— 纯 Rust 客户端 + Agent 框架。
//!
//! 模块：
//! - `config`  配置
//! - `error`   统一错误
//! - `auth`    token 自动更新 + 持久化
//! - `client`  HTTP / PoW / SSE
//! - `api`     文件上传 / 会话 / 对话
//! - `agent`   Agent 主循环 + 扩展点（Model/Memory/Tool）

pub mod agent;
pub mod api;
pub mod auth;
pub mod cli;
pub mod client;
pub mod config;
pub mod error;
pub mod ui;

pub use agent::Agent;
pub use config::Config;
pub use error::{AgentError, Result};

use client::http::HttpClient;
use client::pow::PowSolver;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 便捷构造：一次性建好 HttpClient + PowSolver
pub struct Runtime {
    pub http: Arc<HttpClient>,
    pub solver: Arc<Mutex<PowSolver>>,
}

impl Runtime {
    pub async fn new(cfg: &Config) -> Result<Self> {
        let mut tokens = auth::TokenProvider::new(&cfg.token_file);
        tokens.load_or_refresh(&cfg.base_url).await?;
        let http = Arc::new(HttpClient::new(
            &cfg.base_url,
            cfg.timeout_secs,
            tokens,
        )?);
        let solver = Arc::new(Mutex::new(PowSolver::from_file(&cfg.pow_wasm)?));
        Ok(Self { http, solver })
    }

    /// 构造 DeepSeek model
    pub fn model(&self) -> Arc<dyn agent::model::Model> {
        Arc::new(agent::model::DeepSeekModel::new(
            self.http.clone(),
            self.solver.clone(),
        ))
    }
}