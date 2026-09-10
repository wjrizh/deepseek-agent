//! 终端 UI：logo、信任菜单、spinner、流式渲染（ratatui Inline 活区）。
//!
//! 核心设计：思考内容在 ratatui `Viewport::Inline` 活区里暗灰实时滚动；
//! 答案首字到达时释放活区（光标复位到活区顶部并清除），随后回归普通
//! `print!` 流式输出，思考内容随之消失。

use crate::api::types::DeltaKind;
use crate::error::{AgentError, Result};
use crossterm::cursor::Show;
use crossterm::event::{read, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use crossterm::execute;
use std::io::{stdout, IsTerminal, Write};

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

/// 活区渲染器：思考在 ratatui Viewport::Inline 活区（暗灰），
/// 答案首字到达时**释放活区**，回归普通 print! 流式输出。
pub struct LiveUi {
    terminal: Option<Terminal<CrosstermBackend<std::io::Stdout>>>,
    thinking: String,
    answer_started: bool,
    tick: usize,
    viewport_top: Option<u16>,
    collapse: LineCollapser,
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
                let out = self.collapse.feed(text);
                if !out.is_empty() {
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
        print!("\x1b[1;32mligong > \x1b[0m");
        let _ = std::io::stdout().flush();
    }

    pub fn finish(&mut self) {
        if self.answer_started {
            let tail = self.collapse.finish();
            if !tail.is_empty() {
                print!("{tail}");
            }
            if !tail.ends_with('\n') {
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
}

impl PlainUi {
    pub fn new() -> Self {
        Self {
            answer_started: false,
            collapse: LineCollapser::default(),
        }
    }

    pub fn on_delta(&mut self, kind: DeltaKind, text: &str) {
        match kind {
            DeltaKind::Thinking => {
                let _ = kind;
                let _ = text;
            }
            DeltaKind::Answer => {
                if !self.answer_started {
                    self.answer_started = true;
                    print!("ligong > ");
                }
                let out = self.collapse.feed(text);
                if !out.is_empty() {
                    print!("{out}");
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }

    pub fn finish(&mut self) {
        if self.answer_started {
            let tail = self.collapse.finish();
            if !tail.is_empty() {
                print!("{tail}");
            }
            if !tail.ends_with('\n') {
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