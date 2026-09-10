//! 工具调用解析器 —— 高鲁棒性，抵抗大模型幻觉。
//!
//! 支持两种格式：
//!
//! 1) DeepSeek 原生格式（优先识别）：
//!    最外层 tool_calls 包裹一个 invoke（name 属性指定工具名），
//!    invoke 内部每个 parameter（name 属性指定参数名）元素承载一个参数值。
//!
//! 2) 简易格式（自定义提示词，向后兼容）：
//!    工具开标签 execute_command 内嵌 command 参数元素。
//!
//! 鲁棒性设计：
//! - 只识别 KNOWN_TOOLS 中的工具名；未知工具 → Malformed。
//! - 大小写不敏感；容忍空白、换行、markdown 代码围栏。
//! - 闭合标签宽松匹配：允许标签名前后有多余空白、大小写混用，
//!   甚至完全缺失闭合标签（退回到下一个边界）。
//! - 缺参 / 空参 / 多工具 → Malformed，交由上层有界重试。
//! - 参数内容（尤其 command）原样保留，只裁首尾空白。
//! - 审批不在此处：由 CLI 的 /free 全局开关控制，模型无权决定。

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
    // 归一化标签名：剥离 DSML / 命名空间 / 管道装饰，只动 <...> 内部，不碰正文
    let cleaned = normalize_tag_names(&cleaned);
    // 优先：DeepSeek 原生格式
    if let Some(outcome) = parse_native(&cleaned) {
        return outcome;
    }
    // 回退：简易格式
    parse_simple(&cleaned, text)
}

// ---------- 标签名归一化（专有标记 / 命名空间 / 管道装饰） ----------

/// 归一化标签名：只重写尖括号内部，剥离标签名前后的厂商标记、
/// 管道符、命名空间前缀，正文（标签之外）一个字节都不动。
/// 行为见 tests 中的 dsml/管道/冒号 三个回归测试。
fn normalize_tag_names(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(lt) = rest.find('<') {
        // 正文部分原样拷贝
        out.push_str(&rest[..lt]);
        let tail = &rest[lt..];

        // 找标签的 '>'（跳过引号内的 '>'，避免被属性值误导）
        match find_tag_gt(tail) {
            Some(gt) => {
                let tag = &tail[..=gt]; // 含 '<' 和 '>'
                out.push_str(&rewrite_tag(tag));
                rest = &tail[gt + 1..];
            }
            None => {
                // 没有闭合 '>'，剩余全部原样
                out.push_str(tail);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 公开：归一化标签里的 DSML 厂商标记（`｜｜DSML｜｜` / `||DSML||`，含全角竖线）。
/// 供 UI 层的 TagFilter 等复用，避免逻辑漂移。
/// 输入可以是带 '<' 的完整标签（如 `<｜｜DSML｜｜ invoke`）或纯标签名区。
pub fn normalize_tag_prefix(tag: &str) -> String {
    let had_lt = tag.trim_start().starts_with('<');
    let t = tag.trim_start();
    let t = t.strip_prefix('<').unwrap_or(t);
    // 跳过 `<` 后所有非 ASCII 字母字符（竖线/空格/DSML 标记统统跳过）
    let t = t.trim_start_matches(|c: char| !c.is_ascii_alphabetic() && c != '/');
    if had_lt {
        format!("<{t}")
    } else {
        t.to_string()
    }
}

/// 在 `s`（以 '<' 开头）中找标签的 '>'，跳过引号内的 '>'。
/// 返回相对下标。
fn find_tag_gt(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c == '>' {
            return Some(i);
        }
    }
    None
}

/// 重写单个标签，只清理标签名，保留属性原样。
fn rewrite_tag(tag: &str) -> String {
    // tag 形如 "<...内容...>"
    let inner = &tag[1..tag.len() - 1]; // 去掉 '<' '>'
    let (closing, after_slash) = match inner.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, inner),
    };

    // 1. 成对剥离厂商标记块：形如 ｜｜DSML｜｜ 或 ||DSML||
    //    跳过前导竖线 → 跳到闭合竖线 → 跳过闭合竖线，得到真正的标签名区。
    let mut t = after_slash.trim_start();
    let is_pipe = |c: char| c == '|' || c == '｜';
    if t.starts_with(is_pipe) {
        let s1 = t.trim_start_matches(is_pipe);
        if let Some(close_rel) = s1.find(is_pipe) {
            let after = &s1[close_rel..];
            t = after.trim_start_matches(is_pipe);
        } else {
            t = s1;
        }
    }
    let trimmed = t.trim_start();

    // 2. 只在第一个空白之前（纯标签名区）找命名空间冒号
    let name_zone_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let name_zone = &trimmed[..name_zone_end];
    // name_zone 是 trimmed 的前缀，偏移可直接用于 trimmed。
    let name_start = name_zone.rfind(':').map(|p| p + 1).unwrap_or(0);
    let name_src = &trimmed[name_start..];

    // 3. 取标识符（字母/数字/下划线）
    let name_len = name_src
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(name_src.len());
    let name = &name_src[..name_len];
    if name.is_empty() {
        return tag.to_string(); // 不是合法标签，原样返回
    }

    // 4. 标签名之后的部分（属性）原样保留
    let after_name = &name_src[name_len..];

    let mut out = String::with_capacity(tag.len());
    out.push('<');
    if closing {
        out.push('/');
    }
    out.push_str(name);
    out.push_str(after_name);
    out.push('>');
    out
}

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

/// 收集所有 invoke 开标签上的 name 属性（小写）。
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
        let Some(gt) = lower[start..].find('>') else {
            break;
        };
        let open_tag = &lower[start..start + gt + 1];
        if let Some(n) = extract_attr(open_tag, "name") {
            names.push(n);
        }
        i = start + gt + 1;
    }
    names
}

