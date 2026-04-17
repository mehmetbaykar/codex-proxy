use axum::body::Body;
use axum::extract::{Multipart, Path as AxumPath, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use bytes::Bytes;
use tokio::fs;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::config::{REQUEST_ID_HEADER, REQUESTS_LOG_FILE};
use crate::errors::json_error;
use crate::files::{
    fetch_file_row, fetch_live_file, insert_file_metadata, is_expired, list_live_files,
    mark_file_deleted, now_unix, parse_expires_after, sha256_hex, write_file_atomically,
};
use crate::logging::log_request_event;
use crate::state::{AppState, RequestContext};
use crate::types::{FileObject, FilesListResponse, NewFileMetadata};

pub(crate) async fn post_file(
    State(state): State<AppState>,
    ctx: Option<axum::extract::Extension<RequestContext>>,
    mut multipart: Multipart,
) -> Response {
    let request_context = ctx.as_ref().map(|extension| extension.0.clone());
    let request_id = request_context
        .as_ref()
        .map(|context| context.request_id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let mut purpose: Option<String> = None;
    let mut file_name: Option<String> = None;
    let mut media_type: Option<String> = None;
    let mut file_bytes: Option<Bytes> = None;
    let mut raw_expires_anchor: Option<String> = None;
    let mut raw_expires_seconds: Option<String> = None;
    let mut field_names = Vec::<String>::new();
    let mut file_hints = serde_json::Map::new();

    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_string();
                field_names.push(name.clone());
                match name.as_str() {
                    "purpose" => {
                        if let Ok(text) = field.text().await {
                            purpose = Some(text);
                        }
                    }
                    "expires_after[anchor]" => {
                        if let Ok(text) = field.text().await {
                            raw_expires_anchor = Some(text);
                        }
                    }
                    "expires_after[seconds]" => {
                        if let Ok(text) = field.text().await {
                            raw_expires_seconds = Some(text);
                        }
                    }
                    "file" => {
                        file_name = field.file_name().map(|value| value.to_string());
                        media_type = field.content_type().map(|value| value.to_string());
                        if let Ok(bytes) = field.bytes().await {
                            let hint = format!(
                                "blob:{}b:{}",
                                bytes.len(),
                                media_type.as_deref().unwrap_or("application/octet-stream")
                            );
                            file_hints.insert(name.clone(), serde_json::Value::String(hint));
                            file_bytes = Some(bytes);
                        }
                    }
                    _ => {}
                }
            }
            Ok(None) => break,
            Err(err) => {
                tracing::warn!("multipart parse error: {err}");
                return json_error(StatusCode::BAD_REQUEST, "Invalid multipart form data");
            }
        }
    }

    log_request_event(
        &state,
        REQUESTS_LOG_FILE,
        serde_json::json!({
            "ts": now_unix(),
            "phase": "client_multipart",
            "request_id": request_id,
            "method": request_context.as_ref().map(|context| context.method.as_str()).unwrap_or("POST"),
            "path": request_context.as_ref().map(|context| context.path.as_str()).unwrap_or("/v1/files"),
            "route_kind": request_context.as_ref().map(|context| context.route_kind).unwrap_or("files"),
            "content_type": request_context.as_ref().and_then(|context| context.content_type.clone()),
            "field_names": field_names,
            "file_hints": file_hints,
            "client_ip": request_context.as_ref().and_then(|context| context.client_ip.clone()),
        }),
    )
    .await;

    let Some(file_bytes) = file_bytes else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Missing required multipart field: file",
        );
    };
    let Some(purpose) = purpose else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Missing required multipart field: purpose",
        );
    };

    let expires_after = match parse_expires_after(raw_expires_anchor, raw_expires_seconds) {
        Ok(value) => value,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, &message),
    };

    let file_id = format!("file-{}", Uuid::new_v4().simple());
    let created_at = now_unix();
    let expires_at = expires_after.map(|value| created_at + value.seconds);
    let filename = file_name.unwrap_or_else(|| "upload.bin".to_string());
    let mime = media_type.unwrap_or_else(|| "application/octet-stream".to_string());
    let size = file_bytes.len() as i64;
    let storage_path = state.files_dir.join(format!("{file_id}.bin"));
    let sha = sha256_hex(&file_bytes);

    if let Err(err) = write_file_atomically(&storage_path, &file_bytes).await {
        tracing::error!("file write failed: {err}");
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to persist uploaded file",
        );
    }

    if let Err(err) = insert_file_metadata(
        &state.db,
        &NewFileMetadata {
            file_id: &file_id,
            filename: &filename,
            purpose: &purpose,
            media_type: &mime,
            byte_size: size,
            sha256: &sha,
            storage_path: &storage_path,
            created_at,
            expires_at,
        },
    )
    .await
    {
        tracing::error!("db insert failed: {err}");
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save file metadata",
        );
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(&request_id).unwrap_or(HeaderValue::from_static("n/a")),
    );

    (
        StatusCode::OK,
        headers,
        Json(FileObject {
            id: file_id,
            object: "file",
            bytes: size,
            created_at,
            filename,
            purpose,
            status: "uploaded".to_string(),
            expires_at,
        }),
    )
        .into_response()
}

