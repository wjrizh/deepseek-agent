//! 工具调用解析器 —— 高鲁棒性，抵抗大模型幻觉。
//!
//! 同时支持两种格式。
//!
//! DeepSeek 原生格式（模型默认输出，优先识别）：
//!
//! ```text
//! <tool_calls>
//! <invoke name="execute_command">
//! <parameter name="command" string="true">ls -la</parameter>
//!   </invoke>
//!   </tool_calls>
//!
//! 2) 简易格式（自定义提示词，向后兼容）：
//!   <execute_command>
//!   <command>ls -la</command>
//!   </execute_command>
//!
//! 鲁棒性设计：
//! - 只识别 `KNOWN_TOOLS` 中的工具名；未知工具 → Malformed。
//! - 大小写不敏感；容忍空白、换行、markdown 代码围栏。
//! - 缺参 / 空参 / 多工具 / 未闭合 → `Malformed`，交由上层有界重试。
//! - 参数内容（尤其 command）原样保留，只裁首尾空白。
//! - 审批不在此处：由 CLI 的 `/free` 全局开关控制，模型无权决定。

use std::collections::HashMap;

/// 已知工具名（唯一真源；新增工具需在此登记 + 实现 `Tool`）。
pub const KNOWN_TOOLS: &[&str] = &["execute_command"];

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub params: HashMap<String, String>,
    /// 规整后的原始标签文本（用于回注 / 日志）
    pub raw: String,
}

#[derive(Debug)]
pub enum ParseOutcome {
    /// 普通回答，无工具调用
    Text(String),
    /// 成功解析出一个工具调用
    Call(ToolCall),
    /// 疑似工具调用但结构/参数不合法 → 需重试
    Malformed { raw: String, reason: String },
}

/// 解析模型的完整输出。
pub fn parse(text: &str) -> ParseOutcome {
    let cleaned = strip_fences(text);
    // 优先：DeepSeek 原生格式
    if let Some(outcome) = parse_native(&cleaned) {
        return outcome;
    }
    // 回退：简易格式
    parse_simple(&cleaned, text)
}

// ---------- DeepSeek 原生格式 ----------

fn parse_native(text: &str) -> Option<ParseOutcome> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("<invoke") && !lower.contains("<tool_calls") {
        return None;
    }

    let names = collect_invoke_names(&lower);
    if names.is_empty() {
        return Some(ParseOutcome::Malformed {
            raw: text.trim().to_string(),
            reason: "检测到 <tool_calls> 但缺少 <invoke name=...>".into(),
        });
    }
    if names.len() > 1 {
        return Some(ParseOutcome::Malformed {
            raw: text.trim().to_string(),
            reason: "一次只能调用一个工具".into(),
        });
    }

    let name = &names[0];
    let canonical = match KNOWN_TOOLS.iter().find(|t| t.eq_ignore_ascii_case(name)) {
        Some(t) => (*t).to_string(),
        None => {
            return Some(ParseOutcome::Malformed {
                raw: text.trim().to_string(),
                reason: format!("未知工具: {name}"),
            });
        }
    };

    let body = extract_element(text, "invoke").unwrap_or_default();
    let params = extract_parameters(&body);

    match validate(&canonical, &params) {
        Ok(()) => Some(ParseOutcome::Call(ToolCall {
            name: canonical,
            params,
            raw: text.trim().to_string(),
        })),
        Err(reason) => Some(ParseOutcome::Malformed {
            raw: text.trim().to_string(),
            reason,
        }),
    }
}

/// 收集所有 `<invoke ... name="X" ...>` 的工具名（小写）。
fn collect_invoke_names(lower: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut i = 0;
    while let Some(rel) = lower[i..].find("<invoke") {
        let start = i + rel;
        let after = start + "<invoke".len();
        let boundary_ok = matches!(
            lower[after..].chars().next(),
            Some(c) if c == '>' || c == '/' || c.is_whitespace()
        );
        if !boundary_ok {
            i = after;
            continue;
        }
        let Some(gt) = lower[start..].find('>') else { break };
        let open_tag = &lower[start..start + gt + 1];
        if let Some(n) = extract_attr(open_tag, "name") {
            names.push(n);
        }
        i = start + gt + 1;
    }
    names
}

