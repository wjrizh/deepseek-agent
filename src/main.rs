//! DeepSeek Agent 入口（薄层）。

use clap::Parser;
use deepseek_agent::cli;
use deepseek_agent::config::Config;

#[derive(Parser, Debug)]
#[command(name = "deepseek-agent", version, about = "DeepSeek Agent (Rust)")]
struct Args {
    /// 单次提问（不给则进入交互模式）
    prompt: Option<String>,

    /// 附加文件（可多个）
    #[arg(short, long = "file")]
    files: Vec<String>,

    /// 指定 token（覆盖环境变量与持久化）
    #[arg(long)]
    token: Option<String>,

    /// 开启思考模式（显示推理过程）
    #[arg(long)]
    thinking: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if let Some(t) = &args.token {
        // SAFETY: 单线程启动阶段设置环境变量
        unsafe { std::env::set_var("DS_TOKEN", t) };
    }

    let cfg = Config::from_env()?;
    cli::run(cfg, args.prompt, args.files, args.thinking).await?;
    Ok(())
}