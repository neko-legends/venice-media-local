use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use subtle::ConstantTimeEq;
use tokio::sync::{watch, Mutex};

pub const OPERATIONS_PATH: &str = "/api/v1/operations";
pub const OPERATION_PATH: &str = "/api/v1/operations/:operation_id";
pub const OPERATION_EVENTS_PATH: &str = "/api/v1/operations/:operation_id/events";
pub const OPERATION_EXECUTE_PATH: &str = "/api/v1/operations/:operation_id/execute";
pub const OPERATION_CANCEL_PATH: &str = "/api/v1/operations/:operation_id/cancel";
pub const OPERATION_GRANTS_PATH: &str = "/api/v1/operations/:operation_id/transfer-grants";
pub const UPLOADS_PATH: &str = "/api/v1/artifact-uploads";
pub const UPLOAD_CONTENT_PATH: &str = "/api/v1/artifact-uploads/:upload_id/content";
pub const UPLOAD_COMPLETE_PATH: &str = "/api/v1/artifact-uploads/:upload_id/complete";
pub const UPLOAD_PATH: &str = "/api/v1/artifact-uploads/:upload_id";
pub const ARTIFACT_PATH: &str = "/api/v1/artifacts/:artifact_id";
pub const ARTIFACT_CONTENT_PATH: &str = "/api/v1/artifacts/:artifact_id/content";

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        value => value.clone(),
    }
}

pub fn canonical_digest(value: &Value) -> Result<String, serde_json::Error> {
    serde_json::to_vec(&canonical_json(value)).map(|bytes| sha256_hex(&bytes))
}

pub fn input_digest(
    input: &Value,
    artifacts: &[InputArtifactRef],
) -> Result<String, serde_json::Error> {
    canonical_digest(&json!({ "input": input, "inputArtifacts": artifacts }))
}

pub fn request_digest(request: &SubmitRequest) -> Result<String, serde_json::Error> {
    canonical_digest(&json!({
        "schemaVersion": request.schema_version,
        "type": request.envelope_type,
        "requestId": request.request_id,
        "idempotencyKey": request.idempotency_key,
        "coreOperationId": request.core_operation_id,
        "attempt": request.attempt,
        "assignmentRevision": request.assignment_revision,
        "capability": request.capability,
        "manifestDigest": request.manifest_digest,
        "catalogRevision": request.catalog_revision,
        "inputDigest": request.input_digest,
        "input": request.input,
        "inputArtifacts": request.input_artifacts,
        "callbackUrl": request.callback.url,
        "callbackExpiresAt": request.callback.expires_at,
        "requestedAt": request.requested_at
    }))
}

