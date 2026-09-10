//! 终端 UI：logo、信任菜单、spinner、流式渲染（ratatui Inline 活区）。
//!
//! 核心设计：思考内容在 ratatui `Viewport::Inline` 活区里暗灰实时滚动；
//! 答案首字到达时释放活区（光标复位到活区顶部并清除），随后回归普通
//! `print!` 流式输出，思考内容随之消失。

use crate::api::types::DeltaKind;
use crate::error::{AgentError, Result};
use crossterm::cursor::Show;
use crossterm::event::{Event, KeyCode, KeyEventKind, read};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use std::io::{IsTerminal, Write, stdout};
use std::sync::atomic::Ordering;

// ---------- ANSI 颜色 ----------
pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";
pub const GREEN: &str = "\x1b[1;32m";
pub const CYAN: &str = "\x1b[36m";
pub const RED: &str = "\x1b[31m";

const LOGO: &str = r#"          ██
          ██                      ████████████████████████
          ██                                ████
  ████████████████████████                  ████
          ██            ██                  ████
          ██            ██                  ████
          ██            ██                  ████
          ██            ██                  ████
        ████            ██                  ████
        ██              ██                  ████
      ████              ██                  ████
    ████              ████                  ████
  ████                ████                  ████
████            ████████        ████████████████████████████"#;

fn term_width() -> usize {
    size().map(|(w, _)| w as usize).unwrap_or(80)
}

fn center_pad(text: &str) -> String {
    let w = term_width();
    let len = text.chars().count();
    " ".repeat(w.saturating_sub(len) / 2)
}

/// 打印启动 logo（居中）
pub fn print_logo(cwd: &str) {
    print!("\x1b[2J\x1b[3J\x1b[H");
    let _ = stdout().flush();

    let w = term_width();
    let maxw = LOGO.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    let pad = " ".repeat(w.saturating_sub(maxw) / 2);
    for line in LOGO.lines() {
        println!("{pad}{CYAN}{line}{RESET}");
    }
    println!();
    let title = "力工 Code v1.0.0";
    println!("{}{CYAN}{title}{RESET}", center_pad(title));
    println!("{}{CYAN}{cwd}{RESET}", center_pad(cwd));
    println!();
}

/// 信任目录菜单。返回 true=信任继续，false=退出。非 TTY 直接返回 true。
pub fn trust_menu() -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(true);
    }
    let mut out = stdout();
    enable_raw_mode().map_err(|e| AgentError::Other(format!("raw mode: {e}")))?;
    let result = trust_menu_inner(&mut out);
    let _ = disable_raw_mode();
    let _ = execute!(out, Show);
    print!("\x1b[2J\x1b[3J\x1b[H");
    let _ = out.flush();
    result
}

fn trust_menu_inner(out: &mut std::io::Stdout) -> Result<bool> {
    let options = ["1. Yes, I trust this folder", "2. No, exit"];
    let mut idx = 0usize;
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    loop {
        let mut buf = String::from("\x1b[2J\x1b[H");
        buf.push_str(&format!(
            "\r\n Accessing workspace: {CYAN}{cwd}{RESET}\r\n\r\n\
             Quick safety check: Is this a project you created or one you trust?\r\n\
             (Like your own code, a well-known open source project, or work from your team).\r\n\
             If not, take a moment to review what's in this folder first.\r\n\r\n\
             力工'll be able to read, edit, and execute files here.\r\n\r\n"
        ));
        for (i, opt) in options.iter().enumerate() {
            if i == idx {
                buf.push_str(&format!(" {GREEN}❯ {opt}{RESET}\r\n"));
            } else {
                buf.push_str(&format!("   {opt}\r\n"));
            }
        }
        buf.push_str(&format!("\r\n {DIM}(↑/↓ 选择 · Enter 确认){RESET}\r\n"));
        write!(out, "{buf}").map_err(|e| AgentError::Other(e.to_string()))?;
        out.flush().map_err(|e| AgentError::Other(e.to_string()))?;

        if let Event::Key(k) = read().map_err(|e| AgentError::Other(e.to_string()))? {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Up => idx = idx.saturating_sub(1),
                KeyCode::Down => idx = (idx + 1).min(options.len() - 1),
                KeyCode::Enter => return Ok(idx == 0),
                KeyCode::Char('1') => return Ok(true),
                KeyCode::Char('2') => return Ok(false),
                KeyCode::Esc => return Ok(false),
                _ => {}
            }
        }
    }
}

