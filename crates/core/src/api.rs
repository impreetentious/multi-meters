use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};

use crate::engine::AppEngine;

pub fn router(engine: Arc<AppEngine>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
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
