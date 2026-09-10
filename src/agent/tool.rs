//! Tool trait —— Agent 能力扩展点。
//!
//! 现在 DeepSeek web 端无 function-call，接口先留好；
//! 未来可接本地工具（文件读写、搜索、代码执行等）。

use crate::error::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名（唯一标识）
    fn name(&self) -> &str;
    /// 工具描述（供模型理解用途）
    fn description(&self) -> &str;
    /// 调用工具
    async fn call(&self, input: &str) -> Result<String>;
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
        self.tools
            .iter()
            .map(|t| (t.name(), t.description()))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
