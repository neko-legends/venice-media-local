use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{rejection::JsonRejection, Path, Query, State},
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
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
};
use subtle::ConstantTimeEq;
use tokio::{
    sync::{mpsc, watch, Mutex},
    task::JoinHandle,
};

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
pub const SHUTDOWN_PATH: &str = "/api/v1/actions/shutdown";
pub const SHUTDOWN_SCOPE: &str = "application:shutdown";

#[derive(Clone, Default)]
pub struct TerminalShutdownLatch(Arc<AtomicBool>);

impl TerminalShutdownLatch {
    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn clear(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub fn ensure_open(&self) -> Result<(), &'static str> {
        if self.is_set() {
            Err("APPLICATION_SHUTTING_DOWN")
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Default)]
pub struct SettingsTransaction(Arc<tokio::sync::Mutex<()>>);

impl SettingsTransaction {
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.0.lock().await
    }

    pub fn try_lock(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, &'static str> {
        self.0.try_lock().map_err(|_| "SETTINGS_TRANSACTION_BUSY")
    }
}

pub fn read_optional_json_file<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    unreadable_code: &'static str,
    corrupt_code: &'static str,
) -> Result<Option<T>, &'static str> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(unreadable_code),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| corrupt_code)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentControlPhase {
    Stopped,
    Starting,
    Running,
    Stopping,
}

struct AgentControlOwnershipState<T> {
    phase: AgentControlPhase,
    generation: u64,
    port: Option<u16>,
    owner: Option<T>,
}

pub struct AgentControlOwnership<T>(StdMutex<AgentControlOwnershipState<T>>);

impl<T> Default for AgentControlOwnership<T> {
    fn default() -> Self {
        Self(StdMutex::new(AgentControlOwnershipState {
            phase: AgentControlPhase::Stopped,
            generation: 0,
            port: None,
            owner: None,
        }))
    }
}

impl<T> AgentControlOwnership<T> {
    pub fn reserve_start(&self, port: u16, owner: T) -> Result<u64, &'static str> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "AGENT_CONTROL_OWNERSHIP_UNAVAILABLE")?;
        if state.phase != AgentControlPhase::Stopped {
            return Err("AGENT_CONTROL_ALREADY_OWNED");
        }
        state.generation = state.generation.saturating_add(1);
        state.phase = AgentControlPhase::Starting;
        state.port = Some(port);
        state.owner = Some(owner);
        Ok(state.generation)
    }

    pub fn publish_running(&self, generation: u64) -> Result<(), &'static str> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "AGENT_CONTROL_OWNERSHIP_UNAVAILABLE")?;
        if state.phase != AgentControlPhase::Starting || state.generation != generation {
            return Err("AGENT_CONTROL_START_STALE");
        }
        state.phase = AgentControlPhase::Running;
        Ok(())
    }

    pub fn fail_start(&self, generation: u64) -> Option<T> {
        let mut state = self.0.lock().ok()?;
        if !matches!(
            state.phase,
            AgentControlPhase::Starting | AgentControlPhase::Stopping
        ) || state.generation != generation
        {
            return None;
        }
        state.phase = AgentControlPhase::Stopped;
        state.port = None;
        state.owner.take()
    }

    pub fn begin_stop(&self) -> Result<Option<(u64, T)>, &'static str> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "AGENT_CONTROL_OWNERSHIP_UNAVAILABLE")?;
        match state.phase {
            AgentControlPhase::Stopped => Ok(None),
            AgentControlPhase::Starting | AgentControlPhase::Running => {
                state.phase = AgentControlPhase::Stopping;
                Ok(state.owner.take().map(|owner| (state.generation, owner)))
            }
            AgentControlPhase::Stopping => Err("AGENT_CONTROL_STOP_IN_PROGRESS"),
        }
    }

    pub fn finish_stop(&self, generation: u64) -> bool {
        let Ok(mut state) = self.0.lock() else {
            return false;
        };
        if state.phase != AgentControlPhase::Stopping || state.generation != generation {
            return false;
        }
        state.phase = AgentControlPhase::Stopped;
        state.port = None;
        state.owner = None;
        true
    }

    pub fn is_running(&self, generation: u64) -> bool {
        self.0.lock().is_ok_and(|state| {
            state.phase == AgentControlPhase::Running && state.generation == generation
        })
    }

    pub fn snapshot(&self) -> (AgentControlPhase, u64, Option<u16>) {
        self.0
            .lock()
            .map(|state| (state.phase, state.generation, state.port))
            .unwrap_or((AgentControlPhase::Stopping, 0, None))
    }

    pub fn may_persist_stopped_generation(&self, generation: u64) -> bool {
        self.0.lock().is_ok_and(|state| {
            state.phase == AgentControlPhase::Stopped && state.generation == generation
        })
    }
}

pub struct LatchedAgentControlOwnership<T> {
    pub ownership: AgentControlOwnership<T>,
    pub terminal: TerminalShutdownLatch,
}

impl<T> Default for LatchedAgentControlOwnership<T> {
    fn default() -> Self {
        Self {
            ownership: Default::default(),
            terminal: Default::default(),
        }
    }
}

impl<T> LatchedAgentControlOwnership<T> {
    pub fn reserve_start(&self, port: u16, owner: T) -> Result<u64, &'static str> {
        self.terminal.ensure_open()?;
        self.ownership.reserve_start(port, owner)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatedLifecycleOutcome {
    pub outcome: &'static str,
    pub failure_code: Option<&'static str>,
}

#[derive(Default)]
struct LifecycleSupervisorState {
    generation: usize,
    worker: Option<(tokio::sync::oneshot::Sender<()>, JoinHandle<()>)>,
    unregister_outcome: Option<CoordinatedLifecycleOutcome>,
}

#[derive(Default)]
pub struct LifecycleSupervisor {
    generation: AtomicUsize,
    state: tokio::sync::Mutex<LifecycleSupervisorState>,
}

impl LifecycleSupervisor {
    pub fn set_generation(&self, generation: usize) {
        self.generation.store(generation, Ordering::SeqCst);
    }

    pub fn is_current(&self, generation: usize) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    pub async fn start<F>(&self, generation: usize, spawn: F) -> Result<(), &'static str>
    where
        F: FnOnce(tokio::sync::oneshot::Receiver<()>) -> JoinHandle<()>,
    {
        if !self.is_current(generation) {
            return Err("LIFECYCLE_START_STALE");
        }
        let mut state = self.state.lock().await;
        if state.generation > generation || !self.is_current(generation) {
            return Err("LIFECYCLE_START_STALE");
        }
        stop_lifecycle_worker(state.worker.take()).await?;
        if !self.is_current(generation) {
            return Err("LIFECYCLE_START_STALE");
        }
        let (shutdown, receiver) = tokio::sync::oneshot::channel();
        state.worker = Some((shutdown, spawn(receiver)));
        state.generation = generation;
        state.unregister_outcome = None;
        Ok(())
    }

    pub async fn unregister<F, Fut>(
        &self,
        expected_generation: usize,
        operation: F,
    ) -> CoordinatedLifecycleOutcome
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = CoordinatedLifecycleOutcome>,
    {
        let mut state = self.state.lock().await;
        if state.generation != expected_generation {
            return CoordinatedLifecycleOutcome {
                outcome: "stale_no_op",
                failure_code: None,
            };
        }
        if let Some(outcome) = state.unregister_outcome.clone() {
            return outcome;
        }
        let outcome = match stop_lifecycle_worker(state.worker.take()).await {
            Ok(()) => operation().await,
            Err(code) => CoordinatedLifecycleOutcome {
                outcome: "failed",
                failure_code: Some(code),
            },
        };
        state.unregister_outcome = Some(outcome.clone());
        outcome
    }

    pub async fn stop(&self, expected_generation: usize) -> Result<(), &'static str> {
        let mut state = self.state.lock().await;
        if state.generation != expected_generation {
            return Err("LIFECYCLE_STOP_STALE");
        }
        stop_lifecycle_worker(state.worker.take()).await
    }

    pub async fn has_worker(&self, expected_generation: usize) -> bool {
        let state = self.state.lock().await;
        state.generation == expected_generation && state.worker.is_some()
    }
}

