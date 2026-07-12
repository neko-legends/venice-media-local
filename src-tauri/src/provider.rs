use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use atomicwrites::{AllowOverwrite, AtomicFile};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use futures_util::StreamExt;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use subtle::ConstantTimeEq;
use tokio::sync::oneshot;
use venice_provider_kernel as wire_kernel;

use super::app_data_dir;
use super::provider_kernel::{
    heartbeat_replay_valid, lifecycle_path, may_create_missing_key, next_heartbeat_sequence,
};

#[cfg(not(test))]
const SECRET_KEY_ACCOUNT: &str = "provider-ledger-key-v1";

static LIFECYCLE_SHUTDOWN: OnceLock<Mutex<Option<oneshot::Sender<()>>>> = OnceLock::new();

const LIFECYCLE_CREDENTIAL_ACCOUNT: &str = "provider-lifecycle-credential";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LifecycleState {
    enabled: bool,
    core_origin: String,
    credential_id: String,
    heartbeat_sequence: i64,
    manifest_digest: String,
    registered: bool,
    #[serde(default)]
    pending_heartbeat: Option<PendingHeartbeat>,
    last_error_code: Option<String>,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingHeartbeat {
    sequence: i64,
    digest: String,
    body: Value,
    created_at: String,
}

fn lifecycle_state_path(app: &tauri::AppHandle) -> Result<PathBuf, ApiError> {
    Ok(provider_root(app)?.join("lifecycle.json"))
}

fn read_lifecycle_state(app: &tauri::AppHandle) -> LifecycleState {
    lifecycle_state_path(app)
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_lifecycle_state(app: &tauri::AppHandle, state: &LifecycleState) -> Result<(), ApiError> {
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|error| ApiError::internal(error.to_string()))?;
    atomic_write(&lifecycle_state_path(app)?, &bytes)
}

fn lifecycle_credential_entry() -> Result<keyring::Entry, ApiError> {
    keyring::Entry::new(super::KEYRING_SERVICE, LIFECYCLE_CREDENTIAL_ACCOUNT).map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PROVIDER_NOT_READY",
            "Lifecycle credential store is unavailable",
        )
    })
}

fn read_lifecycle_credential() -> Result<Option<String>, ApiError> {
    match lifecycle_credential_entry()?.get_password() {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PROVIDER_NOT_READY",
            "Lifecycle credential is invalid",
        )),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PROVIDER_NOT_READY",
            "Lifecycle credential store is unavailable",
        )),
    }
}

pub fn configure_lifecycle(
    app: &tauri::AppHandle,
    core_origin: &str,
    credential_id: &str,
    credential: &str,
) -> Result<(), String> {
    let origin = reqwest::Url::parse(core_origin.trim())
        .map_err(|_| "Core origin is invalid".to_string())?;
    let loopback = matches!(origin.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if !(origin.scheme() == "https" || (origin.scheme() == "http" && loopback))
        || origin.host_str().is_none()
        || credential.len() < 16
        || credential_id.trim().is_empty()
    {
        return Err("Lifecycle configuration is invalid".to_string());
    }
    let previous = read_lifecycle_credential().map_err(|error| format!("{error:?}"))?;
    let existing = read_lifecycle_state(app);
    let normalized_origin = core_origin.trim_end_matches('/').to_string();
    let same_provider = existing.core_origin == normalized_origin;
    let entry = lifecycle_credential_entry().map_err(|error| format!("{error:?}"))?;
    entry
        .set_password(credential)
        .map_err(|_| "Lifecycle credential store is unavailable".to_string())?;
    let state = LifecycleState {
        enabled: true,
        core_origin: normalized_origin,
        credential_id: credential_id.to_string(),
        heartbeat_sequence: if same_provider {
            existing.heartbeat_sequence
        } else {
            -1
        },
        manifest_digest: if same_provider {
            existing.manifest_digest
        } else {
            String::new()
        },
        registered: same_provider && existing.registered,
        pending_heartbeat: if same_provider {
            existing.pending_heartbeat
        } else {
            None
        },
        last_error_code: None,
        updated_at: Utc::now().to_rfc3339(),
    };
    if let Err(error) = write_lifecycle_state(app, &state) {
        match previous {
            Some(value) => {
                let _ = entry.set_password(&value);
            }
            None => {
                let _ = entry.delete_credential();
            }
        }
        return Err(format!("{error:?}"));
    }
    Ok(())
}

pub fn clear_lifecycle(app: &tauri::AppHandle) -> Result<(), String> {
    match lifecycle_credential_entry()
        .map_err(|error| format!("{error:?}"))?
        .delete_credential()
    {
        Ok(_) | Err(keyring::Error::NoEntry) => {}
        Err(_) => return Err("Lifecycle credential store is unavailable".to_string()),
    }
    let mut state = read_lifecycle_state(app);
    state.enabled = false;
    state.registered = false;
    state.pending_heartbeat = None;
    state.updated_at = Utc::now().to_rfc3339();
    write_lifecycle_state(app, &state).map_err(|error| format!("{error:?}"))
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    wire_kernel::sha256_hex(bytes)
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

fn random_id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("{prefix}{}", hex(&bytes))
}

fn secure_http_client() -> Result<reqwest::Client, ApiError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|error| ApiError::internal(error.to_string()))
}

