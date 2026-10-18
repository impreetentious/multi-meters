use crate::models::{FailureCategory, RefreshFailure};
use anyhow::Context;
use reqwest::{Client, Method, Proxy};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub headers: HashMap<String, String>,
    pub received_at: chrono::DateTime<chrono::Utc>,
}

impl HttpResponse {
    pub fn refresh_failure(&self) -> Option<RefreshFailure> {
        if self.ok() {
            return None;
        }
        let category = match self.status {
            401 | 403 => FailureCategory::Authentication,
            429 => FailureCategory::RateLimit,
            400..=499 => FailureCategory::Configuration,
            _ => FailureCategory::Transient,
        };
        Some(RefreshFailure {
            category,
            status: Some(self.status),
            retry_after_secs: self
                .header("retry-after")
                .and_then(|raw| retry_after(raw, self.received_at)),
        })
    }

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

#[cfg(test)]
type MockTransport = Arc<dyn Fn(Method, &str) -> anyhow::Result<HttpResponse> + Send + Sync>;

#[derive(Clone)]
pub struct Http {
    client: Client,
    failure: Arc<Mutex<Option<RefreshFailure>>>,
    #[cfg(test)]
    mock: Option<MockTransport>,
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
            failure: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            mock: None,
        })
    }

    pub fn for_refresh(&self) -> Self {
        let mut scoped = self.clone();
        scoped.failure = Arc::new(Mutex::new(None));
        scoped
    }

    pub fn failure(&self) -> Option<RefreshFailure> {
        self.failure.lock().ok()?.clone()
    }

    #[cfg(test)]
    pub(crate) fn mocked(
        mock: impl Fn(Method, &str) -> anyhow::Result<HttpResponse> + Send + Sync + 'static,
    ) -> Self {
        let mut http = Self::with_proxy(None).expect("fixture HTTP");
        http.mock = Some(Arc::new(mock));
        http
    }

    fn observe(&self, response: &HttpResponse) {
        if response.ok() {
            return;
        }
        let failure = response.refresh_failure();
        if let Ok(mut slot) = self.failure.lock() {
            *slot = failure;
        }
    }

    pub async fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
        timeout_secs: u64,
    ) -> anyhow::Result<HttpResponse> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let response = mock(method, url)?;
            self.observe(&response);
            return Ok(response);
        }
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
        let res = match req.send().await {
            Ok(res) => res,
            Err(error) => {
                if let Ok(mut slot) = self.failure.lock() {
                    *slot = Some(RefreshFailure {
                        category: FailureCategory::Transient,
                        status: None,
                        retry_after_secs: None,
                    });
                }
                return Err(error.into());
            }
        };
        let status = res.status().as_u16();
        let received_at = chrono::Utc::now();
        let mut hdrs = HashMap::new();
        for (k, v) in res.headers() {
            if let Ok(val) = v.to_str() {
                hdrs.insert(k.as_str().to_string(), val.to_string());
            }
        }
        let mut response = HttpResponse {
            status,
            body: vec![],
            headers: hdrs,
            received_at,
        };
        self.observe(&response);
        response.body = res.bytes().await?.to_vec();
        Ok(response)
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

fn retry_after(raw: &str, now: chrono::DateTime<chrono::Utc>) -> Option<u64> {
    if let Ok(seconds) = raw.trim().parse::<u64>() {
        return Some(seconds);
    }
    let date = chrono::DateTime::parse_from_rfc2822(raw.trim()).ok()?;
    Some(date.signed_duration_since(now).num_seconds().max(0) as u64)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_after_delta_http_date_and_malformed_values_are_structured() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-03T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(retry_after("3600", now), Some(3600));
        assert_eq!(
            retry_after("Sat, 03 Oct 2026 01:00:00 GMT", now),
            Some(3600)
        );
        assert_eq!(retry_after("Fri, 02 Oct 2026 01:00:00 GMT", now), Some(0));
        for raw in ["-1", "NaN", "Infinity", "bad"] {
            assert_eq!(retry_after(raw, now), None);
        }
        let http = Http::with_proxy(None).unwrap();
        let response = HttpResponse {
            received_at: chrono::Utc::now(),
            status: 429,
            body: vec![],
            headers: HashMap::from([("Retry-After".into(), "7200".into())]),
        };
        http.observe(&response);
        let failure = http.failure().unwrap();
        assert_eq!(failure.category, FailureCategory::RateLimit);
        assert_eq!(failure.retry_after_secs, Some(7200));
        let other = http.for_refresh();
        assert!(other.failure().is_none());
    }
    #[test]
    fn exact_response_failure_retains_receipt_bound_retry_despite_sibling_failure() {
        let receipt = chrono::DateTime::parse_from_rfc3339("2026-10-03T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let limited = HttpResponse {
            received_at: receipt,
            status: 429,
            body: vec![],
            headers: HashMap::from([(
                "retry-after".into(),
                "Sat, 03 Oct 2026 01:00:00 GMT".into(),
            )]),
        };
        let later = HttpResponse {
            received_at: receipt,
            status: 401,
            body: vec![],
            headers: HashMap::new(),
        };
        let http = Http::with_proxy(None).unwrap();
        http.observe(&limited);
        http.observe(&later);
        assert_eq!(
            http.failure().unwrap().category,
            FailureCategory::Authentication
        );
        assert_eq!(
            limited.refresh_failure().unwrap().retry_after_secs,
            Some(3600)
        );
        assert_eq!(limited.refresh_failure().unwrap().status, Some(429));
    }
}
