//! 文件上传 API。

use crate::api::types::{ApiResponse, FileInfo, PowChallenge, PowChallengeReq, PowResponse};
use crate::client::http::HttpClient;
use crate::client::pow::PowSolver;
use crate::error::{AgentError, Result};
use base64::Engine;

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

    let mut headers = http.headers()?;
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
        return Err(AgentError::Api {
            code: status.as_u16() as i64,
            msg: text,
        });
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
    let mut headers = http.headers()?;
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
