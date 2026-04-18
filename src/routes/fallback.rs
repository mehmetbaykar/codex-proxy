use axum::extract::{Request, State};
use axum::response::Response;

use crate::errors::{impossible_upstream_route_error, unsupported_proxy_route_error};
use crate::logging::log_unmatched_request;
use crate::state::{AppState, RequestContext};

pub(crate) async fn handle_known_unsupported(
    State(_state): State<AppState>,
    req: Request,
) -> Response {
    unsupported_proxy_route_error(req.uri().path())
}

pub(crate) async fn handle_impossible_over_upstream(
    State(_state): State<AppState>,
    req: Request,
) -> Response {
    impossible_upstream_route_error(req.uri().path())
}

pub(crate) async fn handle_unmatched(
    State(state): State<AppState>,
    ctx: Option<axum::extract::Extension<RequestContext>>,
    req: Request,
) -> Response {
    log_unmatched_request(&state, ctx.as_ref().map(|extension| &extension.0), req).await
}
