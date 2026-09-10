//! API 请求/响应 Schema（Schema 优先）。

use serde::{Deserialize, Serialize};

// ---------- 通用响应包装 ----------

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    pub code: i64,
    #[serde(default)]
    pub msg: String,
    pub data: Option<ApiData<T>>,
}

#[derive(Debug, Deserialize)]
pub struct ApiData<T> {
    #[serde(default)]
    pub biz_code: i64,
    #[serde(default)]
    pub biz_msg: String,
    pub biz_data: Option<T>,
}

// ---------- PoW 挑战 ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowChallenge {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub signature: String,
    pub difficulty: i32,
    pub expire_at: i64,
    #[serde(default)]
    pub expire_after: i64,
    #[serde(default)]
    pub target_path: String,
}

#[derive(Debug, Serialize)]
pub struct PowChallengeReq {
    pub target_path: String,
}

/// 放入 `x-ds-pow-response` 头的内容
#[derive(Debug, Serialize)]
pub struct PowResponse {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub answer: i64,
    pub signature: String,
    pub target_path: String,
}

// ---------- 文件 ----------

#[derive(Debug, Clone, Deserialize)]
pub struct FileInfo {
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub file_size: i64,
    #[serde(default)]
    pub is_image: bool,
}

// ---------- 会话 ----------

#[derive(Debug, Deserialize)]
pub struct ChatSession {
    pub id: String,
}

/// 会话创建响应的 biz_data 包装
#[derive(Debug, Deserialize)]
pub struct ChatSessionWrapper {
    pub chat_session: ChatSession,
}

// ---------- 对话 ----------

#[derive(Debug, Serialize)]
pub struct CompletionReq {
    pub chat_session_id: String,
    pub parent_message_id: Option<i64>,
    pub model_type: String,
    pub prompt: String,
    pub ref_file_ids: Vec<String>,
    pub thinking_enabled: bool,
    pub search_enabled: bool,
    pub action: Option<String>,
    pub preempt: bool,
}

impl CompletionReq {
    pub fn new(session_id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            chat_session_id: session_id.into(),
            parent_message_id: None,
            model_type: "default".into(),
            prompt: prompt.into(),
            ref_file_ids: Vec::new(),
            thinking_enabled: false,
            search_enabled: false,
            action: None,
            preempt: false,
        }
    }
}

/// 增量类型：思考 or 答案
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    /// 思考过程（THINK fragment）
    Thinking,
    /// 最终答案（RESPONSE fragment）
    Answer,
}

/// SSE 流中的单个事件
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// 就绪，携带 response_message_id（下一轮的 parent）
    Ready { response_message_id: i64 },
    /// 正文增量（带类型：思考/答案）
    Delta(DeltaKind, String),
    /// 标题
    Title(String),
    /// 结束
    Close,
    /// 其他（忽略）
    Other,
}
