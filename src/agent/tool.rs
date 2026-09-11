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

        // 硬拦截 heredoc：PTY 下不可靠（stdin/换行/嵌套都易卡 REPL）。
        // 用宽松检测（<< 后跟引号/字母才拦，误伤不了 1 << 2 之类位运算）。
        let has_heredoc = {
            let bytes = cmd.as_bytes();
            let mut found = false;
            let mut i = 0;
            while i + 1 < bytes.len() {
                if bytes[i] == b'<' && bytes[i + 1] == b'<' {
                    let mut j = i + 2;
                    if j < bytes.len() && bytes[j] == b'-' {
                        j += 1;
                    }
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len()
                        && (bytes[j] == b'\'' || bytes[j] == b'"' || bytes[j].is_ascii_alphabetic())
                    {
                        found = true;
                        break;
                    }
                }
                i += 1;
            }
            found
        };
        if has_heredoc {
            return Ok(
                "[错误] 检测到 heredoc（<<），本环境不支持。请改用以下方式之一：\n\
                 1. python3 -c 'import x; ...'  （单行，分号分隔）\n\
                 2. 先用 write_file 工具/printf 写文件，再 python3 文件.py\n\
                 3. 用 echo -e '...' 管道\n\
                 禁止使用 <<'PY' / <<EOF 等 heredoc 语法。"
                    .to_string(),
            );
        }

        // 只剥离阻塞型管道段（tail/head/分页器），保留 grep/awk 等流式过滤段。
        let (cmd, pipe_stripped) = strip_all_pipes(cmd);
        let cmd = cmd.as_str();

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
        let (tx, mut rx) = mpsc::channel::<String>(256);

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
                        if tx.is_closed() {
                            break;
                        }
                        let _ = tx.try_send(s);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        let stop = Arc::new(AtomicBool::new(false));
        let reads_stdin = reads_whole_stdin(cmd);
        let stdin_thread = if live && !reads_stdin {
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
        let tick = Duration::from_millis(100);
        let mut child_exited = false;

        loop {
            if TOOL_INTERRUPT.load(Ordering::Relaxed) {
                interrupted = true;
                break;
            }

            let mut got_output = false;
            loop {
                match rx.try_recv() {
                    Ok(chunk) => {
                        got_output = true;
                        if tty && !live {
                            print!("{chunk}");
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                        } else {
                            on_output(&chunk);
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        child_exited = true;
                        break;
                    }
                }
            }

            if got_output {
                idle = Duration::ZERO;
            } else {
                idle += tick;
            }

            if !child_exited
                && let Ok(Some(_)) = child.try_wait()
            {
                child_exited = true;
            }
            if child_exited {
                while let Ok(chunk) = rx.try_recv() {
                    if tty && !live {
                        print!("{chunk}");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    } else {
                        on_output(&chunk);
                    }
                }
                break;
            }

            if idle >= Duration::from_secs(timeout_secs) {
                timed_out = true;
                break;
            }

            tokio::time::sleep(tick).await;
        }

        stop.store(true, Ordering::Relaxed);
        let _ = child.kill();
        let _ = child.wait();
        drop(pair.master);
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
        let mut result = format_result(0, &clean);
        if pipe_stripped {
            result.push_str(
                "\n[提示] 已自动剥离阻塞型管道段（tail/head/分页器等）以保证输出实时；grep/awk 等流式过滤段保留。",
            );
        }
        Ok(result)
    }
}

/// 剥离命令中所有顶层管道段，只保留第一段执行。
///
/// 目的：`cmd | tail` / `| head` / `| grep` 等会缓冲输出，在流式显示
/// 场景下表现为命令卡住不打印。这里只执行第一个管道段，让输出实时
/// 到达 agent，由 agent 在结束时按 MAX_TOOL_OUTPUT 自行截断。
///
/// 规则：
/// - 按顶层 `|` 切分，只取第一段。
/// - 忽略单双引号内的 `|`。
/// - 忽略 `||`（逻辑或）。
/// - 保留重定向（> / 2>&1）等，它们不阻塞输出。
///
/// 取舍：会改变 `cat x | grep y` 这类靠管道过滤的语义，以实时性优先。
/// 返回 (改写后的命令, 是否发生剥离)。
fn strip_all_pipes(cmd: &str) -> (String, bool) {
    // 1. 按顶层 | 切分成段（忽略引号内 | 与 ||）
    let segments = split_top_pipes(cmd);
    if segments.len() <= 1 {
        return (cmd.to_string(), false);
    }
    // 2. 剔除阻塞/分页段，保留流式过滤段（grep/awk/sed/cut/sort 等）
    let mut kept: Vec<&str> = Vec::new();
    let mut removed = false;
    for seg in &segments {
        if is_blocking_filter(seg) {
            removed = true;
        } else {
            kept.push(seg);
        }
    }
    if !removed || kept.is_empty() {
        return (cmd.to_string(), removed && kept.is_empty());
    }
    (kept.join(" | "), true)
}