pub fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityRef {
    pub id: String,
    pub revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallbackRequest {
    pub url: String,
    pub authorization: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputArtifactRef {
    pub upload_id: String,
    pub sha256: String,
    pub byte_size: u64,
    pub mime_type: String,
    pub relationship: String,
    #[serde(default)]
    pub core_artifact_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubmitRequest {
    pub schema_version: String,
    #[serde(rename = "type")]
    pub envelope_type: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub core_operation_id: String,
    pub attempt: u32,
    pub assignment_revision: u32,
    pub capability: CapabilityRef,
    pub manifest_digest: String,
    #[serde(default)]
    pub catalog_revision: Option<String>,
    pub input_digest: String,
    pub input: Value,
    #[serde(default)]
    pub input_artifacts: Vec<InputArtifactRef>,
    pub callback: CallbackRequest,
    pub requested_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransferAuthorization {
    pub grant_id: String,
    pub secret: String,
    pub core_operation_id: String,
    pub attempt: u32,
    pub assignment_revision: u32,
    pub capability_id: String,
    pub method: String,
    pub path: String,
    pub scope: String,
    #[serde(default)]
    pub upload_id: Option<String>,
    #[serde(default)]
    pub artifact_id: Option<String>,
    #[serde(default)]
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub expected_byte_size: Option<u64>,
    #[serde(default)]
    pub expected_mime_type: Option<String>,
    pub not_before: String,
    pub expires_at: String,
    pub max_uses: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateUploadRequest {
    pub upload_id: String,
    pub provider_operation_id: String,
    pub expected_mime_type: String,
    pub expected_byte_size: u64,
    pub expected_sha256: String,
    pub capability_id: String,
    pub relationship: String,
    #[serde(default)]
    pub core_artifact_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub phase: String,
    pub completed: Option<u64>,
    pub total: Option<u64>,
    pub unit: Option<String>,
    pub percent: Option<f64>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderArtifact {
    pub schema_version: String,
    pub provider_artifact_id: String,
    pub provider_operation_id: String,
    pub kind: String,
    pub mime_type: String,
    pub byte_size: u64,
    pub sha256: String,
    pub media: Value,
    pub model: Value,
    pub controls: Value,
    pub recipe: Value,
    pub source_artifacts: Vec<Value>,
    pub source_evidence: Option<Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationEvent {
    pub schema_version: String,
    pub event_id: String,
    pub sequence: u64,
    pub state: String,
    pub provider_operation_id: String,
    pub occurred_at: String,
    pub progress: Progress,
    pub artifacts: Vec<ProviderArtifact>,
    pub resource_usage: Value,
    pub result: Value,
    pub output: Value,
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOperation {
    pub schema_version: String,
    pub provider_operation_id: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub request_digest: String,
    pub client_id: String,
    pub core_operation_id: String,
    pub attempt: u32,
    pub assignment_revision: u32,
    pub capability: CapabilityRef,
    pub manifest_digest: String,
    pub catalog_revision: Option<String>,
    pub input_digest: String,
    pub input: Value,
    pub input_artifacts: Vec<InputArtifactRef>,
    pub state: String,
    pub submission_state: String,
    pub submission_certainty: String,
    pub provider_request_id: Option<String>,
    pub upstream_id: Option<String>,
    pub execution_requested: bool,
    pub progress: Progress,
    pub artifacts: Vec<ProviderArtifact>,
    pub resource_usage: Value,
    pub terminal_error: Option<Value>,
    pub result: Value,
    pub output: Value,
    pub event_sequence: u64,
    pub created_at: String,
    pub updated_at: String,
    pub callback_url: String,
    pub callback_secret: Option<EncryptedSecret>,
    pub callback_expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrantRecord {
    grant: TransferAuthorization,
    client_id: String,
    provider_operation_id: String,
    encrypted_secret: Option<EncryptedSecret>,
    binding_digest: String,
    uses: u32,
    last_request_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadRecord {
    upload_id: String,
    provider_operation_id: String,
    client_id: String,
    capability_id: String,
    expected_mime_type: String,
    expected_byte_size: u64,
    expected_sha256: String,
    relationship: String,
    core_artifact_id: Option<String>,
    state: String,
    path: String,
    created_at: String,
    expires_at: String,
    consumed_at: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallbackRecord {
    event_id: String,
    operation_id: String,
    attempts: u32,
    next_attempt_at: String,
    acknowledged_at: Option<String>,
    ack_digest: Option<String>,
    last_status: Option<u16>,
    degraded: bool,
    last_error: Option<String>,
    erase_after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelRecord {
    idempotency_key: String,
    request_digest: String,
    outcome: String,
    event_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Ledger {
    operations: BTreeMap<String, ProviderOperation>,
    idempotency: BTreeMap<String, String>,
    events: BTreeMap<String, Vec<OperationEvent>>,
    grants: BTreeMap<String, GrantRecord>,
    uploads: BTreeMap<String, UploadRecord>,
    artifacts: BTreeMap<String, ProviderArtifact>,
    artifact_paths: BTreeMap<String, String>,
    #[serde(default)]
    callback_outbox: BTreeMap<String, CallbackRecord>,
    #[serde(default)]
    cancels: BTreeMap<String, CancelRecord>,
}

#[derive(Debug, Clone)]
pub struct ExecutionInput {
    pub operation: ProviderOperation,
    pub artifacts: Vec<(InputArtifactRef, Vec<u8>)>,
}

#[derive(Debug, Clone)]
pub struct ExecutionArtifact {
    pub kind: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub media: Value,
    pub model: Value,
    pub controls: Value,
    pub recipe: Value,
    pub source_evidence: Option<Value>,
    pub source_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptedSecret {
    pub key_id: String,
    pub ciphertext: String,
}

pub trait SecretProtector: Send + Sync + 'static {
    fn protect(&self, plaintext: &[u8]) -> Result<EncryptedSecret, String>;
    fn unprotect(&self, encrypted: &EncryptedSecret) -> Result<Vec<u8>, String>;
}

pub trait Storage: Send + Sync + 'static {
    fn path(&self, relative: &str) -> PathBuf;
    fn create_dir_all(&self, relative: &str) -> Result<(), String>;
    fn read(&self, relative: &str) -> Result<Option<Vec<u8>>, String>;
    fn write_atomic(&self, relative: &str, bytes: &[u8]) -> Result<(), String>;
    fn recover_atomic(&self, relative: &str) -> Result<(), String>;
}

#[derive(Clone)]
pub struct FileStorage {
    root: PathBuf,
}
impl FileStorage {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Storage for FileStorage {
    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
    fn create_dir_all(&self, relative: &str) -> Result<(), String> {
        fs::create_dir_all(self.path(relative)).map_err(|error| error.to_string())
    }
    fn read(&self, relative: &str) -> Result<Option<Vec<u8>>, String> {
        match fs::read(self.path(relative)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
    fn write_atomic(&self, relative: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.path(relative);
        let parent = path
            .parent()
            .ok_or_else(|| "Atomic path has no parent".to_string())?;
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let temp = parent.join(format!(
            ".{}.{}.tmp",
            path.file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("ledger"),
            random_id("")
        ));
        let backup = path.with_extension("bak");
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|error| error.to_string())?;
        use std::io::Write as _;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| error.to_string())?;
        drop(file);
        if path.exists() {
            if backup.exists() {
                fs::remove_file(&backup).map_err(|error| error.to_string())?;
            }
            fs::rename(&path, &backup).map_err(|error| error.to_string())?;
        }
        if let Err(error) = fs::rename(&temp, &path) {
            if backup.exists() && !path.exists() {
                let _ = fs::rename(&backup, &path);
            }
            let _ = fs::remove_file(&temp);
            return Err(error.to_string());
        }
        if backup.exists() {
            fs::remove_file(backup).map_err(|error| error.to_string())?;
        }
        #[cfg(unix)]
        {
            fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
    fn recover_atomic(&self, relative: &str) -> Result<(), String> {
        let path = self.path(relative);
        let backup = path.with_extension("bak");
        if !path.exists() && backup.exists() {
            fs::rename(&backup, &path).map_err(|error| error.to_string())?;
        } else if path.exists() && backup.exists() {
            fs::remove_file(backup).map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct SubmissionReceipt {
    pub upstream_id: String,
    pub certainty: String,
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub artifacts: Vec<ExecutionArtifact>,
    pub result: Value,
    pub output: Value,
}

#[async_trait]
pub trait Executor: Send + Sync + 'static {
    async fn validate(&self, _capability: &CapabilityRef, _input: &Value) -> Result<(), String> {
        Ok(())
    }
    async fn submit(
        &self,
        input: ExecutionInput,
        provider_request_id: &str,
    ) -> Result<SubmissionReceipt, String>;
    async fn resume(
        &self,
        input: ExecutionInput,
        upstream_id: &str,
    ) -> Result<ExecutionResult, String>;
    async fn finalize_artifact(
        &self,
        _operation: &ProviderOperation,
        _artifact_id: &str,
        _sha256: &str,
        _byte_size: u64,
        artifact: ExecutionArtifact,
    ) -> Result<ExecutionArtifact, String> {
        Ok(artifact)
    }
}

#[derive(Clone)]
pub struct KernelConfig {
    pub storage: Arc<dyn Storage>,
    pub token: String,
    pub manifest_digest: String,
    pub trusted_callback_origin: String,
    pub executor: Arc<dyn Executor>,
    pub secret_protector: Arc<dyn SecretProtector>,
    pub callback_retry_base_ms: u64,
    pub terminal_replay_window_ms: i64,
    pub maintenance_interval_ms: u64,
}

#[derive(Clone)]
pub struct Kernel {
    config: KernelConfig,
    ledger: Arc<Mutex<Ledger>>,
    callback_workers: Arc<Mutex<std::collections::BTreeSet<String>>>,
    maintenance_shutdown: watch::Sender<bool>,
    maintenance_running: Arc<AtomicBool>,
    maintenance_starts: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct ApiError(StatusCode, &'static str, String);

impl ApiError {
    fn bad(code: &'static str, message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, code, message.into())
    }
    fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self(StatusCode::FORBIDDEN, code, message.into())
    }
    fn internal(message: impl Into<String>) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            message.into(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":{"code":self.1,"message":self.2,"retryable":false,"submissionCertainty":"not_submitted","details":{}}}))).into_response()
    }
}

impl Kernel {
    pub async fn open(config: KernelConfig) -> Result<Self, String> {
        config.storage.create_dir_all("uploads")?;
        config.storage.create_dir_all("artifacts")?;
        config.storage.recover_atomic("ledger.json")?;
        let ledger = match config.storage.read("ledger.json")? {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string())?,
            None => Ledger::default(),
        };
        let (maintenance_shutdown, _) = watch::channel(false);
        let kernel = Self {
            config,
            ledger: Arc::new(Mutex::new(ledger)),
            callback_workers: Arc::new(Mutex::new(std::collections::BTreeSet::new())),
            maintenance_shutdown,
            maintenance_running: Arc::new(AtomicBool::new(false)),
            maintenance_starts: Arc::new(AtomicUsize::new(0)),
        };
        kernel.recover().await?;
        kernel.start_maintenance();
        Ok(kernel)
    }

    pub fn router(self) -> Router {
        Router::new()
            .route(OPERATIONS_PATH, post(submit))
            .route(OPERATION_PATH, get(get_operation))
            .route(OPERATION_EVENTS_PATH, get(get_events))
            .route(OPERATION_EXECUTE_PATH, post(execute))
            .route(OPERATION_CANCEL_PATH, post(cancel))
            .route(OPERATION_GRANTS_PATH, post(register_grant))
            .route(UPLOADS_PATH, post(create_upload))
            .route(UPLOAD_CONTENT_PATH, put(write_upload))
            .route(UPLOAD_COMPLETE_PATH, post(seal_upload))
            .route(UPLOAD_PATH, delete(delete_upload))
            .route(ARTIFACT_PATH, get(get_artifact))
            .route(ARTIFACT_CONTENT_PATH, get(read_artifact))
            .with_state(self)
    }
    fn start_maintenance(&self) {
        if self
            .maintenance_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.maintenance_starts.fetch_add(1, Ordering::SeqCst);
        let kernel = self.clone();
        let mut shutdown = self.maintenance_shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {changed=shutdown.changed()=>{if changed.is_err()||*shutdown.borrow(){break}},_=tokio::time::sleep(std::time::Duration::from_millis(kernel.config.maintenance_interval_ms.max(1)))=>{let _=kernel.maintenance_at(Utc::now()).await;}}
            }
            kernel.maintenance_running.store(false, Ordering::SeqCst);
        });
    }
    pub async fn shutdown(&self) {
        let _ = self.maintenance_shutdown.send(true);
        for _ in 0..100 {
            if !self.maintenance_running.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
    }

    async fn persist_locked(&self, ledger: &Ledger) -> Result<(), ApiError> {
        let bytes =
            serde_json::to_vec_pretty(ledger).map_err(|e| ApiError::internal(e.to_string()))?;
        self.config
            .storage
            .write_atomic("ledger.json", &bytes)
            .map_err(ApiError::internal)
    }

    pub async fn recover(&self) -> Result<(), String> {
        let operations = self
            .ledger
            .lock()
            .await
            .operations
            .values()
            .filter(|op| op.execution_requested && !terminal(&op.state))
            .cloned()
            .collect::<Vec<_>>();
        for operation in operations {
            if operation.submission_state == "submission_started"
                && (operation.submission_certainty != "submitted_confirmed"
                    || operation.upstream_id.as_deref().unwrap_or("").is_empty())
            {
                self.mark_lost(
                    &operation.provider_operation_id,
                    "Submission outcome is ambiguous and has no durable upstream identity",
                )
                .await
                .map_err(|error| error.2)?;
            } else {
                self.spawn_execution(operation.provider_operation_id);
            }
        }
        let callbacks = self
            .ledger
            .lock()
            .await
            .callback_outbox
            .values()
            .filter(|item| item.acknowledged_at.is_none() && !item.degraded)
            .map(|item| (item.event_id.clone(), item.next_attempt_at.clone()))
            .collect::<Vec<_>>();
        for (event_id, _) in callbacks {
            self.schedule_callback(event_id).await;
        }
        self.maintenance_at(Utc::now())
            .await
            .map_err(|error| error.2)?;
        Ok(())
    }

    fn spawn_execution(&self, id: String) {
        let kernel = self.clone();
        tokio::spawn(async move {
            let _ = kernel.run_execution(id).await;
        });
    }

    async fn run_execution(&self, id: String) -> Result<(), ApiError> {
        let (execution, phase, identity) = {
            let mut ledger = self.ledger.lock().await;
            let op = ledger
                .operations
                .get_mut(&id)
                .ok_or_else(|| ApiError::internal("operation missing"))?;
            if terminal(&op.state) {
                return Ok(());
            }
            let phase = op.submission_state.clone();
            if phase == "submission_not_started" {
                op.submission_state = "submission_started".into();
                op.submission_certainty = "not_submitted".into();
                op.provider_request_id =
                    Some(format!("vml-request-{}", &sha256_hex(id.as_bytes())[..32]));
                op.state = "running".into();
                op.updated_at = Utc::now().to_rfc3339();
            }
            let identity = if phase == "submission_not_started" {
                op.provider_request_id.clone()
            } else {
                op.upstream_id.clone()
            };
            let op_clone = op.clone();
            self.persist_locked(&ledger).await?;
            let mut artifacts = Vec::new();
            for reference in &op_clone.input_artifacts {
                let upload = ledger.uploads.get(&reference.upload_id).ok_or_else(|| {
                    ApiError::bad("INPUT_NOT_SEALED", "Declared upload is missing")
                })?;
                artifacts.push((
                    reference.clone(),
                    fs::read(&upload.path).map_err(|e| ApiError::internal(e.to_string()))?,
                ));
            }
            for reference in &op_clone.input_artifacts {
                if let Some(upload) = ledger.uploads.get_mut(&reference.upload_id) {
                    upload
                        .consumed_at
                        .get_or_insert_with(|| Utc::now().to_rfc3339());
                }
            }
            self.persist_locked(&ledger).await?;
            (
                ExecutionInput {
                    operation: op_clone,
                    artifacts,
                },
                phase,
                identity,
            )
        };
        if phase == "submission_not_started" {
            let request_id =
                identity.ok_or_else(|| ApiError::internal("provider request identity missing"))?;
            match self.config.executor.submit(execution, &request_id).await {
                Ok(receipt)
                    if !receipt.upstream_id.trim().is_empty()
                        && receipt.certainty == "submitted_confirmed" =>
                {
                    let mut ledger = self.ledger.lock().await;
                    let op = ledger
                        .operations
                        .get_mut(&id)
                        .ok_or_else(|| ApiError::internal("operation missing"))?;
                    op.upstream_id = Some(receipt.upstream_id);
                    op.submission_certainty = receipt.certainty;
                    op.submission_state = "submitted_confirmed".into();
                    op.updated_at = Utc::now().to_rfc3339();
                    self.persist_locked(&ledger).await?;
                    drop(ledger);
                    self.spawn_execution(id);
                }
                Ok(_) => {
                    self.mark_lost(&id, "Executor returned no confirmed upstream identity")
                        .await?
                }
                Err(message) => {
                    self.mark_lost(&id, &format!("Submission outcome is ambiguous: {message}"))
                        .await?
                }
            }
        } else if phase == "submitted_confirmed" {
            let upstream_id = identity
                .ok_or_else(|| ApiError::internal("confirmed upstream identity missing"))?;
            match self.config.executor.resume(execution, &upstream_id).await {
                Ok(result) => self.complete_execution(&id, result).await?,
                Err(message) => self.fail_execution(&id, message).await?,
            }
        } else {
            self.mark_lost(&id, "Started submission cannot be invoked again")
                .await?;
        }
        Ok(())
    }

    async fn complete_execution(&self, id: &str, result: ExecutionResult) -> Result<(), ApiError> {
        let mut ledger = self.ledger.lock().await;
        let now = Utc::now().to_rfc3339();
        let mut descriptors = Vec::new();
        for (index, output) in result.artifacts.into_iter().enumerate() {
            let artifact_id = format!(
                "vml-artifact-{}-{index}",
                &sha256_hex(format!("{id}:{index}").as_bytes())[..24]
            );
            let operation_snapshot = ledger.operations.get(id).unwrap().clone();
            let output = self
                .config
                .executor
                .finalize_artifact(
                    &operation_snapshot,
                    &artifact_id,
                    &sha256_hex(&output.bytes),
                    output.bytes.len() as u64,
                    output,
                )
                .await
                .map_err(ApiError::internal)?;
            let path = self
                .config
                .storage
                .path(&format!("artifacts/{artifact_id}"));
            fs::write(&path, &output.bytes).map_err(|e| ApiError::internal(e.to_string()))?;
            let operation = ledger.operations.get(id).unwrap();
            let source_artifacts = operation.input_artifacts.iter().map(|item| json!({"relationship":item.relationship,"coreArtifactId":item.core_artifact_id,"sha256":item.sha256})).collect();
            let mut model = output.model;
            model["catalogRevision"] = json!(operation
                .catalog_revision
                .clone()
                .ok_or_else(|| ApiError::internal("frozen catalog revision missing"))?);
            let descriptor = ProviderArtifact {
                schema_version: "1.0".into(),
                provider_artifact_id: artifact_id.clone(),
                provider_operation_id: id.into(),
                kind: output.kind,
                mime_type: output.mime_type,
                byte_size: output.bytes.len() as u64,
                sha256: sha256_hex(&output.bytes),
                media: output.media,
                model,
                controls: output.controls,
                recipe: output.recipe,
                source_artifacts,
                source_evidence: output.source_evidence,
                created_at: now.clone(),
            };
            ledger
                .artifact_paths
                .insert(artifact_id.clone(), path.to_string_lossy().to_string());
            ledger.artifacts.insert(artifact_id, descriptor.clone());
            descriptors.push(descriptor);
        }
        let op = ledger.operations.get_mut(id).unwrap();
        op.state = "completed".into();
        op.submission_state = "submitted_confirmed".into();
        op.artifacts = descriptors.clone();
        op.result = result.result;
        op.output = result.output;
        op.updated_at = now.clone();
        op.event_sequence += 1;
        let event = event(op, descriptors, None);
        ledger
            .events
            .entry(id.into())
            .or_default()
            .push(event.clone());
        queue_callback(&mut ledger, id, &event);
        self.persist_locked(&ledger).await?;
        drop(ledger);
        self.schedule_callback(event.event_id.clone()).await;
        Ok(())
    }

    async fn fail_execution(&self, id: &str, message: String) -> Result<(), ApiError> {
        let mut ledger = self.ledger.lock().await;
        let op = ledger.operations.get_mut(id).unwrap();
        op.state = "failed".into();
        op.terminal_error = Some(json!({"code":"UPSTREAM_REJECTED","message":message}));
        op.updated_at = Utc::now().to_rfc3339();
        op.event_sequence += 1;
        let event = event(op, vec![], op.terminal_error.clone());
        ledger
            .events
            .entry(id.into())
            .or_default()
            .push(event.clone());
        queue_callback(&mut ledger, id, &event);
        self.persist_locked(&ledger).await?;
        drop(ledger);
        self.schedule_callback(event.event_id.clone()).await;
        Ok(())
    }

    async fn mark_lost(&self, id: &str, message: &str) -> Result<(), ApiError> {
        let mut ledger = self.ledger.lock().await;
        let op = ledger
            .operations
            .get_mut(id)
            .ok_or_else(|| ApiError::internal("operation missing"))?;
        if terminal(&op.state) {
            return Ok(());
        }
        op.state = "lost".into();
        op.submission_certainty = "submitted_ambiguous".into();
        op.terminal_error = Some(
            json!({"code":"SUBMISSION_OUTCOME_UNKNOWN","message":message,"reconciliationRequired":true}),
        );
        op.updated_at = Utc::now().to_rfc3339();
        op.event_sequence += 1;
        let terminal_error = op.terminal_error.clone();
        let event = event(op, vec![], terminal_error);
        ledger
            .events
            .entry(id.into())
            .or_default()
            .push(event.clone());
        queue_callback(&mut ledger, id, &event);
        self.persist_locked(&ledger).await?;
        drop(ledger);
        self.schedule_callback(event.event_id.clone()).await;
        Ok(())
    }

    async fn schedule_callback(&self, event_id: String) {
        let mut workers = self.callback_workers.lock().await;
        if !workers.insert(event_id.clone()) {
            return;
        }
        drop(workers);
        let kernel = self.clone();
        tokio::spawn(async move {
            loop {
                let record = {
                    kernel
                        .ledger
                        .lock()
                        .await
                        .callback_outbox
                        .get(&event_id)
                        .cloned()
                };
                let Some(record) = record else { break };
                if record.acknowledged_at.is_some() || record.degraded {
                    break;
                }
                if let Ok(next) = DateTime::parse_from_rfc3339(&record.next_attempt_at) {
                    if let Ok(wait) = (next.with_timezone(&Utc) - Utc::now()).to_std() {
                        tokio::time::sleep(wait).await;
                    }
                }
                kernel.deliver_callback_once(&event_id).await;
            }
            kernel.callback_workers.lock().await.remove(&event_id);
        });
    }
    async fn deliver_callback_once(&self, event_id: &str) {
        let (op, event) = {
            let ledger = self.ledger.lock().await;
            let Some(record) = ledger.callback_outbox.get(event_id).cloned() else {
                return;
            };
            if record.acknowledged_at.is_some() || record.degraded {
                return;
            }
            let Some(op) = ledger.operations.get(&record.operation_id).cloned() else {
                return;
            };
            let Some(event) = ledger
                .events
                .get(&record.operation_id)
                .and_then(|events| events.iter().find(|event| event.event_id == event_id))
                .cloned()
            else {
                return;
            };
            (op, event)
        };
        if DateTime::parse_from_rfc3339(&op.callback_expires_at)
            .map_or(true, |expires| expires <= Utc::now())
        {
            let mut ledger = self.ledger.lock().await;
            if let Some(item) = ledger.callback_outbox.get_mut(event_id) {
                item.degraded = true;
                item.last_error = Some("CALLBACK_EXPIRED".into());
            }
            let _ = self.persist_locked(&ledger).await;
            return;
        }
        let Some(callback_secret) = op.callback_secret.as_ref() else {
            self.degrade_callback(event_id, "CALLBACK_DELIVERY_DEGRADED")
                .await;
            return;
        };
        let secret = match self.config.secret_protector.unprotect(callback_secret) {
            Ok(value) => value,
            Err(_) => {
                self.degrade_callback(event_id, "CALLBACK_DELIVERY_DEGRADED")
                    .await;
                return;
            }
        };
        let authorization = match String::from_utf8(secret) {
            Ok(value) => value,
            Err(_) => {
                self.degrade_callback(event_id, "CALLBACK_DELIVERY_DEGRADED")
                    .await;
                return;
            }
        };
        let client = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(value) => value,
            Err(_) => return,
        };
        let response = client
            .post(op.callback_url)
            .bearer_auth(authorization)
            .json(&event)
            .send()
            .await;
        let mut ledger = self.ledger.lock().await;
        let Some(item) = ledger.callback_outbox.get_mut(event_id) else {
            return;
        };
        item.attempts = item.attempts.saturating_add(1);
        match response {
            Ok(response) if response.status().is_success() => {
                item.last_status = Some(response.status().as_u16());
                item.acknowledged_at = Some(Utc::now().to_rfc3339());
                item.erase_after = Some(
                    (Utc::now() + Duration::milliseconds(self.config.terminal_replay_window_ms))
                        .to_rfc3339(),
                );
                item.ack_digest = Some(
                    canonical_digest(&serde_json::to_value(&event).unwrap_or(Value::Null))
                        .unwrap_or_default(),
                );
                item.last_error = None;
            }
            Ok(response) => {
                item.last_status = Some(response.status().as_u16());
                item.last_error = Some(format!("HTTP_{}", response.status().as_u16()));
            }
            Err(error) => item.last_error = Some(error.to_string()),
        }
        if item.acknowledged_at.is_none() {
            if item.attempts >= 8 {
                item.degraded = true;
            } else {
                let base = self
                    .config
                    .callback_retry_base_ms
                    .saturating_mul(2u64.saturating_pow(item.attempts.saturating_sub(1).min(8)));
                let jitter = u64::from_le_bytes(
                    Sha256::digest(event_id.as_bytes())[..8].try_into().unwrap(),
                ) % (base / 2 + 1);
                item.next_attempt_at =
                    (Utc::now() + Duration::milliseconds((base + jitter) as i64)).to_rfc3339();
            }
        }
        let erase_after = item.erase_after.clone();
        let _ = self.persist_locked(&ledger).await;
        drop(ledger);
        if erase_after.is_some() {
            let _ = self.maintenance_at(Utc::now()).await;
        }
        if let Some(erase_after) = erase_after {
            let kernel = self.clone();
            tokio::spawn(async move {
                if let Ok(at) = DateTime::parse_from_rfc3339(&erase_after) {
                    if let Ok(wait) = (at.with_timezone(&Utc) - Utc::now()).to_std() {
                        tokio::time::sleep(wait).await;
                    }
                }
                let _ = kernel.maintenance_at(Utc::now()).await;
            });
        }
    }
    async fn degrade_callback(&self, event_id: &str, code: &str) {
        let mut ledger = self.ledger.lock().await;
        if let Some(item) = ledger.callback_outbox.get_mut(event_id) {
            if item.degraded || item.acknowledged_at.is_some() {
                return;
            }
            item.attempts = item.attempts.saturating_add(1).min(8);
            item.next_attempt_at = Utc::now().to_rfc3339();
            item.degraded = true;
            item.last_error = Some(code.into());
        }
        let _ = self.persist_locked(&ledger).await;
    }
    async fn maintenance_at(&self, now: DateTime<Utc>) -> Result<(), ApiError> {
        let mut ledger = self.ledger.lock().await;
        let acknowledged = ledger
            .callback_outbox
            .values()
            .filter(|record| record.acknowledged_at.is_some())
            .map(|record| record.operation_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let erase = ledger
            .callback_outbox
            .values()
            .filter(|record| {
                record
                    .erase_after
                    .as_ref()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .is_some_and(|at| at <= now)
            })
            .map(|record| record.operation_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for id in &erase {
            if let Some(operation) = ledger.operations.get_mut(id) {
                operation.callback_secret = None;
            }
            for grant in ledger
                .grants
                .values_mut()
                .filter(|grant| &grant.provider_operation_id == id)
            {
                grant.encrypted_secret = None;
            }
        }
        let cleanup = ledger
            .uploads
            .iter()
            .filter_map(|(id, upload)| {
                let created = DateTime::parse_from_rfc3339(&upload.created_at)
                    .ok()?
                    .with_timezone(&Utc);
                let abandoned = now >= created + Duration::hours(24)
                    && upload.consumed_at.is_none()
                    && upload.state != "failed";
                let failed = upload.state == "failed" && now >= created + Duration::hours(72);
                let consumed = upload.consumed_at.is_some()
                    && acknowledged.contains(&upload.provider_operation_id);
                (abandoned || failed || consumed).then(|| id.clone())
            })
            .collect::<Vec<_>>();
        for id in cleanup {
            if let Some(upload) = ledger.uploads.remove(&id) {
                let _ = fs::remove_file(upload.path);
            }
        }
        self.persist_locked(&ledger).await
    }
}

fn queue_callback(ledger: &mut Ledger, operation_id: &str, event: &OperationEvent) {
    ledger
        .callback_outbox
        .entry(event.event_id.clone())
        .or_insert(CallbackRecord {
            event_id: event.event_id.clone(),
            operation_id: operation_id.into(),
            attempts: 0,
            next_attempt_at: Utc::now().to_rfc3339(),
            acknowledged_at: None,
            ack_digest: None,
            last_status: None,
            degraded: false,
            last_error: None,
            erase_after: None,
        });
}
fn remove_upload_locked(ledger: &mut Ledger, id: &str) -> bool {
    if let Some(upload) = ledger.uploads.remove(id) {
        let _ = fs::remove_file(upload.path);
        true
    } else {
        false
    }
}

fn terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "canceled" | "lost")
}
fn random_id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("{prefix}{}", hex(&bytes))
}
fn progress(phase: &str, message: &str) -> Progress {
    Progress {
        phase: phase.into(),
        completed: None,
        total: None,
        unit: None,
        percent: None,
        message: message.into(),
    }
}
fn usage(bytes: u64) -> Value {
    json!({"schemaVersion":"1.0","unit":"unknown","usage":{"upstreamRequestCount":1,"outputCount":if bytes>0 {1}else{0},"outputBytes":bytes,"providerDurationMs":null},"notes":["Provider does not expose exact billing."]})
}
fn event(
    op: &ProviderOperation,
    artifacts: Vec<ProviderArtifact>,
    error: Option<Value>,
) -> OperationEvent {
    OperationEvent {
        schema_version: "1.0".into(),
        event_id: format!("{}:{}", op.provider_operation_id, op.event_sequence),
        sequence: op.event_sequence,
        state: op.state.clone(),
        provider_operation_id: op.provider_operation_id.clone(),
        occurred_at: op.updated_at.clone(),
        progress: op.progress.clone(),
        resource_usage: op.resource_usage.clone(),
        result: op.result.clone(),
        output: op.output.clone(),
        artifacts,
        error,
    }
}

fn auth(kernel: &Kernel, headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");
    if value.len() != kernel.config.token.len()
        || !bool::from(value.as_bytes().ct_eq(kernel.config.token.as_bytes()))
    {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "AUTHENTICATION_REJECTED",
            "Bearer credential was rejected".into(),
        ));
    }
    Ok(format!("sha256:{}", sha256_hex(value.as_bytes())))
}
fn callback_origin_matches(configured: &str, callback: &str) -> bool {
    let Ok(expected) = reqwest::Url::parse(configured) else {
        return false;
    };
    let Ok(actual) = reqwest::Url::parse(callback) else {
        return false;
    };
    expected.username().is_empty()
        && expected.password().is_none()
        && actual.username().is_empty()
        && actual.password().is_none()
        && expected.scheme() == actual.scheme()
        && expected.host_str() == actual.host_str()
        && expected.port_or_known_default() == actual.port_or_known_default()
        && matches!(actual.scheme(), "http" | "https")
}
fn transfer_auth(headers: &HeaderMap) -> Result<(&str, &str), ApiError> {
    Ok((
        headers
            .get("x-transfer-grant-id")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                ApiError::forbidden(
                    "TRANSFER_GRANT_REQUIRED",
                    "Transfer grant identity is required",
                )
            })?,
        headers
            .get("x-transfer-grant")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                ApiError::forbidden("TRANSFER_GRANT_REQUIRED", "Transfer grant is required")
            })?,
    ))
}

async fn submit(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Json(request): Json<SubmitRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let client = auth(&kernel, &headers)?;
    if request.schema_version != "1.0"
        || request.envelope_type != "veniceMediaOperation.v1"
        || request.capability.revision != "2"
    {
        return Err(ApiError::bad(
            "INVALID_REQUEST",
            "Unsupported operation envelope",
        ));
    }
    if request.manifest_digest != kernel.config.manifest_digest
        || !valid_sha256(&request.input_digest)
        || input_digest(&request.input, &request.input_artifacts)
            .map_err(|e| ApiError::internal(e.to_string()))?
            != request.input_digest
    {
        return Err(ApiError::bad(
            "INVALID_REQUEST",
            "Operation digest does not match production wire input",
        ));
    }
    if !callback_origin_matches(
        &kernel.config.trusted_callback_origin,
        &request.callback.url,
    ) || request.callback.authorization.len() < 16
        || DateTime::parse_from_rfc3339(&request.callback.expires_at)
            .map_err(|_| ApiError::bad("INVALID_REQUEST", "Callback expiry is invalid"))?
            <= Utc::now()
    {
        return Err(ApiError::bad(
            "INVALID_REQUEST",
            "Callback binding is invalid",
        ));
    }
    kernel
        .config
        .executor
        .validate(&request.capability, &request.input)
        .await
        .map_err(|e| ApiError::bad("INVALID_REQUEST", e))?;
    let request_digest = request_digest(&request).map_err(|e| ApiError::internal(e.to_string()))?;
    let identity = format!("{client}:{}", request.idempotency_key);
    let mut ledger = kernel.ledger.lock().await;
    if let Some(id) = ledger.idempotency.get(&identity) {
        let op = ledger.operations.get(id).unwrap();
        if op.request_digest != request_digest {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "IDEMPOTENCY_DIGEST_CONFLICT",
                "Idempotency key conflicts".into(),
            ));
        }
        return Ok((
            StatusCode::OK,
            Json(
                json!({"schemaVersion":"1.0","accepted":true,"providerOperationId":op.provider_operation_id,"state":op.state,"requestId":op.request_id,"idempotencyKey":op.idempotency_key,"requestDigest":op.request_digest,"createdAt":op.created_at,"updatedAt":op.updated_at,"replayed":true}),
            ),
        ));
    }
    let id = random_id("vml-op-");
    let now = Utc::now().to_rfc3339();
    let callback_secret = kernel
        .config
        .secret_protector
        .protect(request.callback.authorization.as_bytes())
        .map_err(ApiError::internal)?;
    let op = ProviderOperation {
        schema_version: "1.0".into(),
        provider_operation_id: id.clone(),
        request_id: request.request_id,
        idempotency_key: request.idempotency_key,
        request_digest,
        client_id: client,
        core_operation_id: request.core_operation_id,
        attempt: request.attempt,
        assignment_revision: request.assignment_revision,
        capability: request.capability,
        manifest_digest: request.manifest_digest,
        catalog_revision: request.catalog_revision,
        input_digest: request.input_digest,
        input: request.input,
        input_artifacts: request.input_artifacts,
        state: "queued".into(),
        submission_state: "submission_not_started".into(),
        submission_certainty: "not_submitted".into(),
        provider_request_id: None,
        upstream_id: None,
        execution_requested: false,
        progress: progress("validating", "Operation durably admitted"),
        artifacts: vec![],
        resource_usage: usage(0),
        terminal_error: None,
        result: Value::Null,
        output: Value::Null,
        event_sequence: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        callback_url: request.callback.url,
        callback_secret: Some(callback_secret),
        callback_expires_at: request.callback.expires_at,
    };
    ledger.idempotency.insert(identity, id.clone());
    ledger
        .events
        .insert(id.clone(), vec![event(&op, vec![], None)]);
    ledger.operations.insert(id.clone(), op.clone());
    kernel.persist_locked(&ledger).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(
            json!({"schemaVersion":"1.0","accepted":true,"providerOperationId":id,"state":"queued","requestId":op.request_id,"idempotencyKey":op.idempotency_key,"requestDigest":op.request_digest,"createdAt":now,"updatedAt":op.updated_at,"replayed":false}),
        ),
    ))
}

async fn get_operation(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ProviderOperation>, ApiError> {
    let client = auth(&kernel, &headers)?;
    kernel
        .ledger
        .lock()
        .await
        .operations
        .get(&id)
        .filter(|o| o.client_id == client)
        .cloned()
        .map(Json)
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "OPERATION_NOT_FOUND",
            "Operation not found".into(),
        ))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventQuery {
    #[serde(default)]
    after_sequence: i64,
    limit: Option<usize>,
}
async fn get_events(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<EventQuery>,
) -> Result<Json<Value>, ApiError> {
    let client = auth(&kernel, &headers)?;
    let ledger = kernel.ledger.lock().await;
    if !ledger
        .operations
        .get(&id)
        .is_some_and(|o| o.client_id == client)
    {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "OPERATION_NOT_FOUND",
            "Operation not found".into(),
        ));
    }
    let events = ledger
        .events
        .get(&id)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.sequence as i64 > query.after_sequence)
        .take(query.limit.unwrap_or(64).min(500))
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"schemaVersion":"1.0","providerOperationId":id,"events":events}),
    ))
}

