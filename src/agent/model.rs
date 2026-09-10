//! Model trait —— LLM 后端抽象（扩展点）。
//!
//! 现在实现 `DeepSeekModel`；未来可加 `OpenAIModel`、`LocalModel` 等。

use crate::api::types::{CompletionReq, DeltaKind};
use crate::client::http::HttpClient;
use crate::client::pow::PowSolver;
use crate::error::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 模型回复
#[derive(Debug, Clone)]
pub struct ModelReply {
    pub content: String,
    /// 本轮回复的消息 id，用于下一轮 parent_message_id
    pub message_id: Option<i64>,
}

#[async_trait]
pub trait Model: Send + Sync {
    /// 发一轮消息。`on_delta` 用于流式回调。
    async fn complete(
        &self,
        req: &CompletionReq,
        on_delta: &mut (dyn for<'a> FnMut(DeltaKind, &'a str) + Send),
    ) -> Result<ModelReply>;

    /// 新建对话会话
    async fn new_session(&self) -> Result<String>;
}

/// DeepSeek 后端实现
pub struct DeepSeekModel {
    http: Arc<HttpClient>,
    solver: Arc<Mutex<PowSolver>>,
}

impl DeepSeekModel {
    pub fn new(http: Arc<HttpClient>, solver: Arc<Mutex<PowSolver>>) -> Self {
        Self { http, solver }
    }
}

#[async_trait]
impl Model for DeepSeekModel {
    async fn complete(
        &self,
        req: &CompletionReq,
        on_delta: &mut (dyn for<'a> FnMut(DeltaKind, &'a str) + Send),
    ) -> Result<ModelReply> {
        let mut solver = self.solver.lock().await;
        let (content, message_id) =
            crate::api::chat::completion(&self.http, &mut solver, req, on_delta).await?;
        Ok(ModelReply { content, message_id })
    }

    async fn new_session(&self) -> Result<String> {
        crate::api::chat::create_session(&self.http).await
    }
}
