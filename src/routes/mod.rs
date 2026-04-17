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
            "/v1/chat/completions",
            post(responses::post_chat_completions),
        )
        .route("/chat/completions", post(responses::post_chat_completions))
        .route("/v1/responses/ws", get(responses::ws_responses))
        .route("/responses/ws", get(responses::ws_responses))
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