async fn register_grant(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(grant): Json<TransferAuthorization>,
) -> Result<impl IntoResponse, ApiError> {
    let client = auth(&kernel, &headers)?;
    let mut ledger = kernel.ledger.lock().await;
    let op = ledger
        .operations
        .get(&id)
        .filter(|o| o.client_id == client)
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "OPERATION_NOT_FOUND",
            "Operation not found".into(),
        ))?;
    let not_before = DateTime::parse_from_rfc3339(&grant.not_before).ok();
    let expires_at = DateTime::parse_from_rfc3339(&grant.expires_at).ok();
    let now = Utc::now();
    if grant.core_operation_id != op.core_operation_id
        || grant.attempt != op.attempt
        || grant.assignment_revision != op.assignment_revision
        || grant.capability_id != op.capability.id
        || grant.secret.len() < 16
        || grant.max_uses == 0
        || grant.max_uses > 16
        || not_before.is_none()
        || expires_at.is_none()
        || not_before.is_some_and(|value| value > now)
        || expires_at.is_some_and(|value| value <= now)
        || matches!((not_before, expires_at), (Some(start), Some(end)) if end <= start)
    {
        return Err(ApiError::forbidden(
            "TRANSFER_GRANT_REJECTED",
            "Transfer grant binding is invalid",
        ));
    }
    let digest = canonical_digest(
        &serde_json::to_value(&grant).map_err(|e| ApiError::internal(e.to_string()))?,
    )
    .map_err(|e| ApiError::internal(e.to_string()))?;
    if let Some(old) = ledger.grants.get(&grant.grant_id) {
        if old.binding_digest != digest {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "IDEMPOTENCY_DIGEST_CONFLICT",
                "Grant identity conflicts".into(),
            ));
        }
        return Ok((
            StatusCode::OK,
            Json(json!({"schemaVersion":"1.0","grantId":grant.grant_id,"replayed":true})),
        ));
    }
    let grant_id = grant.grant_id.clone();
    let encrypted_secret = kernel
        .config
        .secret_protector
        .protect(grant.secret.as_bytes())
        .map_err(ApiError::internal)?;
    let mut persisted_grant = grant;
    persisted_grant.secret.clear();
    ledger.grants.insert(
        grant_id.clone(),
        GrantRecord {
            encrypted_secret: Some(encrypted_secret),
            grant: persisted_grant,
            client_id: client,
            provider_operation_id: id,
            binding_digest: digest,
            uses: 0,
            last_request_digest: None,
        },
    );
    kernel.persist_locked(&ledger).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"schemaVersion":"1.0","grantId":grant_id,"replayed":false})),
    ))
}