/// 提取 `<parameter name="Y" ...>VALUE</parameter>` 为参数表（同名保留首个）。
fn extract_parameters(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let lower = body.to_ascii_lowercase();
    let mut i = 0;
    while let Some(rel) = lower[i..].find("<parameter") {
        let start = i + rel;
        let after = start + "<parameter".len();
        let boundary_ok = matches!(
            lower[after..].chars().next(),
            Some(c) if c == '>' || c == '/' || c.is_whitespace()
        );
        if !boundary_ok {
            i = after;
            continue;
        }
        let Some(gt) = lower[start..].find('>') else { break };
        let open_end = start + gt + 1;
        let open_tag = &body[start..open_end];
        let key = match extract_attr(open_tag, "name") {
            Some(k) => k.to_ascii_lowercase(),
            None => {
                i = open_end;
                continue;
            }
        };
        let Some(crel) = lower[open_end..].find("</parameter>") else {
            i = open_end;
            continue;
        };
        let content_end = open_end + crel;
        let value = body[open_end..content_end].trim().to_string();
        map.entry(key).or_insert(value);
        i = content_end + "</parameter>".len();
    }
    map
}

/// 从标签里取属性值，兼容双引号 / 单引号 / 无引号。
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let pat = format!("{attr}=");
    let idx = lower.find(&pat)?;
    let rest = tag[idx + pat.len()..].trim_start();
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let inner = &rest[1..];
        let end = inner.find(quote)?;
        Some(inner[..end].to_string())
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

// ---------- 简易格式（向后兼容） ----------

fn parse_simple(cleaned: &str, original: &str) -> ParseOutcome {
    let Some((name, _)) = find_open_tag(cleaned) else {
        return ParseOutcome::Text(original.trim().to_string());
    };
    if count_open_tags(cleaned) > 1 {
        return ParseOutcome::Malformed {
            raw: original.trim().to_string(),
            reason: "一次只能调用一个工具".into(),
        };
    }
    let Some(body) = extract_element(cleaned, &name) else {
        return ParseOutcome::Malformed {
            raw: original.trim().to_string(),
            reason: format!("未找到 <{name}> 的闭合标签"),
        };
    };
    let params = extract_params(&body);
    match validate(&name, &params) {
        Ok(()) => ParseOutcome::Call(ToolCall {
            name,
            params,
            raw: original.trim().to_string(),
        }),
        Err(reason) => ParseOutcome::Malformed {
            raw: original.trim().to_string(),
            reason,
        },
    }
}

// ---------- 通用工具 ----------

/// 去掉整体包裹的 markdown 代码围栏（```xml ... ```）。
fn strip_fences(text: &str) -> String {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = match rest.find('\n') {
            Some(i) => &rest[i + 1..],
            None => rest,
        };
        if let Some(end) = rest.rfind("```") {
            return rest[..end].trim().to_string();
        }
    }
    t.to_string()
}

/// 找到最早出现的已知工具开标签，返回 (工具名, 字节下标)。
fn find_open_tag(text: &str) -> Option<(String, usize)> {
    let lower = text.to_ascii_lowercase();
    let mut best: Option<(String, usize)> = None;
    for tool in KNOWN_TOOLS {
        let pat = format!("<{tool}");
        let mut from = 0;
        while let Some(rel) = lower[from..].find(&pat) {
            let idx = from + rel;
            let after = idx + pat.len();
            let boundary_ok = matches!(
                lower[after..].chars().next(),
                Some(c) if c == '>' || c == '/' || c.is_whitespace()
            );
            if boundary_ok {
                if best.as_ref().is_none_or(|(_, b)| idx < *b) {
                    best = Some((tool.to_string(), idx));
                }
                break;
            }
            from = after;
        }
    }
    best
}

/// 统计整段文本中出现的已知工具开标签数量。
fn count_open_tags(text: &str) -> usize {
    let lower = text.to_ascii_lowercase();
    let mut count = 0;
    for tool in KNOWN_TOOLS {
        let pat = format!("<{tool}");
        let mut from = 0;
        while let Some(rel) = lower[from..].find(&pat) {
            let idx = from + rel;
            let after = idx + pat.len();
            if matches!(
                lower[after..].chars().next(),
                Some(c) if c == '>' || c == '/' || c.is_whitespace()
            ) {
                count += 1;
            }
            from = after;
        }
    }
    count
}

/// 提取 `<name ...>内容</name>` 的内容（原样，不 trim 内部）。
fn extract_element(haystack: &str, name: &str) -> Option<String> {
    let lower = haystack.to_ascii_lowercase();
    let open_pat = format!("<{name}");
    let mut from = 0;
    let open_start = loop {
        let idx = lower[from..].find(&open_pat)? + from;
        let after = idx + open_pat.len();
        match lower[after..].chars().next() {
            Some(c) if c == '>' || c == '/' || c.is_whitespace() => break idx,
            Some(_) => from = after,
            None => return None,
        }
    };
    let gt = lower[open_start..].find('>')?;
    let content_start = open_start + gt + 1;
    let close_pat = format!("</{name}");
    let close_rel = lower[content_start..].find(&close_pat)?;
    Some(haystack[content_start..content_start + close_rel].to_string())
}

