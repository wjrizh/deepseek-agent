//! 浏览器 MCP 工具端到端测试。
//!
//! 依赖：已安装 @playwright/mcp（见 scripts/setup_browser_mcp.sh）+ Chromium。
//! 若未安装，测试自动跳过（不失败）。
use deepseek_agent::agent::browser::Browser;
use deepseek_agent::agent::tool::Tool;
use std::collections::HashMap;

fn mcp_available() -> bool {
    if let Ok(p) = std::env::var("LIGONG_MCP_CLI") {
        return std::path::Path::new(&p).exists();
    }
    dirs::home_dir()
        .map(|h| h.join(".ligong-mcp/node_modules/@playwright/mcp/cli.js").exists())
        .unwrap_or(false)
}

fn run(rt: &tokio::runtime::Runtime, tool: &Browser, action: &str, params: &str) -> String {
    let mut m = HashMap::new();
    m.insert("action".to_string(), action.to_string());
    if !params.is_empty() {
        m.insert("params".to_string(), params.to_string());
    }
    rt.block_on(async {
        let mut sink = |_s: &str| {};
        tool.call(&m, false, &mut sink).await.unwrap()
    })
}

#[test]
fn browser_navigate_snapshot_evaluate() {
    if !mcp_available() {
        eprintln!("跳过：未安装 @playwright/mcp");
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tool = Browser::new();

    let help = run(&rt, &tool, "", "");
    assert!(help.contains("navigate"), "help 应列出 action");

    let nav = run(&rt, &tool, "navigate", "{\"url\":\"https://example.com\"}");
    assert!(
        nav.contains("Example Domain") || nav.to_lowercase().contains("navigat"),
        "navigate 结果异常: {nav}"
    );

    let snap = run(&rt, &tool, "snapshot", "");
    assert!(snap.contains("Example Domain"), "snapshot 应含页面标题: {snap}");

    let title = run(&rt, &tool, "evaluate", "{\"function\":\"() => document.title\"}");
    assert!(title.contains("Example Domain"), "evaluate 应返回标题: {title}");
}