fn verify_grant(
    kernel: &Kernel,
    ledger: &mut Ledger,
    client: &str,
    operation_id: &str,
    grant_id: &str,
    secret: &str,
    method: &str,
    path: &str,
    scope: &str,
    upload_id: Option<&str>,
    artifact_id: Option<&str>,
    sha: Option<&str>,
    size: Option<u64>,
    mime: Option<&str>,
    request_digest: &str,
) -> Result<bool, ApiError> {
    let record = ledger.grants.get_mut(grant_id).ok_or_else(|| {
        ApiError::forbidden("TRANSFER_GRANT_REJECTED", "Transfer grant was rejected")
    })?;
    let g = &record.grant;
    let exact = record.client_id == client
        && record.provider_operation_id == operation_id
        && g.method == method
        && g.path == path
        && g.scope == scope
        && g.upload_id.as_deref() == upload_id
        && g.artifact_id.as_deref() == artifact_id
        && g.expected_sha256.as_deref() == sha
        && g.expected_byte_size == size
        && g.expected_mime_type.as_deref() == mime;
    let now = Utc::now();
    let active = DateTime::parse_from_rfc3339(&g.not_before).is_ok_and(|value| value <= now)
        && DateTime::parse_from_rfc3339(&g.expires_at).is_ok_and(|value| value > now);
    let encrypted_secret = record.encrypted_secret.as_ref().ok_or_else(|| {
        ApiError::forbidden(
            "TRANSFER_GRANT_REPLAY_EXPIRED",
            "Transfer grant replay window expired",
        )
    })?;
    let decrypted = kernel
        .config
        .secret_protector
        .unprotect(encrypted_secret)
        .map_err(ApiError::internal)?;
    let valid_secret = decrypted.len() == secret.len()
        && bool::from(decrypted.as_slice().ct_eq(secret.as_bytes()));
    if !exact || !active || !valid_secret {
        return Err(ApiError::forbidden(
            "TRANSFER_GRANT_REJECTED",
            "Transfer grant binding was rejected",
        ));
    }
    if record.last_request_digest.as_deref() == Some(request_digest) {
        return Ok(true);
    }
    if record.uses >= g.max_uses {
        return Err(ApiError::forbidden(
            "TRANSFER_GRANT_CONSUMED",
            "Transfer grant was consumed",
        ));
    }
    record.uses += 1;
    record.last_request_digest = Some(request_digest.into());
    Ok(false)
}