/// 提取 parameter 元素为参数表（同名保留首个）。
///
/// 关键鲁棒性：**不依赖精确闭合标签**。模型的真实输出里闭合标签常带
/// 多余空白或大小写变体（例如标签名与斜杠之间、标签名与右尖括号之间
/// 多一个空格），精确 `find` 会失配并丢弃整个参数。这里改用宽松匹配，
/// 并在完全找不到闭合标签时退回到下一个标签边界。
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
        let Some(gt) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + gt + 1;
        let open_tag = &body[start..open_end];
        let key = match extract_attr(open_tag, "name") {
            Some(k) => k.to_ascii_lowercase(),
            None => {
                i = open_end;
                continue;
            }
        };

        // 宽松闭合：优先找合法的 parameter 闭合标签变体；
        // 找不到则退到下一个标签边界，避免整个参数丢失。
        let (content_end, next_i) = match find_lenient_close(&lower, open_end, "parameter") {
            Some((s, e)) => (s, e),
            None => {
                let end = next_boundary(&lower, open_end);
                (end, end)
            }
        };
        let value = body[open_end..content_end].trim().to_string();
        map.entry(key).or_insert(value);
        i = next_i;
    }
    map
}

/// 宽松匹配闭合标签：允许标签名前后有空白、大小写混用。
///
/// 可匹配（假设 name = "parameter"）：
///   </parameter>      规范
///   </ parameter>     斜杠与标签名之间有空白
///   </parameter >     标签名与右尖括号之间有空白
///   </ parameter >    两者都有
///   </PARAMETER>      大小写混用
///
/// 返回 `(内容结束下标, 闭合标签之后的下标)`。
fn find_lenient_close(lower: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let mut i = from;
    while let Some(rel) = lower[i..].find("</") {
        let lt = i + rel;
        let mut p = lt + 2;
        // 跳过斜杠与标签名之间的空白。
        while let Some(c) = lower[p..].chars().next() {
            if c.is_whitespace() {
                p += c.len_utf8();
            } else {
                break;
            }
        }
        if lower[p..].starts_with(name) {
            let after_name = p + name.len();
            // 排除同前缀误匹配（例如 parameterx）。
            let boundary_ok = matches!(
                lower[after_name..].chars().next(),
                Some(c) if c == '>' || c.is_whitespace() || c == '/'
            );
            if boundary_ok && let Some(gt) = lower[after_name..].find('>') {
                let close_end = after_name + gt + 1;
                return Some((lt, close_end));
            }
        }
        i = lt + 2;
    }
    None
}

/// 完全没有闭合标签时的兜底：截到下一个标签起点；没有则到串尾。
fn next_boundary(lower: &str, from: usize) -> usize {
    lower[from..]
        .find('<')
        .map(|r| from + r)
        .unwrap_or(lower.len())
}

/// 从标签里取属性值，兼容双引号 / 单引号 / 无引号。
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let pat = attr.to_string();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(&pat) {
        let idx = from + rel;
        let prev_ok = idx == 0
            || tag[..idx]
                .chars()
                .last()
                .is_some_and(|c| c.is_whitespace() || c == '<');
        if !prev_ok {
            from = idx + pat.len();
            continue;
        }
        let after = idx + pat.len();
        let rest = tag[after..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            from = after;
            continue;
        };
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote == '"' || quote == '\'' {
            let inner = &rest[1..];
            let end = inner.find(quote)?;
            return Some(inner[..end].to_string());
        } else {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(rest.len());
            return Some(rest[..end].to_string());
        }
    }
    None
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

