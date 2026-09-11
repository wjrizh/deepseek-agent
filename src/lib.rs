//! DeepSeek Agent —— 纯 Rust 客户端 + Agent 框架。
//!
//! 模块：
//! - `config`  配置
//! - `error`   统一错误
//! - `auth`    token 自动更新 + 持久化
//! - `client`  HTTP / PoW / SSE
//! - `api`     文件上传 / 会话 / 对话
//! - `agent`   Agent 主循环 + 扩展点（Model/Memory/Tool）

pub mod accounts;
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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub const PHASE_IDLE: u8 = 0;
pub const PHASE_MODEL: u8 = 1;

/// 全局中断状态：实现「第一次 Ctrl+C 标记中断，第二次 Ctrl+C(3s 内) 退出」语义。
pub struct InterruptState {
    last_interrupt: StdMutex<Option<Instant>>,
    pub phase: AtomicU8,
    pub cancel: StdMutex<CancellationToken>,
}

impl InterruptState {
    pub fn new() -> Self {
        Self {
            last_interrupt: StdMutex::new(None),
            phase: AtomicU8::new(PHASE_IDLE),
            cancel: StdMutex::new(CancellationToken::new()),
        }
    }

    pub fn mark_phase(&self, p: u8) {
        self.phase.store(p, Ordering::Relaxed);
    }

    /// 记录一次 Ctrl+C。若距上次 < 3s 返回 true（应退出）。
    pub fn check_double_and_record(&self) -> bool {
        let mut last = self.last_interrupt.lock().unwrap();
        let now = Instant::now();
        if let Some(t) = *last
            && now.duration_since(t).as_secs() < 3 {
                *last = Some(now);
                return true;
            }
        *last = Some(now);
        false
    }
}

impl Default for InterruptState {
    fn default() -> Self {
        Self::new()
    }
}

/// 便捷构造：一次性建好 HttpClient + PowSolver
pub struct Runtime {
    pub http: Arc<HttpClient>,
    pub solver: Arc<Mutex<PowSolver>>,
    pub interrupt: Arc<InterruptState>,
}

impl Runtime {
    pub async fn new(cfg: &Config) -> Result<Self> {
        let mut tokens = match &cfg.browser_profile {
            Some(profile) => auth::TokenProvider::with_profile(&cfg.token_file, profile),
            None => auth::TokenProvider::new(&cfg.token_file),
        };
        tokens.load_or_refresh(&cfg.base_url).await?;
        let http = Arc::new(HttpClient::new(&cfg.base_url, cfg.timeout_secs, tokens)?);
        let solver = Arc::new(Mutex::new(PowSolver::from_file(&cfg.pow_wasm)?));
        let interrupt = Arc::new(InterruptState::new());
        Ok(Self { http, solver, interrupt })
    }

    /// 构造 DeepSeek model
    pub fn model(&self) -> Arc<dyn agent::model::Model> {
        Arc::new(agent::model::DeepSeekModel::new(
            self.http.clone(),
            self.solver.clone(),
        ))
    }
}