async fn bounded_json_response(
    response: reqwest::Response,
    limit: usize,
) -> Result<Value, ApiError> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "UPSTREAM_REJECTED",
            "Upstream JSON response exceeded its bound",
        ));
    }
    let mut bytes =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ApiError::internal(error.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "UPSTREAM_REJECTED",
                "Upstream JSON response exceeded its bound",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|error| ApiError::internal(error.to_string()))
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    retryable: bool,
    certainty: &'static str,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retryable: false,
            certainty: "not_submitted",
        }
    }

    fn internal(correlation: impl AsRef<str>) -> Self {
        let digest = sha256_hex(correlation.as_ref().as_bytes());
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            format!("Internal provider error (correlation {})", &digest[..16]),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "code": self.code,
                    "message": self.message,
                    "retryable": self.retryable,
                    "submissionCertainty": self.certainty,
                    "details": {}
                }
            })),
        )
            .into_response()
    }
}

#[derive(Clone)]
pub struct TauriSecretProtector {
    app: tauri::AppHandle,
}

impl TauriSecretProtector {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

impl wire_kernel::SecretProtector for TauriSecretProtector {
    fn protect(&self, plaintext: &[u8]) -> Result<wire_kernel::EncryptedSecret, String> {
        let secret =
            String::from_utf8(plaintext.to_vec()).map_err(|_| "Secret is not UTF-8".to_string())?;
        let encrypted = encrypt_secrets(&self.app, &json!({"secret":secret}))
            .map_err(|error| format!("{error:?}"))?;
        Ok(wire_kernel::EncryptedSecret {
            key_id: encrypted.key_id.clone(),
            ciphertext: serde_json::to_string(&encrypted).map_err(|error| error.to_string())?,
        })
    }

