//! 交互式 CLI。

use crate::Runtime;
use crate::agent::Agent;
use crate::agent::session_store::SessionStore;
use crate::api::chat;
use crate::api::file;
use crate::config::Config;
use crate::error::{AgentError, Result};
use crate::ui;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::{Context, Helper};
use rustyline::validate::Validator;

fn extract_at_paths(input: &str) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut paths = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < input.len() {
        let ch = input[i..].chars().next().unwrap();
        if ch != '@' {
            text.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let after = i + 1;
        if after >= input.len() {
            text.push('@');
            break;
        }
        let rest = &input[after..];
        let (path, consumed) = if let Some(stripped) = rest.strip_prefix('"') {
            match stripped.find('"') {
                Some(end) => (stripped[..end].to_string(), end + 2),
                None => {
                    text.push('@');
                    i = after;
                    continue;
                }
            }
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (rest[..end].to_string(), end)
        };
        let _ = bytes;
        paths.push(path);
        i = after + consumed;
    }
    (text.trim().to_string(), paths)
}

const COMMANDS: &[&str] = &[
    "/help", "/new", "/sessions", "/switch", "/delete",
    "/think", "/search", "/free", "/safe", "exit", "quit",
];

const HELP_TEXT: &str = "\
命令：
  /help              显示本帮助
  /new               开新会话
  /sessions          列出在线历史会话（●可续接 / ○仅归档）
  /switch <id前缀>   切换会话（Tab 可补全）
  /delete <id...>    删除 1 个或多个会话（前缀匹配，需确认）
  /think [on|off]    开关思考模式（无参则切换）
  /search [on|off]   开关联网搜索（无参则切换）
  /free              自动批准后续命令（免确认）
  /safe              恢复命令确认
  exit / quit / 退出  退出
  @<路径>            内联附加文件，如: 总结 @./doc.pdf（Tab 补全）
快捷键：
  Tab                补全命令 / 会话 id";

struct AgentCompleter {
    sessions: Vec<(String, String)>,
}

impl Completer for AgentCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        let start = head
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let word = &head[start..];
        let mut out = Vec::new();

        // @路径 补全（含目录列举）
        if let Some(pfx) = word.strip_prefix('@') {
            let (dir, name_pfx) = match pfx.rfind('/') {
                Some(idx) => (&pfx[..=idx], &pfx[idx + 1..]),
                None => ("", pfx),
            };
            let base = if dir.is_empty() { "." } else { dir };
            if let Ok(rd) = std::fs::read_dir(base) {
                for entry in rd.flatten() {
                    let fname = entry.file_name().to_string_lossy().to_string();
                    if !fname.starts_with(name_pfx) {
                        continue;
                    }
                    if fname.starts_with('.') && !name_pfx.starts_with('.') {
                        continue;
                    }
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let disp = if is_dir {
                        format!("{fname}/")
                    } else {
                        fname.clone()
                    };
                    let repl = format!("@{dir}{fname}{}", if is_dir { "/" } else { "" });
                    out.push(Pair {
                        display: disp,
                        replacement: repl,
                    });
                }
            }
            return Ok((start, out));
        }

        let head_trim = head[..start].trim_start();
        if let Some(cmd) = head_trim.split_whitespace().next()
            && matches!(cmd, "/switch" | "/delete")
        {
            let pfx = word.trim();
            // /switch 已输入 id 后，补 "tree"
            if cmd == "/switch" {
                let before = head[..start].trim_end();
                let tokens: Vec<&str> = before.split_whitespace().collect();
                if tokens.len() == 2 && "tree".starts_with(pfx) {
                    out.push(Pair {
                        display: "tree".to_string(),
                        replacement: "tree".to_string(),
                    });
                    return Ok((start, out));
                }
            }
            for (id, title) in &self.sessions {
                if id.starts_with(pfx) {
                    let short = &id[..id.len().min(8)];
                    let label = if title.is_empty() { "(无标题)" } else { title.as_str() };
                    out.push(Pair {
                        display: format!("{short}  {label}"),
                        replacement: id.clone(),
                    });
                }
            }
            return Ok((start, out));
        }

        if start == 0 && (word.starts_with('/') || !word.starts_with(' ')) {
            for c in COMMANDS {
                if c.starts_with(word) {
                    out.push(Pair {
                        display: c.to_string(),
                        replacement: c.to_string(),
                    });
                }
            }
        }
        Ok((start, out))
    }
}

