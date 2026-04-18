use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::middleware;
use axum::routing::{any, get, post};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::trace::TraceLayer;

use crate::middleware::request_context_middleware;
use crate::state::AppState;

pub(crate) const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

pub(crate) mod fallback;
pub(crate) mod files;
pub(crate) mod health;
pub(crate) mod models;
pub(crate) mod responses;

pub(crate) fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/v1/models", get(models::get_models))
        .route("/models", get(models::get_models))
        .route("/v1/models/{model_id}", get(models::get_model))
        .route("/models/{model_id}", get(models::get_model))
        .route("/v1/files", post(files::post_file).get(files::list_files))
        .route("/files", post(files::post_file).get(files::list_files))
        .route(
            "/v1/files/{file_id}",
            get(files::get_file).delete(files::delete_file),
        )
        .route(
            "/files/{file_id}",
            get(files::get_file).delete(files::delete_file),
        )
        .route("/v1/files/{file_id}/content", get(files::get_file_content))
        .route("/files/{file_id}/content", get(files::get_file_content))
        .route("/v1/responses", post(responses::post_responses))
        .route("/responses", post(responses::post_responses))
        .route(
            "/v1/responses/{response_id}",
            any(fallback::handle_impossible_over_upstream),
        )
        .route(
            "/responses/{response_id}",
            any(fallback::handle_impossible_over_upstream),
        )
        .route(
            "/v1/responses/{response_id}/cancel",
            any(fallback::handle_impossible_over_upstream),
        )
        .route(
            "/responses/{response_id}/cancel",
            any(fallback::handle_impossible_over_upstream),
        )
        .route(
            "/v1/responses/{response_id}/input_items",
            any(fallback::handle_impossible_over_upstream),
        )
        .route(
            "/responses/{response_id}/input_items",
            any(fallback::handle_impossible_over_upstream),
        )
        .route(
            "/v1/chat/completions",
            post(responses::post_chat_completions),
        )
        .route("/chat/completions", post(responses::post_chat_completions))
        .route("/v1/responses/ws", get(responses::ws_responses))
        .route("/responses/ws", get(responses::ws_responses))
        .route("/v1/embeddings", any(fallback::handle_known_unsupported))
        .route("/embeddings", any(fallback::handle_known_unsupported))
        .route("/v1/moderations", any(fallback::handle_known_unsupported))
        .route("/moderations", any(fallback::handle_known_unsupported))
        .route("/v1/images", any(fallback::handle_known_unsupported))
        .route("/images", any(fallback::handle_known_unsupported))
        .route("/v1/audio", any(fallback::handle_known_unsupported))
        .route("/audio", any(fallback::handle_known_unsupported))
        .route("/v1/batches", any(fallback::handle_known_unsupported))
        .route("/batches", any(fallback::handle_known_unsupported))
        .route("/v1/uploads", any(fallback::handle_known_unsupported))
        .route("/uploads", any(fallback::handle_known_unsupported))
        .fallback(any(fallback::handle_unmatched))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_context_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(SetSensitiveRequestHeadersLayer::new([
            AUTHORIZATION,
            COOKIE,
        ]))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::util::ServiceExt;

    use super::build_router;
    use crate::test_support::test_state;

    #[tokio::test]
    async fn lifecycle_responses_routes_are_explicitly_rejected() -> Result<()> {
        let state = test_state(String::new()).await?;
        let app = build_router(state);
        let request = Request::builder()
            .method("GET")
            .uri("/v1/responses/resp_123")
            .body(Body::empty())?;

        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let json: Value = serde_json::from_slice(&body)?;
        assert_eq!(json["error"]["type"], json!("upstream_capability_error"));
        Ok(())
    }

    #[tokio::test]
    async fn known_but_unsupported_routes_are_explicitly_rejected() -> Result<()> {
        let state = test_state(String::new()).await?;
        let app = build_router(state);
        let request = Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .body(Body::from("{}"))?;

        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let json: Value = serde_json::from_slice(&body)?;
        assert_eq!(json["error"]["type"], json!("unsupported_route_error"));
        Ok(())
    }

    #[tokio::test]
    async fn unknown_routes_remain_not_found() -> Result<()> {
        let state = test_state(String::new()).await?;
        let app = build_router(state);
        let request = Request::builder()
            .method("GET")
            .uri("/v1/does-not-exist")
            .body(Body::empty())?;

        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let json: Value = serde_json::from_slice(&body)?;
        assert_eq!(json["error"]["message"], json!("Unsupported route: /v1/does-not-exist. Check proxy request logs for diagnostics."));
        Ok(())
    }
}
