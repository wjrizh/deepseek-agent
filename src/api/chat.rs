//! 会话 & 对话 API（completion SSE 流）。

use crate::api::types::{ApiResponse, ChatEvent, ChatSessionWrapper, CompletionReq, DeltaKind};
use crate::client::http::HttpClient;
use crate::client::pow::PowSolver;
use crate::client::sse;
use crate::error::{AgentError, Result};
use futures_util::StreamExt;

pub const PATH_SESSION_CREATE: &str = "/api/v0/chat_session/create";
pub const PATH_COMPLETION: &str = "/api/v0/chat/completion";
pub const PATH_SESSION_LIST: &str = "/api/v0/chat_session/fetch_page";
pub const PATH_HISTORY: &str = "/api/v0/chat/history_messages";
pub const PATH_SESSION_DELETE: &str = "/api/v0/chat_session/delete";

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
) -> Result<(String, Option<i64>, Option<String>)> {
    let pow_header = crate::api::file::make_pow_header(http, solver, PATH_COMPLETION).await?;

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
let mut title: Option<String> = None;

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
                    return Ok((full, resp_id, title));
                }
                match parser.parse_line(json_str) {
                    Some(ChatEvent::Ready {
                        response_message_id,
                    }) => {
                        resp_id = Some(response_message_id);
                    }
                    Some(ChatEvent::Delta(kind, text)) => {
                        on_delta(kind, &text);
                        if kind == DeltaKind::Answer {
                            full.push_str(&text);
                        }
                    }
                    Some(ChatEvent::Title(t)) => {
                        title = Some(t);
                    }
                    _ => {}
                }
            } else if let Some(ev) = line.strip_prefix("event:")
                && sse::is_close_event(ev.trim())
            {
                return Ok((full, resp_id, title));
            }
        }
    }

    if !buf.trim().is_empty() {
        let line = buf.trim();
        if let Some(json_str) = line.strip_prefix("data:") {
            let json_str = json_str.trim();
            if json_str != "[DONE]" {
                match parser.parse_line(json_str) {
                    Some(ChatEvent::Ready {
                        response_message_id,
                    }) => {
                        resp_id = Some(response_message_id);
                    }
                    Some(ChatEvent::Delta(kind, text)) => {
                        on_delta(kind, &text);
                        if kind == DeltaKind::Answer {
                            full.push_str(&text);
                        }
                    }
                    Some(ChatEvent::Title(t)) => {
                        title = Some(t);
                    }
                    _ => {}
                }
            }
        }
    }

    Ok((full, resp_id, title))
}

/// 在线会话摘要
#[derive(Debug, Clone)]
pub struct OnlineSession {
    pub id: String,
    pub title: String,
    pub updated_at: i64,
    pub model_type: String,
    pub pinned: bool,
}

/// 列出全部在线会话
pub async fn list_sessions(http: &HttpClient) -> Result<Vec<OnlineSession>> {
    let url = format!("{}{}", http.base_url(), PATH_SESSION_LIST);
    let headers = http.headers()?;
    let resp = http.client().get(&url).headers(headers).send().await?;
    let text = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let arr = v["data"]["biz_data"]["chat_sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|s| {
            Some(OnlineSession {
                id: s["id"].as_str()?.to_string(),
                title: s["title"].as_str().unwrap_or("").to_string(),
                updated_at: s["updated_at"].as_f64().unwrap_or(0.0) as i64,
                model_type: s["model_type"].as_str().unwrap_or("default").to_string(),
                pinned: s["pinned"].as_bool().unwrap_or(false),
            })
        })
        .collect())
}

/// 取会话最新 message_id（用于切换后作为 parent 续接上下文）
pub async fn latest_message_id(http: &HttpClient, session_id: &str) -> Result<Option<i64>> {
    let url = format!(
        "{}{}?chat_session_id={}",
        http.base_url(),
        PATH_HISTORY,
        session_id
    );
    let headers = http.headers()?;
    let resp = http.client().get(&url).headers(headers).send().await?;
    let text = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let code = v["code"].as_i64().unwrap_or(-1);
    HttpClient::ensure_ok(code, v["msg"].as_str().unwrap_or(""))?;
    let mid = v["data"]["biz_data"]["chat_session"]["current_message_id"].as_i64();
    Ok(mid)
}

/// 会话里的单条历史消息（精简）
#[derive(Debug, Clone)]
pub struct HistoryMessage {
    pub message_id: i64,
    pub role: String,
    pub content: String,
}

/// 取会话最新 message_id + 最近 N 条消息
pub async fn history_tail(
    http: &HttpClient,
    session_id: &str,
    n: usize,
) -> Result<(Option<i64>, Vec<HistoryMessage>)> {
    let url = format!(
        "{}{}?chat_session_id={}",
        http.base_url(),
        PATH_HISTORY,
        session_id
    );
    let headers = http.headers()?;
    let resp = http.client().get(&url).headers(headers).send().await?;
    let text = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    HttpClient::ensure_ok(v["code"].as_i64().unwrap_or(-1), v["msg"].as_str().unwrap_or(""))?;
    let bd = &v["data"]["biz_data"];
    let latest = bd["chat_session"]["current_message_id"].as_i64();

    let mut msgs: Vec<HistoryMessage> = bd["chat_messages"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let role = m["role"].as_str().unwrap_or("").to_string();
                    let content = if let Some(frags) = m["fragments"].as_array() {
                        let want = if role == "USER" { "REQUEST" } else { "RESPONSE" };
                        frags
                            .iter()
                            .filter(|f| f["type"].as_str() == Some(want))
                            .filter_map(|f| f["content"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    } else {
                        m["content"].as_str().unwrap_or("").to_string()
                    };
                    Some(HistoryMessage {
                        message_id: m["message_id"].as_i64()?,
                        role,
                        content,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let start = msgs.len().saturating_sub(n);
    msgs.drain(..start);
    Ok((latest, msgs))
}

pub async fn delete_session(http: &HttpClient, session_id: &str) -> Result<()> {
    let url = format!("{}{}", http.base_url(), PATH_SESSION_DELETE);
    let mut headers = http.headers()?;
    headers.insert("content-type", "application/json".parse().unwrap());
    let resp = http
        .client()
        .post(&url)
        .headers(headers)
        .json(&serde_json::json!({"chat_session_id": session_id}))
        .send()
        .await?;
    let text = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    HttpClient::ensure_ok(v["code"].as_i64().unwrap_or(-1), v["msg"].as_str().unwrap_or(""))?;
    Ok(())
}