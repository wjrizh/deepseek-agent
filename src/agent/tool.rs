//! Tool trait + 内置工具实现（真·交互式 PTY）。
//!
//! 当前只提供 `execute_command`（命令行走天下）；
//! 新增工具只需实现 `Tool` 并在 `parser::KNOWN_TOOLS` 登记。
//!
//! 命令在 **PTY** 中执行：命令以为自己在真终端里，因此
//! wget 进度条、彩色 diff、pip 动画等都原生呈现；
//! live 模式下用户可直接键入与命令交互（y/n、vim 等）。

use crate::error::{AgentError, Result};
use async_trait::async_trait;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// 工具输出的最大字符数（超出截断，防止撑爆上下文）。
pub const MAX_TOOL_OUTPUT: usize = 8000;
/// 非交互模式下的空闲超时（秒）。持续有输出则不中断。
pub const COMMAND_IDLE_TIMEOUT_SECS: u64 = 300;
/// 交互模式下的空闲超时（秒）。用户可 Ctrl+C 结束。
pub const INTERACTIVE_IDLE_TIMEOUT_SECS: u64 = 1800;

/// 工具是否正在运行（决定 Ctrl+C 是「中断命令」还是「退出程序」）
pub static TOOL_RUNNING: AtomicBool = AtomicBool::new(false);
/// 工具被 Ctrl+C 中断的标志（由全局 handler 置位，工具轮询）
pub static TOOL_INTERRUPT: AtomicBool = AtomicBool::new(false);

