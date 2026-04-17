use axum::Json;
use axum::response::IntoResponse;
use serde_json::json;

pub(crate) async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}