/// 打印工具执行结果（暗色）。
pub fn print_tool_result(result: &str) {
    println!("{DIM}──────── tool result ────────{RESET}");
    for line in result.lines() {
        println!("{DIM}{line}{RESET}");
    }
    println!("{DIM}─────────────────────────────{RESET}");
}

/// 通用方向键选择菜单（内联，不清屏）。返回选中项索引。
/// 非 TTY 时返回 Err。prompt 可含 \n 多行。
pub fn select_menu(prompt: &str, options: &[&str]) -> Result<usize> {
    if !std::io::stdin().is_terminal() {
        return Err(AgentError::Other("非交互模式，无法选择".into()));
    }
    let mut out = stdout();
    enable_raw_mode().map_err(|e| AgentError::Other(format!("raw mode: {e}")))?;
    let prompt_lines = prompt.lines().count();
    let total_lines = prompt_lines + options.len() + 1;
    let up = total_lines.saturating_sub(1);
    let mut idx = 0usize;
    let mut first_draw = true;
    let mut dirty = true;

    let result = loop {
        if dirty {
            if !first_draw {
                let _ = write!(out, "\x1b[{up}A");
            }
            first_draw = false;
            let mut buf = String::new();
            for line in prompt.lines() {
                buf.push_str(&format!("\r\x1b[K{line}\r\n"));
            }
            for (i, opt) in options.iter().enumerate() {
                if i == idx {
                    buf.push_str(&format!("\r\x1b[K {GREEN}❯ {opt}{RESET}\r\n"));
                } else {
                    buf.push_str(&format!("\r\x1b[K   {opt}\r\n"));
                }
            }
            buf.push_str(&format!(
                "\r\x1b[K {DIM}(↑/↓ 选择 · Enter 确认 · Esc 取消){RESET}"
            ));
            if write!(out, "{buf}").and_then(|_| out.flush()).is_err() {
                break Err(AgentError::Other("输出失败".into()));
            }
            dirty = false;
        }

        if !crossterm::event::poll(std::time::Duration::from_millis(200)).unwrap_or(false) {
            if crate::agent::tool::TOOL_INTERRUPT.load(Ordering::Relaxed) {
                break Err(AgentError::Other("已中断".into()));
            }
            continue;
        }

        match read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Up => {
                    idx = idx.saturating_sub(1);
                    dirty = true;
                }
                KeyCode::Down => {
                    idx = (idx + 1).min(options.len() - 1);
                    dirty = true;
                }
                KeyCode::Enter => break Ok(idx),
                KeyCode::Char('1') if !options.is_empty() => break Ok(0),
                KeyCode::Char('2') if options.len() >= 2 => break Ok(1),
                KeyCode::Esc => break Err(AgentError::Other("已取消".into())),
                KeyCode::Char('c')
                    if k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    break Err(AgentError::Other("已中断".into()));
                }
                _ => {}
            },
            Ok(_) => {}
            Err(e) => break Err(AgentError::Other(format!("读取按键失败: {e}"))),
        }
    };

    let _ = disable_raw_mode();
    let _ = execute!(out, Show);
    let _ = write!(out, "\r\n");
    let _ = out.flush();
    result
}

/// 命令执行确认。返回 true=允许。
/// 非 TTY 下安全默认拒绝（除非 /free）。
pub fn confirm_command(cmd: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        println!("{DIM}[!] 命令未执行（非交互模式，未开启 /free）: {cmd}{RESET}");
        return Ok(false);
    }
    let prompt = format!("{RED}⚠ 即将执行命令:{RESET}\n  {CYAN}{cmd}{RESET}");
    let options = ["Yes", "No"];
    match select_menu(&prompt, &options) {
        Ok(0) => Ok(true),
        Ok(_) => Ok(false),
        Err(_) => Ok(false),
    }
}

// ---------- 流式渲染（ratatui Viewport::Inline） ----------

use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};

const VIEWPORT_HEIGHT: u16 = 12;
const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn display_height(text: &str, width: u16) -> u16 {
    use unicode_width::UnicodeWidthStr;
    if width == 0 {
        return 1;
    }
    let w = width as usize;
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    if trimmed.is_empty() {
        return 1;
    }
    let mut lines = 0usize;
    for seg in trimmed.split('\n') {
        let sw = UnicodeWidthStr::width(seg);
        lines += if sw == 0 { 1 } else { sw.div_ceil(w) };
    }
    lines.max(1) as u16
}

