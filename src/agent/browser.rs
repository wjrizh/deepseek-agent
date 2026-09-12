//! 浏览器工具：桥接 Microsoft Playwright MCP (@playwright/mcp)。
//!
//! 以单个 `browser` 工具暴露，内部用 stdio JSON-RPC 驱动一个长驻的
//! @playwright/mcp 子进程。模型通过 action + params(JSON) 调用底层浏览器
//! 动作（navigate/click/type/snapshot/find/evaluate/fill_form/...）。

use crate::agent::tool::Tool;
use crate::error::{AgentError, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

const REQUEST_TIMEOUT_SECS: u64 = 120;

struct McpClient {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    child: Mutex<Option<Child>>,
    dead: AtomicBool,
}

impl McpClient {
    fn is_alive(&self) -> bool {
        if self.dead.load(Ordering::Relaxed) {
            return false;
        }
        let mut guard = self.child.lock().unwrap();
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&msg)?;
        line.push_str("\n");
        {
            let mut stdin = self.stdin.lock().await;
            if let Err(e) = stdin.write_all(line.as_bytes()).await {
                self.dead.store(true, Ordering::Relaxed);
                return Err(AgentError::Other(format!("mcp 写入失败: {e}")));
            }
            stdin.flush().await.ok();
        }
        match tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), rx).await {
            Ok(Ok(v)) => {
                if let Some(err) = v.get("error") {
                    return Err(AgentError::Other(format!("mcp 错误: {err}")));
                }
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(Err(_)) => Err(AgentError::Other("mcp 连接已关闭".into())),
            Err(_) => Err(AgentError::Other("mcp 请求超时".into())),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let mut line = serde_json::to_string(&msg)?;
        line.push_str("\n");
        let mut stdin = self.stdin.lock().await;
        if let Err(e) = stdin.write_all(line.as_bytes()).await {
            self.dead.store(true, Ordering::Relaxed);
            return Err(AgentError::Other(format!("mcp 写入失败: {e}")));
        }
        stdin.flush().await.ok();
        Ok(())
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<String> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": args}))
            .await?;
        Ok(render_result(&result))
    }
}

/// 把 MCP tools/call 的返回值渲染为纯文本。
fn render_result(result: &Value) -> String {
    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        let mut parts: Vec<String> = Vec::new();
        for item in content {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                parts.push(text.to_string());
            } else if let Some(t) = item.get("type").and_then(|t| t.as_str()) {
                parts.push(format!("[{t}]"));
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
}

/// 定位 @playwright/mcp 的 cli.js（环境变量优先，其次固定安装目录）。
fn locate_mcp_cli() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LIGONG_MCP_CLI") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".ligong-mcp/node_modules/@playwright/mcp/cli.js"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// 定位 Chromium 可执行文件（环境变量优先，其次常见缓存路径）。
fn locate_chromium() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LIGONG_CHROMIUM") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let home = dirs::home_dir()?;
    let base = home.join(".cache/ms-playwright");
    let mut best: Option<(u32, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(&base) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if let Some(ver) = name.strip_prefix("chromium-")
                && let Ok(n) = ver.parse::<u32>()
            {
                let chrome = e.path().join("chrome-linux64/chrome");
                let headless = e.path().join("chrome-linux/headless_shell");
                let exe = if chrome.exists() {
                    chrome
                } else if headless.exists() {
                    headless
                } else {
                    continue;
                };
                if best.as_ref().is_none_or(|(v, _)| n > *v) {
                    best = Some((n, exe));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// 是否以无头模式运行：环境变量优先，否则有显示环境时用有头（可见）模式。
fn browser_headless() -> bool {
    match std::env::var("LIGONG_BROWSER_HEADLESS").ok().as_deref() {
        Some("1") | Some("true") => true,
        Some("0") | Some("false") => false,
        _ => {
            std::env::var("DISPLAY").is_err()
                && std::env::var("WAYLAND_DISPLAY").is_err()
        }
    }
}

/// 持久化 profile 目录；设置 LIGONG_BROWSER_EPHEMERAL=1 则改用内存隔离（不持久化）。
fn browser_profile() -> Option<PathBuf> {
    if matches!(std::env::var("LIGONG_BROWSER_EPHEMERAL").ok().as_deref(), Some("1") | Some("true")) {
        return None;
    }
    if let Ok(p) = std::env::var("LIGONG_BROWSER_PROFILE") {
        return Some(PathBuf::from(p));
    }
    let dir = dirs::home_dir()?.join(".ligong-mcp/profile");
    Some(dir)
}

/// 启动 MCP 子进程并完成 initialize 握手，返回客户端句柄。
async fn spawn_client() -> Result<Arc<McpClient>> {
    let cli = locate_mcp_cli().ok_or_else(|| {
        AgentError::Other(
            "未找到 @playwright/mcp（cli.js）。请运行 scripts/setup_browser_mcp.sh 安装。".into(),
        )
    })?;

    let mut cmd = Command::new("node");
    cmd.arg(&cli)
        .arg("--no-sandbox")
        .arg("--image-responses")
        .arg("omit")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if browser_headless() {
        cmd.arg("--headless");
    }
    match browser_profile() {
        Some(profile) => { cmd.arg("--user-data-dir").arg(profile); }
        None => { cmd.arg("--isolated"); }
    }
    if let Some(chrome) = locate_chromium() {
        cmd.arg("--executable-path").arg(chrome);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| AgentError::Other(format!("启动 playwright-mcp 失败: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentError::Other("无法获取 mcp stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Other("无法获取 mcp stdout".into()))?;

    let client = Arc::new(McpClient {
        stdin: tokio::sync::Mutex::new(stdin),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        child: Mutex::new(Some(child)),
        dead: AtomicBool::new(false),
    });

    let reader_client = client.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(id) = msg.get("id").and_then(|i| i.as_u64())
                && let Some(tx) = reader_client.pending.lock().unwrap().remove(&id)
            {
                let _ = tx.send(msg);
            }
        }
        reader_client.dead.store(true, Ordering::Relaxed);
        reader_client.pending.lock().unwrap().clear();
    });

    client
        .request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "deepseek-agent", "version": "0.1.0"}
            }),
        )
        .await?;
    client.notify("notifications/initialized", json!({})).await?;
    Ok(client)
}