async fn create_upload(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Json(request): Json<CreateUploadRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let client = auth(&kernel, &headers)?;
    let (gid, secret) = transfer_auth(&headers)?;
    let mut ledger = kernel.ledger.lock().await;
    let op = ledger
        .operations
        .get(&request.provider_operation_id)
        .filter(|o| o.client_id == client)
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "OPERATION_NOT_FOUND",
            "Operation not found".into(),
        ))?;
    if op.capability.id != request.capability_id
        || !op.input_artifacts.iter().any(|a| {
            a.upload_id == request.upload_id
                && a.sha256 == request.expected_sha256
                && a.byte_size == request.expected_byte_size
                && a.mime_type == request.expected_mime_type
                && a.relationship == request.relationship
                && a.core_artifact_id == request.core_artifact_id
        })
    {
        return Err(ApiError::forbidden(
            "TRANSFER_GRANT_SCOPE",
            "Upload was not declared",
        ));
    }
    let digest = canonical_digest(
        &serde_json::to_value(&request).map_err(|e| ApiError::internal(e.to_string()))?,
    )
    .map_err(|e| ApiError::internal(e.to_string()))?;
    verify_grant(
        &kernel,
        &mut ledger,
        &client,
        &request.provider_operation_id,
        gid,
        secret,
        "POST",
        UPLOADS_PATH,
        "upload:create",
        Some(&request.upload_id),
        None,
        Some(&request.expected_sha256),
        Some(request.expected_byte_size),
        Some(&request.expected_mime_type),
        &digest,
    )?;
    if let Some(upload) = ledger.uploads.get(&request.upload_id) {
        return Ok((
            StatusCode::OK,
            Json(
                json!({"schemaVersion":"1.0","uploadId":upload.upload_id,"byteLimit":upload.expected_byte_size,"replayed":true}),
            ),
        ));
    }
    let path = kernel
        .config
        .storage
        .path(&format!("uploads/{}.partial", request.upload_id));
    let record = UploadRecord {
        upload_id: request.upload_id.clone(),
        provider_operation_id: request.provider_operation_id,
        client_id: client,
        capability_id: request.capability_id,
        expected_mime_type: request.expected_mime_type,
        expected_byte_size: request.expected_byte_size,
        expected_sha256: request.expected_sha256,
        relationship: request.relationship,
        core_artifact_id: request.core_artifact_id,
        state: "created".into(),
        path: path.to_string_lossy().to_string(),
        created_at: Utc::now().to_rfc3339(),
        expires_at: (Utc::now() + Duration::hours(24)).to_rfc3339(),
        consumed_at: None,
    };
    ledger
        .uploads
        .insert(record.upload_id.clone(), record.clone());
    kernel.persist_locked(&ledger).await?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"schemaVersion":"1.0","uploadId":record.upload_id,"byteLimit":record.expected_byte_size,"expiresAt":(Utc::now()+Duration::hours(24)).to_rfc3339()}),
        ),
    ))
}