const MAX_NEWLINES: usize = 2;

#[derive(Default)]
struct LineCollapser {
    pending: usize,
}

impl LineCollapser {
    fn feed(&mut self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for ch in text.chars() {
            if ch == '\n' {
                self.pending += 1;
            } else {
                self.flush_pending(&mut out);
                out.push(ch);
            }
        }
        out
    }

    fn flush_pending(&mut self, out: &mut String) {
        let n = self.pending.min(MAX_NEWLINES);
        out.push_str(&"\n".repeat(n));
        self.pending = 0;
    }

    fn finish(&mut self) -> String {
        let mut out = String::new();
        self.flush_pending(&mut out);
        out
    }
}

/// 流式工具标签抑制器：从第一个工具开标签起，抑制到流结束。
/// 不依赖闭合标签（模型常漏闭合），跨 chunk 安全，保留前置文本。
struct TagFilter {
    opens: Vec<String>,
    hold: String,
    suppressing: bool,
}

impl TagFilter {
    fn new() -> Self {
        let mut opens = vec!["<tool_calls".to_string(), "<invoke".to_string()];
        for t in crate::agent::parser::KNOWN_TOOLS {
            opens.push(format!("<{t}"));
        }
        Self {
            opens,
            hold: String::new(),
            suppressing: false,
        }
    }

    fn feed(&mut self, text: &str) -> String {
        if self.suppressing {
            return String::new();
        }
        let mut data = std::mem::take(&mut self.hold);
        data.push_str(text);
        let lower = data.to_ascii_lowercase();

        let mut earliest: Option<usize> = None;
        let mut search = 0;
        while let Some(rel) = lower[search..].find('<') {
            let abs = search + rel;
            let tail = &lower[abs..];
            let end = tail.find('>').map(|p| p + 1).unwrap_or(tail.len());
            let norm = crate::agent::parser::normalize_tag_prefix(&tail[..end]);
            let norm = norm.to_ascii_lowercase();
            if self.opens.iter().any(|o| norm.starts_with(o.as_str())) {
                earliest = Some(abs);
                break;
            }
            search = abs + 1;
        }

        if let Some(pos) = earliest {
            let out = data[..pos].to_string();
            self.suppressing = true;
            self.hold.clear();
            return out;
        }

        if let Some(lt) = data.rfind('<') {
            let tail = &lower[lt..];
            let end = tail.find('>').map(|p| p + 1).unwrap_or(tail.len());
            let norm = crate::agent::parser::normalize_tag_prefix(&tail[..end]);
            let norm = norm.to_ascii_lowercase();
            if self.opens.iter().any(|o| o.starts_with(norm.as_str())) {
                let out = data[..lt].to_string();
                self.hold = data[lt..].to_string();
                return out;
            }
        }
        data
    }

    fn finish(&mut self) -> String {
        if self.suppressing {
            self.hold.clear();
            return String::new();
        }
        std::mem::take(&mut self.hold)
    }
}

impl Default for TagFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// 流式语法着色器：跨块状态机。
/// - 代码块（``` 围栏）→ 青色
/// - 日期 YYYY-MM-DD / 时间 HH:MM(:SS) → 暗青
///
/// 跨块安全：尾部可能是不完整日期/围栏时缓冲，下块续判。
struct Painter {
    in_code: bool,
    hold: String,
    code_color: &'static str,
    date_color: &'static str,
    reset: &'static str,
}

impl Painter {
    fn new() -> Self {
        Self {
            in_code: false,
            hold: String::new(),
            code_color: "\x1b[36m",
            date_color: "\x1b[2;36m",
            reset: "\x1b[0m",
        }
    }

