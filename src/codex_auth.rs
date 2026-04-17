use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tokio::time::sleep;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_AUTH_BASE: &str = "https://auth.openai.com";
const VERIFY_PATH: &str = "/codex/device";
const POLL_TIMEOUT: Duration = Duration::from_secs(900);
const POLL_MIN_INTERVAL: Duration = Duration::from_secs(5);
const EXPIRY_SKEW_S: i64 = 60;
const SCOPE: &str = "openid profile email";

fn auth_base() -> String {
    std::env::var("CODEX_AUTH_BASE_OVERRIDE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_AUTH_BASE.to_string())
}

fn device_code_url() -> String {
    format!("{}/api/accounts/deviceauth/usercode", auth_base())
}

fn device_token_url() -> String {
    format!("{}/api/accounts/deviceauth/token", auth_base())
}

fn oauth_token_url() -> String {
    format!("{}/oauth/token", auth_base())
}

fn redirect_uri() -> String {
    format!("{}/deviceauth/callback", auth_base())
}

fn verify_url() -> String {
    format!("{}{}", auth_base(), VERIFY_PATH)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TokenData {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) id_token: String,
    #[serde(default)]
    pub(crate) expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account_id: Option<String>,
}

pub(crate) struct CodexAuth {
    state: RwLock<TokenData>,
    auth_path: PathBuf,
    http: Client,
}

impl CodexAuth {
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self {
            state: RwLock::new(TokenData {
                access_token: "test-access".to_string(),
                refresh_token: "test-refresh".to_string(),
                id_token: "test-id".to_string(),
                expires_at: Some(i64::MAX),
                account_id: None,
            }),
            auth_path: PathBuf::from("/tmp/codex-proxy-tests-auth.json"),
            http: Client::new(),
        }
    }

    pub(crate) async fn load(auth_path: PathBuf, http: Client) -> Result<Self> {
        let raw = fs::read(&auth_path).await.with_context(|| {
            format!(
                "Cannot read {}. Run: docker exec -it codex-proxy codex-proxy login",
                auth_path.display()
            )
        })?;
        let mut token: TokenData =
            serde_json::from_slice(&raw).context("auth.json is corrupt; re-run login")?;
        if token.expires_at.is_none() {
            token.expires_at = parse_jwt_exp(&token.access_token);
        }
        Ok(Self {
            state: RwLock::new(token),
            auth_path,
            http,
        })
    }

    pub(crate) async fn current_token(&self) -> Result<String> {
        {
            let guard = self.state.read().await;
            if !is_expired(guard.expires_at) {
                return Ok(guard.access_token.clone());
            }
        }
        self.refresh_locked().await
    }

    pub(crate) async fn force_refresh(&self) -> Result<String> {
        let mut guard = self.state.write().await;
        let refreshed = refresh_tokens(&self.http, &guard.refresh_token).await?;
        self.persist_locked(&mut guard, refreshed).await
    }

    async fn refresh_locked(&self) -> Result<String> {
        let mut guard = self.state.write().await;
        if !is_expired(guard.expires_at) {
            return Ok(guard.access_token.clone());
        }
        let refreshed = refresh_tokens(&self.http, &guard.refresh_token).await?;
        self.persist_locked(&mut guard, refreshed).await
    }

    async fn persist_locked(
        &self,
        guard: &mut tokio::sync::RwLockWriteGuard<'_, TokenData>,
        refreshed: RefreshedTokens,
    ) -> Result<String> {
        let expires_at = parse_jwt_exp(&refreshed.access_token);
        let account_id = extract_account_id(&refreshed.access_token)
            .or_else(|| extract_account_id(&refreshed.id_token))
            .or_else(|| guard.account_id.clone());
        let next = TokenData {
            access_token: refreshed.access_token,
            refresh_token: refreshed.refresh_token,
            id_token: refreshed.id_token,
            expires_at,
            account_id,
        };
        write_atomic(&self.auth_path, &next).await?;
        let token = next.access_token.clone();
        **guard = next;
        tracing::info!(
            "refreshed Codex upstream token (expires_at={:?})",
            guard.expires_at
        );
        Ok(token)
    }
}