pub(crate) async fn list_files(State(state): State<AppState>) -> Response {
    match list_live_files(&state.db).await {
        Ok(data) => Json(FilesListResponse {
            object: "list",
            data,
        })
        .into_response(),
        Err(err) => {
            tracing::error!("list files query failed: {err}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to list files")
        }
    }
}

pub(crate) async fn get_file(
    State(state): State<AppState>,
    AxumPath(file_id): AxumPath<String>,
) -> Response {
    match fetch_live_file(&state.db, &file_id).await {
        Ok(Some(file)) => Json(file).into_response(),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "File not found"),
        Err(err) => {
            tracing::error!("get file failed: {err}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve file")
        }
    }
}

pub(crate) async fn get_file_content(
    State(state): State<AppState>,
    AxumPath(file_id): AxumPath<String>,
) -> Response {
    let record = match fetch_file_row(&state.db, &file_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "File not found"),
        Err(err) => {
            tracing::error!("fetch file row failed: {err}");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read file metadata",
            );
        }
    };

    if is_expired(record.expires_at) || record.deleted_at.is_some() {
        return json_error(StatusCode::NOT_FOUND, "File not found");
    }

    let file = match fs::File::open(&record.storage_path).await {
        Ok(file) => file,
        Err(_) => return json_error(StatusCode::NOT_FOUND, "File not found"),
    };
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let mut response = Response::new(body);
    let content_type = HeaderValue::from_str(&record.media_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    response.headers_mut().insert(CONTENT_TYPE, content_type);
    if let Ok(header_value) = HeaderValue::from_str(&record.byte_size.to_string()) {
        response.headers_mut().insert(CONTENT_LENGTH, header_value);
    }
    let disposition = format!(
        "attachment; filename=\"{}\"",
        record.filename.replace('"', "")
    );
    if let Ok(header_value) = HeaderValue::from_str(&disposition) {
        response
            .headers_mut()
            .insert(CONTENT_DISPOSITION, header_value);
    }
    response
}

pub(crate) async fn delete_file(
    State(state): State<AppState>,
    AxumPath(file_id): AxumPath<String>,
) -> Response {
    let record = match fetch_file_row(&state.db, &file_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "File not found"),
        Err(err) => {
            tracing::error!("fetch before delete failed: {err}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete file");
        }
    };
    if record.deleted_at.is_some() || is_expired(record.expires_at) {
        return json_error(StatusCode::NOT_FOUND, "File not found");
    }

    let _ = fs::remove_file(&record.storage_path).await;
    if let Err(err) = mark_file_deleted(&state.db, &file_id).await {
        tracing::error!("delete file update failed: {err}");
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete file");
    }
    Json(serde_json::json!({ "id": file_id, "object": "file", "deleted": true })).into_response()
}