/// 去掉整体包裹的 markdown 代码围栏。
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

/// 提取某个元素的内部内容（原样，不 trim 内部）。
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

/// 扫描标签体，提取所有简易格式的参数元素（同名保留首个）。
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
        let name_end =
            match lower[name_start..].find(|c: char| c == '>' || c == '/' || c.is_whitespace()) {
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
        assert!(matches!(
            parse("你好，这是普通回答。"),
            ParseOutcome::Text(_)
        ));
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

    /// 回归：闭合标签带空白 / 大小写变体时，参数不能被丢弃。
    #[test]
    fn native_lenient_close_tag_variants() {
        let variants = [
            "</parameter>",   // 规范
            "</ parameter>",  // 斜杠后多空格
            "</parameter >",  // 名字后多空格
            "</ parameter >", // 两处都多空格
            "</PARAMETER>",   // 大写
        ];
        for close in variants {
            let input = format!(
                "<tool_calls>\n<invoke name=\"execute_command\">\n\
                 <parameter name=\"command\" string=\"true\">pwd{close}\n\
                 </invoke>\n</tool_calls>"
            );
            match parse(&input) {
                ParseOutcome::Call(c) => {
                    assert_eq!(c.params.get("command").unwrap(), "pwd", "close={close:?}")
                }
                other => panic!("close={close:?} expected Call, got {other:?}"),
            }
        }
    }

    /// 回归：完全没有闭合标签时，退回到下一个边界，而非整段丢失。
    #[test]
    fn native_missing_close_tag_uses_next_boundary() {
        let input = "<invoke name=\"execute_command\">\
                     <parameter name=\"command\">pwd</invoke>";
        match parse(input) {
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

    // ---- 标签名归一化回归 ----

    /// 全角竖线包裹的厂商标记应被归一化，参数正常提取。
    #[test]
    fn dsml_full_width_is_normalized() {
        let fw = '\u{FF5C}'; // 全角竖线
        let input = format!(
            "<tool_calls>\n\
             <{fw}{fw}DSML{fw}{fw} invoke name=\"execute_command\">\n\
             <{fw}{fw}DSML{fw}{fw} parameter name=\"command\" string=\"true\">ls -la\
             </{fw}{fw}DSML{fw}{fw} parameter>\n\
             </{fw}{fw}DSML{fw}{fw} invoke>\n\
             </tool_calls>"
        );
        match parse(&input) {
            ParseOutcome::Call(c) => {
                assert_eq!(c.name, "execute_command");
                assert_eq!(c.params.get("command").unwrap(), "ls -la");
            }
            other => panic!("DSML 归一化失败: {other:?}"),
        }
    }

    /// 正文里的逻辑或与标记字面量必须原样保留。
    #[test]
    fn body_pipes_not_corrupted() {
        let fw = '\u{FF5C}';
        let input = format!(
            "<tool_calls>\n<invoke name=\"execute_command\">\n\
             <parameter name=\"command\">echo \"a\" || echo \"{fw}{fw}\"</parameter>\n\
             </invoke>\n</tool_calls>"
        );
        match parse(&input) {
            ParseOutcome::Call(c) => {
                assert_eq!(
                    c.params.get("command").unwrap().as_str(),
                    format!("echo \"a\" || echo \"{fw}{fw}\"").as_str()
                );
            }
            other => panic!("正文被破坏: {other:?}"),
        }
    }

    /// 属性值含冒号（URL / 端口）时，标签名仍须正确提取。
    #[test]
    fn attr_value_with_colon_not_corrupted() {
        let fw = '\u{FF5C}';
        let input = format!(
            "<tool_calls>\n\
             <{fw}{fw}DSML{fw}{fw} invoke name=\"execute_command\">\n\
             <{fw}{fw}DSML{fw}{fw} parameter name=\"command\" default=\"http://localhost:8080/x\">\
             curl http://localhost:8080</{fw}{fw}DSML{fw}{fw} parameter>\n\
             </{fw}{fw}DSML{fw}{fw} invoke>\n\
             </tool_calls>"
        );
        match parse(&input) {
            ParseOutcome::Call(c) => {
                assert_eq!(c.name, "execute_command");
                assert_eq!(
                    c.params.get("command").unwrap(),
                    "curl http://localhost:8080"
                );
            }
            other => panic!("属性冒号被误伤: {other:?}"),
        }
    }
}
