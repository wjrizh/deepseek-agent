//! Agent 主循环 —— 编排 Model / Memory / Tool。
//!
//! 一轮 = 用户输入 → 模型回复 →（若有工具调用）执行并回注结果 → 直至纯文本回答。
//! 工具的审批由 `/free` 全局开关控制，模型无权决定。

use crate::agent::memory::{InMemory, Memory, Message, Role};
use crate::agent::model::{Model, ModelReply};
use crate::agent::parser::{self, ParseOutcome};
use crate::agent::prompt::{SYSTEM_PROMPT, TOOLS_SECTION};
use crate::agent::session_store::{SessionRecord, SessionStore};
use crate::agent::tool::{ExecuteCommand, ToolRegistry};
use crate::api::types::CompletionReq;
use crate::error::Result;
use std::sync::Arc;
use std::time::Instant;

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
    /// 是否开启联网搜索
    search: bool,
    /// 是否已注入系统提示词（仅首轮）
    prompt_sent: bool,
    /// /free 模式：自动批准所有命令
    free_mode: bool,
    /// 会话持久化
    store: SessionStore,
    /// 是否是从持久化恢复的会话（避免重复覆盖）
    restored: bool,
    /// 是否禁用本地会话恢复（/new 时临时用）
    force_new: bool,
    /// 上一次向服务器同步 fork 状态的时间戳（30s 节流）
    last_fork_check: Option<Instant>,
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
            search: false,
            prompt_sent: false,
            free_mode: false,
            store: SessionStore::default(),
            restored: false,
            force_new: false,
            last_fork_check: None,
        }
    }

    pub fn with_session_store(mut self, store: SessionStore) -> Self {
        self.store = store;
        self
    }

    pub fn set_thinking(&mut self, on: bool) {
        self.thinking = on;
    }

    pub fn set_search(&mut self, on: bool) {
        self.search = on;
    }

    pub fn thinking(&self) -> bool {
        self.thinking
    }

    pub fn search(&self) -> bool {
        self.search
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

    /// 确保会话存在：优先恢复持久化会话，否则新建。
    pub async fn ensure_session(&mut self) -> Result<&str> {
        if self.session_id.is_none() {
            if !self.force_new
                && let Some(rec) = self.store.active_record()
            {
                self.session_id = Some(rec.session_id.clone());
self.parent_message_id = rec.parent_message_id;
self.restored = true;
                let tag = rec.title.as_deref().unwrap_or("(无标题)");
                eprintln!(
                    "[session] 已恢复会话 {} · parent={:?} · {tag}",
                    &rec.session_id[..rec.session_id.len().min(8)],
                    rec.parent_message_id
                );
            } else {
                let id = self.model.new_session().await?;
                let rec = SessionRecord::new(id.clone());
                self.store.upsert_active(rec)?;
                self.session_id = Some(id);
            }
        }
        Ok(self.session_id.as_ref().unwrap())
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub async fn new_session(&mut self) -> Result<String> {
        self.memory.clear();
        self.prompt_sent = false;
        self.session_id = None;
        self.parent_message_id = None;
        self.force_new = true;
        let id = self.model.new_session().await?;
        self.force_new = false;
        let rec = SessionRecord::new(id.clone());
        self.store.upsert_active(rec)?;
        self.session_id = Some(id.clone());
        Ok(id)
    }

    pub fn switch_session(
        &mut self,
        session_id: String,
        parent: Option<i64>,
    ) -> Result<()> {
        self.memory.clear();
        self.prompt_sent = false;
        // 若传入 parent 为 None，但本地已有记录，则保留本地 parent
        let effective = parent.or_else(|| self.store.parent_of(&session_id));
        let mut rec = SessionRecord::new(session_id.clone());
        rec.parent_message_id = effective;
        self.store.activate(rec)?;
        self.session_id = Some(session_id);
self.parent_message_id = effective;
self.force_new = false;
        self.restored = false;
        Ok(())
    }

    pub fn store_contains(&self, sid: &str) -> bool {
        self.store.contains(sid)
    }

    pub fn store_parent_of(&self, sid: &str) -> Option<i64> {
        self.store.parent_of(sid)
    }

    pub fn store_remove(&mut self, sid: &str) -> Result<()> {
        self.store.remove(sid)
    }

    /// 主入口：发一轮消息（含工具循环）。
    pub async fn run(&mut self, input: &str) -> Result<ModelReply> {
        self.ensure_session().await?;

        // fork 同步：30s 节流，检测服务器上该会话的最新 mid 是否领先本地 parent
        {
            let now = Instant::now();
            let should_check = self
                .last_fork_check
                .map(|t| now.duration_since(t).as_secs() >= 30)
                .unwrap_or(true);
            if should_check {
                self.last_fork_check = Some(now);
                if let Some(sid) = self.session_id.clone()
                    && let Ok(Some(server_mid)) =
                        self.model.sync_parent(&sid, self.parent_message_id).await
                {
                    self.parent_message_id = Some(server_mid);
                    let _ = self.store.update_parent(&sid, Some(server_mid));
                }
            }
        }

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

        let mut malformed_retries = 0u32;

        loop {
            // 3. 构造请求
            let mut req = CompletionReq::new(self.session_id.clone().unwrap(), next_prompt.clone());
req.parent_message_id = self.parent_message_id;
            req.ref_file_ids = std::mem::take(&mut self.pending_files);
            if self.thinking {
                req.model_type = "expert".into();
                req.thinking_enabled = true;
            }
            req.search_enabled = self.search;

            // 4. 调模型（流式）
            let mut ui = crate::ui::AnyUi::new();
            let reply = self
                .model
                .complete(&req, &mut |kind, delta| {
                    ui.on_delta(kind, delta);
                })
                .await?;
            ui.finish();

            // 仅当拿到有效 message_id 时才推进 parent（防止异常/中断路径把 parent 污染成 None）
if reply.message_id.is_some() {
                self.parent_message_id = reply.message_id;
                if let Some(sid) = self.session_id.clone() {
                    let _ = self.store.update_parent(&sid, reply.message_id);
                }
            }
            if let Some(sid) = self.session_id.clone()
                && let Some(t) = &reply.title
            {
                let _ = self.store.update_title(&sid, t);
            }

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
                    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
                    let live = tty && self.free_mode;
                    let result = self.run_tool(&call, live).await;
                    if result.starts_with("[INTERRUPTED]") {
                        if tty {
                            println!(
                                "\n{}[已中断]{}",
                                crate::ui::DIM,
                                crate::ui::RESET
                            );
                        }
                        return Ok(reply);
                    }
                    if !tty {
                        crate::ui::print_tool_result(&result);
                    }
                    next_prompt = format!("<tool_result>\n{result}\n</tool_result>");
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
                         or a plain text answer if no tool is needed.\n\
                         Do NOT quote the literal tags <tool_calls>/<invoke>/<parameter> \
                         inside the command value."
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