fn is_expired(expires_at: Option<i64>) -> bool {
    let Some(exp) = expires_at else {
        return true;
    };
    now_unix() >= exp - EXPIRY_SKEW_S
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn parse_jwt_exp(jwt: &str) -> Option<i64> {
    let claims = parse_jwt_claims(jwt)?;
    claims.get("exp").and_then(Value::as_i64)
}

fn parse_jwt_claims(jwt: &str) -> Option<Value> {
    let mut parts = jwt.split('.');
    parts.next()?;
    let payload_b64 = parts.next()?;
    let bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

fn extract_account_id(jwt: &str) -> Option<String> {
    parse_jwt_claims(jwt)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

struct RefreshedTokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
}

async fn refresh_tokens(http: &Client, refresh_token: &str) -> Result<RefreshedTokens> {
    let response = http
        .post(oauth_token_url())
        .json(&json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "scope": SCOPE,
        }))
        .send()
        .await
        .context("refresh request failed to send")?;
    let status = response.status();
    let body: Value = if status.is_success() {
        response.json().await.context("refresh response not JSON")?
    } else {
        let text = response.text().await.unwrap_or_default();
        bail!("refresh_token rejected by upstream: status={status} body={text}");
    };
    parse_refresh_response(&body, refresh_token)
}

fn parse_refresh_response(data: &Value, old_refresh: &str) -> Result<RefreshedTokens> {
    let access_token = data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("refresh response missing access_token"))?
        .to_string();
    let id_token = data
        .get("id_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("refresh response missing id_token"))?
        .to_string();
    // LiteLLM authenticator.py:337 — reuse the old refresh_token if the
    // response does not include a new one.
    let refresh_token = data
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| old_refresh.to_string());
    Ok(RefreshedTokens {
        access_token,
        refresh_token,
        id_token,
    })
}

pub(crate) async fn run_login(auth_path: PathBuf, http: Client) -> Result<()> {
    if let Some(parent) = auth_path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }

    let device = request_device_code(&http).await?;
    println!();
    println!("Open: {}", verify_url());
    println!("Code: {}", device.user_code);
    println!();
    println!("Waiting for sign-in in your browser...");

    let code = poll_for_auth_code(&http, &device).await?;
    let tokens = exchange_code_for_tokens(&http, &code).await?;
    let expires_at = parse_jwt_exp(&tokens.access_token);
    let account_id = extract_account_id(&tokens.access_token)
        .or_else(|| extract_account_id(&tokens.id_token));
    let data = TokenData {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        id_token: tokens.id_token,
        expires_at,
        account_id,
    };
    write_atomic(&auth_path, &data).await?;
    println!("Signed in. Tokens saved to {}", auth_path.display());
    Ok(())
}

struct DeviceCode {
    device_auth_id: String,
    user_code: String,
    interval: Duration,
}

