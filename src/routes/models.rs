use axum::Json;
use axum::extract::{Path, State};
use axum::response::IntoResponse;

use crate::errors::json_error;
use crate::state::AppState;
use crate::types::ModelsResponse;

pub(crate) async fn get_models(State(state): State<AppState>) -> impl IntoResponse {
    let data = state.codex_adapter.list_models();
    Json(ModelsResponse {
        object: "list",
        data,
    })
}

pub(crate) async fn get_model(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> impl IntoResponse {
    match state.codex_adapter.get_model(&model_id) {
        Some(model) => Json(model).into_response(),
        None => json_error(axum::http::StatusCode::NOT_FOUND, "Model not found"),
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use axum::body::to_bytes;
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    use serde_json::json;
    use serde_json::Value;

    use super::{get_model, get_models};
    use crate::config::SUPPORTED_MODELS;
    use crate::test_support::test_state;

    #[tokio::test]
    async fn models_route_returns_all_supported_model_ids() -> Result<()> {
        let state = test_state(String::new()).await?;
        let response = get_models(State(state)).await.into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let response: Value = serde_json::from_slice(&body)?;
        let ids = response["data"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("models response data should be an array"))?
            .iter()
            .filter_map(|item| item["id"].as_str().map(ToOwned::to_owned))
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            SUPPORTED_MODELS
                .iter()
                .map(|model| (*model).to_string())
                .collect::<Vec<_>>()
        );
        Ok(())
    }

    #[tokio::test]
    async fn retrieve_model_returns_known_model() -> Result<()> {
        let state = test_state(String::new()).await?;
        let response = get_model(State(state), Path("gpt-5.4".to_string()))
            .await
            .into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let response: Value = serde_json::from_slice(&body)?;

        assert_eq!(response["id"], json!("gpt-5.4"));
        assert_eq!(response["object"], json!("model"));
        Ok(())
    }
}