    fn unprotect(&self, encrypted: &wire_kernel::EncryptedSecret) -> Result<Vec<u8>, String> {
        let envelope: EncryptedSecrets =
            serde_json::from_str(&encrypted.ciphertext).map_err(|error| error.to_string())?;
        if envelope.key_id != encrypted.key_id {
            return Err("Secret key identity conflicts".to_string());
        }
        let value = decrypt_secrets(&self.app, &envelope).map_err(|error| format!("{error:?}"))?;
        value
            .get("secret")
            .and_then(Value::as_str)
            .map(|secret| secret.as_bytes().to_vec())
            .ok_or_else(|| "Secret envelope is invalid".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptedSecrets {
    algorithm: String,
    key_id: String,
    nonce: String,
    ciphertext: String,
}

fn provider_root(app: &tauri::AppHandle) -> Result<PathBuf, ApiError> {
    let root = app_data_dir(app)
        .map_err(ApiError::internal)?
        .join("provider-v1");
    fs::create_dir_all(root.join("uploads"))
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(root)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    let parent = path
        .parent()
        .ok_or_else(|| ApiError::internal("ledger path has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| ApiError::internal(error.to_string()))?;
    AtomicFile::new(path, AllowOverwrite)
        .write(|file| file.write_all(bytes).and_then(|_| file.sync_all()))
        .map_err(|error| ApiError::internal(error.to_string()))?;
    if let Ok(directory) = OpenOptions::new().read(true).open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn ledger_key(app: Option<&tauri::AppHandle>, allow_create: bool) -> Result<[u8; 32], ApiError> {
    #[cfg(test)]
    if super::TEST_APP_DATA_DIR.get().is_some() {
        let digest = Sha256::digest(b"venice-media-local-integrated-test-ledger-key");
        let mut key = [0u8; 32];
        key.copy_from_slice(&digest);
        return Ok(key);
    }
    #[cfg(test)]
    {
        let _ = may_create_missing_key(allow_create, app.is_some());
        let digest = Sha256::digest(b"venice-media-local-test-ledger-key");
        let mut key = [0u8; 32];
        key.copy_from_slice(&digest);
        return Ok(key);
    }
    #[cfg(not(test))]
    {
        let entry = keyring::Entry::new(super::KEYRING_SERVICE, SECRET_KEY_ACCOUNT)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        match entry.get_secret() {
            Ok(value) if value.len() == 32 => {
                let mut key = [0u8; 32];
                key.copy_from_slice(&value);
                return Ok(key);
            }
            Ok(_) => {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "PROVIDER_NOT_READY",
                    "Provider secret key has an invalid format",
                ));
            }
            Err(keyring::Error::NoEntry) => {}
            Err(_) => {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "PROVIDER_NOT_READY",
                    "Operating-system credential storage is unavailable",
                ));
            }
        }
        let existing_ledger = app
            .and_then(|handle| app_data_dir(handle).ok())
            .map(|root| root.join("provider-v2").join("ledger.json"))
            .is_some_and(|path| path.exists());
        if !may_create_missing_key(allow_create, existing_ledger) {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "PROVIDER_NOT_READY",
                "Provider secret key is missing for the existing ledger",
            ));
        }
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        entry.set_secret(&key).map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "PROVIDER_NOT_READY",
                "Operating-system credential storage is unavailable",
            )
        })?;
        Ok(key)
    }
}

#[cfg(not(test))]
fn ledger_key_configured() -> Result<bool, ApiError> {
    let entry = keyring::Entry::new(super::KEYRING_SERVICE, SECRET_KEY_ACCOUNT).map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PROVIDER_NOT_READY",
            "Operating-system credential storage is unavailable",
        )
    })?;
    match entry.get_secret() {
        Ok(value) if value.len() == 32 => Ok(true),
        Ok(_) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PROVIDER_NOT_READY",
            "Provider secret key has an invalid format",
        )),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(_) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PROVIDER_NOT_READY",
            "Operating-system credential storage is unavailable",
        )),
    }
}

#[cfg(test)]
fn ledger_key_configured() -> Result<bool, ApiError> {
    Ok(true)
}

fn encrypt_secrets(app: &tauri::AppHandle, value: &Value) -> Result<EncryptedSecrets, ApiError> {
    let key = ledger_key(Some(app), true)?;
    encrypt_secrets_with_key(&key, value)
}

fn encrypt_secrets_with_key(key: &[u8; 32], value: &Value) -> Result<EncryptedSecrets, ApiError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| ApiError::internal("cipher init"))?;
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let plaintext =
        serde_json::to_vec(value).map_err(|error| ApiError::internal(error.to_string()))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_ref())
        .map_err(|_| ApiError::internal("secret encryption"))?;
    Ok(EncryptedSecrets {
        algorithm: "AES-256-GCM".to_string(),
        key_id: "os-credential:provider-ledger-key-v1".to_string(),
        nonce: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, nonce),
        ciphertext: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, ciphertext),
    })
}

