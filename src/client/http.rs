//! HTTP 客户端封装 + 统一鉴权头。

use crate::auth::TokenProvider;
use crate::config::{APP_VERSION, BROWSER_UA, CLIENT_BUNDLE_ID, CLIENT_VERSION};
use crate::error::{AgentError, Result};
use wreq::Client;
use wreq::header::{HeaderMap, HeaderName, HeaderValue};
use wreq_util::Emulation;

use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// 默认最小请求间隔。真人操作不会短于这个量级。
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(1200);

pub struct HttpClient {
    client: Client,
    base_url: String,
    tokens: TokenProvider,
    /// 两次请求之间的最小间隔（全局门控）
    min_interval: Duration,
    /// 上次请求发起时刻
    last_request: Mutex<Instant>,
}

impl HttpClient {
    pub fn new(
        base_url: impl Into<String>,
        timeout_secs: u64,
        tokens: TokenProvider,
    ) -> Result<Self> {
        let client = Client::builder()
            .emulation(Emulation::Chrome133)
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            tokens,
            min_interval: DEFAULT_MIN_INTERVAL,
            last_request: Mutex::new(Instant::now() - DEFAULT_MIN_INTERVAL),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn tokens_mut(&mut self) -> &mut TokenProvider {
        &mut self.tokens
    }

    /// 全局请求节流：保证距上次请求至少 min_interval，并附加随机抖动。
    /// 所有对外请求（含文件上传）都应先调用本方法。
    pub async fn throttle(&self) {
        let jitter = {
            use rand::Rng;
            let extra = self.min_interval.as_millis() as u64 / 2;
            if extra > 0 {
                Duration::from_millis(rand::rng().random_range(0..=extra))
            } else {
                Duration::ZERO
            }
        };
        let target = self.min_interval + jitter;
        let mut last = self.last_request.lock().await;
        let elapsed = last.elapsed();
        if elapsed < target {
            tokio::time::sleep(target - elapsed).await;
        }
        *last = Instant::now();
    }

    /// 节流 + 构造请求头。发请求前用这个替代 headers()。
    pub async fn prepare(&self) -> Result<HeaderMap> {
        self.throttle().await;
        self.headers()
    }

    /// 构造默认请求头（含鉴权）
    pub fn headers(&self) -> Result<HeaderMap> {
        let token = self.tokens.token()?;
        let mut h = HeaderMap::new();
        let mut set = |k: &'static str, v: &str| {
            h.insert(
                HeaderName::from_static(k),
                HeaderValue::from_str(v).unwrap_or(HeaderValue::from_static("")),
            );
        };
        set("authorization", &format!("Bearer {token}"));
        set("x-client-bundle-id", CLIENT_BUNDLE_ID);
        set("x-client-version", CLIENT_VERSION);
        set("x-client-platform", "web");
        set("x-client-locale", "zh-CN");
        set("x-model-type", "default");
        set("x-thinking-enabled", "0");
        set("x-client-timezone-offset", "28800");
        set("x-app-version", APP_VERSION);
        // --- 浏览器特征头（降低被识别为纯 API 客户端的概率） ---
        set("accept", "*/*");
        set("accept-language", "zh-CN,zh;q=0.9,en-US;q=0.8,en;q=0.7");
        set("pragma", "no-cache");
        set("priority", "u=1, i");
        set("sec-ch-ua", "\"Chromium\";v=\"133\", \"Google Chrome\";v=\"133\", \"Not?A_Brand\";v=\"99\"");
        set("sec-ch-ua-mobile", "?0");
        set("sec-ch-ua-platform", "\"Windows\"");
        set("sec-fetch-dest", "empty");
        set("sec-fetch-mode", "cors");
        set("sec-fetch-site", "same-origin");
        set("user-agent", BROWSER_UA);
        set("origin", &self.base_url);
        set("referer", &format!("{}/", self.base_url));
        Ok(h)
    }

    /// 检查 API 响应是否业务成功
    pub fn ensure_ok(code: i64, msg: &str) -> Result<()> {
        if code != 0 {
            return Err(AgentError::Api {
                code,
                msg: msg.to_string(),
            });
        }
        Ok(())
    }
}
