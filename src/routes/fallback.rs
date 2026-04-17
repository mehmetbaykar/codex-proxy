use axum::extract::{Request, State};
use axum::response::Response;

use crate::logging::log_unmatched_request;
use crate::state::{AppState, RequestContext};

pub(crate) async fn handle_unmatched(
    State(state): State<AppState>,
    ctx: Option<axum::extract::Extension<RequestContext>>,
    req: Request,
) -> Response {
    log_unmatched_request(&state, ctx.as_ref().map(|extension| &extension.0), req).await
}
