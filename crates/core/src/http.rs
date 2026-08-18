use anyhow::Context;
use reqwest::{Client, Method, Proxy};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub headers: HashMap<String, String>,
}

impl HttpResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&want))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone)]
pub struct Http {
    client: Client,
}

impl Http {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_proxy(load_proxy())
    }

    pub fn with_proxy(proxy: Option<String>) -> anyhow::Result<Self> {
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("MultiMeters/", env!("CARGO_PKG_VERSION")));
        if let Some(url) = proxy {
            builder = builder.proxy(Proxy::all(&url).context("invalid proxy URL")?);
        }
        Ok(Self {
            client: builder.build()?,
        })
    }

    pub async fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
        timeout_secs: u64,
    ) -> anyhow::Result<HttpResponse> {
        let mut req = self
            .client
            .request(method, url)
            .timeout(Duration::from_secs(timeout_secs));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(body) = body {
            req = req.body(body);
        }
        let res = req.send().await?;
        let status = res.status().as_u16();
        let mut hdrs = HashMap::new();
        for (k, v) in res.headers() {
            if let Ok(val) = v.to_str() {
                hdrs.insert(k.as_str().to_string(), val.to_string());
            }
        }
        let body = res.bytes().await?.to_vec();
        Ok(HttpResponse {
            status,
            body,
            headers: hdrs,
        })
    }

    pub async fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<HttpResponse> {
        self.send(Method::GET, url, headers, None, 15).await
    }

    pub async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        json: &Value,
    ) -> anyhow::Result<HttpResponse> {
        let mut hdrs: Vec<(&str, &str)> = headers.to_vec();
        if !hdrs
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            hdrs.push(("Content-Type", "application/json"));
        }
        self.send(
            Method::POST,
            url,
            &hdrs,
            Some(serde_json::to_vec(json)?),
            15,
        )
        .await
    }
}

fn load_proxy() -> Option<String> {
    let path = crate::paths::proxy_config_path();
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| {
            value
                .get("proxy")
                .and_then(|proxy| proxy.as_str())
                .or_else(|| value.get("url").and_then(|url| url.as_str()))
                .filter(|url| !url.trim().is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
                .into_iter()
                .find_map(|name| std::env::var(name).ok())
                .filter(|url| !url.trim().is_empty())
        })
}
