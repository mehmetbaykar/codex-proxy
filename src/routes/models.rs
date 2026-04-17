use axum::Json;
use axum::response::IntoResponse;

use crate::config::SUPPORTED_MODELS;
use crate::files::now_unix;
use crate::types::{ModelItem, ModelsResponse};

pub(crate) async fn get_models() -> impl IntoResponse {
    let created = now_unix();
    let data = SUPPORTED_MODELS
        .iter()
        .map(|model| ModelItem {
            id: (*model).to_string(),
            object: "model",
            created,
            owned_by: "openai",
        })
        .collect::<Vec<_>>();
    Json(ModelsResponse {
        object: "list",
        data,
    })
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use anyhow::Result;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;
    use serde_json::Value;

    use super::get_models;
    use crate::config::SUPPORTED_MODELS;

    #[tokio::test]
    async fn models_route_returns_all_supported_model_ids() -> Result<()> {
        let response = get_models().await.into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let response: Value = serde_json::from_slice(&body)?;
        let ids = response["data"]
            .as_array()
            .context("models response data should be an array")?
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
}
