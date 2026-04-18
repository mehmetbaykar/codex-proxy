use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub(crate) struct ErrorEnvelope {
    pub(crate) error: ErrorBody,
}

#[derive(Serialize)]
pub(crate) struct ErrorBody {
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) param: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ModelsResponse {
    pub(crate) object: &'static str,
    pub(crate) data: Vec<ModelItem>,
}

#[derive(Serialize)]
pub(crate) struct ModelItem {
    pub(crate) id: String,
    pub(crate) object: &'static str,
    pub(crate) created: i64,
    pub(crate) owned_by: &'static str,
}

#[derive(Serialize)]
pub(crate) struct FilesListResponse {
    pub(crate) object: &'static str,
    pub(crate) data: Vec<FileObject>,
}

#[derive(Serialize, Clone)]
pub(crate) struct FileObject {
    pub(crate) id: String,
    pub(crate) object: &'static str,
    pub(crate) bytes: i64,
    pub(crate) created_at: i64,
    pub(crate) filename: String,
    pub(crate) purpose: String,
    pub(crate) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct ExpiresAfter {
    pub(crate) seconds: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct FileRow {
    pub(crate) filename: String,
    pub(crate) media_type: String,
    pub(crate) byte_size: i64,
    pub(crate) storage_path: String,
    pub(crate) expires_at: Option<i64>,
    pub(crate) deleted_at: Option<i64>,
}

pub(crate) struct NewFileMetadata<'a> {
    pub(crate) file_id: &'a str,
    pub(crate) filename: &'a str,
    pub(crate) purpose: &'a str,
    pub(crate) media_type: &'a str,
    pub(crate) byte_size: i64,
    pub(crate) sha256: &'a str,
    pub(crate) storage_path: &'a std::path::Path,
    pub(crate) created_at: i64,
    pub(crate) expires_at: Option<i64>,
}