static CLIENT: Mutex<Option<Arc<McpClient>>> = Mutex::new(None);

async fn client() -> Result<Arc<McpClient>> {
    if let Some(existing) = CLIENT.lock().unwrap().clone()
        && existing.is_alive()
    {
        return Ok(existing);
    }
    let fresh = spawn_client().await?;
    *CLIENT.lock().unwrap() = Some(fresh.clone());
    Ok(fresh)
}

/// 支持的浏览器动作（action → 底层 MCP 工具）。
const ACTIONS: &[(&str, &str)] = &[
    ("navigate", "browser_navigate"),
    ("back", "browser_navigate_back"),
    ("snapshot", "browser_snapshot"),
    ("find", "browser_find"),
    ("click", "browser_click"),
    ("type", "browser_type"),
    ("fill_form", "browser_fill_form"),
    ("select_option", "browser_select_option"),
    ("hover", "browser_hover"),
    ("press_key", "browser_press_key"),
    ("drag", "browser_drag"),
    ("drop", "browser_drop"),
    ("evaluate", "browser_evaluate"),
    ("run_code", "browser_run_code_unsafe"),
    ("upload", "browser_file_upload"),
    ("tabs", "browser_tabs"),
    ("wait_for", "browser_wait_for"),
    ("resize", "browser_resize"),
    ("console", "browser_console_messages"),
    ("network", "browser_network_requests"),
    ("network_detail", "browser_network_request"),
    ("screenshot", "browser_take_screenshot"),
    ("dialog", "browser_handle_dialog"),
    ("close", "browser_close"),
];

fn resolve_action(action: &str) -> Option<&'static str> {
    ACTIONS
        .iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(action))
        .map(|(_, tool)| *tool)
}

fn actions_help() -> String {
    let mut s = String::from("可用 action（params 为 JSON 对象字符串）：");
    for (a, tool) in ACTIONS {
        s.push_str(&format!("- {a}  ({tool})"));
        s.push('\n');
    }
    s.push_str("示例：");
    s.push_str("  <parameter name=\"action\">navigate</parameter>");
    s.push_str("  <parameter name=\"params\">{\"url\":\"https://example.com\"}</parameter>");
    s.push_str("常用 params：navigate{url}、click{target}、type{target,text}、find{text}、evaluate{function}。");
    s
}

pub struct Browser {
    _private: (),
}

impl Browser {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for Browser {
    fn default() -> Self {
        Self::new()
    }
}
#[async_trait]
impl Tool for Browser {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "控制无头浏览器（Playwright）：导航、读取页面无障碍快照、点击、输入、填表、执行 JS 等。参数 action + params(JSON)。"
    }

    async fn call(
        &self,
        params: &HashMap<String, String>,
        _live: bool,
        _on_output: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<String> {
        let action = params.get("action").map(|s| s.trim()).unwrap_or("");
        if action.is_empty() {
            return Ok(actions_help());
        }
        let tool = match resolve_action(action) {
            Some(t) => t,
            None => return Ok(format!("[错误] 未知 action: {action}\n\n{}", actions_help())),
        };
        let args: Value = match params.get("params").map(|s| s.trim()) {
            Some(s) if !s.is_empty() => match serde_json::from_str(s) {
                Ok(v) => v,
                Err(e) => return Ok(format!("[错误] params 不是合法 JSON: {e}\n原文: {s}")),
            },
            _ => json!({}),
        };
        let client = client().await?;
        let out = client.call_tool(tool, args).await?;
        Ok(crate::agent::tool::truncate_output(out))
    }
}
