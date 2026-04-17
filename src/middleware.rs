use axum::extract::{Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, header::AUTHORIZATION};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

use crate::config::{CLIENT_REQUEST_ID_HEADER, REQUEST_ID_HEADER};
use crate::logging::{classify_pathname, forwarded_client_ip};
use crate::state::{AppState, RequestContext};

pub(crate) async fn request_context_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let request_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let route_kind = classify_pathname(&path);
    let content_type = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let client_ip = forwarded_client_ip(req.headers());
    let client_request_id = req
        .headers()
        .get(CLIENT_REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);

    let is_public_route = matches!(path.as_str(), "/healthz");
    if !is_public_route
        && let Some(expected) = &state.static_api_key
    {
        let bearer = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_bearer);
        if bearer.as_deref() != Some(expected.as_str()) {
            let mut response = crate::errors::json_error(
                axum::http::StatusCode::UNAUTHORIZED,
                "Invalid or missing API key",
            );
            if let Ok(header_value) = HeaderValue::from_str(&request_id) {
                response
                    .headers_mut()
                    .insert(REQUEST_ID_HEADER, header_value);
            }
            return response;
        }
    }

    req.extensions_mut().insert(RequestContext {
        request_id: request_id.clone(),
        method,
        path,
        route_kind,
        content_type,
        client_ip,
        client_request_id,
        request_started_at: std::time::Instant::now(),
    });
    let mut response = next.run(req).await;
    if let Ok(header_value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(REQUEST_ID_HEADER, header_value);
    }
    response
}

fn parse_bearer(value: &str) -> Option<String> {
    let mut parts = value.splitn(2, ' ');
    let scheme = parts.next()?.trim();
    let token = parts.next()?.trim();
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
        Some(token.to_string())
    } else {
        None
    }
}