async fn write_upload(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let client = auth(&kernel, &headers)?;
    let (gid, secret) = transfer_auth(&headers)?;
    let mut ledger = kernel.ledger.lock().await;
    let upload = ledger
        .uploads
        .get(&id)
        .filter(|u| u.client_id == client)
        .cloned()
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "STAGING_EXPIRED",
            "Upload not found".into(),
        ))?;
    let path = format!("/api/v1/artifact-uploads/{id}/content");
    let digest = sha256_hex(
        format!(
            "PUT:{path}:{}:{}",
            upload.expected_sha256, upload.expected_byte_size
        )
        .as_bytes(),
    );
    let replay = verify_grant(
        &kernel,
        &mut ledger,
        &client,
        &upload.provider_operation_id,
        gid,
        secret,
        "PUT",
        &path,
        "upload:write",
        Some(&id),
        None,
        Some(&upload.expected_sha256),
        Some(upload.expected_byte_size),
        Some(&upload.expected_mime_type),
        &digest,
    )?;
    if !replay {
        if body.len() as u64 != upload.expected_byte_size
            || sha256_hex(&body) != upload.expected_sha256
        {
            ledger.uploads.get_mut(&id).unwrap().state = "failed".into();
            kernel.persist_locked(&ledger).await?;
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ARTIFACT_INTEGRITY_MISMATCH",
                "Upload integrity mismatch".into(),
            ));
        }
        fs::write(&upload.path, &body).map_err(|e| ApiError::internal(e.to_string()))?;
        ledger.uploads.get_mut(&id).unwrap().state = "written".into();
    }
    kernel.persist_locked(&ledger).await?;
    Ok(Json(
        json!({"schemaVersion":"1.0","uploadId":id,"byteSize":upload.expected_byte_size,"sha256":upload.expected_sha256,"replayed":replay}),
    ))
}

async fn seal_upload(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let client = auth(&kernel, &headers)?;
    let (gid, secret) = transfer_auth(&headers)?;
    let mut ledger = kernel.ledger.lock().await;
    let upload = ledger
        .uploads
        .get(&id)
        .filter(|u| u.client_id == client)
        .cloned()
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "STAGING_EXPIRED",
            "Upload not found".into(),
        ))?;
    let path = format!("/api/v1/artifact-uploads/{id}/complete");
    let digest = sha256_hex(format!("POST:{path}:{}", upload.expected_sha256).as_bytes());
    let replay = verify_grant(
        &kernel,
        &mut ledger,
        &client,
        &upload.provider_operation_id,
        gid,
        secret,
        "POST",
        &path,
        "upload:write",
        Some(&id),
        None,
        Some(&upload.expected_sha256),
        Some(upload.expected_byte_size),
        Some(&upload.expected_mime_type),
        &digest,
    )?;
    if upload.state != "written" && upload.state != "sealed" {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "INVALID_REQUEST",
            "Upload content is incomplete".into(),
        ));
    }
    ledger.uploads.get_mut(&id).unwrap().state = "sealed".into();
    kernel.persist_locked(&ledger).await?;
    Ok(Json(
        json!({"schemaVersion":"1.0","uploadId":id,"state":"sealed","sealed":true,"byteSize":upload.expected_byte_size,"sha256":upload.expected_sha256,"mimeType":upload.expected_mime_type,"replayed":replay||upload.state=="sealed"}),
    ))
}

