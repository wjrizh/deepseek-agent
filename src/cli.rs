//! 交互式 CLI。

use crate::agent::Agent;
use crate::api::file;
use crate::config::Config;
use crate::error::{AgentError, Result};
use crate::ui;
use crate::Runtime;
use rustyline::error::ReadlineError;

pub async fn run(
    cfg: Config,
    once: Option<String>,
    files: Vec<String>,
    thinking: bool,
) -> Result<()> {
    if !ui::trust_menu()? {
        return Ok(());
    }

    let rt = Runtime::new(&cfg).await?;
    let mut agent = Agent::new(rt.model());
    agent.set_thinking(thinking);

    for f in &files {
        let mut solver = rt.solver.lock().await;
        let info = file::upload(&rt.http, &mut solver, f).await?;
        drop(solver);
        println!("{}[*] 已上传: {} ({}){}", ui::DIM, info.file_name, info.id, ui::RESET);
        agent.attach_file(info.id);
    }

    if let Some(q) = once {
        agent.run(&q).await?;
        return Ok(());
    }

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    ui::print_logo(&cwd);

    let _ = crossterm::terminal::disable_raw_mode();

    let mut rl = rustyline::DefaultEditor::new()
        .map_err(|e| AgentError::Other(format!("readline: {e}")))?;
    let prompt_str = format!("{}❯{} ", ui::GREEN, ui::RESET);

    loop {
        match rl.readline(&prompt_str) {
            Ok(line) => {
                let q = line.trim();
                if q.is_empty() {
                    continue;
                }
                if matches!(q, "exit" | "quit" | "退出") {
                    break;
                }
                let _ = rl.add_history_entry(q);
                if let Err(e) = agent.run(q).await {
                    eprintln!("{}[!] 出错: {}{}", ui::RED, e, ui::RESET);
                }
            }
            Err(ReadlineError::Interrupted) => break,
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("{}[!] 输入错误: {}{}", ui::RED, e, ui::RESET);
                break;
            }
        }
    }
    println!("{}[*] 结束{}", ui::DIM, ui::RESET);
    Ok(())
}