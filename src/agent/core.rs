//! Agent 主循环 —— 编排 Model / Memory / Tool。
//!
//! 一轮 = 用户输入 → 模型回复 →（若有工具调用）执行并回注结果 → 直至纯文本回答。
//! 工具的审批由 `/free` 全局开关控制，模型无权决定。

use crate::agent::memory::{InMemory, Memory, Message, Role};
use crate::agent::model::{Model, ModelReply};
use crate::agent::parser::{self, ParseOutcome};
use crate::agent::prompt::{SYSTEM_PROMPT, TOOLS_SECTION, NO_INSTRUCTION_PROMPT, DONE_SIGNAL};
use crate::agent::session_store::{SessionRecord, SessionStore};
use crate::agent::tool::{ExecuteCommand, ToolRegistry};
use crate::api::types::CompletionReq;
use crate::error::Result;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// （已移除重试上限，改由"未检测到指令"提示驱动）

/// 每次向模型发送内容前的随机延迟（7-11 秒），防误触并降低风控触发。
/// 可被 cancel 打断。
async fn send_delay(cancel: &CancellationToken) {
    let ms = rand::random_range(7000..=11000u64);
    let token = cancel.clone();
    let bar = tokio::task::spawn_blocking(move || crate::ui::send_delay_bar(ms, &token));
    tokio::select! {
        _ = bar => {}
        _ = cancel.cancelled() => {}
    }
}

const REPORT_PROMPT: &str = "请对本次对话做一份工作交接报告，用中文输出，包含以下部分：\n1. 任务目标\n2. 已完成的工作与产出\n3. 积累的经验/结论\n4. 尚未解决的问题\n5. 接下来要解决的问题\n只输出报告本身，不要客套。";

const HANDOFF_INTRO: &str = "以下是从上一会话交接过来的工作进度报告。请先确认收到这份报告，然后我们继续。\n\n===== 交接报告开始 =====\n";

/// `run_with_cancel` 的返回值：区分正常完成和被用户中断丢弃。
#[derive(Debug)]
pub enum RunOutcome {
    Done(ModelReply),
    /// 用户 Ctrl+C 标记；本轮输出照常跑完，但结果整轮丢弃。
    Interrupted,
}

pub struct Agent {
    model: Arc<dyn Model>,
    memory: Box<dyn Memory>,
    tools: ToolRegistry,
    session_id: Option<String>,
    /// 上一轮回复的消息 id（续接上下文用）
    parent_message_id: Option<i64>,
    /// 待引用文件 id
    pending_files: Arc<Mutex<Vec<String>>>,
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
            pending_files: Arc::new(Mutex::new(Vec::new())),
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

    /// 取得待上传文件队列的共享句柄（供 upload_file 工具写入）。
    pub fn pending_handle(&self) -> Arc<Mutex<Vec<String>>> {
        self.pending_files.clone()
    }

    /// 注册工具（扩展点）
    pub fn register_tool(&mut self, tool: Box<dyn crate::agent::tool::Tool>) {
        self.tools.register(tool);
    }

