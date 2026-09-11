//! 文件上传 API。

use crate::api::types::{ApiResponse, FileInfo, PowChallenge, PowChallengeReq, PowResponse};
use crate::client::http::HttpClient;
use crate::client::pow::PowSolver;
use crate::error::{AgentError, Result};
use base64::Engine;


/// 已知的风控/限流业务码。命中表示请求被限制，不应盲目重试。
/// - 5: user is muted（用户被禁言/限流）
/// - 429: HTTP Too Many Requests
fn risk_code_msg(code: i64, msg: &str) -> Option<AgentError> {
    match code {
        5 | 429 => Some(AgentError::RateLimited {
            code,
            msg: msg.to_string(),
        }),
        _ => None,
    }
}

/// 解析响应顶层 code / data.biz_code，命中风控码则返回 RateLimited。
fn check_risk(text: &str) -> Option<AgentError> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let top_code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
    let top_msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
    if let Some(e) = risk_code_msg(top_code, top_msg) {
        return Some(e);
    }
    let data = v.get("data")?;
    let biz_code = data.get("biz_code").and_then(|c| c.as_i64()).unwrap_or(0);
    let biz_msg = data.get("biz_msg").and_then(|m| m.as_str()).unwrap_or("");
    risk_code_msg(biz_code, biz_msg)
}

pub const PATH_UPLOAD: &str = "/api/v0/file/upload_file";

/// 单次调用最多上传的文件数。
pub const MAX_UPLOAD_FILES: usize = 50;
/// 单个文件大小上限（100MB）。
pub const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// 上传文件，返回 FileInfo
pub async fn upload(
    http: &HttpClient,
    solver: &mut PowSolver,
    path: impl AsRef<std::path::Path>,
) -> Result<FileInfo> {
    let path = path.as_ref();
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let bytes = std::fs::read(path)?;
    let file_size = bytes.len();

    // 1. 取 PoW 挑战并求解
    let pow_header = make_pow_header(http, solver, PATH_UPLOAD).await?;

    // 2. 构造 multipart
    let part = wreq::multipart::Part::bytes(bytes).file_name(file_name.clone());
    let form = wreq::multipart::Form::new().part("file", part);

    let mut headers = http.prepare().await?;
    headers.insert("x-ds-pow-response", pow_header.parse().unwrap());
    headers.insert("x-file-size", file_size.to_string().parse().unwrap());

    let url = format!("{}{}", http.base_url(), PATH_UPLOAD);
    let resp = http
        .client()
        .post(&url)
        .headers(headers)
        .multipart(form)
        .send()
        .await?;

    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        let code = status.as_u16() as i64;
        if let Some(e) = risk_code_msg(code, &text) {
            return Err(e);
        }
        return Err(AgentError::Api { code, msg: text });
    }
    if let Some(e) = check_risk(&text) {
        return Err(e);
    }
    let parsed: ApiResponse<FileInfo> = serde_json::from_str(&text)?;
    HttpClient::ensure_ok(parsed.code, &parsed.msg)?;
    parsed
        .data
        .and_then(|d| d.biz_data)
        .ok_or_else(|| AgentError::Api {
            code: -1,
            msg: "上传响应缺少 biz_data".into(),
        })
}

/// 生成 `x-ds-pow-response` 头的值
pub async fn make_pow_header(
    http: &HttpClient,
    solver: &mut PowSolver,
    target_path: &str,
) -> Result<String> {
    let ch = create_challenge(http, target_path).await?;
    let answer = solver.solve_challenge(&ch.challenge, &ch.salt, ch.expire_at, ch.difficulty)?;
    let payload = PowResponse {
        algorithm: ch.algorithm,
        challenge: ch.challenge,
        salt: ch.salt,
        answer: answer.round() as i64,
        signature: ch.signature,
        target_path: target_path.to_string(),
    };
    let json = serde_json::to_string(&payload)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(json.as_bytes()))
}

/// 取 PoW 挑战
pub async fn create_challenge(http: &HttpClient, target_path: &str) -> Result<PowChallenge> {
    let url = format!("{}/api/v0/chat/create_pow_challenge", http.base_url());
    let mut headers = http.prepare().await?;
    headers.insert("content-type", "application/json".parse().unwrap());

    let resp = http
        .client()
        .post(&url)
        .headers(headers)
        .json(&PowChallengeReq {
            target_path: target_path.to_string(),
        })
        .send()
        .await?;
    let text = resp.text().await?;
    if let Some(e) = check_risk(&text) {
        return Err(e);
    }
    let parsed: ApiResponse<serde_json::Value> = serde_json::from_str(&text)?;
    HttpClient::ensure_ok(parsed.code, &parsed.msg)?;
    let ch_val = parsed
        .data
        .and_then(|d| d.biz_data)
        .and_then(|v| v.get("challenge").cloned())
        .ok_or_else(|| AgentError::Api {
            code: -1,
            msg: "挑战响应缺少 challenge".into(),
        })?;
    Ok(serde_json::from_value(ch_val)?)
}