fn decrypt_secrets(app: &tauri::AppHandle, value: &EncryptedSecrets) -> Result<Value, ApiError> {
    let key = ledger_key(Some(app), false)?;
    decrypt_secrets_with_key(&key, value)
}

fn decrypt_secrets_with_key(key: &[u8; 32], value: &EncryptedSecrets) -> Result<Value, ApiError> {
    if value.algorithm != "AES-256-GCM" || value.key_id != "os-credential:provider-ledger-key-v1" {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "CALLBACK_DELIVERY_DEGRADED",
            "Operation secret material cannot be decrypted",
        ));
    }
    let nonce = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &value.nonce)
        .map_err(|_| ApiError::internal("secret nonce decode"))?;
    if nonce.len() != 12 {
        return Err(ApiError::internal("invalid secret nonce"));
    }
    let ciphertext = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &value.ciphertext,
    )
    .map_err(|_| ApiError::internal("secret ciphertext decode"))?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| ApiError::internal("cipher init"))?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "CALLBACK_DELIVERY_DEGRADED",
                "Operation secret material cannot be decrypted",
            )
        })?;
    serde_json::from_slice(&plaintext).map_err(|error| ApiError::internal(error.to_string()))
}

fn canonical_json(value: &Value) -> Value {
    wire_kernel::canonical_json(value)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub fn configured_core_origin(app: &tauri::AppHandle) -> Option<String> {
    let state = read_lifecycle_state(app);
    if state.enabled && !state.core_origin.is_empty() {
        Some(state.core_origin)
    } else {
        std::env::var("EVA_CORE_ORIGIN")
            .ok()
            .filter(|value| !value.trim().is_empty())
    }
}

fn current_manifest_digest(app: &tauri::AppHandle) -> Result<String, ApiError> {
    let lifecycle = read_lifecycle_state(app);
    if lifecycle.registered && valid_sha256(&lifecycle.manifest_digest) {
        return Ok(lifecycle.manifest_digest);
    }
    let settings = super::read_settings(app);
    let address =
        super::agent_control_address(settings.agent_control_port, settings.agent_control_bind_all);
    let manifest = super::capability_manifest(app, &address).map_err(ApiError::internal)?;
    serde_json::to_vec(&canonical_json(&manifest))
        .map(|bytes| sha256_hex(&bytes))
        .map_err(|error| ApiError::internal(error.to_string()))
}

pub fn shared_manifest_digest(app: &tauri::AppHandle) -> Result<String, String> {
    current_manifest_digest(app).map_err(|error| format!("{error:?}"))
}

async fn register_lifecycle_once(
    app: &tauri::AppHandle,
    state: &mut LifecycleState,
    credential: &str,
) -> Result<(), ApiError> {
    let settings = super::read_settings(app);
    let address =
        super::agent_control_address(settings.agent_control_port, settings.agent_control_bind_all);
    let manifest = super::capability_manifest(app, &address).map_err(ApiError::internal)?;
    let response = secure_http_client()?
        .post(format!(
            "{}{}",
            state.core_origin,
            lifecycle_path("register", "").ok_or_else(|| ApiError::internal("lifecycle path"))?
        ))
        .bearer_auth(credential)
        .json(&json!({
            "supportedSchemaVersions": ["1.0"],
            "manifest": manifest,
            "leaseDurationMs": 90_000
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "UPSTREAM_UNAVAILABLE",
                "Core registration is unavailable",
            )
        })?;
    if !response.status().is_success() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "UPSTREAM_REJECTED",
            "Core registration was rejected",
        ));
    }
    let payload = bounded_json_response(response, 256 * 1024).await?;
    state.manifest_digest = payload
        .get("manifestDigest")
        .or_else(|| payload.pointer("/provider/manifestDigest"))
        .and_then(Value::as_str)
        .filter(|value| valid_sha256(value))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "UPSTREAM_REJECTED",
                "Core registration omitted manifest digest",
            )
        })?
        .to_string();
    state.registered = true;
    if state.pending_heartbeat.as_ref().is_some_and(|pending| {
        pending.body.get("manifestDigest").and_then(Value::as_str)
            != Some(state.manifest_digest.as_str())
    }) {
        state.pending_heartbeat = None;
    }
    state.last_error_code = None;
    state.updated_at = Utc::now().to_rfc3339();
    write_lifecycle_state(app, state)
}

