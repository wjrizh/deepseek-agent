//! SSE 流解析。DeepSeek 的格式：
//!   event: ready
//!   data: {"response_message_id":2,...}
//!   data: {"v":{"response":{"fragments":[{"type":"THINK",...}]}}}   ← 初始对象
//!   data: {"p":"response/fragments","o":"APPEND","v":[{"type":"RESPONSE",...}]}  ← 新 fragment
//!   data: {"p":"response/fragments/-1/content","o":"APPEND","v":"正文"}          ← 追加
//!   data: {"v":"正文"}                                              ← 追加(当前 fragment)
//!   data: {"p":"response/status","o":"SET","v":"FINISHED"}          ← 状态(跳过)
//!
//! 关键：维护"当前 fragment 类型"，把增量归到 Thinking 或 Answer。

use crate::api::types::{ChatEvent, DeltaKind};
use serde_json::Value;

#[derive(Debug, Default)]
pub struct SseParser {
    current: Option<DeltaKind>,
}

impl SseParser {
    pub fn new() -> Self {
        Self { current: None }
    }

    pub fn parse_line(&mut self, json_str: &str) -> Option<ChatEvent> {
        let v: Value = serde_json::from_str(json_str).ok()?;

        if let Some(rid) = v.get("response_message_id").and_then(|x| x.as_i64()) {
            return Some(ChatEvent::Ready { response_message_id: rid });
        }

        let op = v.get("o").and_then(|x| x.as_str());
        let path = v.get("p").and_then(|x| x.as_str()).unwrap_or("");
        let val = v.get("v")?;

        if let Some((kind, content)) = extract_fragment(val) {
            self.current = Some(kind);
            if !content.is_empty() {
                return Some(ChatEvent::Delta(kind, content));
            }
            return Some(ChatEvent::Other);
        }

        if let Some(s) = val.as_str() {
            if path == "response/status" || path == "quasi_status" || path == "response" {
                return None;
            }
            if op == Some("APPEND") || op.is_none() {
                let kind = self.current.unwrap_or(DeltaKind::Answer);
                return Some(ChatEvent::Delta(kind, s.to_string()));
            }
        }

        None
    }
}

fn extract_fragment(val: &Value) -> Option<(DeltaKind, String)> {
    let frags: Option<&Vec<Value>> = if let Some(resp) = val.get("response") {
        resp.get("fragments").and_then(|f| f.as_array())
    } else {
        val.as_array()
    };
    let arr = frags?;
    let last = arr.last()?;
    let kind = match last.get("type").and_then(|t| t.as_str()) {
        Some("THINK") => DeltaKind::Thinking,
        Some("RESPONSE") => DeltaKind::Answer,
        _ => return None,
    };
    let content = last
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    Some((kind, content))
}

pub fn is_close_event(event_name: &str) -> bool {
    event_name == "close"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_think_then_answer() {
        let mut p = SseParser::new();

        // 初始 THINK 带 content → 应输出
        let e = p.parse_line(
            r#"{"v":{"response":{"fragments":[{"type":"THINK","content":"思考"}]}}}"#,
        );
        assert!(matches!(e, Some(ChatEvent::Delta(DeltaKind::Thinking, s)) if s == "思考"));

        let e = p.parse_line(r#"{"v":"更多思考"}"#).unwrap();
        assert!(matches!(e, ChatEvent::Delta(DeltaKind::Thinking, s) if s == "更多思考"));

        // 新 RESPONSE fragment 带 content → 应输出
        let e = p.parse_line(
            r#"{"p":"response/fragments","o":"APPEND","v":[{"type":"RESPONSE","content":"答案"}]}"#,
        );
        assert!(matches!(e, Some(ChatEvent::Delta(DeltaKind::Answer, s)) if s == "答案"));

        let e = p.parse_line(r#"{"v":"更多答案"}"#).unwrap();
        assert!(matches!(e, ChatEvent::Delta(DeltaKind::Answer, s) if s == "更多答案"));
    }

    #[test]
    fn skips_status() {
        let mut p = SseParser::new();
        assert!(p
            .parse_line(r#"{"p":"response/status","o":"SET","v":"FINISHED"}"#)
            .is_none());
    }

    #[test]
    fn content_path_delta() {
        let mut p = SseParser::new();
        p.parse_line(r#"{"v":{"response":{"fragments":[{"type":"THINK"}]}}}"#);
        let e = p.parse_line(r#"{"p":"response/fragments/-1/content","o":"APPEND","v":"你好"}"#);
        assert!(matches!(e, Some(ChatEvent::Delta(DeltaKind::Thinking, s)) if s == "你好"));
    }

    #[test]
    fn fragment_append_carries_content() {
        let mut p = SseParser::new();
        p.parse_line(r#"{"v":{"response":{"fragments":[{"type":"THINK"}]}}}"#);
        let e = p.parse_line(
            r#"{"p":"response/fragments","o":"APPEND","v":[{"type":"RESPONSE","content":"9"}]}"#,
        );
        assert!(
            matches!(e, Some(ChatEvent::Delta(DeltaKind::Answer, s)) if s == "9"),
            "fragment 自带的 content 必须输出"
        );
    }
}