/// 按顶层 `|` 切分命令（忽略单双引号内的 `|`，忽略 `||`）。
/// 返回各段（已 trim）；不含管道时返回单元素。
fn split_top_pipes(cmd: &str) -> Vec<&str> {
    let bytes = cmd.as_bytes();
    let mut quote: Option<u8> = None;
    let mut segs: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i <bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
        } else if b == 39u8 || b == 34u8 {
            quote = Some(b);
        } else if b == 124u8 {
            let is_or = (i + 1 < bytes.len() && bytes[i + 1] == 124u8)
                || (i > 0 && bytes[i - 1] == 124u8);
            if !is_or {
                segs.push(cmd[start..i].trim());
                start = i + 1;
            }
        }
        i += 1;
    }
    segs.push(cmd[start..].trim());
    segs
}

/// 该管道段是否为阻塞/分页类（会导致输出不实时）。
/// 阻塞类：tail/head 必须读到 EOF 或足够行数；分页器独占终端。
/// 流式过滤类（grep/awk/sed/cut/sort/uniq/wc…）实时，保留。
fn is_blocking_filter(segment: &str) -> bool {
    let first = segment.split_whitespace().next().unwrap_or("");
    let base = first.rsplit(47u8 as char).next().unwrap_or(first);
    matches!(
        base,
        "tail" | "head" | "less" | "more" | "pg" | "most" | "bat" | "ov" | "glow"
    )
}

fn reads_whole_stdin(cmd: &str) -> bool {
    let last = cmd
        .rsplit(['\n', ';', '&', '|'])
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    let mut parts = last.split_whitespace();
    let prog = parts.next().unwrap_or("");
    let args: Vec<&str> = parts.collect();

    let base = prog.rsplit('/').next().unwrap_or(prog);
    if (base == "python" || base == "python3" || base.starts_with("python3."))
        && args == ["-"] {
            return true;
        }
    if (base == "cat" || base == "read" || base == "tee") && args.is_empty() {
        return true;
    }
    false
}