    fn feed(&mut self, text: &str) -> String {
        let mut data = std::mem::take(&mut self.hold);
        data.push_str(text);
        let mut out = String::new();
        let mut i = 0usize;

        while i < data.len() {
            if data[i..].starts_with("```") {
                if self.in_code {
                    out.push_str(self.reset);
                    self.in_code = false;
                } else {
                    out.push_str(self.code_color);
                    self.in_code = true;
                }
                i += 3;
                while i < data.len() && data.as_bytes()[i] != b'\n' {
                    i += 1;
                }
                continue;
            }

            if i + 3 > data.len() {
                let tail = &data[i..];
                if "```".starts_with(tail) {
                    self.hold = tail.to_string();
                    break;
                }
            }

            let ch = data[i..].chars().next().unwrap();
            let ch_len = ch.len_utf8();

            if self.in_code {
                out.push(ch);
                i += ch_len;
                continue;
            }

            if let Some((matched, consumed)) = self.try_match_datetime(&data[i..]) {
                out.push_str(self.date_color);
                out.push_str(&matched);
                out.push_str(self.reset);
                i += consumed;
                continue;
            }

            if let Some(tail) = self.partial_datetime_tail(&data[i..]) {
                self.hold = tail;
                break;
            }

            out.push(ch);
            i += ch_len;
        }

        if self.in_code && i >= data.len() {
            out.push_str(self.code_color);
        }
        out
    }

    fn try_match_datetime(&self, s: &str) -> Option<(String, usize)> {
        let b = s.as_bytes();
        if b.len() >= 10
            && b[0..4].iter().all(|c| c.is_ascii_digit())
            && b[4] == b'-'
            && b[5..7].iter().all(|c| c.is_ascii_digit())
            && b[7] == b'-'
            && b[8..10].iter().all(|c| c.is_ascii_digit())
        {
            return Some((s[..10].to_string(), 10));
        }
        if b.len() >= 5
            && b[0..2].iter().all(|c| c.is_ascii_digit())
            && b[2] == b':'
            && b[3..5].iter().all(|c| c.is_ascii_digit())
        {
            let mut len = 5;
            if b.len() >= 8 && b[5] == b':' && b[6..8].iter().all(|c| c.is_ascii_digit()) {
                len = 8;
            }
            return Some((s[..len].to_string(), len));
        }
        None
    }

    fn partial_datetime_tail(&self, s: &str) -> Option<String> {
        let b = s.as_bytes();
        let n = b.len();
        let is_dateish = !s.is_empty()
            && s.chars().all(|c| c.is_ascii_digit() || c == '-' || c == ':')
            && n < 10;
        if is_dateish {
            let likely_date = s.contains('-') && s.chars().take_while(|c| c.is_ascii_digit()).count() >= 2;
            let likely_time = s.contains(':');
            if likely_date || likely_time {
                return Some(s.to_string());
            }
        }
        None
    }

    fn finish(&mut self) -> String {
        let mut out = String::new();
        let tail = std::mem::take(&mut self.hold);
        if !tail.is_empty() {
            out.push_str(&tail);
        }
        if self.in_code {
            out.push_str(self.reset);
            self.in_code = false;
        }
        out
    }
}

impl Default for Painter {
    fn default() -> Self {
        Self::new()
    }
}

/// 活区渲染器：思考在 ratatui Viewport::Inline 活区（暗灰），
/// 答案首字到达时**释放活区**，回归普通 print! 流式输出。
pub struct LiveUi {
    terminal: Option<Terminal<CrosstermBackend<std::io::Stdout>>>,
    thinking: String,
    answer_started: bool,
    tick: usize,
    viewport_top: Option<u16>,
    collapse: LineCollapser,
    filter: TagFilter,
    painter: Painter,
    prefix_printed: bool,
}

impl LiveUi {
    pub fn new() -> Result<Self> {
        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .map_err(|e| AgentError::Other(format!("ratatui init: {e}")))?;
        let mut ui = Self {
            terminal: Some(terminal),
            thinking: String::new(),
            answer_started: false,
            tick: 0,
            viewport_top: None,
            collapse: LineCollapser::default(),
            filter: TagFilter::new(),
            painter: Painter::new(),
            prefix_printed: false,
        };
        let _ = ui.draw_thinking();
        Ok(ui)
    }

