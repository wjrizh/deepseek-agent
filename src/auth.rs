//! Token 管理：自动更新 + 持久化。
//!
//! 优先级：
//!   1. 环境变量 `DS_TOKEN`（手动指定，最高优先级）
//!   2. 持久化文件 `~/.deepseek-agent/token.json`
//!   3. 从浏览器 localStorage 自动提取（通过本地提取脚本）
//!
//! 每次启动都会校验 token 有效性；失效则尝试自动刷新。

use crate::error::{AgentError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// 持久化的 token 记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    pub token: String,
    /// 最后更新时间（Unix 秒）
    pub updated_at: u64,
    /// 来源：env / file / browser
    pub source: String,
}

impl TokenRecord {
    pub fn new(token: String, source: &str) -> Self {
        Self {
            token,
            updated_at: now_secs(),
            source: source.to_string(),
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Token 提供者。负责加载、校验、刷新、持久化。
#[derive(Debug, Clone)]
pub struct TokenProvider {
    file: std::path::PathBuf,
    profile_dir: Option<std::path::PathBuf>,
    current: Option<TokenRecord>,
}

impl TokenProvider {
    pub fn new(file: impl AsRef<Path>) -> Self {
        Self {
            file: file.as_ref().to_path_buf(),
            profile_dir: None,
            current: None,
        }
    }

    pub fn with_profile(
        file: impl AsRef<Path>,
        profile: impl AsRef<Path>,
    ) -> Self {
        Self {
            file: file.as_ref().to_path_buf(),
            profile_dir: Some(profile.as_ref().to_path_buf()),
            current: None,
        }
    }

    /// 加载 token：env -> file -> (可选)浏览器提取
    pub fn load(&mut self) -> Result<String> {
        // 1. 环境变量
        if let Ok(t) = std::env::var("DS_TOKEN") {
            let t = t.trim().to_string();
            if !t.is_empty() {
                let rec = TokenRecord::new(t.clone(), "env");
                self.current = Some(rec.clone());
                // 环境变量也持久化一份，方便下次
                let _ = self.save(&rec);
                return Ok(t);
            }
        }

        // 2. 持久化文件
        if let Ok(rec) = self.load_file() {
            self.current = Some(rec.clone());
            return Ok(rec.token);
        }

        // 3. 浏览器提取
        if let Ok(t) = extract_from_browser(self.profile_dir.as_deref()) {
            let rec = TokenRecord::new(t.clone(), "browser");
            self.current = Some(rec.clone());
            let _ = self.save(&rec);
            return Ok(t);
        }

        Err(AgentError::Auth(
            "未找到可用 token。请设置 DS_TOKEN，或先登录 https://chat.deepseek.com".into(),
        ))
    }

    pub async fn load_or_refresh(&mut self, base_url: &str) -> Result<String> {
        if let Ok(t) = std::env::var("DS_TOKEN") {
            let t = t.trim().to_string();
            if !t.is_empty() {
                let rec = TokenRecord::new(t.clone(), "env");
                self.current = Some(rec.clone());
                let _ = self.save(&rec);
                return Ok(t);
            }
        }

        if let Ok(rec) = self.load_file() {
            self.current = Some(rec.clone());
            if self.validate(base_url).await {
                return Ok(rec.token);
            }
            eprintln!("[auth] 缓存 token 已失效，尝试从浏览器重新提取...");
        }

        match extract_from_browser(self.profile_dir.as_deref()) {
            Ok(t) => {
                let rec = TokenRecord::new(t.clone(), "browser");
                self.current = Some(rec.clone());
                let _ = self.save(&rec);
                Ok(t)
            }
            Err(e) => Err(AgentError::Auth(format!(
                "无可用 token，自动提取也失败：{e}\n请设置 DS_TOKEN 或手动登录"
            ))),
        }
    }

    /// 当前 token
    pub fn token(&self) -> Result<String> {
        self.current
            .as_ref()
            .map(|r| r.token.clone())
            .ok_or_else(|| AgentError::Auth("token 未初始化".into()))
    }

    /// 更新并持久化
    pub fn update(&mut self, token: String, source: &str) -> Result<()> {
        let rec = TokenRecord::new(token, source);
        self.save(&rec)?;
        self.current = Some(rec);
        Ok(())
    }

    pub async fn validate(&self, base_url: &str) -> bool {
        let token = match self.token() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let client = wreq::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| wreq::Client::new());
        let resp = client
            .post(format!("{base_url}/api/v0/chat/create_pow_challenge"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("x-client-bundle-id", "com.deepseek.chat")
            .header("x-client-version", "2.4.0")
            .json(&serde_json::json!({"target_path": "/api/v0/file/upload_file"}))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                Ok(v) => v.get("code").and_then(|c| c.as_i64()) == Some(0),
                Err(_) => false,
            },
            _ => false,
        }
    }

    pub fn refresh_from_browser(&mut self, profile_dir: Option<&Path>) -> Result<()> {
        let t = extract_from_browser(profile_dir)?;
        self.update(t, "browser")
    }

    fn load_file(&self) -> Result<TokenRecord> {
        let data = std::fs::read_to_string(&self.file)?;
        let rec: TokenRecord = serde_json::from_str(&data)?;
        Ok(rec)
    }

    fn save(&self, rec: &TokenRecord) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let data = serde_json::to_string_pretty(rec)?;
        std::fs::write(&self.file, data)?;
        // 权限收紧（含密钥）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.file, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

fn pick_python() -> Result<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(p) = std::env::var("DEEPSEEK_PYTHON")
        && !p.trim().is_empty() {
            candidates.push(p.trim().to_string());
        }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(format!("{home}/.ligong_venv/bin/python3"));
    }
    candidates.push("python3".to_string());
    candidates.push("python".to_string());

    for cand in candidates {
        let ok = std::process::Command::new(&cand)
            .args(["-c", "import playwright"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if matches!(ok, Ok(s) if s.success()) {
            return Ok(cand);
        }
    }
    Err(AgentError::Auth(
        "未找到带 Playwright 的 Python 解释器。请安装或设置 DEEPSEEK_PYTHON".into(),
    ))
}

/// 从浏览器 localStorage 提取 userToken。
///
/// `profile_dir` 为 None 时用默认 profile（单账号兼容）。
/// 传入账号专属目录可实现多账号登录态隔离。
pub fn extract_from_browser(profile_dir: Option<&Path>) -> Result<String> {
    let script = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/extract_token.py"
    ));
    if !script.exists() {
        return Err(AgentError::Auth("浏览器提取脚本不存在".into()));
    }
    let py = pick_python()?;
    let mut cmd = std::process::Command::new(&py);
    cmd.arg(script);
    if let Some(dir) = profile_dir {
        cmd.arg("--profile").arg(dir);
    }
    let out = cmd
        .output()
        .map_err(|e| AgentError::Auth(format!("调用提取脚本失败: {e}")))?;
    if !out.status.success() {
        return Err(AgentError::Auth(format!(
            "提取脚本执行失败: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if token.is_empty() {
        return Err(AgentError::Auth("提取到的 token 为空".into()));
    }
    Ok(token)
}