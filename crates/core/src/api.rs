use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::header::HeaderValue;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::engine::AppEngine;

/// Local tools served from loopback may read the API from a browser. Any other origin must not:
/// a page on the open web could otherwise silently read the signed-in user's plans and spend.
fn is_loopback_origin(origin: &HeaderValue) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    // A serialized origin carries only host and port. Anything else is malformed, and userinfo
    // in particular would let `127.0.0.1@example.com` read as loopback.
    if authority.contains('@') || authority.contains('/') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((host, _)) => host,
            None => return false,
        },
        None => authority
            .split_once(':')
            .map_or(authority, |(host, _)| host),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

pub fn router(engine: Arc<AppEngine>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            is_loopback_origin(origin)
        }))
        .allow_methods([Method::GET, Method::OPTIONS]);
    Router::new()
        .route("/v1/limits", get(limits))
        .route("/v1/limits/{id}", get(limits_one))
        .route("/v1/usage", get(usage))
        .route("/v1/usage/{id}", get(usage_one))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(engine)
        .layer(cors)
}

pub async fn start(engine: Arc<AppEngine>) {
    let addr = SocketAddr::from(([127, 0, 0, 1], 6736));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(%error, "local API disabled because 127.0.0.1:6736 is unavailable");
            return;
        }
    };
    tracing::info!("local API listening on http://127.0.0.1:6736");
    if let Err(error) = axum::serve(listener, router(engine)).await {
        tracing::error!(%error, "local API stopped unexpectedly");
    }
}

async fn limits(State(engine): State<Arc<AppEngine>>) -> Response {
    Json(
        engine
            .limits_json(None)
            .await
            .expect("unfiltered limits are always available"),
    )
    .into_response()
}

async fn limits_one(State(engine): State<Arc<AppEngine>>, Path(id): Path<String>) -> Response {
    match engine.limits_json(Some(&id)).await {
        Some(value) => Json(value).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "provider_not_found"),
    }
}

async fn usage(State(engine): State<Arc<AppEngine>>) -> Response {
    Json(
        engine
            .usage_json(None)
            .await
            .expect("unfiltered usage is always available"),
    )
    .into_response()
}

async fn usage_one(State(engine): State<Arc<AppEngine>>, Path(id): Path<String>) -> Response {
    match engine.usage_json(Some(&id)).await {
        Some(value) => Json(value).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "provider_not_found"),
    }
}

async fn not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "not_found")
}

async fn method_not_allowed() -> Response {
    api_error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed")
}

fn api_error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_origins_may_read_the_local_api() {
        for allowed in [
            "http://localhost:5173",
            "http://127.0.0.1:6736",
            "http://[::1]:8080",
        ] {
            assert!(is_loopback_origin(&HeaderValue::from_static(allowed)));
        }
        for blocked in [
            "https://example.com",
            "http://localhost.example.com",
            "http://127.0.0.1.example.com",
            "http://127.0.0.1:6736@example.com",
            "http://localhost/../example.com",
            "null",
        ] {
            assert!(!is_loopback_origin(&HeaderValue::from_static(blocked)));
        }
    }
}