/// RAII：进入作用域置 TOOL_RUNNING=true，离开自动复位。
struct ToolGuard;
impl ToolGuard {
    fn new() -> Self {
        TOOL_INTERRUPT.store(false, Ordering::Relaxed);
        TOOL_RUNNING.store(true, Ordering::Relaxed);
        Self
    }
}
impl Drop for ToolGuard {
    fn drop(&mut self) {
        TOOL_RUNNING.store(false, Ordering::Relaxed);
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名（唯一标识，须与 `parser::KNOWN_TOOLS` 一致）
    fn name(&self) -> &str;
    /// 工具描述（供模型理解用途）
    fn description(&self) -> &str;
    /// 调用工具。
    /// - `live`：是否允许用户实时键入交互（TTY 模式下为 true）
    /// - `on_output`：命令产生输出时被实时调用（用于终端流式显示）
    async fn call(
        &self,
        params: &HashMap<String, String>,
        live: bool,
        on_output: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<String>;
}

/// 内置工具：在 PTY 中执行 shell 命令。
pub struct ExecuteCommand {
    cwd: PathBuf,
}

impl ExecuteCommand {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

#[async_trait]
impl Tool for ExecuteCommand {
    fn name(&self) -> &str {
        "execute_command"
    }

    fn description(&self) -> &str {
        "在项目工作目录执行一条 shell 命令（PTY，可交互），返回 exit_code 与输出。"
    }

    async fn call(
        &self,
        params: &HashMap<String, String>,
        live: bool,
        on_output: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<String> {
        let cmd = params
            .get("command")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AgentError::Other("execute_command: 缺少非空 <command>".into()))?;

        let _guard = ToolGuard::new();

        let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AgentError::Other(format!("openpty: {e}")))?;

        let mut builder = CommandBuilder::new("bash");
        builder.arg("-c");
        builder.arg(cmd);
        builder.cwd(&self.cwd);
        builder.env("TERM", "xterm-256color");
        builder.env("PAGER", "cat");
        builder.env("GIT_PAGER", "cat");
        builder.env("DEBIAN_FRONTEND", "noninteractive");

        let mut child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| AgentError::Other(format!("spawn: {e}")))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| AgentError::Other(format!("reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| AgentError::Other(format!("writer: {e}")))?;

        let captured = Arc::new(Mutex::new(String::new()));
        let captured_thread = captured.clone();
        let (tx, mut rx) = mpsc::channel::<String>(64);
        let reader_handle = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let s = String::from_utf8_lossy(&buf[..n]).to_string();
                        if let Ok(mut c) = captured_thread.lock() {
                            c.push_str(&s);
                        }
                        if tx.blocking_send(s).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let stop = Arc::new(AtomicBool::new(false));
        // heredoc（含 `<<`）：即使 live 也不转发用户 stdin。
        // 否则键盘输入会被 heredoc 消费命令的 stdin 吞掉，导致解释器进 REPL。
        let has_heredoc = cmd.contains("<<");
        let stdin_thread = if live && !has_heredoc {
            Some(spawn_stdin_forwarder(writer, stop.clone()))
        } else {
            drop(writer);
            None
        };

        let timeout_secs = if live {
            INTERACTIVE_IDLE_TIMEOUT_SECS
        } else {
            COMMAND_IDLE_TIMEOUT_SECS
        };

        let tty = std::io::stdout().is_terminal();

        let mut timed_out = false;
        let mut interrupted = false;
        let mut idle = Duration::ZERO;
        let tick = Duration::from_millis(200);
        loop {
            if TOOL_INTERRUPT.load(Ordering::Relaxed) {
                interrupted = true;
                break;
            }
            match tokio::time::timeout(tick, rx.recv()).await {
                Ok(Some(chunk)) => {
                    idle = Duration::ZERO;
                    if tty && !live {
                        print!("{chunk}");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    } else {
                        on_output(&chunk);
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    idle += tick;
                    if idle >= Duration::from_secs(timeout_secs) {
                        timed_out = true;
                        break;
                    }
                }
            }
        }

        stop.store(true, Ordering::Relaxed);
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader_handle.join();
        if let Some(t) = stdin_thread {
            let _ = t.join();
        }

        let captured = captured.lock().map(|c| c.clone()).unwrap_or_default();

        let clean = strip_ansi(&collapse_progress(&captured));
        if interrupted {
            return Ok(format!(
                "[INTERRUPTED]\nexit_code: -1\n[中断] 命令被 Ctrl+C 终止\n{}",
                truncate(clean, MAX_TOOL_OUTPUT)
            ));
        }
        if timed_out {
            return Ok(format!(
                "exit_code: -1\n[超时] 命令超过 {timeout_secs}s 无输出，已终止\n{}",
                truncate(clean, MAX_TOOL_OUTPUT)
            ));
        }
        Ok(format_result(0, &clean))
    }
}

fn spawn_stdin_forwarder(
    mut writer: Box<dyn Write + Send>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use crossterm::event::{Event, poll, read};
        let _ = crossterm::terminal::enable_raw_mode();
        while !stop.load(Ordering::Relaxed) {
            match poll(Duration::from_millis(100)) {
                Ok(true) => {
                    if let Ok(Event::Key(k)) = read() {
                        let bytes = key_to_bytes(&k);
                        if !bytes.is_empty() {
                            if writer.write_all(&bytes).is_err() {
                                break;
                            }
                            let _ = writer.flush();
                        }
                    }
                }
                Ok(false) => {}
                Err(_) => break,
            }
        }
        let _ = crossterm::terminal::disable_raw_mode();
    })
}

fn key_to_bytes(k: &crossterm::event::KeyEvent) -> Vec<u8> {
    use crossterm::event::{KeyCode, KeyModifiers};
    if k.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = k.code
    {
        return vec![(c.to_ascii_lowercase() as u8) & 0x1f];
    }
    match k.code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        _ => vec![],
    }
}

fn format_result(code: i32, output: &str) -> String {
    let mut s = format!("exit_code: {code}\n");
    let o = output.trim_end();
    if !o.is_empty() {
        s.push_str(o);
        s.push('\n');
    }
    truncate(s, MAX_TOOL_OUTPUT)
}

fn collapse_progress(s: &str) -> String {
    let normalized = s.replace("\r\n", "\n");
    normalized
        .split('\n')
        .map(|line| line.rsplit('\r').next().unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&c2) = chars.peek() {
                        chars.next();
                        if c2.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&c2) = chars.peek() {
                        chars.next();
                        if c2 == '\x07' || c2 == '\x1b' {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if c == '\n' || c == '\t' || c == '\r' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

fn truncate(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut t: String = s.chars().take(max).collect();
    t.push_str("\n...[输出已截断]");
    t
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    pub fn list(&self) -> Vec<(&str, &str)> {
        self.tools
            .iter()
            .map(|t| (t.name(), t.description()))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}