async fn request_device_code(http: &Client) -> Result<DeviceCode> {
    let response = http
        .post(device_code_url())
        .json(&json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .context("device-code request failed to send")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("device-code init failed: status={status} body={body}");
    }
    let data: Value = response
        .json()
        .await
        .context("device-code response not JSON")?;
    let device_auth_id = data
        .get("device_auth_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("device-code response missing device_auth_id"))?
        .to_string();
    let user_code = data
        .get("user_code")
        .or_else(|| data.get("usercode"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("device-code response missing user_code"))?
        .to_string();
    let interval = data
        .get("interval")
        .and_then(Value::as_i64)
        .map(|seconds| Duration::from_secs(seconds.max(1) as u64))
        .unwrap_or(POLL_MIN_INTERVAL);
    Ok(DeviceCode {
        device_auth_id,
        user_code,
        interval,
    })
}

struct AuthCodeResult {
    authorization_code: String,
    code_verifier: String,
}

async fn poll_for_auth_code(http: &Client, device: &DeviceCode) -> Result<AuthCodeResult> {
    let deadline = Instant::now() + POLL_TIMEOUT;
    let interval = std::cmp::max(device.interval, POLL_MIN_INTERVAL);
    loop {
        let response = http
            .post(device_token_url())
            .json(&json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }))
            .send()
            .await
            .context("device-code poll failed to send")?;
        let status = response.status();
        if status == StatusCode::OK {
            let data: Value = response
                .json()
                .await
                .context("device-code poll response not JSON")?;
            if let (Some(code), Some(verifier)) = (
                data.get("authorization_code").and_then(Value::as_str),
                data.get("code_verifier").and_then(Value::as_str),
            ) {
                return Ok(AuthCodeResult {
                    authorization_code: code.to_string(),
                    code_verifier: verifier.to_string(),
                });
            }
        } else if status != StatusCode::FORBIDDEN && status != StatusCode::NOT_FOUND {
            let body = response.text().await.unwrap_or_default();
            bail!("device-code poll failed: status={status} body={body}");
        }
        if Instant::now() >= deadline {
            bail!("Timed out waiting for device authorization");
        }
        sleep(interval).await;
    }
}

struct ExchangedTokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
}

async fn exchange_code_for_tokens(
    http: &Client,
    code: &AuthCodeResult,
) -> Result<ExchangedTokens> {
    let redirect = redirect_uri();
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.authorization_code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", CLIENT_ID),
        ("code_verifier", code.code_verifier.as_str()),
    ];
    let response = http
        .post(oauth_token_url())
        .form(&form)
        .send()
        .await
        .context("token-exchange request failed to send")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("token exchange failed: status={status} body={body}");
    }
    let data: Value = response
        .json()
        .await
        .context("token-exchange response not JSON")?;
    let access_token = data
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token-exchange response missing access_token"))?
        .to_string();
    let refresh_token = data
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token-exchange response missing refresh_token"))?
        .to_string();
    let id_token = data
        .get("id_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token-exchange response missing id_token"))?
        .to_string();
    Ok(ExchangedTokens {
        access_token,
        refresh_token,
        id_token,
    })
}

