use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::Value;

use crate::config::SUPPORTED_MODELS;
use crate::files::now_unix;
use crate::normalization::{normalize_chat_payload, normalize_responses_payload};
use crate::state::{AppState, ClientStreamOptions, RequestContext};
use crate::streaming::{
    ToolStreamState, map_response_event_to_chat_chunk, response_to_chat_message,
};
use crate::types::ModelItem;
use crate::upstream::open_upstream_response;

#[derive(Debug, Default)]
pub(crate) struct CodexAdapter;

impl CodexAdapter {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn list_models(&self) -> Vec<ModelItem> {
        let created = now_unix();
        SUPPORTED_MODELS
            .iter()
            .map(|model| ModelItem {
                id: (*model).to_string(),
                object: "model",
                created,
                owned_by: "openai",
            })
            .collect()
    }

    pub(crate) fn get_model(&self, model_id: &str) -> Option<ModelItem> {
        self.list_models()
            .into_iter()
            .find(|model| model.id == model_id)
    }

    #[allow(clippy::result_large_err)]
    pub(crate) async fn normalize_responses_payload(
        &self,
        state: &AppState,
        payload: &mut Value,
    ) -> std::result::Result<(), Response> {
        normalize_responses_payload(state, payload).await
    }

    #[allow(clippy::result_large_err)]
    pub(crate) async fn normalize_chat_payload(
        &self,
        state: &AppState,
        payload: &mut Value,
    ) -> std::result::Result<ClientStreamOptions, Response> {
        normalize_chat_payload(state, payload).await
    }

    pub(crate) async fn open_upstream_response(
        &self,
        state: &AppState,
        request_context: Option<&RequestContext>,
        headers: &HeaderMap,
        body: &Value,
    ) -> anyhow::Result<reqwest::Response> {
        open_upstream_response(state, request_context, headers, body).await
    }

    pub(crate) fn map_response_event_to_chat_chunk(
        &self,
        event: &Value,
        tool_state: &mut ToolStreamState,
        chunk_id: &str,
        created: i64,
        model: &str,
    ) -> Option<String> {
        map_response_event_to_chat_chunk(event, tool_state, chunk_id, created, model)
    }

    pub(crate) fn response_to_chat_message(&self, output: Option<&Value>) -> Value {
        response_to_chat_message(output)
    }
}
