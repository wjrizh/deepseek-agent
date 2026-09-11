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

    /// 指定账号（覆盖默认）
    #[arg(long)]
    account: Option<String>,

    /// 新增账号（交互式登录后保存）
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    add_account: Option<String>,

    /// 删除账号
    #[arg(long)]
    del_account: Option<String>,

    /// 列出账号
    #[arg(long)]
    list_accounts: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if let Some(t) = &args.token {
        unsafe { std::env::set_var("DS_TOKEN", t) };
    }

    // 账号子命令优先
    if let Some(name) = &args.del_account {
        let mut mgr = deepseek_agent::accounts::AccountManager::load_default()?;
        mgr.remove(name)?;
        println!("已删除账号: {name}");
        return Ok(());
    }
    if args.list_accounts {
        let mgr = deepseek_agent::accounts::AccountManager::load_default()?;
        let def = mgr.default_account();
        for a in mgr.list() {
            let mark = if Some(&a.name) == def.as_ref() { "●" } else { " " };
            println!("{mark} {}", a.name);
        }
        return Ok(());
    }
    if let Some(name) = &args.add_account {
        deepseek_agent::cli::add_account_interactive(name).await?;
        return Ok(());
    }

    // 决定用哪个账号：--account > last_account > 交互选择
    let cfg = if let Some(name) = &args.account {
        Config::for_account(name)?
    } else {
        let mgr = deepseek_agent::accounts::AccountManager::load_default()?;
        if mgr.list().is_empty() {
            Config::from_env()?
        } else {
            let name = mgr
                .default_account()
                .or_else(|| mgr.list().first().map(|a| a.name.clone()))
                .unwrap();
            Config::for_account(&name)?
        }
    };
    cli::run(cfg, args.prompt, args.files, args.thinking).await?;
    Ok(())
}