//! HTTP 客户端封装 + 统一鉴权头。

use crate::auth::TokenProvider;
use crate::config::{APP_VERSION, BROWSER_UA, CLIENT_BUNDLE_ID, CLIENT_VERSION};
use crate::error::{AgentError, Result};
use wreq::Client;
use wreq::header::{HeaderMap, HeaderName, HeaderValue};
use wreq_util::Emulation;

pub struct HttpClient {
    client: Client,
    base_url: String,
    tokens: TokenProvider,
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