async fn execute(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let client = auth(&kernel, &headers)?;
    let mut ledger = kernel.ledger.lock().await;
    let op = ledger
        .operations
        .get(&id)
        .filter(|o| o.client_id == client)
        .cloned()
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "OPERATION_NOT_FOUND",
            "Operation not found".into(),
        ))?;
    if !op.input_artifacts.iter().all(|a| {
        ledger
            .uploads
            .get(&a.upload_id)
            .is_some_and(|u| u.state == "sealed")
    }) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "INPUT_NOT_SEALED",
            "All declared inputs must be sealed".into(),
        ));
    }
    let replay = op.execution_requested;
    ledger.operations.get_mut(&id).unwrap().execution_requested = true;
    kernel.persist_locked(&ledger).await?;
    drop(ledger);
    if !replay {
        kernel.spawn_execution(id.clone());
    }
    Ok((
        if replay {
            StatusCode::OK
        } else {
            StatusCode::ACCEPTED
        },
        Json(
            json!({"schemaVersion":"1.0","providerOperationId":id,"state":op.state,"replayed":replay}),
        ),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelRequest {
    idempotency_key: String,
}
async fn cancel(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<CancelRequest>,
) -> Result<Json<Value>, ApiError> {
    let client = auth(&kernel, &headers)?;
    let mut ledger = kernel.ledger.lock().await;
    let operation = ledger
        .operations
        .get(&id)
        .filter(|operation| operation.client_id == client)
        .cloned()
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "OPERATION_NOT_FOUND",
            "Operation not found".into(),
        ))?;
    let record_key = format!("{id}:{}", request.idempotency_key);
    let request_digest = sha256_hex(format!("cancel:{id}:{}", request.idempotency_key).as_bytes());
    if let Some(record) = ledger.cancels.get(&record_key) {
        if record.request_digest != request_digest {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "IDEMPOTENCY_DIGEST_CONFLICT",
                "Cancel idempotency conflicts".into(),
            ));
        }
        return Ok(Json(
            json!({"schemaVersion":"1.0","providerOperationId":id,"outcome":record.outcome,"replayed":true}),
        ));
    }
    let mut event_id = None;
    let outcome = if terminal(&operation.state) {
        "too_late"
    } else if operation.submission_state == "submission_not_started" {
        let current = ledger.operations.get_mut(&id).unwrap();
        current.state = "canceled".into();
        current.updated_at = Utc::now().to_rfc3339();
        current.event_sequence += 1;
        let event = event(
            current,
            vec![],
            Some(json!({"code":"CANCELED","message":"Canceled before upstream submission"})),
        );
        event_id = Some(event.event_id.clone());
        ledger
            .events
            .entry(id.clone())
            .or_default()
            .push(event.clone());
        queue_callback(&mut ledger, &id, &event);
        "canceled"
    } else {
        "unsupported"
    };
    ledger.cancels.insert(
        record_key,
        CancelRecord {
            idempotency_key: request.idempotency_key,
            request_digest,
            outcome: outcome.into(),
            event_id: event_id.clone(),
        },
    );
    kernel.persist_locked(&ledger).await?;
    if let Some(event_id) = event_id.clone() {
        drop(ledger);
        kernel.schedule_callback(event_id).await;
    } else {
        drop(ledger);
    }
    Ok(Json(
        json!({"schemaVersion":"1.0","providerOperationId":id,"outcome":outcome,"replayed":false}),
    ))
}

async fn delete_upload(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let client = auth(&kernel, &headers)?;
    let (gid, secret) = transfer_auth(&headers)?;
    let mut ledger = kernel.ledger.lock().await;
    let upload = ledger
        .uploads
        .get(&id)
        .filter(|upload| upload.client_id == client)
        .cloned()
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "STAGING_EXPIRED",
            "Upload not found".into(),
        ))?;
    let path = format!("/api/v1/artifact-uploads/{id}");
    let digest = sha256_hex(format!("DELETE:{path}").as_bytes());
    verify_grant(
        &kernel,
        &mut ledger,
        &client,
        &upload.provider_operation_id,
        gid,
        secret,
        "DELETE",
        &path,
        "upload:write",
        Some(&id),
        None,
        Some(&upload.expected_sha256),
        Some(upload.expected_byte_size),
        Some(&upload.expected_mime_type),
        &digest,
    )?;
    remove_upload_locked(&mut ledger, &id);
    kernel.persist_locked(&ledger).await?;
    Ok(Json(
        json!({"schemaVersion":"1.0","uploadId":id,"deleted":true}),
    ))
}