/// 扫描标签体，提取所有 `<key>value</key>` 为参数表（同名保留首个）。
fn extract_params(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let lower = body.to_ascii_lowercase();
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find('<') {
        let lt = i + rel;
        if lower[lt..].starts_with("</") {
            match lower[lt..].find('>') {
                Some(g) => {
                    i = lt + g + 1;
                    continue;
                }
                None => break,
            }
        }
        let name_start = lt + 1;
        let name_end = match lower[name_start..]
            .find(|c: char| c == '>' || c == '/' || c.is_whitespace())
        {
            Some(r) => name_start + r,
            None => break,
        };
        let key = lower[name_start..name_end].trim().to_string();
        let Some(g) = lower[name_end..].find('>') else {
            break;
        };
        let open_end = name_end + g + 1;
        let close_pat = format!("</{key}");
        let Some(crel) = lower[open_end..].find(&close_pat) else {
            i = open_end;
            continue;
        };
        let content_end = open_end + crel;
        let value = body[open_end..content_end].trim().to_string();
        if !key.is_empty() {
            map.entry(key).or_insert(value);
        }
        i = match lower[content_end..].find('>') {
            Some(x) => content_end + x + 1,
            None => content_end,
        };
    }
    map
}

/// 参数校验（L2）。
fn validate(name: &str, params: &HashMap<String, String>) -> std::result::Result<(), String> {
    match name {
        "execute_command" => {
            let cmd = params.get("command").map(|s| s.trim()).unwrap_or("");
            if cmd.is_empty() {
                return Err("缺少非空的 <command> 参数".into());
            }
            Ok(())
        }
        _ => Err(format!("未知工具: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_text() {
        assert!(matches!(parse("你好，这是普通回答。"), ParseOutcome::Text(_)));
    }

    // ---- DeepSeek 原生格式 ----

    #[test]
    fn native_basic() {
        let r = parse(
            "<tool_calls>\n<invoke name=\"execute_command\">\n\
             <parameter name=\"command\" string=\"true\">ls -la</parameter>\n\
             </invoke>\n</tool_calls>",
        );
        match r {
            ParseOutcome::Call(c) => {
                assert_eq!(c.name, "execute_command");
                assert_eq!(c.params.get("command").unwrap(), "ls -la");
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn native_without_wrapper() {
        let r = parse(
            "<invoke name=\"execute_command\">\
             <parameter name=\"command\">pwd</parameter></invoke>",
        );
        match r {
            ParseOutcome::Call(c) => assert_eq!(c.params.get("command").unwrap(), "pwd"),
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn native_unknown_tool_is_malformed() {
        let r = parse("<invoke name=\"rm_rf\"><parameter name=\"command\">x</parameter></invoke>");
        assert!(matches!(r, ParseOutcome::Malformed { .. }));
    }

    #[test]
    fn native_missing_param_is_malformed() {
        let r = parse("<invoke name=\"execute_command\"></invoke>");
        assert!(matches!(r, ParseOutcome::Malformed { .. }));
    }

    // ---- 简易格式 ----

    #[test]
    fn basic_command() {
        let r = parse("<execute_command>\n<command>ls -la</command>\n</execute_command>");
        match r {
            ParseOutcome::Call(c) => {
                assert_eq!(c.name, "execute_command");
                assert_eq!(c.params.get("command").unwrap(), "ls -la");
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn case_insensitive_and_fenced() {
        let r = parse("```xml\n<EXECUTE_COMMAND><COMMAND>pwd</COMMAND></EXECUTE_COMMAND>\n```");
        match r {
            ParseOutcome::Call(c) => assert_eq!(c.params.get("command").unwrap(), "pwd"),
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn unclosed_is_malformed() {
        let r = parse("<execute_command><command>ls</command>");
        assert!(matches!(r, ParseOutcome::Malformed { .. }));
    }

    #[test]
    fn double_tag_is_malformed() {
        let r = parse(
            "<execute_command><command>a</command></execute_command>\
             <execute_command><command>b</command></execute_command>",
        );
        assert!(matches!(r, ParseOutcome::Malformed { .. }));
    }

    #[test]
    fn multiline_command_preserved() {
        let r = parse("<execute_command>\n<command>echo a\necho b</command>\n</execute_command>");
        match r {
            ParseOutcome::Call(c) => assert_eq!(c.params.get("command").unwrap(), "echo a\necho b"),
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn text_mention_is_text() {
        assert!(matches!(
            parse("你可以用 execute_command 来跑命令。"),
            ParseOutcome::Text(_)
        ));
    }
}