async fn send_lifecycle_heartbeat(
    app: &tauri::AppHandle,
    state: &mut LifecycleState,
    credential: &str,
) -> Result<(), ApiError> {
    let instance_id = super::installation_instance_id(app).map_err(ApiError::internal)?;
    if state.pending_heartbeat.is_none() {
        let health = super::capability_health(app).map_err(ApiError::internal)?;
        let body = json!({
            "sequence": next_heartbeat_sequence(state.heartbeat_sequence),
            "observedAt": Utc::now().to_rfc3339(),
            "manifestDigest": state.manifest_digest,
            "health": { "state": health.get("status").cloned().unwrap_or(json!("degraded")), "detail": health },
            "activeOperationCount": operation_health(app).get("activeOperationCount").cloned().unwrap_or(json!(0))
        });
        let digest = sha256_hex(
            &serde_json::to_vec(&canonical_json(&body))
                .map_err(|error| ApiError::internal(error.to_string()))?,
        );
        state.pending_heartbeat = Some(PendingHeartbeat {
            sequence: next_heartbeat_sequence(state.heartbeat_sequence),
            digest,
            body,
            created_at: Utc::now().to_rfc3339(),
        });
        state.updated_at = Utc::now().to_rfc3339();
        write_lifecycle_state(app, state)?;
    }
    let pending = state
        .pending_heartbeat
        .clone()
        .ok_or_else(|| ApiError::internal("pending heartbeat missing"))?;
    let persisted_digest = sha256_hex(
        &serde_json::to_vec(&canonical_json(&pending.body))
            .map_err(|error| ApiError::internal(error.to_string()))?,
    );
    if !heartbeat_replay_valid(
        &pending.digest,
        &persisted_digest,
        pending.sequence,
        pending.body.get("sequence").and_then(Value::as_i64),
    ) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "HEARTBEAT_EVIDENCE_INVALID",
            "Persisted heartbeat replay evidence is invalid",
        ));
    }
    let response = secure_http_client()?
        .post(format!(
            "{}{}",
            state.core_origin,
            lifecycle_path("heartbeat", &instance_id)
                .ok_or_else(|| ApiError::internal("lifecycle path"))?
        ))
        .bearer_auth(credential)
        .json(&pending.body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "HEARTBEAT_DELIVERY_AMBIGUOUS",
                "Core heartbeat delivery is ambiguous and will be replayed",
            )
        })?;
    if !response.status().is_success() {
        let status = response.status();
        let payload = bounded_json_response(response, 64 * 1024)
            .await
            .unwrap_or(Value::Null);
        let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
        let registration_required = status == StatusCode::NOT_FOUND
            || matches!(
                code,
                "PROVIDER_NOT_FOUND" | "PROVIDER_STOPPED" | "PROVIDER_MANIFEST_MISMATCH"
            );
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            if registration_required {
                "LIFECYCLE_REGISTRATION_REQUIRED"
            } else {
                "HEARTBEAT_RETRY_PENDING"
            },
            "Core heartbeat was not acknowledged and will retain its exact replay body",
        ));
    }
    state.heartbeat_sequence = pending.sequence;
    state.pending_heartbeat = None;
    state.last_error_code = None;
    state.updated_at = Utc::now().to_rfc3339();
    write_lifecycle_state(app, state)
}

