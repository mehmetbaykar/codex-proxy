use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};
use sqlx::{Pool, Row, Sqlite};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::state::AppState;
use crate::types::{ExpiresAfter, FileObject, FileRow, NewFileMetadata};

pub(crate) async fn init_db(db: &Pool<Sqlite>) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS files (
            id TEXT PRIMARY KEY,
            filename TEXT NOT NULL,
            purpose TEXT NOT NULL,
            media_type TEXT NOT NULL,
            byte_size INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            storage_path TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            expires_at INTEGER,
            status TEXT NOT NULL DEFAULT 'uploaded',
            deleted_at INTEGER
        )
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_files_expires_at ON files(expires_at)")
        .execute(db)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_files_deleted_at ON files(deleted_at)")
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) async fn insert_file_metadata(
    db: &Pool<Sqlite>,
    metadata: &NewFileMetadata<'_>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO files(id, filename, purpose, media_type, byte_size, sha256, storage_path, created_at, expires_at, status, deleted_at)
        VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, 'uploaded', NULL)
        "#,
    )
    .bind(metadata.file_id)
    .bind(metadata.filename)
    .bind(metadata.purpose)
    .bind(metadata.media_type)
    .bind(metadata.byte_size)
    .bind(metadata.sha256)
    .bind(metadata.storage_path.to_string_lossy().to_string())
    .bind(metadata.created_at)
    .bind(metadata.expires_at)
    .execute(db)
    .await?;
    Ok(())
}

pub(crate) async fn list_live_files(db: &Pool<Sqlite>) -> Result<Vec<FileObject>> {
    let rows = sqlx::query(
        r#"
        SELECT id, filename, purpose, byte_size, created_at, expires_at, status
        FROM files
        WHERE deleted_at IS NULL
          AND (expires_at IS NULL OR expires_at > ?)
        ORDER BY created_at DESC
        "#,
    )
    .bind(now_unix())
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| FileObject {
            id: row.get("id"),
            object: "file",
            bytes: row.get("byte_size"),
            created_at: row.get("created_at"),
            filename: row.get("filename"),
            purpose: row.get("purpose"),
            status: row.get("status"),
            expires_at: row.get("expires_at"),
        })
        .collect())
}

pub(crate) async fn fetch_live_file(
    db: &Pool<Sqlite>,
    file_id: &str,
) -> Result<Option<FileObject>> {
    let row = sqlx::query(
        r#"
        SELECT id, filename, purpose, byte_size, created_at, expires_at, status
        FROM files
        WHERE id = ?
          AND deleted_at IS NULL
          AND (expires_at IS NULL OR expires_at > ?)
        "#,
    )
    .bind(file_id)
    .bind(now_unix())
    .fetch_optional(db)
    .await?;

    Ok(row.map(|row| FileObject {
        id: row.get("id"),
        object: "file",
        bytes: row.get("byte_size"),
        created_at: row.get("created_at"),
        filename: row.get("filename"),
        purpose: row.get("purpose"),
        status: row.get("status"),
        expires_at: row.get("expires_at"),
    }))
}

pub(crate) async fn fetch_file_row(db: &Pool<Sqlite>, file_id: &str) -> Result<Option<FileRow>> {
    let row = sqlx::query(
        r#"
        SELECT id, filename, media_type, byte_size, storage_path, expires_at, deleted_at
        FROM files
        WHERE id = ?
        "#,
    )
    .bind(file_id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(|row| FileRow {
        filename: row.get("filename"),
        media_type: row.get("media_type"),
        byte_size: row.get("byte_size"),
        storage_path: row.get("storage_path"),
        expires_at: row.get("expires_at"),
        deleted_at: row.get("deleted_at"),
    }))
}

pub(crate) async fn mark_file_deleted(db: &Pool<Sqlite>, file_id: &str) -> Result<()> {
    sqlx::query("UPDATE files SET deleted_at = ? WHERE id = ?")
        .bind(now_unix())
        .bind(file_id)
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) fn is_expired(expires_at: Option<i64>) -> bool {
    expires_at
        .map(|timestamp| timestamp <= now_unix())
        .unwrap_or(false)
}

pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) async fn write_file_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp_path).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    drop(file);
    fs::rename(tmp_path, path).await?;
    Ok(())
}

pub(crate) fn parse_expires_after(
    raw_anchor: Option<String>,
    raw_seconds: Option<String>,
) -> std::result::Result<Option<ExpiresAfter>, String> {
    if raw_anchor.is_none() && raw_seconds.is_none() {
        return Ok(None);
    }

    let anchor = raw_anchor.unwrap_or_default();
    let seconds_str = raw_seconds.unwrap_or_default();
    if anchor != "created_at" {
        return Err("expires_after[anchor] must be 'created_at'".to_string());
    }
    let seconds = seconds_str
        .parse::<i64>()
        .map_err(|_| "expires_after[seconds] must be an integer".to_string())?;
    if !(3600..=2_592_000).contains(&seconds) {
        return Err("expires_after[seconds] must be between 3600 and 2592000".to_string());
    }
    Ok(Some(ExpiresAfter { seconds }))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(crate) async fn resolve_file_id_to_data_url(
    state: &AppState,
    file_id: &str,
) -> Result<(String, String)> {
    let record = fetch_file_row(&state.db, file_id)
        .await?
        .context("file_id not found")?;
    if record.deleted_at.is_some() || is_expired(record.expires_at) {
        anyhow::bail!("file unavailable");
    }
    let bytes = fs::read(&record.storage_path).await?;
    let base64 = BASE64.encode(bytes);
    let data_url = format!("data:{};base64,{}", record.media_type, base64);
    Ok((data_url, record.filename))
}