async fn stop_lifecycle_worker(
    previous: Option<(tokio::sync::oneshot::Sender<()>, JoinHandle<()>)>,
) -> Result<(), &'static str> {
    if let Some((shutdown, mut task)) = previous {
        let _ = shutdown.send(());
        if tokio::time::timeout(std::time::Duration::from_secs(12), &mut task)
            .await
            .is_err()
        {
            task.abort();
            tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .map_err(|_| "LIFECYCLE_WORKER_ABORT_TIMEOUT")?
                .map_err(|error| {
                    if error.is_cancelled() {
                        "LIFECYCLE_WORKER_STOP_TIMEOUT"
                    } else {
                        "LIFECYCLE_WORKER_JOIN_FAILED"
                    }
                })?;
            return Err("LIFECYCLE_WORKER_STOP_TIMEOUT");
        }
    }
    Ok(())
}

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
pub struct ShutdownRequest {
    pub schema_version: String,
    #[serde(rename = "type")]
    pub envelope_type: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub scope: String,
    pub provider_id: String,
    pub instance_id: String,
    pub manifest_digest: String,
    pub requested_at: String,
    pub expires_at: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShutdownReceipt {
    pub schema_version: String,
    pub accepted: bool,
    pub action: String,
    pub scope: String,
    pub provider_id: String,
    pub instance_id: String,
    pub manifest_digest: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub request_digest: String,
    pub accepted_at: String,
    pub state: String,
    pub replayed: bool,
    #[serde(skip)]
    pub ownership_generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShutdownStageRecord {
    pub stage: String,
    pub outcome: String,
    pub recorded_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShutdownAuditRecord {
    pub action: String,
    pub scope: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub request_digest: String,
    pub client_credential_fingerprint: String,
    pub provider_id: String,
    pub instance_id: String,
    pub manifest_digest: String,
    pub requested_at: String,
    pub expires_at: String,
    pub decided_at: String,
    pub decision: String,
    pub reason_code: String,
    pub active_operation_count: usize,
    pub ambiguous_operation_count: usize,
    #[serde(default)]
    pub stages: Vec<ShutdownStageRecord>,
}

#[derive(Debug, Default)]
struct AdmissionGate {
    accepting: bool,
    compatibility_in_flight: usize,
    accepted: Option<ShutdownReceipt>,
}

#[derive(Default)]
struct BackgroundTasks {
    accepting: bool,
    handles: Vec<JoinHandle<()>>,
}

#[derive(Clone, Debug)]
pub struct AdmissionController(Arc<StdMutex<AdmissionGate>>);

impl Default for AdmissionController {
    fn default() -> Self {
        Self(Arc::new(StdMutex::new(AdmissionGate {
            accepting: true,
            ..Default::default()
        })))
    }
}

pub struct CompatibilityPermit {
    controller: AdmissionController,
}

impl Drop for CompatibilityPermit {
    fn drop(&mut self) {
        if let Ok(mut gate) = self.controller.0.lock() {
            gate.compatibility_in_flight = gate.compatibility_in_flight.saturating_sub(1);
        }
    }
}

impl AdmissionController {
    pub fn claim_compatibility(&self) -> Result<CompatibilityPermit, &'static str> {
        let mut gate = self.0.lock().map_err(|_| "ADMISSION_GATE_UNAVAILABLE")?;
        if !gate.accepting {
            return Err("APPLICATION_SHUTTING_DOWN");
        }
        gate.compatibility_in_flight += 1;
        Ok(CompatibilityPermit {
            controller: self.clone(),
        })
    }
    fn compatibility_in_flight(&self) -> Result<usize, ApiError> {
        self.0
            .lock()
            .map(|gate| gate.compatibility_in_flight)
            .map_err(|_| ApiError::internal("admission gate unavailable"))
    }
    pub fn active_work_count(&self) -> usize {
        self.0
            .lock()
            .map(|gate| gate.compatibility_in_flight)
            .unwrap_or(usize::MAX)
    }
    fn ensure_accepting(&self) -> Result<(), ApiError> {
        if self
            .0
            .lock()
            .map_err(|_| ApiError::internal("admission gate unavailable"))?
            .accepting
        {
            Ok(())
        } else {
            Err(ApiError::unavailable(
                "APPLICATION_SHUTTING_DOWN",
                "Application shutdown has already been accepted",
            ))
        }
    }
    fn close(&self, receipt: ShutdownReceipt) -> Result<(), ApiError> {
        let mut gate = self
            .0
            .lock()
            .map_err(|_| ApiError::internal("admission gate unavailable"))?;
        if !gate.accepting {
            return Err(ApiError::unavailable(
                "APPLICATION_SHUTTING_DOWN",
                "Application shutdown has already been accepted",
            ));
        }
        if gate.compatibility_in_flight > 0 {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "SHUTDOWN_OPERATIONS_ACTIVE",
                "Compatibility operations block shutdown".into(),
            ));
        }
        gate.accepting = false;
        gate.accepted = Some(receipt);
        Ok(())
    }
    fn reopen_after_failed_acceptance(&self) {
        if let Ok(mut gate) = self.0.lock() {
            gate.accepting = true;
            gate.accepted = None;
        }
    }
    pub fn accepted_receipt(&self) -> Option<ShutdownReceipt> {
        self.0.lock().ok()?.accepted.clone()
    }
    pub fn is_accepting(&self) -> bool {
        self.0.lock().is_ok_and(|gate| gate.accepting)
    }
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
    #[serde(default)]
    shutdown_actions: Vec<ShutdownAuditRecord>,
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
    fn write_atomic(
        &self,
        relative: &str,
        bytes: &[u8],
    ) -> Result<AtomicWriteOutcome, AtomicWriteError>;
    fn recover_atomic(&self, relative: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtomicWriteOutcome {
    Committed,
    CommittedWithDurabilityWarning(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtomicWriteError {
    NotCommitted(String),
    CommitStateUnknown(String),
}

impl std::fmt::Display for AtomicWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(message) => write!(formatter, "not_committed:{message}"),
            Self::CommitStateUnknown(message) => {
                write!(formatter, "commit_state_unknown:{message}")
            }
        }
    }
}

#[derive(Clone)]
pub struct FileStorage {
    root: PathBuf,
    #[cfg(test)]
    fault: Option<AtomicWriteFault>,
}
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum AtomicWriteFault {
    BeforeRename,
    BackupCleanup,
    DirectorySync,
}
impl FileStorage {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            #[cfg(test)]
            fault: None,
        }
    }
    #[cfg(test)]
    fn with_fault(root: PathBuf, fault: AtomicWriteFault) -> Self {
        Self {
            root,
            fault: Some(fault),
        }
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
    fn write_atomic(
        &self,
        relative: &str,
        bytes: &[u8],
    ) -> Result<AtomicWriteOutcome, AtomicWriteError> {
        #[cfg(test)]
        let inject = bytes == b"accepted"
            || String::from_utf8_lossy(bytes).contains("\"decision\":\"accepted\"")
            || String::from_utf8_lossy(bytes).contains("\"decision\": \"accepted\"");
        let path = self.path(relative);
        let parent = path
            .parent()
            .ok_or_else(|| AtomicWriteError::NotCommitted("Atomic path has no parent".into()))?;
        fs::create_dir_all(parent)
            .map_err(|error| AtomicWriteError::NotCommitted(error.to_string()))?;
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
            .map_err(|error| AtomicWriteError::NotCommitted(error.to_string()))?;
        use std::io::Write as _;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| AtomicWriteError::NotCommitted(error.to_string()))?;
        drop(file);
        if path.exists() {
            if backup.exists() {
                fs::remove_file(&backup)
                    .map_err(|error| AtomicWriteError::NotCommitted(error.to_string()))?;
            }
            fs::rename(&path, &backup)
                .map_err(|error| AtomicWriteError::NotCommitted(error.to_string()))?;
        }
        #[cfg(test)]
        if inject && matches!(self.fault, Some(AtomicWriteFault::BeforeRename)) {
            let _ = fs::remove_file(&temp);
            if backup.exists() && !path.exists() {
                let _ = fs::rename(&backup, &path);
            }
            return Err(AtomicWriteError::NotCommitted(
                "injected_before_rename".into(),
            ));
        }
        if let Err(error) = fs::rename(&temp, &path) {
            let restored = if backup.exists() && !path.exists() {
                fs::rename(&backup, &path).is_ok()
            } else {
                path.exists()
            };
            if restored {
                let _ = fs::remove_file(&temp);
                return Err(AtomicWriteError::NotCommitted(error.to_string()));
            }
            let _ = fs::remove_file(&temp);
            return Err(AtomicWriteError::CommitStateUnknown(error.to_string()));
        }
        let mut warnings = Vec::new();
        if backup.exists() {
            #[cfg(test)]
            let cleanup = if inject && matches!(self.fault, Some(AtomicWriteFault::BackupCleanup)) {
                Err(std::io::Error::other("injected_backup_cleanup"))
            } else {
                fs::remove_file(&backup)
            };
            #[cfg(not(test))]
            let cleanup = fs::remove_file(&backup);
            if let Err(error) = cleanup {
                warnings.push(format!("backup_cleanup:{error}"));
            }
        }
        #[cfg(unix)]
        {
            #[cfg(test)]
            let sync = if inject && matches!(self.fault, Some(AtomicWriteFault::DirectorySync)) {
                Err(std::io::Error::other("injected_directory_sync"))
            } else {
                fs::File::open(parent).and_then(|directory| directory.sync_all())
            };
            #[cfg(not(test))]
            let sync = fs::File::open(parent).and_then(|directory| directory.sync_all());
            if let Err(error) = sync {
                warnings.push(format!("directory_sync:{error}"));
            }
        }
        if warnings.is_empty() {
            Ok(AtomicWriteOutcome::Committed)
        } else {
            Ok(AtomicWriteOutcome::CommittedWithDurabilityWarning(
                warnings.join(","),
            ))
        }
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

pub struct GenerationOwnedJsonFile {
    storage: FileStorage,
    relative: String,
}

impl GenerationOwnedJsonFile {
    pub fn new(root: PathBuf, relative: impl Into<String>) -> Self {
        Self {
            storage: FileStorage::new(root),
            relative: relative.into(),
        }
    }

    pub fn publish(&self, generation: u64, mut value: Value) -> Result<(), String> {
        value["generation"] = json!(generation);
        let bytes = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
        self.storage
            .recover_atomic(&self.relative)
            .map_err(|error| error.to_string())?;
        self.storage
            .write_atomic(&self.relative, &bytes)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn remove_if_generation(&self, generation: u64) -> Result<bool, String> {
        self.storage
            .recover_atomic(&self.relative)
            .map_err(|error| error.to_string())?;
        let Some(bytes) = self
            .storage
            .read(&self.relative)
            .map_err(|error| error.to_string())?
        else {
            return Ok(false);
        };
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if value.get("generation").and_then(Value::as_u64) != Some(generation) {
            return Ok(false);
        }
        fs::remove_file(self.storage.path(&self.relative)).map_err(|error| error.to_string())?;
        Ok(true)
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

#[async_trait]
pub trait ShutdownHooks: Send + Sync {
    async fn release_resources(&self) -> Result<(), &'static str>;
    async fn unregister_lifecycle(&self) -> Result<&'static str, &'static str>;
    async fn request_exit(&self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeardownOutcome {
    Exited,
    Withheld(&'static str),
    NotAccepted,
    Duplicate,
}

pub async fn await_server_drain(
    mut server: JoinHandle<Result<(), &'static str>>,
    timeout: std::time::Duration,
) -> Result<(), &'static str> {
    match tokio::time::timeout(timeout, &mut server).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("SERVER_TASK_JOIN_FAILED"),
        Err(_) => {
            server.abort();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server)
                .await
                .map_err(|_| "SERVER_TASK_ABORT_TIMEOUT")?
                .map_err(|error| {
                    if error.is_cancelled() {
                        "SERVER_DRAIN_TIMEOUT"
                    } else {
                        "SERVER_TASK_JOIN_FAILED"
                    }
                })?;
            Err("SERVER_DRAIN_TIMEOUT")
        }
    }
}

pub async fn run_until_shutdown_then_drain<S>(
    shutdown: S,
    server: JoinHandle<Result<(), &'static str>>,
    timeout: std::time::Duration,
) -> Result<(), &'static str>
where
    S: std::future::Future<Output = ()>,
{
    shutdown.await;
    await_server_drain(server, timeout).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentControlEnableTransaction {
    previous: bool,
    intended: bool,
}

pub fn transactional_disable_then_delete<P, D, R>(
    persist_disabled: P,
    delete_credential: D,
    restore_previous: R,
) -> Result<(), &'static str>
where
    P: FnOnce() -> Result<(), &'static str>,
    D: FnOnce() -> Result<(), &'static str>,
    R: FnOnce() -> Result<(), &'static str>,
{
    persist_disabled()?;
    if let Err(code) = delete_credential() {
        restore_previous().map_err(|_| "LIFECYCLE_CLEAR_ROLLBACK_FAILED")?;
        return Err(code);
    }
    Ok(())
}

pub async fn run_post_kernel_startup<C, CFut, R, RFut>(
    convert_listener: C,
    rollback: R,
) -> Result<(), &'static str>
where
    C: FnOnce() -> CFut,
    CFut: std::future::Future<Output = Result<(), &'static str>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = ()>,
{
    if let Err(code) = convert_listener().await {
        rollback().await;
        return Err(code);
    }
    Ok(())
}

impl AgentControlEnableTransaction {
    pub fn new(previous: bool, intended: bool) -> Self {
        Self { previous, intended }
    }
    pub fn persisted_before_start(self) -> bool {
        self.intended
    }
    pub fn persisted_after_start(self, started: bool) -> bool {
        if started {
            self.intended
        } else if self.intended {
            self.previous
        } else {
            false
        }
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
    pub provider_id: String,
    pub instance_id: String,
    pub shutdown_tx: Option<mpsc::UnboundedSender<ShutdownReceipt>>,
    pub token_scopes: BTreeSet<String>,
    pub admission: AdmissionController,
    pub ownership_generation: u64,
    pub terminal_shutdown: TerminalShutdownLatch,
    pub shutdown_transaction: SettingsTransaction,
}

#[derive(Clone)]
pub struct Kernel {
    config: KernelConfig,
    ledger: Arc<Mutex<Ledger>>,
    callback_workers: Arc<Mutex<std::collections::BTreeSet<String>>>,
    background_tasks: Arc<StdMutex<BackgroundTasks>>,
    maintenance_shutdown: watch::Sender<bool>,
    maintenance_running: Arc<AtomicBool>,
    maintenance_starts: Arc<AtomicUsize>,
    teardown_claimed: Arc<AtomicBool>,
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
    fn unavailable(code: &'static str, message: impl Into<String>) -> Self {
        Self(StatusCode::SERVICE_UNAVAILABLE, code, message.into())
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
            background_tasks: Arc::new(StdMutex::new(BackgroundTasks {
                accepting: true,
                handles: Vec::new(),
            })),
            maintenance_shutdown,
            maintenance_running: Arc::new(AtomicBool::new(false)),
            maintenance_starts: Arc::new(AtomicUsize::new(0)),
            teardown_claimed: Arc::new(AtomicBool::new(false)),
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
            .route(SHUTDOWN_PATH, post(shutdown_application))
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
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {changed=shutdown.changed()=>{if changed.is_err()||*shutdown.borrow(){break}},_=tokio::time::sleep(std::time::Duration::from_millis(kernel.config.maintenance_interval_ms.max(1)))=>{let _=kernel.maintenance_at(Utc::now()).await;}}
                kernel.reap_background_tasks();
            }
            kernel.maintenance_running.store(false, Ordering::SeqCst);
        });
        self.track_task(task);
    }
    fn track_task(&self, task: JoinHandle<()>) {
        if let Ok(mut tasks) = self.background_tasks.lock() {
            tasks.handles.retain(|handle| !handle.is_finished());
            if tasks.accepting {
                tasks.handles.push(task);
            } else {
                task.abort();
            }
        } else {
            task.abort();
        }
    }
    pub fn reap_background_tasks(&self) -> usize {
        let Ok(mut tasks) = self.background_tasks.lock() else {
            return 0;
        };
        tasks.handles.retain(|handle| !handle.is_finished());
        tasks.handles.len()
    }

    #[cfg(test)]
    fn tracked_background_tasks(&self) -> usize {
        self.background_tasks
            .lock()
            .map(|tasks| tasks.handles.len())
            .unwrap_or(usize::MAX)
    }
    pub async fn shutdown_resources(&self) -> Result<(), &'static str> {
        if !self.config.admission.is_accepting()
            && self
                .ledger
                .lock()
                .await
                .operations
                .values()
                .any(|operation| !terminal(&operation.state))
        {
            return Err("NONTERMINAL_OPERATION_PRESENT");
        }
        let _ = self.maintenance_shutdown.send(true);
        let tasks = {
            let mut ownership = self
                .background_tasks
                .lock()
                .map_err(|_| "BACKGROUND_TASK_LOCK_FAILED")?;
            ownership.accepting = false;
            ownership.handles.drain(..).collect::<Vec<_>>()
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            if tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .is_err()
            {
                return Err("BACKGROUND_TASK_SHUTDOWN_TIMEOUT");
            }
        }
        self.maintenance_running.store(false, Ordering::SeqCst);
        self.callback_workers.lock().await.clear();
        Ok(())
    }

    pub async fn is_accepting(&self) -> bool {
        self.config.admission.is_accepting()
    }

    pub fn admission(&self) -> AdmissionController {
        self.config.admission.clone()
    }

    pub fn accepted_shutdown(&self) -> Option<ShutdownReceipt> {
        self.config.admission.accepted_receipt()
    }

    pub async fn orchestrate_shutdown(
        &self,
        server_result: Result<(), &'static str>,
        hooks: &dyn ShutdownHooks,
    ) -> TeardownOutcome {
        let Some(receipt) = self.accepted_shutdown() else {
            return TeardownOutcome::NotAccepted;
        };
        if self
            .teardown_claimed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return TeardownOutcome::Duplicate;
        }
        let digest = receipt.request_digest;
        if let Err(code) = server_result {
            if self
                .record_shutdown_stage(&digest, "response_drained", "failed", Some(code))
                .await
                .is_err()
            {
                let _ = self.write_emergency_audit(&digest, "response_drained", code);
            }
            return TeardownOutcome::Withheld(code);
        }
        if self
            .record_shutdown_stage(&digest, "response_drained", "succeeded", None)
            .await
            .is_err()
        {
            let _ = self.write_emergency_audit(
                &digest,
                "response_drained",
                "RESPONSE_STAGE_PERSIST_FAILED",
            );
            return TeardownOutcome::Withheld("RESPONSE_STAGE_PERSIST_FAILED");
        }
        if let Err(code) = hooks.release_resources().await {
            if self
                .record_shutdown_stage(&digest, "resource_release", "failed", Some(code))
                .await
                .is_err()
            {
                let _ = self.write_emergency_audit(&digest, "resource_release", code);
            }
            return TeardownOutcome::Withheld(code);
        }
        if self
            .record_shutdown_stage(&digest, "resource_release", "succeeded", None)
            .await
            .is_err()
        {
            let _ = self.write_emergency_audit(
                &digest,
                "resource_release",
                "RESOURCE_STAGE_PERSIST_FAILED",
            );
            return TeardownOutcome::Withheld("RESOURCE_STAGE_PERSIST_FAILED");
        }
        match hooks.unregister_lifecycle().await {
            Ok(outcome) => {
                if self
                    .record_shutdown_stage(&digest, "lifecycle_unregister", outcome, None)
                    .await
                    .is_err()
                {
                    let _ = self.write_emergency_audit(
                        &digest,
                        "lifecycle_unregister",
                        "LIFECYCLE_STAGE_PERSIST_FAILED",
                    );
                    return TeardownOutcome::Withheld("LIFECYCLE_STAGE_PERSIST_FAILED");
                }
            }
            Err(code) => {
                if self
                    .record_shutdown_stage(&digest, "lifecycle_unregister", "failed", Some(code))
                    .await
                    .is_err()
                {
                    let _ = self.write_emergency_audit(&digest, "lifecycle_unregister", code);
                }
                return TeardownOutcome::Withheld(code);
            }
        }
        if self
            .record_shutdown_stage(&digest, "exit_requested", "succeeded", None)
            .await
            .is_err()
        {
            let _ =
                self.write_emergency_audit(&digest, "exit_requested", "EXIT_STAGE_PERSIST_FAILED");
            return TeardownOutcome::Withheld("EXIT_STAGE_PERSIST_FAILED");
        }
        hooks.request_exit().await;
        TeardownOutcome::Exited
    }

    fn write_emergency_audit(
        &self,
        request_digest: &str,
        stage: &str,
        failure_code: &str,
    ) -> Result<(), String> {
        let directory = self.config.storage.path("emergency-audit");
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .map_err(|error| error.to_string())?;
        }
        let id = random_id("shutdown-");
        let final_path = directory.join(format!("{id}.json"));
        let temporary = directory.join(format!(".{id}.tmp"));
        let bytes = serde_json::to_vec_pretty(&json!({
            "schemaVersion": "1.0",
            "type": "veniceMediaShutdownEmergencyAudit.v1",
            "requestDigest": request_digest,
            "stage": stage,
            "outcome": "failed",
            "failureCode": failure_code,
            "recordedAt": Utc::now().to_rfc3339()
        }))
        .map_err(|error| error.to_string())?;
        use std::io::Write as _;
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| {
                let _ = fs::remove_file(&temporary);
                error.to_string()
            })?;
        drop(file);
        fs::hard_link(&temporary, &final_path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            error.to_string()
        })?;
        fs::remove_file(&temporary).map_err(|error| error.to_string())?;
        fs::File::open(&final_path)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        fs::File::open(&directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub async fn record_shutdown_stage(
        &self,
        request_digest: &str,
        stage: &str,
        outcome: &str,
        failure_code: Option<&str>,
    ) -> Result<(), String> {
        let mut ledger = self.ledger.lock().await;
        let mut updated = ledger.clone();
        let record = updated
            .shutdown_actions
            .iter_mut()
            .find(|record| record.request_digest == request_digest && record.decision == "accepted")
            .ok_or_else(|| "Accepted shutdown audit record is missing".to_string())?;
        if record.stages.iter().any(|record| record.stage == stage) {
            return Ok(());
        }
        record.stages.push(ShutdownStageRecord {
            stage: stage.to_string(),
            outcome: outcome.to_string(),
            recorded_at: Utc::now().to_rfc3339(),
            failure_code: failure_code.map(str::to_string),
        });
        self.persist_locked(&updated)
            .await
            .map_err(|error| error.2)?;
        *ledger = updated;
        Ok(())
    }

    async fn persist_locked(&self, ledger: &Ledger) -> Result<(), ApiError> {
        self.persist_locked_outcome(ledger)
            .await
            .map(|_| ())
            .map_err(|error| ApiError::internal(error.to_string()))
    }

    async fn persist_locked_outcome(
        &self,
        ledger: &Ledger,
    ) -> Result<AtomicWriteOutcome, AtomicWriteError> {
        let bytes = serde_json::to_vec_pretty(ledger)
            .map_err(|error| AtomicWriteError::NotCommitted(error.to_string()))?;
        self.config.storage.write_atomic("ledger.json", &bytes)
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
        let task = tokio::spawn(async move {
            let _ = kernel.run_execution(id).await;
        });
        self.track_task(task);
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
        let task = tokio::spawn(async move {
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
        self.track_task(task);
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
            let task = tokio::spawn(async move {
                if let Ok(at) = DateTime::parse_from_rfc3339(&erase_after) {
                    if let Ok(wait) = (at.with_timezone(&Utc) - Utc::now()).to_std() {
                        tokio::time::sleep(wait).await;
                    }
                }
                let _ = kernel.maintenance_at(Utc::now()).await;
            });
            self.track_task(task);
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
fn bounded_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
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
    kernel.config.admission.ensure_accepting()?;
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

async fn shutdown_application(
    State(kernel): State<Kernel>,
    headers: HeaderMap,
    request: Result<Json<ShutdownRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ShutdownReceipt>), ApiError> {
    // Global order: Settings/lifecycle transaction barrier, then provider ledger.
    let _transaction = kernel.config.shutdown_transaction.lock().await;
    let Json(request) = request.map_err(|_| {
        ApiError::bad(
            "INVALID_SHUTDOWN_REQUEST",
            "Shutdown request body is malformed or contains unknown fields",
        )
    })?;
    let client = auth(&kernel, &headers)?;
    let request_value =
        serde_json::to_value(&request).map_err(|error| ApiError::internal(error.to_string()))?;
    let digest =
        canonical_digest(&request_value).map_err(|error| ApiError::internal(error.to_string()))?;
    let now = Utc::now();
    let requested_at =
        DateTime::parse_from_rfc3339(&request.requested_at).map(|value| value.with_timezone(&Utc));
    let expires_at =
        DateTime::parse_from_rfc3339(&request.expires_at).map(|value| value.with_timezone(&Utc));
    let mut reason_code = None;
    if request.schema_version != "1.0"
        || request.envelope_type != "veniceMediaApplicationShutdown.v1"
        || request.scope != SHUTDOWN_SCOPE
        || request.reason != "phase5h-release-slot-transition"
        || !bounded_identifier(&request.request_id)
        || !bounded_identifier(&request.idempotency_key)
    {
        reason_code = Some((
            StatusCode::BAD_REQUEST,
            "INVALID_SHUTDOWN_REQUEST",
            "Shutdown envelope is invalid",
        ));
    } else if request.provider_id != kernel.config.provider_id {
        reason_code = Some((
            StatusCode::CONFLICT,
            "PROVIDER_BINDING_MISMATCH",
            "Shutdown provider binding does not match",
        ));
    } else if request.instance_id != kernel.config.instance_id {
        reason_code = Some((
            StatusCode::CONFLICT,
            "INSTANCE_BINDING_MISMATCH",
            "Shutdown instance binding does not match",
        ));
    } else if !valid_sha256(&request.manifest_digest)
        || request.manifest_digest != kernel.config.manifest_digest
    {
        reason_code = Some((
            StatusCode::CONFLICT,
            "MANIFEST_BINDING_MISMATCH",
            "Shutdown manifest binding does not match",
        ));
    } else if requested_at.is_err() || expires_at.is_err() {
        reason_code = Some((
            StatusCode::BAD_REQUEST,
            "INVALID_VALIDITY_INTERVAL",
            "Shutdown validity timestamps are invalid",
        ));
    } else {
        let requested_at = requested_at.as_ref().unwrap();
        let expires_at = expires_at.as_ref().unwrap();
        if *expires_at <= *requested_at || *expires_at - *requested_at > Duration::seconds(60) {
            reason_code = Some((
                StatusCode::BAD_REQUEST,
                "INVALID_VALIDITY_INTERVAL",
                "Shutdown validity interval is invalid",
            ));
        } else if *requested_at > now + Duration::seconds(5) {
            reason_code = Some((
                StatusCode::BAD_REQUEST,
                "SHUTDOWN_REQUEST_IN_FUTURE",
                "Shutdown request time is materially in the future",
            ));
        } else if now < *requested_at || now >= *expires_at {
            reason_code = Some((
                StatusCode::GONE,
                "SHUTDOWN_REQUEST_STALE",
                "Shutdown request is outside its validity interval",
            ));
        }
    }

    let mut ledger = kernel.ledger.lock().await;
    let decision_now = Utc::now();
    let active_operation_count = ledger
        .operations
        .values()
        .filter(|operation| !terminal(&operation.state))
        .count();
    let ambiguous_operation_count = ledger
        .operations
        .values()
        .filter(|operation| {
            operation.submission_certainty == "submitted_ambiguous" || operation.state == "lost"
        })
        .count();
    let compatibility_in_flight = kernel.config.admission.compatibility_in_flight()?;
    if !kernel.config.token_scopes.contains(SHUTDOWN_SCOPE) {
        reason_code = Some((
            StatusCode::FORBIDDEN,
            "SHUTDOWN_PERMISSION_DENIED",
            "Authenticated principal lacks application shutdown permission",
        ));
    } else if kernel
        .config
        .shutdown_tx
        .as_ref()
        .is_none_or(mpsc::UnboundedSender::is_closed)
    {
        reason_code = Some((
            StatusCode::SERVICE_UNAVAILABLE,
            "SHUTDOWN_OWNER_UNAVAILABLE",
            "Application shutdown owner is unavailable",
        ));
    }
    if reason_code.is_none() {
        let requested = requested_at.as_ref().expect("validated requestedAt");
        let expires = expires_at.as_ref().expect("validated expiresAt");
        if *requested > decision_now + Duration::seconds(5) {
            reason_code = Some((
                StatusCode::BAD_REQUEST,
                "SHUTDOWN_REQUEST_IN_FUTURE",
                "Shutdown request time is materially in the future",
            ));
        } else if decision_now < *requested || decision_now >= *expires {
            reason_code = Some((
                StatusCode::GONE,
                "SHUTDOWN_REQUEST_STALE",
                "Shutdown request is outside its validity interval",
            ));
        }
    }
    if let Some(existing) = ledger
        .shutdown_actions
        .iter()
        .find(|record| record.idempotency_key == request.idempotency_key)
    {
        let (status, code, message) = if existing.request_digest == digest {
            (
                StatusCode::CONFLICT,
                "SHUTDOWN_REPLAY_REJECTED",
                "Shutdown request was already decided",
            )
        } else {
            (
                StatusCode::CONFLICT,
                "IDEMPOTENCY_DIGEST_CONFLICT",
                "Shutdown idempotency key conflicts",
            )
        };
        let mut updated = ledger.clone();
        updated.shutdown_actions.push(ShutdownAuditRecord {
            action: "shutdown".into(),
            scope: request.scope,
            request_id: request.request_id,
            idempotency_key: request.idempotency_key,
            request_digest: digest,
            client_credential_fingerprint: client,
            provider_id: request.provider_id,
            instance_id: request.instance_id,
            manifest_digest: request.manifest_digest,
            requested_at: request.requested_at,
            expires_at: request.expires_at,
            decided_at: now.to_rfc3339(),
            decision: "denied".into(),
            reason_code: code.into(),
            active_operation_count,
            ambiguous_operation_count,
            stages: vec![],
        });
        kernel.persist_locked(&updated).await?;
        *ledger = updated;
        return Err(ApiError(status, code, message.into()));
    }
    if let Some(existing) = ledger
        .shutdown_actions
        .iter()
        .find(|record| record.request_id == request.request_id)
    {
        let code = if existing.request_digest == digest {
            "SHUTDOWN_REPLAY_REJECTED"
        } else {
            "REQUEST_ID_CONFLICT"
        };
        reason_code = Some((
            StatusCode::CONFLICT,
            code,
            "Shutdown request ID was already used",
        ));
    }
    if reason_code.is_none() && !kernel.config.admission.is_accepting() {
        let mut updated = ledger.clone();
        updated.shutdown_actions.push(ShutdownAuditRecord {
            action: "shutdown".into(),
            scope: request.scope,
            request_id: request.request_id,
            idempotency_key: request.idempotency_key,
            request_digest: digest,
            client_credential_fingerprint: client,
            provider_id: request.provider_id,
            instance_id: request.instance_id,
            manifest_digest: request.manifest_digest,
            requested_at: request.requested_at,
            expires_at: request.expires_at,
            decided_at: now.to_rfc3339(),
            decision: "denied".into(),
            reason_code: "APPLICATION_ALREADY_SHUTTING_DOWN".into(),
            active_operation_count,
            ambiguous_operation_count,
            stages: vec![],
        });
        kernel.persist_locked(&updated).await?;
        *ledger = updated;
        return Err(ApiError(
            StatusCode::CONFLICT,
            "APPLICATION_ALREADY_SHUTTING_DOWN",
            "Application shutdown has already been accepted".into(),
        ));
    }
    if reason_code.is_none() && (active_operation_count > 0 || compatibility_in_flight > 0) {
        reason_code = Some((
            StatusCode::CONFLICT,
            "SHUTDOWN_OPERATIONS_ACTIVE",
            "Active provider or compatibility operations block shutdown",
        ));
    }
    if reason_code.is_none() && ambiguous_operation_count > 0 {
        reason_code = Some((
            StatusCode::CONFLICT,
            "SHUTDOWN_RECONCILIATION_REQUIRED",
            "Ambiguous operation evidence blocks shutdown",
        ));
    }
    let decided_at = decision_now.to_rfc3339();
    if let Some((status, code, message)) = reason_code {
        let mut updated = ledger.clone();
        updated.shutdown_actions.push(ShutdownAuditRecord {
            action: "shutdown".into(),
            scope: request.scope,
            request_id: request.request_id,
            idempotency_key: request.idempotency_key,
            request_digest: digest,
            client_credential_fingerprint: client,
            provider_id: request.provider_id,
            instance_id: request.instance_id,
            manifest_digest: request.manifest_digest,
            requested_at: request.requested_at,
            expires_at: request.expires_at,
            decided_at,
            decision: "denied".into(),
            reason_code: code.into(),
            active_operation_count,
            ambiguous_operation_count,
            stages: vec![],
        });
        kernel.persist_locked(&updated).await?;
        *ledger = updated;
        return Err(ApiError(status, code, message.into()));
    }
    let receipt = ShutdownReceipt {
        schema_version: "1.0".into(),
        accepted: true,
        action: "shutdown".into(),
        scope: request.scope.clone(),
        provider_id: request.provider_id.clone(),
        instance_id: request.instance_id.clone(),
        manifest_digest: request.manifest_digest.clone(),
        request_id: request.request_id.clone(),
        idempotency_key: request.idempotency_key.clone(),
        request_digest: digest.clone(),
        accepted_at: decided_at.clone(),
        state: "shutting_down".into(),
        replayed: false,
        ownership_generation: kernel.config.ownership_generation,
    };
    let mut updated = ledger.clone();
    updated.shutdown_actions.push(ShutdownAuditRecord {
        action: "shutdown".into(),
        scope: request.scope,
        request_id: request.request_id,
        idempotency_key: request.idempotency_key,
        request_digest: digest,
        client_credential_fingerprint: client,
        provider_id: request.provider_id,
        instance_id: request.instance_id,
        manifest_digest: request.manifest_digest,
        requested_at: request.requested_at,
        expires_at: request.expires_at,
        decided_at,
        decision: "accepted".into(),
        reason_code: "SHUTDOWN_ACCEPTED".into(),
        active_operation_count,
        ambiguous_operation_count,
        stages: vec![],
    });
    kernel.config.terminal_shutdown.set();
    if let Err(error) = kernel.config.admission.close(receipt.clone()) {
        kernel.config.terminal_shutdown.clear();
        return Err(error);
    }
    match kernel.persist_locked_outcome(&updated).await {
        Ok(AtomicWriteOutcome::Committed) => {
            *ledger = updated;
        }
        Ok(AtomicWriteOutcome::CommittedWithDurabilityWarning(warning)) => {
            *ledger = updated;
            let _ = kernel.write_emergency_audit(
                &receipt.request_digest,
                "acceptance_persistence",
                &format!("COMMITTED_WITH_DURABILITY_WARNING:{warning}"),
            );
        }
        Err(AtomicWriteError::NotCommitted(error)) => {
            kernel.config.admission.reopen_after_failed_acceptance();
            kernel.config.terminal_shutdown.clear();
            return Err(ApiError::internal(error));
        }
        Err(AtomicWriteError::CommitStateUnknown(error)) => {
            *ledger = updated;
            let _ = kernel.write_emergency_audit(
                &receipt.request_digest,
                "acceptance_persistence",
                "COMMIT_STATE_UNKNOWN",
            );
            return Err(ApiError::unavailable(
                "SHUTDOWN_ACCEPTANCE_UNCERTAIN",
                format!("Shutdown acceptance persistence is uncertain: {error}"),
            ));
        }
    }
    let _ = kernel
        .config
        .shutdown_tx
        .as_ref()
        .unwrap()
        .send(receipt.clone());
    Ok((StatusCode::ACCEPTED, Json(receipt)))
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
    kernel.config.admission.ensure_accepting()?;
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
    kernel.config.admission.ensure_accepting()?;
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
    kernel.config.admission.ensure_accepting()?;
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
    kernel.config.admission.ensure_accepting()?;
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
    kernel.config.admission.ensure_accepting()?;
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
    kernel.config.admission.ensure_accepting()?;
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
    kernel.config.admission.ensure_accepting()?;
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
    use axum::{body::Body, http::Request};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;
    struct FailingStorage {
        inner: FileStorage,
        writes: AtomicUsize,
        fail_at: usize,
    }
    impl Storage for FailingStorage {
        fn path(&self, relative: &str) -> PathBuf {
            self.inner.path(relative)
        }
        fn create_dir_all(&self, relative: &str) -> Result<(), String> {
            self.inner.create_dir_all(relative)
        }
        fn read(&self, relative: &str) -> Result<Option<Vec<u8>>, String> {
            self.inner.read(relative)
        }
        fn recover_atomic(&self, relative: &str) -> Result<(), String> {
            self.inner.recover_atomic(relative)
        }
        fn write_atomic(
            &self,
            relative: &str,
            bytes: &[u8],
        ) -> Result<AtomicWriteOutcome, AtomicWriteError> {
            let write = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
            if write == self.fail_at {
                Err(AtomicWriteError::NotCommitted(
                    "injected persistence failure".into(),
                ))
            } else {
                self.inner.write_atomic(relative, bytes)
            }
        }
    }
    struct FakeShutdownHooks {
        events: Arc<StdMutex<Vec<&'static str>>>,
        resource_error: Option<&'static str>,
        lifecycle_error: Option<&'static str>,
        exits: Arc<AtomicUsize>,
    }
    struct DrainBoundaryHooks {
        drained: Arc<AtomicBool>,
        exits: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ShutdownHooks for DrainBoundaryHooks {
        async fn release_resources(&self) -> Result<(), &'static str> {
            assert!(self.drained.load(Ordering::SeqCst));
            Ok(())
        }
        async fn unregister_lifecycle(&self) -> Result<&'static str, &'static str> {
            Ok("succeeded")
        }
        async fn request_exit(&self) {
            self.exits.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[async_trait]
    impl ShutdownHooks for FakeShutdownHooks {
        async fn release_resources(&self) -> Result<(), &'static str> {
            self.events.lock().unwrap().push("resources");
            self.resource_error.map_or(Ok(()), Err)
        }
        async fn unregister_lifecycle(&self) -> Result<&'static str, &'static str> {
            self.events.lock().unwrap().push("lifecycle");
            self.lifecycle_error.map_or(Ok("succeeded"), Err)
        }
        async fn request_exit(&self) {
            self.events.lock().unwrap().push("exit");
            self.exits.fetch_add(1, Ordering::SeqCst);
        }
    }
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
    fn test_config(
        root: &std::path::Path,
        shutdown_tx: Option<mpsc::UnboundedSender<ShutdownReceipt>>,
    ) -> KernelConfig {
        KernelConfig {
            storage: Arc::new(FileStorage::new(root.into())),
            token: "0123456789abcdef".into(),
            manifest_digest: "a".repeat(64),
            trusted_callback_origin: "http://127.0.0.1".into(),
            executor: Arc::new(Noop),
            secret_protector: Arc::new(TestProtector),
            callback_retry_base_ms: 10,
            terminal_replay_window_ms: 50,
            maintenance_interval_ms: 10,
            provider_id: "venice-media-local".into(),
            instance_id: "vml-test".into(),
            shutdown_tx,
            token_scopes: BTreeSet::from([SHUTDOWN_SCOPE.into()]),
            admission: AdmissionController::default(),
            ownership_generation: 0,
            terminal_shutdown: Default::default(),
            shutdown_transaction: Default::default(),
        }
    }
    fn shutdown_body(key: &str) -> Value {
        let requested = Utc::now() - Duration::seconds(1);
        json!({
            "schemaVersion":"1.0",
            "type":"veniceMediaApplicationShutdown.v1",
            "requestId":format!("request-{key}"),
            "idempotencyKey":key,
            "scope":"application:shutdown",
            "providerId":"venice-media-local",
            "instanceId":"vml-test",
            "manifestDigest":"a".repeat(64),
            "requestedAt":requested.to_rfc3339(),
            "expiresAt":(requested+Duration::seconds(30)).to_rfc3339(),
            "reason":"phase5h-release-slot-transition"
        })
    }
    fn submit_body(key: &str) -> Value {
        let input = json!({});
        json!({
            "schemaVersion":"1.0",
            "type":"veniceMediaOperation.v1",
            "requestId":format!("submit-{key}"),
            "idempotencyKey":key,
            "coreOperationId":format!("core-{key}"),
            "attempt":1,
            "assignmentRevision":1,
            "capability":{"id":"media.models.list","revision":"2"},
            "manifestDigest":"a".repeat(64),
            "inputDigest":input_digest(&input, &[]).unwrap(),
            "input":input,
            "inputArtifacts":[],
            "callback":{
                "url":"http://127.0.0.1/callback",
                "authorization":"synthetic-callback-secret",
                "expiresAt":(Utc::now()+Duration::minutes(5)).to_rfc3339()
            },
            "requestedAt":Utc::now().to_rfc3339()
        })
    }
    async fn post_json(router: Router, path: &str, body: Value) -> Response {
        router
            .oneshot(
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer 0123456789abcdef")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    async fn post_shutdown(router: Router, token: Option<&str>, body: Value) -> Response {
        let mut request =
            Request::post(SHUTDOWN_PATH).header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        router
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
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

    #[test]
    fn atomic_write_distinguishes_precommit_failure_and_postcommit_warnings() {
        for fault in [
            AtomicWriteFault::BeforeRename,
            AtomicWriteFault::BackupCleanup,
            AtomicWriteFault::DirectorySync,
        ] {
            let root = tempfile::tempdir().unwrap();
            let baseline = FileStorage::new(root.path().into());
            baseline.write_atomic("ledger.json", b"old").unwrap();
            let storage = FileStorage::with_fault(root.path().into(), fault);
            let result = storage.write_atomic("ledger.json", b"accepted");
            match fault {
                AtomicWriteFault::BeforeRename => {
                    assert!(result.is_err());
                    assert_eq!(fs::read(root.path().join("ledger.json")).unwrap(), b"old");
                }
                AtomicWriteFault::BackupCleanup | AtomicWriteFault::DirectorySync => {
                    assert!(matches!(
                        result,
                        Ok(AtomicWriteOutcome::CommittedWithDurabilityWarning(_))
                    ));
                    assert_eq!(
                        fs::read(root.path().join("ledger.json")).unwrap(),
                        b"accepted"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn shutdown_acceptance_reopens_only_on_definite_precommit_failure() {
        for (fault, accepted) in [
            (AtomicWriteFault::BeforeRename, false),
            (AtomicWriteFault::BackupCleanup, true),
            (AtomicWriteFault::DirectorySync, true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let baseline = FileStorage::new(root.path().into());
            baseline
                .write_atomic(
                    "ledger.json",
                    &serde_json::to_vec(&Ledger::default()).unwrap(),
                )
                .unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut config = test_config(root.path(), Some(tx));
            config.storage = Arc::new(FileStorage::with_fault(root.path().into(), fault));
            let kernel = Kernel::open(config).await.unwrap();
            let response = post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                shutdown_body("atomic-fault"),
            )
            .await;
            if accepted {
                assert_eq!(response.status(), StatusCode::ACCEPTED);
                assert!(!kernel.is_accepting().await);
                let disk: Value =
                    serde_json::from_slice(&fs::read(root.path().join("ledger.json")).unwrap())
                        .unwrap();
                assert_eq!(disk["shutdown_actions"][0]["decision"], "accepted");
            } else {
                assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert!(kernel.is_accepting().await);
                let disk: Value =
                    serde_json::from_slice(&fs::read(root.path().join("ledger.json")).unwrap())
                        .unwrap();
                assert!(disk["shutdown_actions"].as_array().unwrap().is_empty());
            }
        }
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
            provider_id: "venice-media-local".into(),
            instance_id: "vml-test".into(),
            shutdown_tx: None,
            token_scopes: BTreeSet::new(),
            admission: AdmissionController::default(),
            ownership_generation: 0,
            terminal_shutdown: Default::default(),
            shutdown_transaction: Default::default(),
        };
        let kernel = Kernel::open(config.clone()).await.unwrap();
        let ledger = kernel.ledger.lock().await;
        kernel.persist_locked(&ledger).await.unwrap();
        drop(ledger);
        Kernel::open(config).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_auth_scope_binding_freshness_and_strict_body_are_enforced() {
        for (mut body, expected) in [
            (
                {
                    let mut value = shutdown_body("scope");
                    value["scope"] = json!("application:restart");
                    value
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                {
                    let mut value = shutdown_body("provider");
                    value["providerId"] = json!("other");
                    value
                },
                StatusCode::CONFLICT,
            ),
            (
                {
                    let mut value = shutdown_body("instance");
                    value["instanceId"] = json!("vml-other");
                    value
                },
                StatusCode::CONFLICT,
            ),
            (
                {
                    let mut value = shutdown_body("manifest");
                    value["manifestDigest"] = json!("b".repeat(64));
                    value
                },
                StatusCode::CONFLICT,
            ),
            (
                {
                    let mut value = shutdown_body("unknown");
                    value["unknownField"] = json!(true);
                    value
                },
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let kernel = Kernel::open(test_config(root.path(), Some(tx)))
                .await
                .unwrap();
            let response =
                post_shutdown(kernel.router(), Some("0123456789abcdef"), body.take()).await;
            assert_eq!(response.status(), expected);
        }
        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let kernel = Kernel::open(test_config(root.path(), Some(tx)))
            .await
            .unwrap();
        assert_eq!(
            post_shutdown(kernel.clone().router(), None, shutdown_body("missing-auth"))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            post_shutdown(
                kernel.router(),
                Some("wrong-credential"),
                shutdown_body("bad-auth")
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );

        for (key, requested, expires, expected) in [
            (
                "stale",
                Utc::now() - Duration::seconds(90),
                Utc::now() - Duration::seconds(60),
                StatusCode::GONE,
            ),
            (
                "future",
                Utc::now() + Duration::seconds(20),
                Utc::now() + Duration::seconds(40),
                StatusCode::BAD_REQUEST,
            ),
            (
                "too-wide",
                Utc::now() - Duration::seconds(1),
                Utc::now() + Duration::seconds(90),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let kernel = Kernel::open(test_config(root.path(), Some(tx)))
                .await
                .unwrap();
            let mut body = shutdown_body(key);
            body["requestedAt"] = json!(requested.to_rfc3339());
            body["expiresAt"] = json!(expires.to_rfc3339());
            assert_eq!(
                post_shutdown(kernel.router(), Some("0123456789abcdef"), body)
                    .await
                    .status(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn authenticated_principal_without_shutdown_permission_is_denied_and_audited() {
        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut config = test_config(root.path(), Some(tx));
        config.token_scopes.clear();
        let kernel = Kernel::open(config).await.unwrap();
        let response = post_shutdown(
            kernel.clone().router(),
            Some("0123456789abcdef"),
            shutdown_body("permission"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let ledger = kernel.ledger.lock().await;
        assert_eq!(
            ledger.shutdown_actions.last().unwrap().reason_code,
            "SHUTDOWN_PERMISSION_DENIED"
        );
        assert!(kernel.config.admission.is_accepting());
    }

    #[tokio::test]
    async fn owner_unavailable_and_reused_request_id_are_audited() {
        let root = tempfile::tempdir().unwrap();
        let kernel = Kernel::open(test_config(root.path(), None)).await.unwrap();
        assert_eq!(
            post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                shutdown_body("owner")
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            kernel
                .ledger
                .lock()
                .await
                .shutdown_actions
                .last()
                .unwrap()
                .reason_code,
            "SHUTDOWN_OWNER_UNAVAILABLE"
        );

        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let kernel = Kernel::open(test_config(root.path(), Some(tx)))
            .await
            .unwrap();
        let first = shutdown_body("request-id");
        assert_eq!(
            post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                first.clone()
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        let mut second = first;
        second["idempotencyKey"] = json!("new-key");
        assert_eq!(
            post_shutdown(kernel.clone().router(), Some("0123456789abcdef"), second)
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            kernel
                .ledger
                .lock()
                .await
                .shutdown_actions
                .last()
                .unwrap()
                .reason_code,
            "REQUEST_ID_CONFLICT"
        );
    }

    #[tokio::test]
    async fn compatibility_permit_blocks_shutdown_and_cannot_cross_acceptance() {
        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let kernel = Kernel::open(test_config(root.path(), Some(tx)))
            .await
            .unwrap();
        let permit = kernel.admission().claim_compatibility().unwrap();
        assert_eq!(
            post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                shutdown_body("legacy-active")
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        drop(permit);
        assert_eq!(
            post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                shutdown_body("legacy-idle")
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        assert!(kernel.admission().claim_compatibility().is_err());
    }

    #[tokio::test]
    async fn direct_work_permit_blocks_shutdown_rejects_after_acceptance_and_projects_count() {
        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let kernel = Kernel::open(test_config(root.path(), Some(tx)))
            .await
            .unwrap();
        let permit = kernel.admission().claim_compatibility().unwrap();
        assert_eq!(kernel.admission().active_work_count(), 1);
        assert_eq!(
            post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                shutdown_body("direct-active"),
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        drop(permit);
        assert_eq!(kernel.admission().active_work_count(), 0);
        assert_eq!(
            post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                shutdown_body("direct-idle"),
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            kernel.admission().claim_compatibility().err(),
            Some("APPLICATION_SHUTTING_DOWN")
        );
    }

    #[tokio::test]
    async fn async_direct_permit_releases_on_cancellation() {
        let admission = AdmissionController::default();
        let worker_admission = admission.clone();
        let task = tokio::spawn(async move {
            let _permit = worker_admission.claim_compatibility().unwrap();
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(admission.active_work_count(), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(admission.active_work_count(), 0);
    }

    #[test]
    fn each_work_lane_contributes_exactly_one_activity_unit() {
        for lane in ["http_compatibility", "direct_tauri"] {
            let admission = AdmissionController::default();
            let permit = admission.claim_compatibility().unwrap();
            assert_eq!(admission.active_work_count(), 1, "lane {lane}");
            drop(permit);
            assert_eq!(admission.active_work_count(), 0, "lane {lane}");
        }
        let admission = AdmissionController::default();
        assert_eq!(
            admission.active_work_count(),
            0,
            "revision-2 is represented by ledger"
        );
    }

    #[tokio::test]
    async fn settings_stop_releases_barrier_before_waiting_for_shutdown_acceptance() {
        let transaction = SettingsTransaction::default();
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        let held = transaction.lock().await;
        drop(held);
        let shutdown = {
            let transaction = transaction.clone();
            tokio::spawn(async move {
                let _guard = transaction.lock().await;
                completion_tx.send(()).unwrap();
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), completion_rx)
            .await
            .unwrap()
            .unwrap();
        shutdown.await.unwrap();
    }

    #[tokio::test]
    async fn one_idle_shutdown_is_accepted_persisted_and_replays_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let kernel = Kernel::open(test_config(root.path(), Some(tx)))
            .await
            .unwrap();
        let router = kernel.clone().router();
        let body = shutdown_body("one");
        let first = post_shutdown(router.clone(), Some("0123456789abcdef"), body.clone()).await;
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let receipt = rx.recv().await.unwrap();
        assert_eq!(receipt.state, "shutting_down");
        let persisted: Value =
            serde_json::from_slice(&fs::read(root.path().join("ledger.json")).unwrap()).unwrap();
        assert_eq!(persisted["shutdown_actions"][0]["decision"], "accepted");
        assert!(!persisted.to_string().contains("0123456789abcdef"));
        assert_eq!(
            post_shutdown(router, Some("0123456789abcdef"), body)
                .await
                .status(),
            StatusCode::CONFLICT
        );
        drop(kernel);
        let reopened = Kernel::open(test_config(root.path(), None)).await.unwrap();
        assert!(reopened.is_accepting().await);
    }

    #[tokio::test]
    async fn concurrent_shutdown_accepts_once_and_submit_cannot_cross_transition() {
        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let kernel = Kernel::open(test_config(root.path(), Some(tx)))
            .await
            .unwrap();
        let router = kernel.clone().router();
        let mut tasks = Vec::new();
        for index in 0..12 {
            tasks.push(tokio::spawn(post_shutdown(
                router.clone(),
                Some("0123456789abcdef"),
                shutdown_body(&format!("concurrent-{index}")),
            )));
        }
        let mut accepted = 0;
        for task in tasks {
            if task.await.unwrap().status() == StatusCode::ACCEPTED {
                accepted += 1;
            }
        }
        assert_eq!(accepted, 1);
        assert!(!kernel.is_accepting().await);
    }

    #[tokio::test]
    async fn submit_shutdown_race_never_accepts_both() {
        for iteration in 0..24 {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let kernel = Kernel::open(test_config(root.path(), Some(tx)))
                .await
                .unwrap();
            let router = kernel.router();
            let submit = tokio::spawn(post_json(
                router.clone(),
                OPERATIONS_PATH,
                submit_body(&format!("race-{iteration}")),
            ));
            let shutdown = tokio::spawn(post_shutdown(
                router,
                Some("0123456789abcdef"),
                shutdown_body(&format!("race-{iteration}")),
            ));
            let submit_status = submit.await.unwrap().status();
            let shutdown_status = shutdown.await.unwrap().status();
            assert_ne!(
                (submit_status, shutdown_status),
                (StatusCode::ACCEPTED, StatusCode::ACCEPTED)
            );
            assert!(
                (submit_status == StatusCode::ACCEPTED && shutdown_status == StatusCode::CONFLICT)
                    || (submit_status == StatusCode::SERVICE_UNAVAILABLE
                        && shutdown_status == StatusCode::ACCEPTED)
            );
        }
    }

    #[tokio::test]
    async fn graceful_server_transmits_accepted_receipt_before_fake_exit() {
        let root = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let kernel = Kernel::open(test_config(root.path(), Some(tx)))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let exited = Arc::new(AtomicUsize::new(0));
        let server_exited = exited.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, kernel.router())
                .with_graceful_shutdown(async move {
                    rx.recv().await.unwrap();
                })
                .await
                .unwrap();
            server_exited.fetch_add(1, Ordering::SeqCst);
        });
        let response = reqwest::Client::new()
            .post(format!("http://{address}{SHUTDOWN_PATH}"))
            .bearer_auth("0123456789abcdef")
            .json(&shutdown_body("response-first"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["state"], "shutting_down");
        server.await.unwrap();
        assert_eq!(exited.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn teardown_orchestrator_orders_success_once_and_withholds_on_failures() {
        for (server, resource_error, lifecycle_error, expected) in [
            (Ok(()), None, None, TeardownOutcome::Exited),
            (
                Err("SERVER_DRAIN_FAILED"),
                None,
                None,
                TeardownOutcome::Withheld("SERVER_DRAIN_FAILED"),
            ),
            (
                Ok(()),
                Some("RESOURCE_FAILED"),
                None,
                TeardownOutcome::Withheld("RESOURCE_FAILED"),
            ),
            (
                Ok(()),
                None,
                Some("LIFECYCLE_FAILED"),
                TeardownOutcome::Withheld("LIFECYCLE_FAILED"),
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let kernel = Kernel::open(test_config(root.path(), Some(tx)))
                .await
                .unwrap();
            assert_eq!(
                post_shutdown(
                    kernel.clone().router(),
                    Some("0123456789abcdef"),
                    shutdown_body("orchestrate")
                )
                .await
                .status(),
                StatusCode::ACCEPTED
            );
            let events = Arc::new(StdMutex::new(Vec::new()));
            let exits = Arc::new(AtomicUsize::new(0));
            let hooks = FakeShutdownHooks {
                events: events.clone(),
                resource_error,
                lifecycle_error,
                exits: exits.clone(),
            };
            assert_eq!(kernel.orchestrate_shutdown(server, &hooks).await, expected);
            if expected == TeardownOutcome::Exited {
                assert_eq!(
                    *events.lock().unwrap(),
                    vec!["resources", "lifecycle", "exit"]
                );
                assert_eq!(exits.load(Ordering::SeqCst), 1);
            } else {
                assert_eq!(exits.load(Ordering::SeqCst), 0);
            }
            assert_eq!(
                kernel.orchestrate_shutdown(Ok(()), &hooks).await,
                TeardownOutcome::Duplicate
            );
        }
    }

    #[tokio::test]
    async fn settings_only_and_stage_persistence_failure_never_exit() {
        let root = tempfile::tempdir().unwrap();
        let kernel = Kernel::open(test_config(root.path(), None)).await.unwrap();
        let events = Arc::new(StdMutex::new(Vec::new()));
        let exits = Arc::new(AtomicUsize::new(0));
        let hooks = FakeShutdownHooks {
            events,
            resource_error: None,
            lifecycle_error: None,
            exits: exits.clone(),
        };
        assert_eq!(
            kernel.orchestrate_shutdown(Ok(()), &hooks).await,
            TeardownOutcome::NotAccepted
        );
        assert_eq!(exits.load(Ordering::SeqCst), 0);

        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut config = test_config(root.path(), Some(tx));
        config.storage = Arc::new(FailingStorage {
            inner: FileStorage::new(root.path().into()),
            writes: AtomicUsize::new(0),
            fail_at: 3,
        });
        let kernel = Kernel::open(config).await.unwrap();
        assert_eq!(
            post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                shutdown_body("persist-stage")
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        let events = Arc::new(StdMutex::new(Vec::new()));
        let hooks = FakeShutdownHooks {
            events,
            resource_error: None,
            lifecycle_error: None,
            exits: exits.clone(),
        };
        assert_eq!(
            kernel.orchestrate_shutdown(Ok(()), &hooks).await,
            TeardownOutcome::Withheld("RESPONSE_STAGE_PERSIST_FAILED")
        );
        assert_eq!(exits.load(Ordering::SeqCst), 0);
        let persisted: Value =
            serde_json::from_slice(&fs::read(root.path().join("ledger.json")).unwrap()).unwrap();
        assert_eq!(persisted["shutdown_actions"][0]["decision"], "accepted");
        let emergency = fs::read_dir(root.path().join("emergency-audit"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let emergency: Value = serde_json::from_slice(&fs::read(emergency).unwrap()).unwrap();
        assert_eq!(emergency["stage"], "response_drained");
        assert_eq!(emergency["failureCode"], "RESPONSE_STAGE_PERSIST_FAILED");
        assert!(!emergency.to_string().contains("0123456789abcdef"));
    }

    #[tokio::test]
    async fn emergency_audit_is_private_collision_safe_readable_and_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let kernel = Kernel::open(test_config(root.path(), None)).await.unwrap();
        kernel
            .write_emergency_audit(&"a".repeat(64), "resource_release", "FIRST")
            .unwrap();
        kernel
            .write_emergency_audit(&"b".repeat(64), "resource_release", "SECOND")
            .unwrap();
        let directory = root.path().join("emergency-audit");
        let entries = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .all(|path| path.extension().and_then(|value| value.to_str()) == Some("json")));
        assert!(!fs::read_dir(&directory).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        let records = entries
            .iter()
            .map(|path| serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert!(records
            .iter()
            .any(|record| record["failureCode"] == "FIRST"));
        assert!(records
            .iter()
            .any(|record| record["failureCode"] == "SECOND"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for path in &entries {
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }

        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut config = test_config(root.path(), Some(tx));
        config.storage = Arc::new(FailingStorage {
            inner: FileStorage::new(root.path().into()),
            writes: AtomicUsize::new(0),
            fail_at: 3,
        });
        let kernel = Kernel::open(config).await.unwrap();
        assert_eq!(
            post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                shutdown_body("dual-sink")
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        fs::write(root.path().join("emergency-audit"), b"blocked").unwrap();
        let exits = Arc::new(AtomicUsize::new(0));
        let hooks = FakeShutdownHooks {
            events: Arc::new(StdMutex::new(Vec::new())),
            resource_error: None,
            lifecycle_error: None,
            exits: exits.clone(),
        };
        assert_eq!(
            kernel.orchestrate_shutdown(Ok(()), &hooks).await,
            TeardownOutcome::Withheld("RESPONSE_STAGE_PERSIST_FAILED")
        );
        assert_eq!(exits.load(Ordering::SeqCst), 0);
        let accepted: Value =
            serde_json::from_slice(&fs::read(root.path().join("ledger.json")).unwrap()).unwrap();
        assert_eq!(accepted["shutdown_actions"][0]["decision"], "accepted");
    }

    #[tokio::test]
    async fn integrated_loopback_reads_response_then_drains_and_orchestrates_once() {
        let root = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let kernel = Kernel::open(test_config(root.path(), Some(tx)))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_kernel = kernel.clone();
        let exits = Arc::new(AtomicUsize::new(0));
        let drained = Arc::new(AtomicBool::new(false));
        let hooks = Arc::new(DrainBoundaryHooks {
            drained: drained.clone(),
            exits: exits.clone(),
        });
        let server_hooks = hooks.clone();
        let server = tokio::spawn(async move {
            let result = axum::serve(listener, server_kernel.clone().router())
                .with_graceful_shutdown(async move {
                    rx.recv().await.unwrap();
                })
                .await
                .map_err(|_| "SERVER_DRAIN_FAILED");
            drained.store(true, Ordering::SeqCst);
            server_kernel
                .orchestrate_shutdown(result, server_hooks.as_ref())
                .await
        });
        let response = reqwest::Client::new()
            .post(format!("http://{address}{SHUTDOWN_PATH}"))
            .bearer_auth("0123456789abcdef")
            .json(&shutdown_body("integrated"))
            .send()
            .await
            .unwrap();
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["state"], "shutting_down");
        assert_eq!(server.await.unwrap(), TeardownOutcome::Exited);
        assert_eq!(exits.load(Ordering::SeqCst), 1);
        let persisted: Value =
            serde_json::from_slice(&fs::read(root.path().join("ledger.json")).unwrap()).unwrap();
        let stages = persisted["shutdown_actions"][0]["stages"]
            .as_array()
            .unwrap();
        assert_eq!(
            stages
                .iter()
                .map(|stage| stage["stage"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "response_drained",
                "resource_release",
                "lifecycle_unregister",
                "exit_requested"
            ]
        );
    }

    #[tokio::test]
    async fn integrated_loopback_failure_persists_stage_and_withholds_exit() {
        let root = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let kernel = Kernel::open(test_config(root.path(), Some(tx)))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_kernel = kernel.clone();
        let exits = Arc::new(AtomicUsize::new(0));
        let hooks = Arc::new(FakeShutdownHooks {
            events: Arc::new(StdMutex::new(Vec::new())),
            resource_error: Some("RESOURCE_FAILED"),
            lifecycle_error: None,
            exits: exits.clone(),
        });
        let server_hooks = hooks.clone();
        let server = tokio::spawn(async move {
            let result = axum::serve(listener, server_kernel.clone().router())
                .with_graceful_shutdown(async move {
                    rx.recv().await.unwrap();
                })
                .await
                .map_err(|_| "SERVER_DRAIN_FAILED");
            server_kernel
                .orchestrate_shutdown(result, server_hooks.as_ref())
                .await
        });
        let response = reqwest::Client::new()
            .post(format!("http://{address}{SHUTDOWN_PATH}"))
            .bearer_auth("0123456789abcdef")
            .json(&shutdown_body("integrated-failure"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let _: Value = response.json().await.unwrap();
        assert_eq!(
            server.await.unwrap(),
            TeardownOutcome::Withheld("RESOURCE_FAILED")
        );
        assert_eq!(exits.load(Ordering::SeqCst), 0);
        let persisted: Value =
            serde_json::from_slice(&fs::read(root.path().join("ledger.json")).unwrap()).unwrap();
        let stages = persisted["shutdown_actions"][0]["stages"]
            .as_array()
            .unwrap();
        assert_eq!(stages[0]["stage"], "response_drained");
        assert_eq!(stages[0]["outcome"], "succeeded");
        assert_eq!(stages[1]["stage"], "resource_release");
        assert_eq!(stages[1]["outcome"], "failed");
        assert_eq!(stages[1]["failureCode"], "RESOURCE_FAILED");
    }

    #[tokio::test]
    async fn lifecycle_unregister_callers_share_failure_and_worker_replacement_joins_prior() {
        let supervisor = Arc::new(LifecycleSupervisor::default());
        supervisor.set_generation(1);
        let stopped = Arc::new(AtomicUsize::new(0));
        let first_stopped = stopped.clone();
        supervisor
            .start(1, |receiver| {
                tokio::spawn(async move {
                    let _ = receiver.await;
                    first_stopped.fetch_add(1, Ordering::SeqCst);
                })
            })
            .await
            .unwrap();
        let second_stopped = stopped.clone();
        supervisor
            .start(1, |receiver| {
                tokio::spawn(async move {
                    let _ = receiver.await;
                    second_stopped.fetch_add(1, Ordering::SeqCst);
                })
            })
            .await
            .unwrap();
        assert_eq!(stopped.load(Ordering::SeqCst), 1);

        let calls = Arc::new(AtomicUsize::new(0));
        let first = {
            let supervisor = supervisor.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                supervisor
                    .unregister(1, || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        CoordinatedLifecycleOutcome {
                            outcome: "failed",
                            failure_code: Some("LIFECYCLE_UNREGISTER_REJECTED"),
                        }
                    })
                    .await
            })
        };
        let second = {
            let supervisor = supervisor.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                supervisor
                    .unregister(1, || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        CoordinatedLifecycleOutcome {
                            outcome: "succeeded",
                            failure_code: None,
                        }
                    })
                    .await
            })
        };
        let first = first.await.unwrap();
        let second = second.await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.failure_code, Some("LIFECYCLE_UNREGISTER_REJECTED"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(stopped.load(Ordering::SeqCst), 2);

        supervisor.set_generation(2);
        supervisor
            .start(2, |receiver| {
                tokio::spawn(async move {
                    let _ = receiver.await;
                })
            })
            .await
            .unwrap();
        let stale = supervisor
            .unregister(1, || async {
                CoordinatedLifecycleOutcome {
                    outcome: "failed",
                    failure_code: Some("MUST_NOT_RUN"),
                }
            })
            .await;
        assert_eq!(stale.outcome, "stale_no_op");
        assert_eq!(supervisor.stop(1).await, Err("LIFECYCLE_STOP_STALE"));
        assert_eq!(
            supervisor
                .start(1, |_receiver| tokio::spawn(async {}))
                .await,
            Err("LIFECYCLE_START_STALE")
        );

        let noncooperative = Arc::new(LifecycleSupervisor::default());
        noncooperative.set_generation(1);
        noncooperative
            .start(1, |_receiver| {
                tokio::spawn(async move { std::future::pending::<()>().await })
            })
            .await
            .unwrap();
        tokio::time::pause();
        let stop = tokio::spawn(async move {
            noncooperative
                .unregister(1, || async {
                    CoordinatedLifecycleOutcome {
                        outcome: "succeeded",
                        failure_code: None,
                    }
                })
                .await
        });
        tokio::time::advance(std::time::Duration::from_secs(13)).await;
        assert_eq!(
            stop.await.unwrap().failure_code,
            Some("LIFECYCLE_WORKER_STOP_TIMEOUT")
        );
    }

    #[tokio::test]
    async fn shutdown_and_settings_overlap_share_exact_generation_unregister() {
        let supervisor = Arc::new(LifecycleSupervisor::default());
        supervisor.set_generation(7);
        supervisor
            .start(7, |receiver| {
                tokio::spawn(async move {
                    let _ = receiver.await;
                })
            })
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let settings = {
            let supervisor = supervisor.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                supervisor
                    .unregister(7, || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        CoordinatedLifecycleOutcome {
                            outcome: "failed",
                            failure_code: Some("UNREGISTER_FAILED"),
                        }
                    })
                    .await
            })
        };
        let shutdown = {
            let supervisor = supervisor.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                supervisor
                    .unregister(7, || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        CoordinatedLifecycleOutcome {
                            outcome: "succeeded",
                            failure_code: None,
                        }
                    })
                    .await
            })
        };
        let settings = settings.await.unwrap();
        let shutdown = shutdown.await.unwrap();
        assert_eq!(settings, shutdown);
        assert_eq!(settings.failure_code, Some("UNREGISTER_FAILED"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn transactional_enable_persists_intent_and_rolls_back_failed_start() {
        let enable = AgentControlEnableTransaction::new(false, true);
        assert!(enable.persisted_before_start());
        assert!(enable.persisted_after_start(true));
        assert!(!enable.persisted_after_start(false));
        let disable = AgentControlEnableTransaction::new(true, false);
        assert!(!disable.persisted_before_start());
        assert!(!disable.persisted_after_start(false));
    }

    #[tokio::test]
    async fn settings_transaction_lock_serializes_enable_disable_outcomes() {
        let transaction = Arc::new(tokio::sync::Mutex::new(()));
        let ownership = Arc::new(AgentControlOwnership::default());
        let persisted = Arc::new(AtomicBool::new(false));
        let enable = {
            let transaction = transaction.clone();
            let ownership = ownership.clone();
            let persisted = persisted.clone();
            tokio::spawn(async move {
                let _guard = transaction.lock().await;
                persisted.store(true, Ordering::SeqCst);
                let generation = ownership.reserve_start(9876, ()).unwrap();
                ownership.publish_running(generation).unwrap();
            })
        };
        let disable = {
            let transaction = transaction.clone();
            let ownership = ownership.clone();
            let persisted = persisted.clone();
            tokio::spawn(async move {
                let _guard = transaction.lock().await;
                persisted.store(false, Ordering::SeqCst);
                if let Some((generation, _)) = ownership.begin_stop().unwrap() {
                    ownership.finish_stop(generation);
                }
            })
        };
        enable.await.unwrap();
        disable.await.unwrap();
        assert!(!persisted.load(Ordering::SeqCst));
        assert_eq!(ownership.snapshot().0, AgentControlPhase::Stopped);
    }

    #[tokio::test]
    async fn shutdown_barrier_serializes_settings_configure_and_clear() {
        for label in ["settings", "configure", "clear"] {
            let transaction = SettingsTransaction::default();
            let latch = TerminalShutdownLatch::default();
            let order = Arc::new(StdMutex::new(Vec::new()));
            let held = transaction.lock().await;
            let shutdown = {
                let transaction = transaction.clone();
                let latch = latch.clone();
                let order = order.clone();
                tokio::spawn(async move {
                    let _guard = transaction.lock().await;
                    latch.set();
                    order.lock().unwrap().push("shutdown");
                })
            };
            order.lock().unwrap().push(label);
            drop(held);
            shutdown.await.unwrap();
            assert_eq!(*order.lock().unwrap(), vec![label, "shutdown"]);
            assert_eq!(latch.ensure_open(), Err("APPLICATION_SHUTTING_DOWN"));
        }
    }

    #[tokio::test]
    async fn saved_start_recheck_cannot_follow_completed_disable() {
        let transaction = Arc::new(SettingsTransaction::default());
        let persisted = Arc::new(AtomicBool::new(true));
        let ownership = Arc::new(AgentControlOwnership::default());
        let disable = {
            let transaction = transaction.clone();
            let persisted = persisted.clone();
            tokio::spawn(async move {
                let _guard = transaction.lock().await;
                persisted.store(false, Ordering::SeqCst);
            })
        };
        disable.await.unwrap();
        let _guard = transaction.lock().await;
        if persisted.load(Ordering::SeqCst) {
            ownership.reserve_start(9876, ()).unwrap();
        }
        assert_eq!(ownership.snapshot().0, AgentControlPhase::Stopped);
    }

    #[tokio::test]
    async fn rollback_worker_restoration_failure_is_reported() {
        let supervisor = LifecycleSupervisor::default();
        supervisor.set_generation(1);
        supervisor
            .start(1, |_receiver| tokio::spawn(async {}))
            .await
            .unwrap();
        supervisor.stop(1).await.unwrap();
        supervisor.set_generation(2);
        assert_eq!(
            supervisor
                .start(1, |_receiver| tokio::spawn(async {}))
                .await,
            Err("LIFECYCLE_START_STALE")
        );
        assert!(!supervisor.has_worker(1).await);
    }

    #[test]
    fn lifecycle_clear_transaction_preserves_credential_state_invariants() {
        let disabled = Arc::new(AtomicBool::new(false));
        let credential = Arc::new(AtomicBool::new(true));
        assert_eq!(
            transactional_disable_then_delete(
                || Err("PERSIST_FAILED"),
                || {
                    credential.store(false, Ordering::SeqCst);
                    Ok(())
                },
                || Ok(()),
            ),
            Err("PERSIST_FAILED")
        );
        assert!(credential.load(Ordering::SeqCst));
        let disabled_for_persist = disabled.clone();
        let disabled_for_restore = disabled.clone();
        assert_eq!(
            transactional_disable_then_delete(
                || {
                    disabled_for_persist.store(true, Ordering::SeqCst);
                    Ok(())
                },
                || Err("DELETE_FAILED"),
                || {
                    disabled_for_restore.store(false, Ordering::SeqCst);
                    Ok(())
                },
            ),
            Err("DELETE_FAILED")
        );
        assert!(!disabled.load(Ordering::SeqCst));
        assert!(credential.load(Ordering::SeqCst));
    }

    #[test]
    fn optional_lifecycle_state_distinguishes_missing_corrupt_and_unreadable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lifecycle.json");
        assert_eq!(
            read_optional_json_file::<Value>(&path, "UNREADABLE", "CORRUPT").unwrap(),
            None
        );
        fs::write(&path, b"not-json").unwrap();
        assert_eq!(
            read_optional_json_file::<Value>(&path, "UNREADABLE", "CORRUPT"),
            Err("CORRUPT")
        );
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(
            read_optional_json_file::<Value>(&path, "UNREADABLE", "CORRUPT"),
            Err("UNREADABLE")
        );
    }

    #[tokio::test]
    async fn terminal_latch_blocks_restart_and_rotation_reservations() {
        let control = LatchedAgentControlOwnership::<()>::default();
        let generation = control.reserve_start(9876, ()).unwrap();
        control.ownership.publish_running(generation).unwrap();
        control.terminal.set();
        let (stopped_generation, _) = control.ownership.begin_stop().unwrap().unwrap();
        control.ownership.finish_stop(stopped_generation);
        assert_eq!(
            control.reserve_start(9877, ()),
            Err("APPLICATION_SHUTTING_DOWN")
        );
        assert_eq!(
            control.terminal.ensure_open(),
            Err("APPLICATION_SHUTTING_DOWN")
        );
    }

    #[tokio::test]
    async fn completed_background_tasks_are_reaped_before_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let kernel = Kernel::open(test_config(root.path(), None)).await.unwrap();
        for _ in 0..1000 {
            kernel.track_task(tokio::spawn(async {}));
        }
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if kernel.reap_background_tasks() <= 1 {
                break;
            }
        }
        let retained = kernel.reap_background_tasks();
        assert!(retained <= 1, "retained {retained} completed tasks");
        assert!(kernel.tracked_background_tasks() <= 1);
        let started = std::time::Instant::now();
        kernel.shutdown_resources().await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[tokio::test]
    async fn dropped_startup_ack_after_publish_returns_to_stopped_and_allows_restart() {
        let ownership = AgentControlOwnership::default();
        let generation = ownership.reserve_start(9876, ()).unwrap();
        ownership.publish_running(generation).unwrap();
        let _ = ownership.begin_stop().unwrap();
        assert!(ownership.finish_stop(generation));
        assert_eq!(ownership.snapshot().0, AgentControlPhase::Stopped);
        assert!(ownership.reserve_start(9877, ()).is_ok());
    }

    #[test]
    fn dropped_waiter_persists_false_only_for_matching_stopped_generation() {
        let ownership = AgentControlOwnership::default();
        let generation = ownership.reserve_start(9876, ()).unwrap();
        ownership.publish_running(generation).unwrap();
        let _ = ownership.begin_stop().unwrap();
        ownership.finish_stop(generation);
        assert!(ownership.may_persist_stopped_generation(generation));
        let newer = ownership.reserve_start(9877, ()).unwrap();
        assert!(!ownership.may_persist_stopped_generation(generation));
        assert_eq!(newer, generation + 1);
    }

    #[tokio::test]
    async fn listener_conversion_failure_runs_central_rollback_and_stops_kernel() {
        let root = tempfile::tempdir().unwrap();
        let kernel = Kernel::open(test_config(root.path(), None)).await.unwrap();
        let rolled_back = Arc::new(AtomicBool::new(false));
        let rollback_flag = rolled_back.clone();
        let rollback_kernel = kernel.clone();
        assert_eq!(
            run_post_kernel_startup(
                || async { Err("LISTENER_CONVERSION_FAILED") },
                || async move {
                    rollback_kernel.shutdown_resources().await.unwrap();
                    rollback_flag.store(true, Ordering::SeqCst);
                },
            )
            .await,
            Err("LISTENER_CONVERSION_FAILED")
        );
        assert!(rolled_back.load(Ordering::SeqCst));
        assert_eq!(kernel.reap_background_tasks(), 0);
        assert!(!kernel.maintenance_running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn resize_and_control_save_share_transaction_and_preserve_fields() {
        let transaction = Arc::new(SettingsTransaction::default());
        let state = Arc::new(tokio::sync::Mutex::new(json!({
            "enableAgentControl": false,
            "agentControlPort": 9876,
            "windowWidth": 800,
            "windowHeight": 600
        })));
        let control = {
            let transaction = transaction.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let _guard = transaction.lock().await;
                let mut value = state.lock().await;
                value["enableAgentControl"] = json!(true);
                value["agentControlPort"] = json!(9988);
            })
        };
        let resize = {
            let transaction = transaction.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let _guard = transaction.lock().await;
                let mut value = state.lock().await;
                value["windowWidth"] = json!(1440);
                value["windowHeight"] = json!(900);
            })
        };
        control.await.unwrap();
        resize.await.unwrap();
        let value = state.lock().await;
        assert_eq!(value["enableAgentControl"], true);
        assert_eq!(value["agentControlPort"], 9988);
        assert_eq!(value["windowWidth"], 1440);
        assert_eq!(value["windowHeight"], 900);
    }

    #[test]
    fn agent_control_ownership_allows_one_app_wide_start_and_shutdown_owner() {
        let ownership = Arc::new(AgentControlOwnership::default());
        let winners = Arc::new(StdMutex::new(Vec::new()));
        std::thread::scope(|scope| {
            for port in [9876, 9877, 9878, 9879] {
                let ownership = ownership.clone();
                let winners = winners.clone();
                scope.spawn(move || {
                    if let Ok(generation) = ownership.reserve_start(port, port) {
                        winners.lock().unwrap().push((generation, port));
                    }
                });
            }
        });
        let winners = winners.lock().unwrap().clone();
        assert_eq!(winners.len(), 1);
        let (generation, port) = winners[0];
        assert_eq!(
            ownership.snapshot(),
            (AgentControlPhase::Starting, generation, Some(port))
        );
        ownership.publish_running(generation).unwrap();
        let owner = ownership.begin_stop().unwrap().unwrap();
        assert_eq!(owner, (generation, port));
        assert_eq!(
            ownership.begin_stop(),
            Err("AGENT_CONTROL_STOP_IN_PROGRESS")
        );
        assert!(ownership.finish_stop(generation));
        assert_eq!(ownership.snapshot().0, AgentControlPhase::Stopped);

        let generation = ownership.reserve_start(9880, 9880).unwrap();
        let owner = ownership.begin_stop().unwrap().unwrap();
        assert_eq!(owner, (generation, 9880));
        assert_eq!(ownership.fail_start(generation), None);
        assert_eq!(ownership.snapshot().0, AgentControlPhase::Stopped);
    }

    #[test]
    fn atomic_settings_style_write_recovers_and_preserves_valid_previous_file() {
        let root = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(root.path().into());
        storage
            .write_atomic("settings.json", br#"{"enableAgentControl":false}"#)
            .unwrap();
        let fault = FileStorage::with_fault(root.path().into(), AtomicWriteFault::BeforeRename);
        assert!(fault
            .write_atomic(
                "settings.json",
                br#"{"enableAgentControl":true,"decision":"accepted"}"#
            )
            .is_err());
        let previous: Value =
            serde_json::from_slice(&fs::read(root.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(previous["enableAgentControl"], false);
        fs::rename(
            root.path().join("settings.json"),
            root.path().join("settings.bak"),
        )
        .unwrap();
        storage.recover_atomic("settings.json").unwrap();
        let recovered: Value =
            serde_json::from_slice(&fs::read(root.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(recovered["enableAgentControl"], false);
    }

    #[test]
    fn discovery_publication_and_removal_are_generation_owned() {
        let root = tempfile::tempdir().unwrap();
        let discovery = GenerationOwnedJsonFile::new(root.path().into(), "control-api.json");
        assert!(!root.path().join("control-api.json").exists());
        discovery.publish(1, json!({"port":9876})).unwrap();
        let first: Value =
            serde_json::from_slice(&fs::read(root.path().join("control-api.json")).unwrap())
                .unwrap();
        assert_eq!(first["generation"], 1);
        discovery.publish(2, json!({"port":9877})).unwrap();
        assert!(!discovery.remove_if_generation(1).unwrap());
        let second: Value =
            serde_json::from_slice(&fs::read(root.path().join("control-api.json")).unwrap())
                .unwrap();
        assert_eq!(second["generation"], 2);
        assert_eq!(second["port"], 9877);
        assert!(discovery.remove_if_generation(2).unwrap());
        assert!(!root.path().join("control-api.json").exists());
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_server_drain_times_out_without_leaking_ownership() {
        let dropped = Arc::new(AtomicBool::new(false));
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flag = dropped.clone();
        let server = tokio::spawn(async move {
            let _guard = DropFlag(flag);
            std::future::pending::<Result<(), &'static str>>().await
        });
        let wait = tokio::spawn(await_server_drain(
            server,
            std::time::Duration::from_secs(20),
        ));
        tokio::time::advance(std::time::Duration::from_secs(21)).await;
        assert_eq!(wait.await.unwrap(), Err("SERVER_DRAIN_TIMEOUT"));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_server_runs_past_drain_bound_until_signaled() {
        let (signal_tx, signal_rx) = tokio::sync::oneshot::channel();
        let server =
            tokio::spawn(async move { std::future::pending::<Result<(), &'static str>>().await });
        let wait = tokio::spawn(run_until_shutdown_then_drain(
            async move {
                let _ = signal_rx.await;
            },
            server,
            std::time::Duration::from_secs(20),
        ));
        tokio::time::advance(std::time::Duration::from_secs(25)).await;
        assert!(!wait.is_finished());
        signal_tx.send(()).unwrap();
        tokio::time::advance(std::time::Duration::from_secs(21)).await;
        assert_eq!(wait.await.unwrap(), Err("SERVER_DRAIN_TIMEOUT"));
    }

    #[tokio::test(start_paused = true)]
    async fn accepted_stalled_drain_persists_failure_and_never_exits() {
        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let kernel = Kernel::open(test_config(root.path(), Some(tx)))
            .await
            .unwrap();
        assert_eq!(
            post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                shutdown_body("drain-timeout"),
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        let server =
            tokio::spawn(async move { std::future::pending::<Result<(), &'static str>>().await });
        let wait = tokio::spawn(await_server_drain(
            server,
            std::time::Duration::from_secs(20),
        ));
        tokio::time::advance(std::time::Duration::from_secs(21)).await;
        let server_result = wait.await.unwrap();
        let exits = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(StdMutex::new(Vec::new()));
        let hooks = FakeShutdownHooks {
            events: events.clone(),
            resource_error: None,
            lifecycle_error: None,
            exits: exits.clone(),
        };
        assert_eq!(
            kernel.orchestrate_shutdown(server_result, &hooks).await,
            TeardownOutcome::Withheld("SERVER_DRAIN_TIMEOUT")
        );
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(exits.load(Ordering::SeqCst), 0);
        assert!(!kernel.is_accepting().await);
        let persisted: Value =
            serde_json::from_slice(&fs::read(root.path().join("ledger.json")).unwrap()).unwrap();
        let stage = &persisted["shutdown_actions"][0]["stages"][0];
        assert_eq!(stage["stage"], "response_drained");
        assert_eq!(stage["outcome"], "failed");
        assert_eq!(stage["failureCode"], "SERVER_DRAIN_TIMEOUT");
    }

    #[tokio::test]
    async fn active_and_ambiguous_operations_block_shutdown() {
        for (state, certainty, code) in [
            ("running", "not_submitted", "SHUTDOWN_OPERATIONS_ACTIVE"),
            (
                "lost",
                "submitted_ambiguous",
                "SHUTDOWN_RECONCILIATION_REQUIRED",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let kernel = Kernel::open(test_config(root.path(), Some(tx)))
                .await
                .unwrap();
            let now = Utc::now().to_rfc3339();
            kernel.ledger.lock().await.operations.insert(
                "blocked".into(),
                ProviderOperation {
                    schema_version: "1.0".into(),
                    provider_operation_id: "blocked".into(),
                    request_id: "r".into(),
                    idempotency_key: "k".into(),
                    request_digest: "b".repeat(64),
                    client_id: "client".into(),
                    core_operation_id: "core".into(),
                    attempt: 1,
                    assignment_revision: 1,
                    capability: CapabilityRef {
                        id: "media.models.list".into(),
                        revision: "2".into(),
                    },
                    manifest_digest: "a".repeat(64),
                    catalog_revision: None,
                    input_digest: "c".repeat(64),
                    input: json!({}),
                    input_artifacts: vec![],
                    state: state.into(),
                    submission_state: "submission_started".into(),
                    submission_certainty: certainty.into(),
                    provider_request_id: None,
                    upstream_id: None,
                    execution_requested: false,
                    progress: progress("blocked", "blocked"),
                    artifacts: vec![],
                    resource_usage: usage(0),
                    terminal_error: None,
                    result: Value::Null,
                    output: Value::Null,
                    event_sequence: 0,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    callback_url: "http://127.0.0.1/callback".into(),
                    callback_secret: None,
                    callback_expires_at: now,
                },
            );
            let response = post_shutdown(
                kernel.clone().router(),
                Some("0123456789abcdef"),
                shutdown_body(code),
            )
            .await;
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(
                kernel
                    .ledger
                    .lock()
                    .await
                    .shutdown_actions
                    .last()
                    .unwrap()
                    .reason_code,
                code
            );
            assert!(kernel.is_accepting().await);
        }
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
            provider_id: "venice-media-local".into(),
            instance_id: "vml-test".into(),
            shutdown_tx: None,
            token_scopes: BTreeSet::new(),
            admission: AdmissionController::default(),
            ownership_generation: 0,
            terminal_shutdown: Default::default(),
            shutdown_transaction: Default::default(),
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
            provider_id: "venice-media-local".into(),
            instance_id: "vml-test".into(),
            shutdown_tx: None,
            token_scopes: BTreeSet::new(),
            admission: AdmissionController::default(),
            ownership_generation: 0,
            terminal_shutdown: Default::default(),
            shutdown_transaction: Default::default(),
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
        kernel.shutdown_resources().await.unwrap();
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
            provider_id: "venice-media-local".into(),
            instance_id: "vml-test".into(),
            shutdown_tx: None,
            token_scopes: BTreeSet::new(),
            admission: AdmissionController::default(),
            ownership_generation: 0,
            terminal_shutdown: Default::default(),
            shutdown_transaction: Default::default(),
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
        kernel.shutdown_resources().await.unwrap();
    }
}
