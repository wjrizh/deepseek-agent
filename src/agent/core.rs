//! Agent 主循环 —— 编排 Model / Memory / Tool。
//!
//! 设计为可扩展：当前一轮 = 用户输入 -> 模型回复 -> 存储；
//! 未来可在循环中加入 tool-call 检测与回流。

use crate::agent::memory::{InMemory, Memory, Message, Role};
use crate::agent::model::{Model, ModelReply};
use crate::agent::tool::ToolRegistry;
use crate::api::types::CompletionReq;
use crate::error::Result;
use std::sync::Arc;

pub struct Agent {
    model: Arc<dyn Model>,
    memory: Box<dyn Memory>,
    tools: ToolRegistry,
    session_id: Option<String>,
    /// 上一轮回复的消息 id（续接上下文用）
    parent_message_id: Option<i64>,
    /// 待引用文件 id
    pending_files: Vec<String>,
    /// 是否开启思考模式（expert + thinking）
    thinking: bool,
}

impl Agent {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            memory: Box::new(InMemory::new()),
            tools: ToolRegistry::new(),
            session_id: None,
            parent_message_id: None,
            pending_files: Vec::new(),
            thinking: false,
        }
    }

    pub fn set_thinking(&mut self, on: bool) {
        self.thinking = on;
    }

    /// 替换记忆实现（扩展点）
    pub fn with_memory(mut self, memory: Box<dyn Memory>) -> Self {
        self.memory = memory;
        self
    }

    /// 注册工具（扩展点）
    pub fn register_tool(&mut self, tool: Box<dyn crate::agent::tool::Tool>) {
        self.tools.register(tool);
    }

    /// 附加文件 id 到下一次请求
    pub fn attach_file(&mut self, file_id: impl Into<String>) {
        self.pending_files.push(file_id.into());
    }

    /// 确保会话存在
    pub async fn ensure_session(&mut self) -> Result<&str> {
        if self.session_id.is_none() {
            self.session_id = Some(self.model.new_session().await?);
        }
        Ok(self.session_id.as_ref().unwrap())
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// 主入口：发一轮消息
    pub async fn run(&mut self, input: &str) -> Result<ModelReply> {
        self.ensure_session().await?;

        // 1. 记录用户消息
        self.memory.push(Message {
            role: Role::User,
            content: input.to_string(),
        });

        // 2. 构造请求
        let mut req = CompletionReq::new(self.session_id.clone().unwrap(), input);
        req.parent_message_id = self.parent_message_id;
        req.ref_file_ids = std::mem::take(&mut self.pending_files);
        if self.thinking {
            req.model_type = "expert".into();
            req.thinking_enabled = true;
        }

        // 3. 调模型（流式）
        // 流式回调：通过 ui 渲染（思考走备用屏幕，答案走主屏幕）
        let mut ui = crate::ui::AnyUi::new();
        let reply = self
            .model
            .complete(&req, &mut |kind, delta| {
                ui.on_delta(kind, delta);
            })
            .await?;
        ui.finish();

        // 4. 更新上下文指针 + 记忆
        self.parent_message_id = reply.message_id;
        self.memory.push(Message {
            role: Role::Assistant,
            content: reply.content.clone(),
        });

        // 5. [扩展点] 工具调用检测
        //    当前 DeepSeek web 端无 function-call；未来在此解析 reply.content，
        //    若命中 tool call 则 self.tools.get(name).call(input)，再回流给模型。
        let _ = &self.tools;

        Ok(reply)
    }

    pub fn clear_memory(&mut self) {
        self.memory.clear();
    }

    pub fn history(&self) -> &[Message] {
        self.memory.history()
    }
}