pub fn start_lifecycle(app: tauri::AppHandle) {
    let shutdown = LIFECYCLE_SHUTDOWN.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = shutdown.lock() {
        if let Some(previous) = guard.take() {
            let _ = previous.send(());
        }
        let (tx, mut rx) = oneshot::channel();
        *guard = Some(tx);
        tauri::async_runtime::spawn(async move {
            let mut state = read_lifecycle_state(&app);
            if !state.enabled {
                return;
            }
            let credential = match read_lifecycle_credential() {
                Ok(Some(value)) => value,
                _ => {
                    state.last_error_code = Some("LIFECYCLE_CREDENTIAL_UNAVAILABLE".to_string());
                    let _ = write_lifecycle_state(&app, &state);
                    return;
                }
            };
            loop {
                let result = if !state.registered {
                    register_lifecycle_once(&app, &mut state, &credential).await
                } else {
                    send_lifecycle_heartbeat(&app, &mut state, &credential).await
                };
                if let Err(error) = result {
                    state.last_error_code = Some(error.code.to_string());
                    if error.code == "LIFECYCLE_REGISTRATION_REQUIRED" {
                        state.registered = false;
                    }
                    state.updated_at = Utc::now().to_rfc3339();
                    let _ = write_lifecycle_state(&app, &state);
                }
                tokio::select! {
                    _ = &mut rx => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                }
            }
        });
    }
}

pub async fn unregister_lifecycle(app: &tauri::AppHandle) {
    if let Ok(mut guard) = LIFECYCLE_SHUTDOWN.get_or_init(|| Mutex::new(None)).lock() {
        if let Some(tx) = guard.take() {
            let _ = tx.send(());
        }
    }
    let mut state = read_lifecycle_state(app);
    if !state.enabled || !state.registered {
        return;
    }
    let Ok(Some(credential)) = read_lifecycle_credential() else {
        return;
    };
    let Ok(instance_id) = super::installation_instance_id(app) else {
        return;
    };
    let response = match secure_http_client() {
        Ok(client) => client,
        Err(_) => return,
    }
    .delete(format!(
        "{}{}",
        state.core_origin,
        lifecycle_path("unregister", &instance_id).unwrap_or_default()
    ))
    .bearer_auth(credential)
    .timeout(std::time::Duration::from_secs(10))
    .send()
    .await;
    if response
        .as_ref()
        .is_ok_and(|value| value.status().is_success())
    {
        state.registered = false;
        state.pending_heartbeat = None;
        state.updated_at = Utc::now().to_rfc3339();
        let _ = write_lifecycle_state(app, &state);
    }
}

pub fn lifecycle_health(app: &tauri::AppHandle) -> Value {
    let state = read_lifecycle_state(app);
    json!({
        "enabled": state.enabled,
        "registered": state.registered,
        "credentialId": state.credential_id,
        "heartbeatSequence": state.heartbeat_sequence,
        "pendingHeartbeatSequence": state.pending_heartbeat.as_ref().map(|pending| pending.sequence),
        "pendingHeartbeatDigest": state.pending_heartbeat.as_ref().map(|pending| pending.digest.clone()),
        "lastErrorCode": state.last_error_code,
        "fallbackActive": !state.enabled || !state.registered
    })
}

pub fn operation_health(app: &tauri::AppHandle) -> Value {
    let secret_store_ready = ledger_key_configured().unwrap_or(false);
    let root = app_data_dir(app).ok().map(|path| path.join("provider-v2"));
    let artifact_writable = root.as_ref().is_some_and(|root| {
        let _ = fs::create_dir_all(root);
        let probe = root.join(format!(".write-probe-{}", random_id("")));
        let result = fs::write(&probe, b"ok");
        let _ = fs::remove_file(probe);
        result.is_ok()
    });
    let ledger = root
        .and_then(|root| fs::read(root.join("ledger.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    let active = ledger
        .get("operations")
        .and_then(Value::as_object)
        .map(|operations| {
            operations
                .values()
                .filter(|operation| {
                    !matches!(
                        operation.get("state").and_then(Value::as_str),
                        Some("completed" | "failed" | "canceled" | "lost")
                    )
                })
                .count()
        })
        .unwrap_or(0);
    json!({"ready":secret_store_ready && artifact_writable,"secretStoreReady":secret_store_ready,"activeOperationCount":active,
        "pendingCallbackCount":0,"callbackDegradedCount":0,"artifactWritable":artifact_writable,"ledgerOwner":"venice-provider-kernel"})
}