impl Helper for AgentCompleter {}
impl Hinter for AgentCompleter {
    type Hint = String;
}
impl Highlighter for AgentCompleter {}
impl Validator for AgentCompleter {}

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
    let store = SessionStore::load(&cfg.session_file);
    let mut agent = Agent::new(rt.model()).with_session_store(store);
    agent.set_thinking(thinking);

    for f in &files {
        let mut solver = rt.solver.lock().await;
        let info = file::upload(&rt.http, &mut solver, f).await?;
        drop(solver);
        println!(
            "{}[*] 已上传: {} ({}){}",
            ui::DIM,
            info.file_name,
            info.id,
            ui::RESET
        );
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

    let sessions = match chat::list_sessions(&rt.http).await {
        Ok(l) => l.into_iter().map(|s| (s.id, s.title)).collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    let helper = AgentCompleter { sessions };
    let mut rl = rustyline::Editor::with_config(
        rustyline::Config::builder()
            .completion_type(rustyline::CompletionType::List)
            .build(),
    )
    .map_err(|e| AgentError::Other(format!("readline: {e}")))?;
    rl.set_helper(Some(helper));
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
                if q == "/help" {
                    println!("{}{}{}", ui::CYAN, HELP_TEXT, ui::RESET);
                    continue;
                }
                if let Some(rest) = q.strip_prefix("/think") {
                    let arg = rest.trim();
                    let on = match arg {
                        "on" => true,
                        "off" => false,
                        "" => !agent.thinking(),
                        _ => {
                            eprintln!("{}[!] 用法: /think [on|off]{}", ui::RED, ui::RESET);
                            continue;
                        }
                    };
                    agent.set_thinking(on);
                    println!(
                        "{}[✓] 思考模式: {}{}",
                        ui::GREEN,
                        if on { "开" } else { "关" },
                        ui::RESET
                    );
                    continue;
                }
                if let Some(rest) = q.strip_prefix("/search") {
                    let arg = rest.trim();
                    let on = match arg {
                        "on" => true,
                        "off" => false,
                        "" => !agent.search(),
                        _ => {
                            eprintln!("{}[!] 用法: /search [on|off]{}", ui::RED, ui::RESET);
                            continue;
                        }
                    };
                    agent.set_search(on);
                    println!(
                        "{}[✓] 联网搜索: {}{}",
                        ui::GREEN,
                        if on { "开" } else { "关" },
                        ui::RESET
                    );
                    continue;
                }
                if q == "/free" {
                    agent.set_free_mode(true);
                    println!(
                        "{}[✓] /free 已开启：后续命令自动批准{}",
                        ui::GREEN,
                        ui::RESET
                    );
                    continue;
                }
                if q == "/safe" {
                    agent.set_free_mode(false);
                    println!("{}[✓] 已切回安全模式：命令需确认{}", ui::GREEN, ui::RESET);
                    continue;
                }
                if q == "/new" {
                    match agent.new_session().await {
                        Ok(id) => {
                            println!(
                                "{}[✓] 已开新会话 {}{}",
                                ui::GREEN,
                                &id[..id.len().min(8)],
                                ui::RESET
                            );
                            let sessions = match chat::list_sessions(&rt.http).await {
                                Ok(l) => l.into_iter().map(|s| (s.id, s.title)).collect(),
                                Err(_) => Vec::new(),
                            };
                            rl.set_helper(Some(AgentCompleter { sessions }));
                        }
                        Err(e) => eprintln!("{}[!] 新建失败: {e}{}", ui::RED, ui::RESET),
                    }
                    continue;
                }
                if q == "/sessions" {
                    match chat::list_sessions(&rt.http).await {
                        Ok(list) => {
                            if list.is_empty() {
                                println!("{}（无历史会话）{}", ui::DIM, ui::RESET);
                            }
                            for s in &list {
                                let mark = if agent.store_contains(&s.id) {
                                    "●"
                                } else {
                                    "○"
                                };
                                let short = &s.id[..s.id.len().min(8)];
                                let title = if s.title.is_empty() {
                                    "(无标题)"
                                } else {
                                    &s.title
                                };
                                println!(
                                    "{}{} {}  {}{}{}",
                                    ui::CYAN,
                                    mark,
                                    short,
                                    title,
                                    ui::DIM,
                                    if s.pinned { " [置顶]" } else { "" }
                                );
                                print!("{}", ui::RESET);
                            }
                            println!(
                                "{}(● = 有本地上下文可完美续接 · ○ = 仅归档){}",
                                ui::DIM,
                                ui::RESET
                            );
                            let sessions = list.iter().map(|s| (s.id.clone(), s.title.clone())).collect();
                            rl.set_helper(Some(AgentCompleter { sessions }));
                        }
                        Err(e) => eprintln!("{}[!] 获取会话列表失败: {e}{}", ui::RED, ui::RESET),
                    }
                    continue;
                }
                if let Some(arg) = q.strip_prefix("/switch ") {
                    let arg = arg.trim();
                    // 解析「<id前缀> [tree]」
                    let mut parts = arg.split_whitespace();
                    let id_arg = parts.next().unwrap_or("").to_string();
                    let want_tree = parts.next() == Some("tree");
                    match chat::list_sessions(&rt.http).await {
                        Ok(list) => {
                            let hits: Vec<_> = list
                                .iter()
                                .filter(|s| s.id.starts_with(&id_arg))
                                .collect();
                            let target = match hits.as_slice() {
                                [] => {
                                    eprintln!(
                                        "{}[!] 无匹配会话: {}{}",
                                        ui::RED, arg, ui::RESET
                                    );
                                    continue;
                                }
                                [one] => (*one).clone(),
                                _ => {
                                    eprintln!(
                                        "{}[!] 前缀不唯一，匹配 {} 个，请补全{}",
                                        ui::RED,
                                        hits.len(),
                                        ui::RESET
                                    );
                                    continue;
                                }
                            };

                            // /switch <id> tree —— 列出分支供选择
                            if want_tree {
                                match chat::session_branches(&rt.http, &target.id).await {
                                    Ok(branches) if !branches.is_empty() => {
                                        println!(
                                            "{}会话 {} · {} 个分支{}",
                                            ui::DIM,
                                            &target.id[..target.id.len().min(8)],
                                            branches.len(),
                                            ui::RESET
                                        );
                                        let mut options: Vec<String> = Vec::new();
                                        for (i, b) in branches.iter().enumerate() {
                                            let mark = if b.is_current { "●" } else { " " };
                                            let label = format!(
                                                "[{}] {} {} · {}（{} 条）",
                                                i + 1,
                                                mark,
                                                b.leaf_mid,
                                                if b.summary.is_empty() { "(空)".into() } else { b.summary.clone() },
                                                b.size
                                            );
                                            options.push(label);
                                        }
                                        let opts_ref: Vec<&str> =
                                            options.iter().map(|s| s.as_str()).collect();
                                        match crate::ui::select_menu("选择要续接的分支：", &opts_ref) {
                                            Ok(idx) => {
                                                let chosen = &branches[idx];
                                                let parent = Some(chosen.leaf_mid);
                                                match agent.switch_session(target.id.clone(), parent) {
                                                    Ok(()) => println!(
                                                        "{}[✓] 已切到分支 mid={} · {}{}",
                                                        ui::GREEN,
                                                        chosen.leaf_mid,
                                                        chosen.summary,
                                                        ui::RESET
                                                    ),
                                                    Err(e) => eprintln!(
                                                        "{}[!] 切换失败: {e}{}",
                                                        ui::RED,
                                                        ui::RESET
                                                    ),
                                                }
                                            }
                                            Err(_) => println!("{}已取消{}", ui::DIM, ui::RESET),
                                        }
                                    }
                                    Ok(_) => println!("{}(该会话无分支){}", ui::DIM, ui::RESET),
                                    Err(e) => eprintln!(
                                        "{}[!] 拉取分支失败: {e}{}",
                                        ui::RED,
                                        ui::RESET
                                    ),
                                }
                                continue;
                            }

                            let local_parent = agent.store_parent_of(&target.id);
                            let (online_latest, recent) =
                                match chat::history_tail(&rt.http, &target.id, 3).await {
                                    Ok(x) => (x.0, x.1),
                                    Err(e) => {
                                        eprintln!(
                                            "{}[!] 拉取历史失败: {e}{}",
                                            ui::RED,
                                            ui::RESET
                                        );
                                        continue;
                                    }
                                };
                            let parent = local_parent.or(online_latest);
                            if parent.is_none() {
                                eprintln!(
                                    "{}[!] 该会话无可续接的 parent，将从新开始（历史仍在服务端）{}",
                                    ui::RED,
                                    ui::RESET
                                );
                            }
                            match agent.switch_session(target.id.clone(), parent) {
                                Ok(()) => {
                                    println!(
                                        "{}[✓] 已切换到 {} · {}{}",
                                        ui::GREEN,
                                        &target.id[..target.id.len().min(8)],
                                        target.title,
                                        ui::RESET
                                    );
                                    if !recent.is_empty() {
                                        println!(
                                            "{}── 最近 {} 条 ──{}",
                                            ui::DIM,
                                            recent.len(),
                                            ui::RESET
                                        );
                                        for m in &recent {
                                            let who = match m.role.as_str() {
                                                "USER" => "你",
                                                "ASSISTANT" => "ligong",
                                                _ => &m.role,
                                            };
                                            let color = if m.role == "USER" {
                                                ui::CYAN
                                            } else {
                                                ui::GREEN
                                            };
                                            let raw = if m.role == "USER" {
                                                let seg = m
                                                    .content
                                                    .rsplit("\n\n")
                                                    .next()
                                                    .unwrap_or(&m.content);
                                                if seg.chars().count() > 200 {
                                                    seg.chars().take(200).collect()
                                                } else {
                                                    seg.to_string()
                                                }
                                            } else {
                                                m.content.clone()
                                            };
                                            let cleaned = raw.replace('\n', " ");
                                            let total = cleaned.chars().count();
                                            let mut text: String =
                                                cleaned.trim().chars().take(80).collect();
                                            if total > 80 {
                                                text.push('…');
                                            }
                                            println!(
                                                "{}{} > {}{}",
                                                color, who, text, ui::RESET
                                            );
                                        }
                                    } else {
                                        println!(
                                            "{}(该会话无历史消息){}",
                                            ui::DIM,
                                            ui::RESET
                                        );
                                    }
                                }
                                Err(e) => {
                                    eprintln!(
                                        "{}[!] 切换失败: {e}{}",
                                        ui::RED,
                                        ui::RESET
                                    )
                                }
                            }
                        }
                        Err(e) => eprintln!(
                            "{}[!] 获取会话列表失败: {e}{}",
                            ui::RED,
                            ui::RESET
                        ),
                    }
                    continue;
                }
                if let Some(arg) = q.strip_prefix("/delete ") {
                    let prefixes: Vec<&str> = arg.split_whitespace().collect();
                    if prefixes.is_empty() {
                        println!("{}用法: /delete <id前缀> [id前缀...]{}", ui::DIM, ui::RESET);
                        continue;
                    }
                    let list = match chat::list_sessions(&rt.http).await {
                        Ok(l) => l,
                        Err(e) => {
                            eprintln!("{}[!] 获取会话列表失败: {e}{}", ui::RED, ui::RESET);
                            continue;
                        }
                    };
                    let mut targets: Vec<chat::OnlineSession> = Vec::new();
                    let mut bad = false;
                    for pfx in &prefixes {
                        let hits: Vec<_> =
                            list.iter().filter(|s| s.id.starts_with(pfx)).collect();
                        match hits.as_slice() {
                            [] => {
                                eprintln!("{}[!] 无匹配: {}{}", ui::RED, pfx, ui::RESET);
                                bad = true;
                            }
                            [one] => {
                                if !targets.iter().any(|t| t.id == one.id) {
                                    targets.push((*one).clone());
                                }
                            }
                            _ => {
                                eprintln!(
                                    "{}[!] 前缀不唯一: {}（匹配 {} 个）{}",
                                    ui::RED, pfx, hits.len(), ui::RESET
                                );
                                bad = true;
                            }
                        }
                    }
                    if bad || targets.is_empty() {
                        continue;
                    }
                    let mut prompt = format!(
                        "{}将删除以下 {} 个会话（不可恢复）：{}",
                        ui::RED,
                        targets.len(),
                        ui::RESET
                    );
                    for t in &targets {
                        prompt.push_str(&format!(
                            "\n  {}{}  {}{}",
                            ui::CYAN,
                            &t.id[..t.id.len().min(8)],
                            t.title,
                            ui::RESET
                        ));
                    }
                    let confirmed = matches!(ui::select_menu(&prompt, &["确认删除", "取消"]), Ok(0));
                    if !confirmed {
                        println!("{}已取消{}", ui::DIM, ui::RESET);
                        continue;
                    }
                    let current = agent.session_id().map(|s| s.to_string());
                    let mut deleted_current = false;
                    for t in &targets {
                        match chat::delete_session(&rt.http, &t.id).await {
                            Ok(()) => {
                                let _ = agent.store_remove(&t.id);
                                if current.as_deref() == Some(t.id.as_str()) {
                                    deleted_current = true;
                                }
                                println!(
                                    "{}[✓] 已删除 {}{}",
                                    ui::GREEN,
                                    &t.id[..t.id.len().min(8)],
                                    ui::RESET
                                );
                            }
                            Err(e) => eprintln!(
                                "{}[!] 删除 {} 失败: {e}{}",
                                ui::RED,
                                &t.id[..t.id.len().min(8)],
                                ui::RESET
                            ),
                        }
                    }
                    if deleted_current {
                        match agent.new_session().await {
                            Ok(id) => println!(
                                "{}[✓] 当前会话已删，已开新会话 {}{}",
                                ui::GREEN,
                                &id[..id.len().min(8)],
                                ui::RESET
                            ),
                            Err(e) => {
                                eprintln!("{}[!] 新建失败: {e}{}", ui::RED, ui::RESET)
                            }
                        }
                    }
                    let sessions = match chat::list_sessions(&rt.http).await {
                        Ok(l) => l.into_iter().map(|s| (s.id, s.title)).collect(),
                        Err(_) => Vec::new(),
                    };
                    rl.set_helper(Some(AgentCompleter { sessions }));
                    continue;
                }
                let _ = rl.add_history_entry(q);

                let (text, paths) = extract_at_paths(q);
                for p in &paths {
                    let path = std::path::Path::new(p);
                    if !path.exists() {
                        eprintln!("{}[!] 文件不存在: {}{}", ui::RED, p, ui::RESET);
                        continue;
                    }
                    let mut solver = rt.solver.lock().await;
                    let r = file::upload(&rt.http, &mut solver, path).await;
                    drop(solver);
                    match r {
                        Ok(info) => {
                            println!(
                                "{}[*] 已附加: {} ({}){}",
                                ui::DIM, info.file_name, info.id, ui::RESET
                            );
                            agent.attach_file(info.id);
                        }
                        Err(e) => {
                            eprintln!("{}[!] 上传失败 {}: {e}{}", ui::RED, p, ui::RESET)
                        }
                    }
                }
                let final_input = if text.is_empty() && !paths.is_empty() {
                    "请分析我上传的文件".to_string()
                } else {
                    text
                };
                if final_input.is_empty() {
                    continue;
                }
                if let Err(e) = agent.run(&final_input).await {
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