async fn get_artifact(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ProviderArtifact>, ApiError> {
    let client = auth(&kernel, &headers)?;
    let (gid, secret) = transfer_auth(&headers)?;
    let mut ledger = kernel.ledger.lock().await;
    let artifact = ledger.artifacts.get(&id).cloned().ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "ARTIFACT_NOT_FOUND",
        "Artifact not found".into(),
    ))?;
    let path = format!("/api/v1/artifacts/{id}");
    let digest = sha256_hex(format!("GET:{path}:descriptor").as_bytes());
    verify_grant(
        &kernel,
        &mut ledger,
        &client,
        &artifact.provider_operation_id,
        gid,
        secret,
        "GET",
        &path,
        "artifact:read",
        None,
        Some(&id),
        Some(&artifact.sha256),
        Some(artifact.byte_size),
        Some(&artifact.mime_type),
        &digest,
    )?;
    kernel.persist_locked(&ledger).await?;
    Ok(Json(artifact))
}
async fn read_artifact(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let client = auth(&kernel, &headers)?;
    let (gid, secret) = transfer_auth(&headers)?;
    let mut ledger = kernel.ledger.lock().await;
    let artifact = ledger.artifacts.get(&id).cloned().ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "ARTIFACT_NOT_FOUND",
        "Artifact not found".into(),
    ))?;
    let path = format!("/api/v1/artifacts/{id}/content");
    let digest = sha256_hex(format!("GET:{path}").as_bytes());
    verify_grant(
        &kernel,
        &mut ledger,
        &client,
        &artifact.provider_operation_id,
        gid,
        secret,
        "GET",
        &path,
        "artifact:read",
        None,
        Some(&id),
        Some(&artifact.sha256),
        Some(artifact.byte_size),
        Some(&artifact.mime_type),
        &digest,
    )?;
    kernel.persist_locked(&ledger).await?;
    let file = ledger
        .artifact_paths
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError::internal("Artifact path missing"))?;
    drop(ledger);
    let bytes = tokio::fs::read(file)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok((
        [
            (header::CONTENT_TYPE, artifact.mime_type),
            ("x-content-sha256".parse().unwrap(), artifact.sha256),
        ],
        bytes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Noop;
    #[async_trait]
    impl Executor for Noop {
        async fn submit(
            &self,
            _: ExecutionInput,
            request_id: &str,
        ) -> Result<SubmissionReceipt, String> {
            Ok(SubmissionReceipt {
                upstream_id: request_id.into(),
                certainty: "submitted_confirmed".into(),
            })
        }
        async fn resume(&self, _: ExecutionInput, _: &str) -> Result<ExecutionResult, String> {
            Ok(ExecutionResult {
                artifacts: vec![],
                result: Value::Null,
                output: Value::Null,
            })
        }
    }
    struct TestProtector;
    impl SecretProtector for TestProtector {
        fn protect(&self, plaintext: &[u8]) -> Result<EncryptedSecret, String> {
            Ok(EncryptedSecret {
                key_id: "test".into(),
                ciphertext: hex(plaintext),
            })
        }
        fn unprotect(&self, encrypted: &EncryptedSecret) -> Result<Vec<u8>, String> {
            (0..encrypted.ciphertext.len())
                .step_by(2)
                .map(|index| {
                    u8::from_str_radix(&encrypted.ciphertext[index..index + 2], 16)
                        .map_err(|error| error.to_string())
                })
                .collect()
        }
    }
    #[test]
    fn digest_is_canonical_and_shared() {
        let a = json!({"z":1,"a":{"y":2,"x":3}});
        let b = json!({"a":{"x":3,"y":2},"z":1});
        assert_eq!(canonical_digest(&a).unwrap(), canonical_digest(&b).unwrap());
    }
    #[test]
    fn callback_origin_requires_exact_parsed_origin() {
        assert!(callback_origin_matches(
            "https://core.example:443",
            "https://core.example/callback"
        ));
        assert!(!callback_origin_matches(
            "https://core.example",
            "https://core.example.evil/callback"
        ));
        assert!(!callback_origin_matches(
            "https://core.example",
            "https://user@core.example/callback"
        ));
        assert!(!callback_origin_matches(
            "https://core.example",
            "http://core.example/callback"
        ));
        assert!(!callback_origin_matches(
            "https://core.example:444",
            "https://core.example/callback"
        ));
    }
    #[test]
    fn file_storage_recovers_interrupted_backup_and_uses_unique_synced_temp() {
        let root = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(root.path().into());
        storage.write_atomic("ledger.json", b"one").unwrap();
        assert_eq!(storage.read("ledger.json").unwrap().unwrap(), b"one");
        fs::rename(
            root.path().join("ledger.json"),
            root.path().join("ledger.bak"),
        )
        .unwrap();
        storage.recover_atomic("ledger.json").unwrap();
        assert_eq!(storage.read("ledger.json").unwrap().unwrap(), b"one");
        storage.write_atomic("ledger.json", b"two").unwrap();
        assert_eq!(storage.read("ledger.json").unwrap().unwrap(), b"two");
    }
    #[tokio::test]
    async fn ledger_reopens_without_tauri() {
        let root = tempfile::tempdir().unwrap();
        let config = KernelConfig {
            storage: Arc::new(FileStorage::new(root.path().into())),
            token: "0123456789abcdef".into(),
            manifest_digest: "a".repeat(64),
            trusted_callback_origin: "http://127.0.0.1".into(),
            executor: Arc::new(Noop),
            secret_protector: Arc::new(TestProtector),
            callback_retry_base_ms: 10,
            terminal_replay_window_ms: 50,
            maintenance_interval_ms: 10,
        };
        let kernel = Kernel::open(config.clone()).await.unwrap();
        let ledger = kernel.ledger.lock().await;
        kernel.persist_locked(&ledger).await.unwrap();
        drop(ledger);
        Kernel::open(config).await.unwrap();
    }

    struct CountingExecutor(AtomicUsize);
    #[async_trait]
    impl Executor for CountingExecutor {
        async fn submit(&self, _: ExecutionInput, _: &str) -> Result<SubmissionReceipt, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err("must not run".into())
        }
        async fn resume(&self, _: ExecutionInput, _: &str) -> Result<ExecutionResult, String> {
            Err("must not run".into())
        }
    }

    #[tokio::test]
    async fn ambiguous_started_submission_becomes_lost_without_resubmission() {
        let root = tempfile::tempdir().unwrap();
        let executor = Arc::new(CountingExecutor(AtomicUsize::new(0)));
        let config = KernelConfig {
            storage: Arc::new(FileStorage::new(root.path().into())),
            token: "0123456789abcdef".into(),
            manifest_digest: "a".repeat(64),
            trusted_callback_origin: "http://127.0.0.1".into(),
            executor: executor.clone(),
            secret_protector: Arc::new(TestProtector),
            callback_retry_base_ms: 10,
            terminal_replay_window_ms: 50,
            maintenance_interval_ms: 10,
        };
        let kernel = Kernel::open(config.clone()).await.unwrap();
        let now = Utc::now().to_rfc3339();
        let operation = ProviderOperation {
            schema_version: "1.0".into(),
            provider_operation_id: "ambiguous-op".into(),
            request_id: "request".into(),
            idempotency_key: "key".into(),
            request_digest: "b".repeat(64),
            client_id: "client".into(),
            core_operation_id: "core".into(),
            attempt: 1,
            assignment_revision: 1,
            capability: CapabilityRef {
                id: "media.image.generate".into(),
                revision: "2".into(),
            },
            manifest_digest: "a".repeat(64),
            catalog_revision: Some("catalog".into()),
            input_digest: "c".repeat(64),
            input: json!({}),
            input_artifacts: vec![],
            state: "running".into(),
            submission_state: "submission_started".into(),
            submission_certainty: "not_submitted".into(),
            provider_request_id: Some("request-id".into()),
            upstream_id: None,
            execution_requested: true,
            progress: progress("submitting", "ambiguous"),
            artifacts: vec![],
            resource_usage: usage(0),
            terminal_error: None,
            result: Value::Null,
            output: Value::Null,
            event_sequence: 0,
            created_at: now.clone(),
            updated_at: now.clone(),
            callback_url: "http://127.0.0.1:1/callback".into(),
            callback_secret: Some(config.secret_protector.protect(b"callback-secret").unwrap()),
            callback_expires_at: now,
        };
        let mut ledger = kernel.ledger.lock().await;
        ledger
            .operations
            .insert(operation.provider_operation_id.clone(), operation);
        kernel.persist_locked(&ledger).await.unwrap();
        drop(ledger);
        drop(kernel);
        let reopened = Kernel::open(config).await.unwrap();
        assert_eq!(
            reopened.ledger.lock().await.operations["ambiguous-op"].state,
            "lost"
        );
        assert_eq!(executor.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn time_controlled_staging_cleanup_keeps_canonical_outputs() {
        let root = tempfile::tempdir().unwrap();
        let config = KernelConfig {
            storage: Arc::new(FileStorage::new(root.path().into())),
            token: "0123456789abcdef".into(),
            manifest_digest: "a".repeat(64),
            trusted_callback_origin: "http://127.0.0.1".into(),
            executor: Arc::new(Noop),
            secret_protector: Arc::new(TestProtector),
            callback_retry_base_ms: 10,
            terminal_replay_window_ms: 50,
            maintenance_interval_ms: 10,
        };
        let kernel = Kernel::open(config).await.unwrap();
        let now = Utc::now();
        let old = (now - Duration::hours(25)).to_rfc3339();
        let failed_old = (now - Duration::hours(73)).to_rfc3339();
        let abandoned = root.path().join("uploads/abandoned.partial");
        let failed = root.path().join("uploads/failed.partial");
        let explicit = root.path().join("uploads/explicit.partial");
        let output = root.path().join("artifacts/canonical");
        fs::write(&abandoned, b"x").unwrap();
        fs::write(&failed, b"x").unwrap();
        fs::write(&explicit, b"x").unwrap();
        fs::write(&output, b"output").unwrap();
        let record =
            |id: &str, state: &str, created: String, path: &std::path::Path| UploadRecord {
                upload_id: id.into(),
                provider_operation_id: "op".into(),
                client_id: "client".into(),
                capability_id: "media.image.edit".into(),
                expected_mime_type: "image/png".into(),
                expected_byte_size: 1,
                expected_sha256: "a".repeat(64),
                relationship: "source".into(),
                core_artifact_id: None,
                state: state.into(),
                path: path.to_string_lossy().into(),
                created_at: created.clone(),
                expires_at: (DateTime::parse_from_rfc3339(&created).unwrap() + Duration::hours(24))
                    .to_rfc3339(),
                consumed_at: None,
            };
        let mut ledger = kernel.ledger.lock().await;
        ledger.uploads.insert(
            "abandoned".into(),
            record("abandoned", "created", old, &abandoned),
        );
        ledger.uploads.insert(
            "explicit".into(),
            record("explicit", "created", now.to_rfc3339(), &explicit),
        );
        assert!(remove_upload_locked(&mut ledger, "explicit"));
        assert!(!explicit.exists());
        assert!(!remove_upload_locked(&mut ledger, "explicit"));
        ledger.uploads.insert(
            "failed".into(),
            record("failed", "failed", failed_old, &failed),
        );
        kernel.persist_locked(&ledger).await.unwrap();
        drop(ledger);
        kernel.start_maintenance();
        assert_eq!(kernel.maintenance_starts.load(Ordering::SeqCst), 1);
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_millis(11)).await;
        tokio::task::yield_now().await;
        assert!(!abandoned.exists());
        assert!(!failed.exists());
        assert!(output.exists());
        assert!(kernel.ledger.lock().await.uploads.is_empty());
        kernel.shutdown().await;
        assert!(!kernel.maintenance_running.load(Ordering::SeqCst));
    }
    #[tokio::test]
    async fn corrupt_callback_secret_degrades_once_and_preserves_poll_event() {
        let root = tempfile::tempdir().unwrap();
        let config = KernelConfig {
            storage: Arc::new(FileStorage::new(root.path().into())),
            token: "0123456789abcdef".into(),
            manifest_digest: "a".repeat(64),
            trusted_callback_origin: "http://127.0.0.1:1".into(),
            executor: Arc::new(Noop),
            secret_protector: Arc::new(TestProtector),
            callback_retry_base_ms: 5,
            terminal_replay_window_ms: 50,
            maintenance_interval_ms: 1000,
        };
        let kernel = Kernel::open(config).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer 0123456789abcdef".parse().unwrap(),
        );
        let input = json!({});
        let request = SubmitRequest {
            schema_version: "1.0".into(),
            envelope_type: "veniceMediaOperation.v1".into(),
            request_id: "corrupt-request".into(),
            idempotency_key: "corrupt-key".into(),
            core_operation_id: "corrupt-core".into(),
            attempt: 1,
            assignment_revision: 1,
            capability: CapabilityRef {
                id: "media.models.list".into(),
                revision: "2".into(),
            },
            manifest_digest: "a".repeat(64),
            catalog_revision: None,
            input_digest: input_digest(&input, &[]).unwrap(),
            input,
            input_artifacts: vec![],
            callback: CallbackRequest {
                url: "http://127.0.0.1:1/callback".into(),
                authorization: "callback-secret-000000".into(),
                expires_at: (Utc::now() + Duration::hours(1)).to_rfc3339(),
            },
            requested_at: Utc::now().to_rfc3339(),
        };
        submit(State(kernel.clone()), headers.clone(), Json(request))
            .await
            .unwrap();
        let id = kernel
            .ledger
            .lock()
            .await
            .operations
            .keys()
            .next()
            .unwrap()
            .clone();
        {
            let mut ledger = kernel.ledger.lock().await;
            ledger.operations.get_mut(&id).unwrap().callback_secret = Some(EncryptedSecret {
                key_id: "test".into(),
                ciphertext: "not-hex".into(),
            });
            kernel.persist_locked(&ledger).await.unwrap();
        }
        let _ = cancel(
            State(kernel.clone()),
            headers,
            Path(id.clone()),
            Json(CancelRequest {
                idempotency_key: "cancel".into(),
            }),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let ledger = kernel.ledger.lock().await;
        let record = ledger.callback_outbox.values().next().unwrap();
        assert!(record.degraded);
        assert_eq!(record.attempts, 1);
        assert_eq!(
            record.last_error.as_deref(),
            Some("CALLBACK_DELIVERY_DEGRADED")
        );
        assert_eq!(
            ledger.events[&id]
                .iter()
                .filter(|event| event.state == "canceled")
                .count(),
            1
        );
        drop(ledger);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(
            kernel
                .ledger
                .lock()
                .await
                .callback_outbox
                .values()
                .next()
                .unwrap()
                .attempts,
            1
        );
        kernel.shutdown().await;
    }
}