fn spawn_stdin_forwarder(
    mut writer: Box<dyn Write + Send>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use crossterm::event::{Event, poll, read};

        struct RawGuard;
        impl RawGuard {
            fn new() -> Self {
                let _ = crossterm::terminal::enable_raw_mode();
                RawGuard
            }
        }
        impl Drop for RawGuard {
            fn drop(&mut self) {
                let _ = crossterm::terminal::disable_raw_mode();
            }
        }

        let _guard = RawGuard::new();
        while !stop.load(Ordering::Relaxed) {
            match poll(Duration::from_millis(50)) {
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

/// 内置工具：上传需视觉读取的文件（图片/PDF），返回 file_id 并注入下一轮请求。
pub struct UploadFile {
    http: Arc<crate::client::http::HttpClient>,
    solver: Arc<tokio::sync::Mutex<crate::client::pow::PowSolver>>,
    pending: Arc<Mutex<Vec<String>>>,
}

impl UploadFile {
    pub fn new(
        http: Arc<crate::client::http::HttpClient>,
        solver: Arc<tokio::sync::Mutex<crate::client::pow::PowSolver>>,
        pending: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            http,
            solver,
            pending,
        }
    }
}

#[async_trait]
impl Tool for UploadFile {
    fn name(&self) -> &str {
        "upload_file"
    }

    fn description(&self) -> &str {
        "上传文件给模型（图片、PDF 及任意类型文件均可）。参数 paths 为待上传文件路径，多个用换行或逗号分隔。单次最多 50 个文件，单个文件不超过 100MB。"
    }

    async fn call(
        &self,
        params: &HashMap<String, String>,
        _live: bool,
        _on_output: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<String> {
        let raw = params
            .get("paths")
            .map(|s| s.as_str())
            .ok_or_else(|| AgentError::Other("upload_file: 缺少 <paths>".into()))?;

        let paths: Vec<String> = raw
            .split(['\n', ','])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if paths.is_empty() {
            return Err(AgentError::Other("upload_file: <paths> 为空".into()));
        }

        if paths.len() > crate::api::file::MAX_UPLOAD_FILES {
            return Err(AgentError::Other(format!(
                "upload_file: 一次最多上传 {} 个文件，当前 {} 个，请分批次上传",
                crate::api::file::MAX_UPLOAD_FILES,
                paths.len()
            )));
        }


        let mut lines = Vec::new();
        for p in &paths {
            let path = std::path::Path::new(p);
            if !path.exists() {
                lines.push(format!("[!] 文件不存在: {p}"));
                continue;
            }
            if let Ok(meta) = std::fs::metadata(path)
                && meta.len() > crate::api::file::MAX_FILE_SIZE
            {
                lines.push(format!(
                    "[!] 文件过大（{} 字节，上限 {} 字节），不能直接上传: {p}",
                    meta.len(),
                    crate::api::file::MAX_FILE_SIZE
                ));
                continue;
            }
            let mut solver = self.solver.lock().await;
            let r = crate::api::file::upload(&self.http, &mut solver, path).await;
            drop(solver);
            let mut aborted = false;
            match r {
                Ok(info) => {
                    if let Ok(mut pend) = self.pending.lock() {
                        pend.push(info.id.clone());
                    }
                    lines.push(format!(
                        "[✓] 已上传: {} (file_id={}, is_image={})",
                        info.file_name, info.id, info.is_image
                    ));
                }
                Err(AgentError::RateLimited { code, msg }) => {
                    lines.push(format!(
                        "[!] 触发风控/限流 (code={code}): {msg}；已中止本批上传，请稍后再试"
                    ));
                    aborted = true;
                }
                Err(e) => lines.push(format!("[!] 上传失败 {p}: {e}")),
            }
            if aborted {
                break;
            }
            // 批量上传间隔：模拟人工逐个选择文件的节奏（1~3s 随机）
            let pause = {
                use rand::Rng;
                rand::rng().random_range(1000..=3000)
            };
            tokio::time::sleep(std::time::Duration::from_millis(pause)).await;
        }
        Ok(lines.join("\n"))
    }
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


#[cfg(test)]
mod tests {
    use super::strip_all_pipes;

    fn strip(cmd: &str) -> String {
        strip_all_pipes(cmd).0
    }

    #[test]
    fn strips_tail_pipe() {
        assert_eq!(strip("pip install x | tail -20"), "pip install x");
    }

    #[test]
    fn keeps_grep_pipe() {
        assert_eq!(strip("cat x | grep y"), "cat x | grep y");
    }

    #[test]
    fn keeps_streaming_filters_removes_tail() {
        assert_eq!(strip("cat a | grep b | tail -5"), "cat a | grep b");
    }

    #[test]
    fn keeps_grep_and_awk() {
        assert_eq!(strip("ps aux | grep node | awk \"{print $2}\""), "ps aux | grep node | awk \"{print $2}\"");
    }

    #[test]
    fn strips_head_only() {
        assert_eq!(strip("ls | head"), "ls");
    }

    #[test]
    fn strips_pager() {
        assert_eq!(strip("git log | less"), "git log");
    }

    #[test]
    fn keeps_quoted_pipe() {
        assert_eq!(strip("grep \"a|b\" file"), "grep \"a|b\" file");
    }

    #[test]
    fn keeps_logical_or() {
        assert_eq!(strip("a || b"), "a || b");
    }

    #[test]
    fn keeps_redirection() {
        assert_eq!(strip("make > /tmp/o.log 2>&1"), "make > /tmp/o.log 2>&1");
    }

    #[test]
    fn no_pipe_unchanged() {
        assert_eq!(strip("ls -la"), "ls -la");
    }

    #[test]
    fn strip_flag_true_when_blocking() {
        assert!(strip_all_pipes("a | tail").1);
        assert!(!strip_all_pipes("a | grep b").1);
        assert!(!strip_all_pipes("a").1);
    }
}