    /// 附加文件 id 到下一次请求
    pub fn attach_file(&self, file_id: impl Into<String>) {
        if let Ok(mut p) = self.pending_files.lock() {
            p.push(file_id.into());
        }
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

    /// 刷新当前会话续接点（等价 /switch <id> tree 选最新节点）。
    /// 成功则更新本地 parent 并落盘；失败静默保留原 parent。
    pub async fn refresh_current_session(&mut self) -> Result<()> {
        if let Some(sid) = self.session_id.clone()
            && let Ok(Some(mid)) = self.model.sync_parent(&sid, self.parent_message_id).await
        {
            self.parent_message_id = Some(mid);
            let _ = self.store.update_parent(&sid, Some(mid));
        }
        Ok(())
    }

    pub async fn message_count(&self) -> Option<i64> {
        let sid = self.session_id.as_ref()?;
        self.model.message_count(sid).await.ok().flatten()
    }

    pub async fn generate_report(&mut self, cancel: CancellationToken) -> Result<String> {
        let outcome = self.run_with_cancel(REPORT_PROMPT, cancel).await?;
        match outcome {
            RunOutcome::Done(reply) => Ok(reply.content),
            RunOutcome::Interrupted => Err(crate::error::AgentError::Other("[已中断]".into())),
        }
    }

    pub async fn start_with_handoff(
        &mut self,
        report: &str,
        user_input: &str,
        cancel: CancellationToken,
    ) -> Result<RunOutcome> {
        self.new_session().await?;
        let handoff = format!("{HANDOFF_INTRO}{report}\n===== 交接报告结束 =====");
        self.run_with_cancel(&handoff, cancel.clone()).await?;
        if cancel.is_cancelled() {
            return Ok(RunOutcome::Interrupted);
        }
        self.run_with_cancel(user_input, cancel).await
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
    ///
    /// `cancel` 被触发时 **不掐断 SSE 流**——让这一轮输出照常完整打印，
    /// 等 `ui.finish()` 之后才检查取消标志，若已取消则丢弃整轮结果
    /// （不推 parent / 不写 memory / 不执行后续工具调用）。
    pub async fn run_with_cancel(
        &mut self,
        input: &str,
        cancel: CancellationToken,
    ) -> Result<RunOutcome> {
        self.ensure_session().await?;

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

        self.memory.push(Message {
            role: Role::User,
            content: input.to_string(),
        });

        let mut next_prompt = if !self.prompt_sent {
            self.prompt_sent = true;
            format!("{SYSTEM_PROMPT}\n\n{TOOLS_SECTION}\n\n{input}")
        } else {
            input.to_string()
        };

loop {
            let mut req =
                CompletionReq::new(self.session_id.clone().unwrap(), next_prompt.clone());
            req.parent_message_id = self.parent_message_id;
            req.ref_file_ids = self
                .pending_files
                .lock()
                .map(|mut p| std::mem::take(&mut *p))
                .unwrap_or_default();
            if self.thinking {
                req.model_type = "expert".into();
                req.thinking_enabled = true;
            }
            req.search_enabled = self.search;

            send_delay(&cancel).await;
            if cancel.is_cancelled() {
                return Ok(RunOutcome::Interrupted);
            }

            let mut ui = crate::ui::AnyUi::new();
            let reply = match self
                .complete_with_retry(&req, &cancel, &mut |kind, delta| {
                    ui.on_delta(kind, delta);
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    ui.finish();
                    return Err(e);
                }
            };
            ui.finish();

            if cancel.is_cancelled() {
                return Ok(RunOutcome::Interrupted);
            }

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

            // 先判完成暗号：回复中出现（容错）即结束
            if is_done_signal(&reply.content) {
            self.memory.push(Message {
            role: Role::Assistant,
            content: reply.content.clone(),
            });
            return Ok(RunOutcome::Done(reply));
            }

            match parser::parse(&reply.content) {
            ParseOutcome::Call(call) => {
            if cancel.is_cancelled() {
            return Ok(RunOutcome::Interrupted);
            }
            let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
            let live = tty;
            let result = self.run_tool(&call, live).await;
            if result.starts_with("[INTERRUPTED]") {
            if tty {
            println!(
            "\n{}[已中断]{}",
            crate::ui::DIM,
            crate::ui::RESET
            );
            }
            return Ok(RunOutcome::Done(reply));
            }
            if !tty {
            crate::ui::print_tool_result(&result);
            }
            next_prompt = format!("<tool_result>\n{result}\n</tool_result>");
            }
            ParseOutcome::Text(_) | ParseOutcome::Malformed { .. } => {
            if cancel.is_cancelled() {
            return Ok(RunOutcome::Interrupted);
            }
            next_prompt = NO_INSTRUCTION_PROMPT.to_string();
            }
            }
        }
    }

    /// 调用模型；空回复（疑似限流）时自动刷新+重试。
    /// 非空错误（网络等）直接上抛，不影响原有行为。
    async fn complete_with_retry(
        &mut self,
        req: &CompletionReq,
        cancel: &CancellationToken,
        on_delta: &mut (dyn for<'a> FnMut(crate::api::types::DeltaKind, &'a str) + Send),
    ) -> Result<ModelReply> {
        match self.model.complete(req, on_delta).await {
            Ok(r) => return Ok(r),
            Err(crate::error::AgentError::EmptyReply) => {}
            Err(e) => return Err(e),
        }

        crate::ui::notice_waiting("检测到空回复（疑似限流），正在刷新会话并自动重试…");
        let _ = self.refresh_current_session().await;

        for attempt in 1..=5u32 {
            if cancel.is_cancelled() {
                return Err(crate::error::AgentError::Other("[已中断]".into()));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            crate::ui::notice_waiting(&format!("自动重试中 {attempt}/5 …"));
            match self.model.complete(req, on_delta).await {
                Ok(r) => return Ok(r),
                Err(crate::error::AgentError::EmptyReply) => continue,
                Err(e) => return Err(e),
            }
        }

        let mut wait_secs = 60u64;
        loop {
            if cancel.is_cancelled() {
                return Err(crate::error::AgentError::Other("[已中断]".into()));
            }
            crate::ui::notice_waiting(&format!(
                "限流未解除，等待 {wait_secs}s 后自动重试（Ctrl+C 停止）…"
            ));
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(wait_secs)) => {}
                _ = cancel.cancelled() => {
                    return Err(crate::error::AgentError::Other("[已中断]".into()));
                }
            }
            match self.model.complete(req, on_delta).await {
                Ok(r) => return Ok(r),
                Err(crate::error::AgentError::EmptyReply) => {
                    wait_secs = (wait_secs + 60).min(180);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 执行工具调用（流式输出），返回结果文本（含审批）。
    async fn run_tool(&self, call: &parser::ToolCall, live: bool) -> String {
        let cmd = call.params.get("command").map(|s| s.as_str()).unwrap_or("");

        if !self.free_mode {
            match crate::ui::confirm_command(cmd) {
                Ok(true) => {}
                Ok(false) => return "[用户拒绝执行该命令]".to_string(),
                Err(_) => return "[确认失败，已跳过]".to_string(),
            }
        }

        if live {
            println!(
                "{}⚙ $ {}{}",
                crate::ui::TOOL,
                cmd,
                crate::ui::RESET
            );
        }

        match self.tools.get(&call.name) {
            Some(tool) => {
                let mut sink = |chunk: &str| {
                    if live {
                        print!("{}{chunk}{}", crate::ui::TOOL_OUT, crate::ui::RESET);
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

/// 判定回复是否含完成暗号（容错：忽略空白、统一中英逗号、字母小写）。
fn is_done_signal(text: &str) -> bool {
    fn norm(s: &str) -> String {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| if c == ',' { '，' } else { c.to_ascii_lowercase() })
            .collect()
    }
    norm(text).contains(&norm(DONE_SIGNAL))
}