    pub fn on_delta(&mut self, kind: DeltaKind, text: &str) {
        self.tick += 1;
        match kind {
            DeltaKind::Thinking => {
                if !self.answer_started {
                    self.thinking.push_str(text);
                    let _ = self.draw_thinking();
                }
            }
            DeltaKind::Answer => {
                if !self.answer_started {
                    self.answer_started = true;
                    self.end_thinking();
                }
                let filtered = self.filter.feed(text);
                let painted = self.painter.feed(&filtered);
                let out = self.collapse.feed(&painted);
                if !out.is_empty() {
                    if !self.prefix_printed {
                        self.prefix_printed = true;
                        print!("\x1b[1;32mligong > \x1b[0m");
                    }
                    print!("{out}");
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }

    fn draw_thinking(&mut self) -> std::io::Result<()> {
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(());
        };
        let thinking = self.thinking.clone();
        let spin = SPIN[self.tick % SPIN.len()];
        let mut top = None;
        terminal.draw(|frame| {
            let area = frame.area();
            top = Some(area.top());
            let content = if thinking.is_empty() {
                format!("{spin} thinking...")
            } else {
                thinking.clone()
            };
            let total = display_height(&content, area.width);
            let scroll = total.saturating_sub(area.height);
            let para = Paragraph::new(content)
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(para, area);
        })?;
        if let Some(t) = top {
            self.viewport_top = Some(t);
        }
        std::io::stdout().flush()?;
        Ok(())
    }

    fn end_thinking(&mut self) {
        if let Some(mut t) = self.terminal.take() {
            if let Some(top) = self.viewport_top {
                let _ = t.set_cursor_position(Position::new(0, top));
            }
            let _ = t.clear();
            let _ = std::io::stdout().flush();
            drop(t);
        }
    }

    pub fn finish(&mut self) {
        if self.answer_started {
            let f = self.filter.finish();
            let mut p = self.painter.feed(&f);
            p.push_str(&self.painter.finish());
            let mut tail = self.collapse.feed(&p);
            tail.push_str(&self.collapse.finish());
            if !tail.is_empty() {
                if !self.prefix_printed {
                    self.prefix_printed = true;
                    print!("\x1b[1;32mligong > \x1b[0m");
                }
                print!("{tail}");
            }
            if self.prefix_printed {
                println!();
            }
            let _ = std::io::stdout().flush();
        }
    }
}

// ---------- 兼容：非 TTY 回退 ----------

/// 无 ratatui 时（或非 TTY）的纯文本回退渲染器。
pub struct PlainUi {
    answer_started: bool,
    collapse: LineCollapser,
    filter: TagFilter,
    painter: Painter,
    prefix_printed: bool,
}

impl PlainUi {
    pub fn new() -> Self {
        Self {
            answer_started: false,
            collapse: LineCollapser::default(),
            filter: TagFilter::new(),
            painter: Painter::new(),
            prefix_printed: false,
        }
    }

    pub fn on_delta(&mut self, kind: DeltaKind, text: &str) {
        match kind {
            DeltaKind::Thinking => {}
            DeltaKind::Answer => {
                if !self.answer_started {
                    self.answer_started = true;
                }
                let filtered = self.filter.feed(text);
                let painted = self.painter.feed(&filtered);
                let out = self.collapse.feed(&painted);
                if !out.is_empty() {
                    if !self.prefix_printed {
                        self.prefix_printed = true;
                        print!("ligong > ");
                    }
                    print!("{out}");
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }

    pub fn finish(&mut self) {
        if self.answer_started {
            let f = self.filter.finish();
            let mut p = self.painter.feed(&f);
            p.push_str(&self.painter.finish());
            let mut tail = self.collapse.feed(&p);
            tail.push_str(&self.collapse.finish());
            if !tail.is_empty() {
                if !self.prefix_printed {
                    self.prefix_printed = true;
                    print!("ligong > ");
                }
                print!("{tail}");
            }
            if self.prefix_printed {
                println!();
            }
            let _ = std::io::stdout().flush();
        }
    }
}

impl Default for PlainUi {
    fn default() -> Self {
        Self::new()
    }
}

/// 根据是否 TTY 选择渲染器
pub enum AnyUi {
    Live(LiveUi),
    Plain(PlainUi),
}

impl AnyUi {
    pub fn new() -> Self {
        if std::io::stdout().is_terminal() {
            match LiveUi::new() {
                Ok(ui) => AnyUi::Live(ui),
                Err(_) => AnyUi::Plain(PlainUi::new()),
            }
        } else {
            AnyUi::Plain(PlainUi::new())
        }
    }

    pub fn on_delta(&mut self, kind: DeltaKind, text: &str) {
        match self {
            AnyUi::Live(ui) => ui.on_delta(kind, text),
            AnyUi::Plain(ui) => ui.on_delta(kind, text),
        }
    }

    pub fn finish(&mut self) {
        match self {
            AnyUi::Live(ui) => ui.finish(),
            AnyUi::Plain(ui) => ui.finish(),
        }
    }
}

impl Default for AnyUi {
    fn default() -> Self {
        Self::new()
    }
}