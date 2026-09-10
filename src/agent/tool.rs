//! Tool trait + 内置工具实现。
//!
//! 当前只提供 `execute_command`（命令行走天下）；
//! 新增工具只需实现 `Tool` 并在 `parser::KNOWN_TOOLS` 登记。

use crate::error::{AgentError, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// 工具输出的最大字符数（超出截断，防止撑爆上下文）。
pub const MAX_TOOL_OUTPUT: usize = 8000;
/// 单条命令的超时时间（秒）。
pub const COMMAND_TIMEOUT_SECS: u64 = 120;

#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名（唯一标识，须与 `parser::KNOWN_TOOLS` 一致）
    fn name(&self) -> &str;
    /// 工具描述（供模型理解用途）
    fn description(&self) -> &str;
    /// 调用工具，参数为解析后的键值对
    async fn call(&self, params: &HashMap<String, String>) -> Result<String>;
}

/// 内置工具：在指定工作目录执行 shell 命令。
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
        "在项目工作目录执行一条 shell 命令，返回 exit_code / stdout / stderr。"
    }

    async fn call(&self, params: &HashMap<String, String>) -> Result<String> {
        let cmd = params
            .get("command")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AgentError::Other("execute_command: 缺少非空 <command>".into()))?;

        let fut = Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .output();

        let output =
            match tokio::time::timeout(Duration::from_secs(COMMAND_TIMEOUT_SECS), fut).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => return Err(AgentError::Other(format!("命令启动失败: {e}"))),
                Err(_) => {
                    return Ok(format!(
                        "[超时] 命令超过 {COMMAND_TIMEOUT_SECS}s 未完成，已终止"
                    ));
                }
            };

        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(format_result(code, &stdout, &stderr))
    }
}

fn format_result(code: i32, stdout: &str, stderr: &str) -> String {
    let mut s = format!("exit_code: {code}\n");
    if !stdout.trim().is_empty() {
        s.push_str("stdout:\n");
        s.push_str(stdout.trim_end());
        s.push('\n');
    }
    if !stderr.trim().is_empty() {
        s.push_str("stderr:\n");
        s.push_str(stderr.trim_end());
        s.push('\n');
    }
    truncate(s, MAX_TOOL_OUTPUT)
}

fn truncate(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut t: String = s.chars().take(max).collect();
    t.push_str("\n...[输出已截断]");
    t
}

/// 工具注册表
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
        self.tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
    }

    pub fn list(&self) -> Vec<(&str, &str)> {
        self.tools.iter().map(|t| (t.name(), t.description())).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}