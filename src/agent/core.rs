//! Agent 主循环 —— 编排 Model / Memory / Tool。
//!
//! 一轮 = 用户输入 → 模型回复 →（若有工具调用）执行并回注结果 → 直至纯文本回答。
//! 工具的审批由 `/free` 全局开关控制，模型无权决定。

use crate::agent::memory::{InMemory, Memory, Message, Role};
use crate::agent::model::{Model, ModelReply};
use crate::agent::parser::{self, ParseOutcome};
use crate::agent::prompt::{SYSTEM_PROMPT, TOOLS_SECTION};
use crate::agent::tool::{ExecuteCommand, ToolRegistry};
use crate::api::types::CompletionReq;
use crate::error::Result;
use std::sync::Arc;

/// 单个用户请求内允许的最大工具调用轮数（防死循环）。
const MAX_TOOL_ROUNDS: usize = 12;
/// 解析失败的最大重试次数。
const MAX_MALFORMED_RETRIES: u32 = 2;

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
    /// 是否已注入系统提示词（仅首轮）
    prompt_sent: bool,
    /// /free 模式：自动批准所有命令
    free_mode: bool,
}

impl Agent {
    pub fn new(model: Arc<dyn Model>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(ExecuteCommand::new(cwd)));
        Self {
            model,
            memory: Box::new(InMemory::new()),
            tools,
            session_id: None,
            parent_message_id: None,
            pending_files: Vec::new(),
            thinking: false,
            prompt_sent: false,
            free_mode: false,
        }
    }

    pub fn set_thinking(&mut self, on: bool) {
        self.thinking = on;
    }

    /// 切换 /free 模式（自动批准所有命令）。
    pub fn set_free_mode(&mut self, on: bool) {
        self.free_mode = on;
    }

    pub fn free_mode(&self) -> bool {
        self.free_mode
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

    /// 主入口：发一轮消息（含工具循环）。
    pub async fn run(&mut self, input: &str) -> Result<ModelReply> {
        self.ensure_session().await?;

        // 1. 记录用户消息
        self.memory.push(Message {
            role: Role::User,
            content: input.to_string(),
        });

        // 2. 首轮注入系统提示词 + 工具说明
        let mut next_prompt = if !self.prompt_sent {
            self.prompt_sent = true;
            format!("{SYSTEM_PROMPT}\n\n{TOOLS_SECTION}\n\n{input}")
        } else {
            input.to_string()
        };

        let mut tool_rounds = 0usize;
        let mut malformed_retries = 0u32;

        loop {
            // 3. 构造请求
            let mut req =
                CompletionReq::new(self.session_id.clone().unwrap(), next_prompt.clone());
            req.parent_message_id = self.parent_message_id;
            req.ref_file_ids = std::mem::take(&mut self.pending_files);
            if self.thinking {
                req.model_type = "expert".into();
                req.thinking_enabled = true;
            }

            // 4. 调模型（流式）
            let mut ui = crate::ui::AnyUi::new();
            let reply = self
                .model
                .complete(&req, &mut |kind, delta| {
                    ui.on_delta(kind, delta);
                })
                .await?;
            ui.finish();
            self.parent_message_id = reply.message_id;

// 5. 解析输出
match parser::parse(&reply.content) {
                ParseOutcome::Text(_) => {
                    self.memory.push(Message {
                        role: Role::Assistant,
                        content: reply.content.clone(),
                    });
                    return Ok(reply);
                }
                ParseOutcome::Call(call) => {
                    malformed_retries = 0;
                    tool_rounds += 1;
                    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
                    let result = self.run_tool(&call, tty).await;
                    if tty {
                        crate::ui::print_tool_done();
                    } else {
                        crate::ui::print_tool_result(&result);
                    }
                    next_prompt = format!("<tool_result>\n{result}\n</tool_result>");

                    if tool_rounds >= MAX_TOOL_ROUNDS {
                        let msg = "[已达工具调用轮数上限，停止]";
                        crate::ui::print_tool_result(msg);
                        self.memory.push(Message {
                            role: Role::Assistant,
                            content: reply.content.clone(),
                        });
                        return Ok(reply);
                    }
                }
                ParseOutcome::Malformed { raw, reason } => {
                    malformed_retries += 1;
                    if malformed_retries > MAX_MALFORMED_RETRIES {
                        // 降级：当作普通回答返回，避免卡死
                        self.memory.push(Message {
                            role: Role::Assistant,
                            content: reply.content.clone(),
                        });
                        return Ok(reply);
                    }
                    next_prompt = format!(
                        "Your previous reply could not be parsed as a tool call.\n\
                         Error: {reason}\n\
                         Your output was:\n{raw}\n\n\
                         Reply with EXACTLY ONE valid <execute_command> tool call, \
                         or a plain text answer if no tool is needed."
                    );
                }
            }
        }
    }

    /// 执行工具调用（流式输出），返回结果文本（含审批）。
    async fn run_tool(&self, call: &parser::ToolCall, live: bool) -> String {
        let cmd = call.params.get("command").map(|s| s.as_str()).unwrap_or("");

        // 审批：非 /free 模式需用户确认
        if !self.free_mode {
            match crate::ui::confirm_command(cmd) {
                Ok(true) => {}
                Ok(false) => return "[用户拒绝执行该命令]".to_string(),
                Err(_) => return "[确认失败，已跳过]".to_string(),
            }
        }

        match self.tools.get(&call.name) {
            Some(tool) => {
                let mut sink = |chunk: &str| {
                    if live {
                        print!("{chunk}");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                };
                match tool.call(&call.params, live, &mut sink).await {
                    Ok(out) => out,
                    Err(e) => format!("[工具错误] {e}"),
                }
            }
            None => format!("[未知工具] {}", call.name),
        }
    }

    pub fn clear_memory(&mut self) {
        self.memory.clear();
    }

    pub fn history(&self) -> &[Message] {
        self.memory.history()
    }
}