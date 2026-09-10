//! 会话 & 对话 API（completion SSE 流）。

use crate::api::types::{ApiResponse, ChatEvent, ChatSessionWrapper, CompletionReq, DeltaKind};
use crate::client::http::HttpClient;
use crate::client::pow::PowSolver;
use crate::client::sse;
use crate::error::{AgentError, Result};
use futures_util::StreamExt;

pub const PATH_SESSION_CREATE: &str = "/api/v0/chat_session/create";
pub const PATH_COMPLETION: &str = "/api/v0/chat/completion";

/// 新建会话，返回 session_id
pub async fn create_session(http: &HttpClient) -> Result<String> {
    let url = format!("{}{}", http.base_url(), PATH_SESSION_CREATE);
    let mut headers = http.headers()?;
    headers.insert("content-type", "application/json".parse().unwrap());

    let resp = http
        .client()
        .post(&url)
        .headers(headers)
        .json(&serde_json::json!({}))
        .send()
        .await?;
    let text = resp.text().await?;
    let parsed: ApiResponse<ChatSessionWrapper> = serde_json::from_str(&text)?;
    HttpClient::ensure_ok(parsed.code, &parsed.msg)?;
    parsed
        .data
        .and_then(|d| d.biz_data)
        .map(|w| w.chat_session.id)
        .ok_or_else(|| AgentError::Api {
            code: -1,
            msg: "会话响应缺少 id".into(),
        })
}

/// 发一轮消息，返回 (完整回复, response_message_id)。
/// `parent_message_id` 传上一轮的 response_message_id 以延续上下文。
pub async fn completion(
    http: &HttpClient,
    solver: &mut PowSolver,
    req: &CompletionReq,
    on_delta: &mut (dyn for<'a> FnMut(DeltaKind, &'a str) + Send),
) -> Result<(String, Option<i64>)>
{
    let pow_header =
        crate::api::file::make_pow_header(http, solver, PATH_COMPLETION).await?;

    let url = format!("{}{}", http.base_url(), PATH_COMPLETION);
    let mut headers = http.headers()?;
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("x-ds-pow-response", pow_header.parse().unwrap());

    let resp = http
        .client()
        .post(&url)
        .headers(headers)
        .json(req)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(AgentError::Api {
            code: status.as_u16() as i64,
            msg: text,
        });
    }

    let mut stream = resp.bytes_stream();
    let mut parser = sse::SseParser::new();
    let mut buf = String::new();
    let mut full = String::new();
    let mut resp_id = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim_end_matches('\r').to_string();
            buf.drain(..=pos);

            if line.is_empty() {
                continue;
            }
            if let Some(json_str) = line.strip_prefix("data:") {
                let json_str = json_str.trim();
                if json_str == "[DONE]" {
                    return Ok((full, resp_id));
                }
                match parser.parse_line(json_str) {
                    Some(ChatEvent::Ready { response_message_id }) => {
                        resp_id = Some(response_message_id);
                    }
                    Some(ChatEvent::Delta(kind, text)) => {
                        on_delta(kind, &text);
                        if kind == DeltaKind::Answer {
                            full.push_str(&text);
                        }
                    }
                    _ => {}
                }
            } else if let Some(ev) = line.strip_prefix("event:")
                && sse::is_close_event(ev.trim())
            {
                return Ok((full, resp_id));
            }
        }
    }

    if !buf.trim().is_empty() {
        let line = buf.trim();
        if let Some(json_str) = line.strip_prefix("data:") {
            let json_str = json_str.trim();
            if json_str != "[DONE]" {
                match parser.parse_line(json_str) {
                    Some(ChatEvent::Ready { response_message_id }) => {
                        resp_id = Some(response_message_id);
                    }
                    Some(ChatEvent::Delta(kind, text)) => {
                        on_delta(kind, &text);
                        if kind == DeltaKind::Answer {
                            full.push_str(&text);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok((full, resp_id))
}