async fn write_atomic(path: &Path, data: &TokenData) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let json = serde_json::to_vec_pretty(data).context("serialize auth.json")?;
    {
        use tokio::fs::OpenOptions;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .await
            .with_context(|| format!("open {}", tmp.display()))?;
        file.write_all(&json).await?;
        file.flush().await?;
        file.sync_all().await?;
    }
    fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;
    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Json};
    use axum::routing::post;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use reqwest::Client;
    use serde_json::json;
    use uuid::Uuid;

    use super::{TokenData, parse_jwt_exp, parse_refresh_response, run_login, write_atomic};

    fn make_jwt(payload: &serde_json::Value) -> Result<String> {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload)?);
        Ok(format!("{header}.{body}.sig"))
    }

    #[test]
    fn parse_jwt_exp_reads_exp_claim() -> Result<()> {
        let jwt = make_jwt(&json!({ "exp": 1_700_000_000i64 }))?;
        assert_eq!(parse_jwt_exp(&jwt), Some(1_700_000_000));
        Ok(())
    }

    #[test]
    fn parse_jwt_exp_returns_none_for_malformed() {
        assert_eq!(parse_jwt_exp("not-a-jwt"), None);
        assert_eq!(parse_jwt_exp("only.one"), None);
        assert_eq!(parse_jwt_exp("one.@invalid-base64@.three"), None);
    }

    #[test]
    fn refresh_response_preserves_old_refresh_token_when_absent() -> Result<()> {
        let data = json!({
            "access_token": "new-access",
            "id_token": "new-id",
        });
        let out = parse_refresh_response(&data, "old-refresh")?;
        assert_eq!(out.access_token, "new-access");
        assert_eq!(out.id_token, "new-id");
        assert_eq!(out.refresh_token, "old-refresh");
        Ok(())
    }

    #[test]
    fn refresh_response_uses_new_refresh_token_when_present() -> Result<()> {
        let data = json!({
            "access_token": "new-access",
            "id_token": "new-id",
            "refresh_token": "new-refresh",
        });
        let out = parse_refresh_response(&data, "old-refresh")?;
        assert_eq!(out.refresh_token, "new-refresh");
        Ok(())
    }

    #[derive(Clone, Default)]
    struct MockState {
        poll_attempts: Arc<AtomicUsize>,
    }

    async fn mock_device_code() -> impl IntoResponse {
        Json(json!({
            "device_auth_id": "device-abc",
            "user_code": "ABCD-1234",
            "interval": 1
        }))
    }

    async fn mock_device_token(State(state): State<MockState>) -> impl IntoResponse {
        let attempt = state.poll_attempts.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            // First poll: user hasn't approved yet.
            return (StatusCode::FORBIDDEN, Json(json!({}))).into_response();
        }
        Json(json!({
            "authorization_code": "auth-code-xyz",
            "code_challenge": "challenge",
            "code_verifier": "verifier"
        }))
        .into_response()
    }

    async fn mock_oauth_token() -> impl IntoResponse {
        Json(json!({
            "access_token": "access.jwt.sig",
            "refresh_token": "refresh-xyz",
            "id_token": "id.jwt.sig"
        }))
    }

    #[tokio::test]
    async fn run_login_writes_auth_json_from_mock_server() -> Result<()> {
        let state = MockState::default();
        let app = Router::new()
            .route("/api/accounts/deviceauth/usercode", post(mock_device_code))
            .route("/api/accounts/deviceauth/token", post(mock_device_token))
            .route("/oauth/token", post(mock_oauth_token))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr: SocketAddr = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = std::env::temp_dir().join(format!("codex-auth-login-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await?;
        let auth_path = dir.join("auth.json");

        // SAFETY: test is serialized per process via the unique temp dir; env
        // override is scoped to this binary.
        // SAFETY: `set_var` requires unsafe in Rust 2024 because env access is
        // process-global; this test binary has no concurrent env readers.
        unsafe {
            std::env::set_var("CODEX_AUTH_BASE_OVERRIDE", format!("http://{addr}"));
        }
        let outcome = run_login(auth_path.clone(), Client::new()).await;
        unsafe {
            std::env::remove_var("CODEX_AUTH_BASE_OVERRIDE");
        }
        outcome?;

        let raw = tokio::fs::read(&auth_path).await?;
        let data: TokenData = serde_json::from_slice(&raw)?;
        assert_eq!(data.access_token, "access.jwt.sig");
        assert_eq!(data.refresh_token, "refresh-xyz");
        assert_eq!(data.id_token, "id.jwt.sig");

        let meta = tokio::fs::metadata(&auth_path).await?;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        tokio::fs::remove_dir_all(&dir).await.ok();
        Ok(())
    }

    #[tokio::test]
    async fn write_atomic_creates_0600_file_and_roundtrips() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("codex-auth-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join("auth.json");
        let data = TokenData {
            access_token: "a".into(),
            refresh_token: "r".into(),
            id_token: "i".into(),
            expires_at: Some(42),
            account_id: Some("acct-1".into()),
        };
        write_atomic(&path, &data).await?;

        let meta = tokio::fs::metadata(&path).await?;
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600 perms, got {:o}", mode);

        let raw = tokio::fs::read(&path).await?;
        let back: TokenData = serde_json::from_slice(&raw)?;
        assert_eq!(back.access_token, "a");
        assert_eq!(back.refresh_token, "r");
        assert_eq!(back.id_token, "i");
        assert_eq!(back.expires_at, Some(42));
        assert_eq!(back.account_id.as_deref(), Some("acct-1"));

        tokio::fs::remove_dir_all(&dir).await.ok();
        Ok(())
    }
}
