#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use atomicwrites::{AllowOverwrite, AtomicFile};
use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose, Engine as _};
use chrono::{Local, Utc};
use futures_util::StreamExt;
use local_ip_address::list_afinet_netifas;
use reqwest::header::CONTENT_TYPE;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager, PhysicalSize, Size, WebviewWindow, WindowEvent};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tower_http::cors::CorsLayer;

mod provider;
mod provider_kernel;

const VENICE_BASE_URL: &str = "https://api.venice.ai/api/v1";
#[cfg(test)]
static TEST_APP_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
#[cfg(test)]
static TEST_VENICE_BASE_URL: OnceLock<String> = OnceLock::new();
const GITHUB_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/neko-legends/venice-media-local/releases/latest";
const KEYRING_SERVICE: &str = "venice-media-local";
const KEYRING_ACCOUNT: &str = "venice-api-key";
const AGENT_CONTROL_KEYRING_ACCOUNT: &str = "agent-control-token";
const PHASE5H_LEGACY_TOKEN_MIGRATION_ACTION: &str =
    "venice-media-local:migrate-legacy-agent-control-token";
const CAPABILITY_SCHEMA_VERSION: &str = "1.0";
const MIN_WINDOW_WIDTH: u32 = 960;
const MIN_WINDOW_HEIGHT: u32 = 540;
const FALLBACK_WINDOW_WIDTH: u32 = 1280;
const FALLBACK_WINDOW_HEIGHT: u32 = 720;
const EDIT_MODEL_PATTERNS: &[&str] = &[
    "inpaint",
    "image_edit",
    "image-edit",
    "imageedit",
    "edit_image",
    "edit-image",
    "editimage",
    "image_to_image",
    "image-to-image",
    "source_image",
    "source image",
    "reference_image",
    "reference image",
    "mask_image",
    "mask image",
];

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    theme: String,
    output_dir: String,
    #[serde(default = "default_true")]
    write_metadata_sidecars: bool,
    #[serde(default)]
    private_session: bool,
    #[serde(default)]
    generic_filenames: bool,
    #[serde(default)]
    show_diem_balance: bool,
    #[serde(default)]
    window_width: Option<u32>,
    #[serde(default)]
    window_height: Option<u32>,
    // AI Agent Remote Control (HTTP API for Hermes-style agents over Tailscale)
    #[serde(default)]
    enable_agent_control: bool,
    #[serde(default = "default_agent_control_port")]
    agent_control_port: u16,
    #[serde(default)]
    agent_control_bind_all: bool,
    #[serde(default)]
    agent_control_token: Option<String>,
    #[serde(default)]
    selected_models: BTreeMap<String, String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: "eva-dark".to_string(),
            output_dir: String::new(),
            write_metadata_sidecars: true,
            private_session: false,
            generic_filenames: false,
            show_diem_balance: false,
            window_width: None,
            window_height: None,
            enable_agent_control: false,
            agent_control_port: default_agent_control_port(),
            agent_control_bind_all: false,
            agent_control_token: None,
            selected_models: BTreeMap::new(),
        }
    }
}

fn default_settings(app: &AppHandle) -> AppSettings {
    AppSettings {
        theme: "eva-dark".to_string(),
        output_dir: default_output_dir(app).unwrap_or_default(),
        write_metadata_sidecars: true,
        private_session: false,
        generic_filenames: false,
        show_diem_balance: false,
        window_width: None,
        window_height: None,
        enable_agent_control: false,
        agent_control_port: default_agent_control_port(),
        agent_control_bind_all: false,
        agent_control_token: None,
        selected_models: BTreeMap::new(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelRecord {
    id: String,
    name: String,
    kind: String,
    modes: Vec<String>,
    controls: Value,
    raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelCache {
    last_fetched: String,
    #[serde(default)]
    catalog_source: String,
    #[serde(default)]
    category_errors: Vec<Value>,
    image_models: Vec<ModelRecord>,
    edit_models: Vec<ModelRecord>,
    video_models: Vec<ModelRecord>,
    music_models: Vec<ModelRecord>,
    sfx_models: Vec<ModelRecord>,
    voice_models: Vec<ModelRecord>,
    #[serde(default)]
    transcribe_models: Vec<ModelRecord>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppState {
    settings: AppSettings,
    key_configured: bool,
    models: ModelCache,
    build_version: String,
    agent_control_address: String,
    startup_timings: StartupTimings,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartupTimingEntry {
    name: String,
    ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartupTimings {
    setup_sections: Vec<StartupTimingEntry>,
    app_state_sections: Vec<StartupTimingEntry>,
    app_state_total_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveSettingsRequest {
    theme: Option<String>,
    output_dir: Option<String>,
    write_metadata_sidecars: Option<bool>,
    private_session: Option<bool>,
    generic_filenames: Option<bool>,
    show_diem_balance: Option<bool>,
    enable_agent_control: Option<bool>,
    agent_control_port: Option<u16>,
    agent_control_bind_all: Option<bool>,
    selected_models: Option<BTreeMap<String, String>>,
    // We do not allow the frontend to set the token directly for security
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveDataUrlRequest {
    #[serde(alias = "data_url")]
    data_url: String,
    #[serde(alias = "destination_path")]
    destination_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageGenerationRequest {
    model: String,
    title: Option<String>,
    prompt: String,
    #[serde(alias = "negative_prompt")]
    negative_prompt: Option<String>,
    #[serde(alias = "aspect_ratio")]
    aspect_ratio: Option<String>,
    resolution: Option<String>,
    variants: Option<u8>,
    steps: Option<u32>,
    #[serde(alias = "cfg_scale")]
    cfg_scale: Option<f32>,
    seed: Option<u64>,
    #[serde(alias = "hide_watermark")]
    hide_watermark: Option<bool>,
    #[serde(alias = "safe_mode")]
    safe_mode: Option<bool>,
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundRemoveRequest {
    #[serde(alias = "source_image")]
    source_image: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageUpscaleRequest {
    #[serde(alias = "source_image")]
    source_image: String,
    scale: u8,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageMultiEditRequest {
    model: String,
    prompt: String,
    images: Vec<String>,
    #[serde(alias = "aspect_ratio")]
    aspect_ratio: Option<String>,
    resolution: Option<String>,
    #[serde(alias = "safe_mode")]
    safe_mode: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueMediaRequest {
    model: String,
    prompt: String,
    #[serde(alias = "negative_prompt")]
    negative_prompt: Option<String>,
    #[serde(alias = "source_image")]
    source_image: Option<String>,
    #[serde(alias = "source_video")]
    source_video: Option<String>,
    duration: Option<String>,
    #[serde(alias = "duration_seconds")]
    duration_seconds: Option<String>,
    resolution: Option<String>,
    #[serde(alias = "aspect_ratio")]
    aspect_ratio: Option<String>,
    #[serde(alias = "upscale_factor")]
    upscale_factor: Option<u8>,
    #[serde(alias = "force_instrumental")]
    force_instrumental: Option<bool>,
    #[serde(alias = "lyrics_prompt")]
    lyrics_prompt: Option<String>,
    #[serde(alias = "lyrics_optimizer")]
    lyrics_optimizer: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetrieveRequest {
    #[serde(alias = "queue_id")]
    queue_id: String,
    model: Option<String>,
    kind: Option<String>,
    #[serde(alias = "download_url")]
    download_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpeechRequest {
    model: String,
    input: String,
    voice: Option<String>,
    speed: Option<f32>,
    language: Option<String>,
    #[serde(alias = "response_format")]
    response_format: Option<String>,
    #[serde(alias = "style_prompt")]
    style_prompt: Option<String>,
    temperature: Option<f32>,
    #[serde(alias = "top_p")]
    top_p: Option<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptionRequest {
    model: String,
    audio: String,
    #[serde(alias = "file_name")]
    file_name: Option<String>,
    #[serde(alias = "mime_type")]
    mime_type: Option<String>,
    #[serde(alias = "response_format")]
    response_format: Option<String>,
    timestamps: Option<bool>,
    language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaResult {
    id: String,
    kind: String,
    name: String,
    mime_type: String,
    data_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    file_path: String,
    metadata: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct QueueResult {
    queue_id: String,
    status: String,
    progress_label: String,
    download_url: String,
    raw: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetrieveResult {
    status: String,
    progress_label: String,
    result: Option<MediaResult>,
    raw: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BurnFolderStats {
    file_count: usize,
    total_bytes: u64,
    burn_dir: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiemBalanceSnapshot {
    success: bool,
    diem_balance: Option<f64>,
    usd_balance: Option<f64>,
    diem_epoch_allocation: Option<f64>,
    percent_remaining: Option<f64>,
    consumption_currency: Option<String>,
    can_consume: Option<bool>,
    source: String,
    timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAsset {
    name: String,
    url: String,
    size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(target_os = "windows", allow(dead_code))]
#[serde(rename_all = "camelCase")]
enum UpdateTarget {
    WindowsSetup,
    WindowsPortable,
    ReleasePage,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCheckResult {
    checked_at: String,
    current_version: String,
    latest_version: String,
    update_available: bool,
    release_name: String,
    release_url: String,
    setup_asset: Option<UpdateAsset>,
    portable_asset: Option<UpdateAsset>,
    recommended_asset: Option<UpdateAsset>,
    update_target: UpdateTarget,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    name: Option<String>,
    html_url: String,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
    size: Option<u64>,
}

// Handle for dynamically starting/stopping the agent control HTTP server
// when the user toggles the setting in the UI (no app restart needed).
struct AgentControlHandle {
    control: venice_provider_kernel::LatchedAgentControlOwnership<AgentControlOwner>,
    transaction: venice_provider_kernel::SettingsTransaction,
    admission: venice_provider_kernel::AdmissionController,
}

struct AgentControlOwner {
    shutdown: oneshot::Sender<()>,
    completion: oneshot::Receiver<Result<(), String>>,
}

impl Default for AgentControlHandle {
    fn default() -> Self {
        Self {
            control: Default::default(),
            transaction: Default::default(),
            admission: Default::default(),
        }
    }
}

#[derive(Default)]
struct StartupMetricsHandle {
    setup_sections: std::sync::Mutex<Vec<StartupTimingEntry>>,
}

impl StartupMetricsHandle {
    fn push(&self, name: &str, started_at: Instant) {
        if let Ok(mut sections) = self.setup_sections.lock() {
            sections.push(StartupTimingEntry {
                name: name.to_string(),
                ms: elapsed_ms(started_at),
            });
        }
    }

    fn sections(&self) -> Vec<StartupTimingEntry> {
        self.setup_sections
            .lock()
            .map(|sections| sections.clone())
            .unwrap_or_default()
    }
}

fn elapsed_ms(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn keyring_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).map_err(|err| err.to_string())
}

fn read_api_key() -> Result<String, String> {
    #[cfg(test)]
    if TEST_VENICE_BASE_URL.get().is_some() {
        return Ok("synthetic-test-api-key".to_string());
    }
    if let Ok(value) = std::env::var("VENICE_API_KEY") {
        let trimmed = value.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }

    let entry = keyring_entry()?;
    entry.get_password().map_err(|err| err.to_string())
}

fn has_api_key() -> bool {
    read_api_key()
        .map(|key| !key.trim().is_empty())
        .unwrap_or(false)
}

const DEFAULT_AGENT_CONTROL_PORT: u16 = 9876;
const TAILSCALE_IP_SUCCESS_CACHE_TTL: Duration = Duration::from_secs(30);
const TAILSCALE_IP_MISS_CACHE_TTL: Duration = Duration::from_secs(3);
const TAILSCALE_IP_LOOKUP_TIMEOUT: Duration = Duration::from_millis(600);
const TAILSCALE_IP_LOOKUP_POLL: Duration = Duration::from_millis(10);

#[derive(Clone)]
struct TailscaleLookupCache {
    value: Option<String>,
    checked_at: Instant,
}

static TAILSCALE_IPV4_CACHE: OnceLock<Mutex<Option<TailscaleLookupCache>>> = OnceLock::new();
static INSTALLATION_INSTANCE_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static SETTINGS_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn default_agent_control_port() -> u16 {
    DEFAULT_AGENT_CONTROL_PORT
}

fn validate_agent_control_port(port: u16) -> Result<u16, String> {
    if port == 0 {
        return Err("Agent Control port must be between 1 and 65535.".to_string());
    }
    Ok(port)
}

fn generate_agent_control_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("vl-{}", provider::hex(&bytes))
}

fn control_token_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, AGENT_CONTROL_KEYRING_ACCOUNT)
        .map_err(|err| err.to_string())
}

fn read_control_token(settings: &AppSettings) -> Result<Option<String>, String> {
    match control_token_entry()?.get_password() {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        _ => Ok(settings
            .agent_control_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)),
    }
}

fn store_control_token(token: &str) -> Result<(), String> {
    control_token_entry()?
        .set_password(token)
        .map_err(|err| err.to_string())
}

fn keyring_has_control_token() -> bool {
    control_token_entry()
        .and_then(|entry| entry.get_password().map_err(|error| error.to_string()))
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

#[derive(Debug, PartialEq)]
enum LegacyControlTokenMigration {
    AlreadySanitized,
    ExistingReplacementProven,
    ReplacementMigrated,
}

trait ControlTokenStore {
    fn read(&self) -> Result<Option<String>, String>;
    fn write(&self, value: &str) -> Result<(), String>;
}

struct WindowsControlTokenStore;

impl ControlTokenStore for WindowsControlTokenStore {
    fn read(&self) -> Result<Option<String>, String> {
        match control_token_entry()?.get_password() {
            Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
            Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn write(&self, value: &str) -> Result<(), String> {
        store_control_token(value)
    }
}

fn migrate_legacy_control_token(
    settings_path: &Path,
    store: &impl ControlTokenStore,
) -> Result<LegacyControlTokenMigration, String> {
    let bytes = fs::read(settings_path).map_err(|error| error.to_string())?;
    let mut settings: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let object = settings
        .as_object_mut()
        .ok_or_else(|| "settings.json must contain a JSON object".to_string())?;
    let legacy = object
        .get("agentControlToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let Some(legacy) = legacy else {
        return Ok(LegacyControlTokenMigration::AlreadySanitized);
    };

    let outcome = match store.read()? {
        Some(_) => LegacyControlTokenMigration::ExistingReplacementProven,
        None => {
            store.write(&legacy)?;
            match store.read()? {
                Some(replacement) if replacement == legacy => {
                    LegacyControlTokenMigration::ReplacementMigrated
                }
                _ => return Err("Secure-store replacement verification failed".to_string()),
            }
        }
    };

    object.remove("agentControlToken");
    let sanitized = serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?;
    atomic_write_bytes(settings_path, &sanitized)?;
    let verified: Value =
        serde_json::from_slice(&fs::read(settings_path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    if verified.get("agentControlToken").is_some() {
        return Err("Sanitized settings verification failed".to_string());
    }
    Ok(outcome)
}

fn validate_phase5h_migration_session(payload: &Value) -> Result<(), String> {
    let trust = payload
        .get("trust")
        .ok_or_else(|| "Verified-action trust is missing".to_string())?;
    let expires_at = trust
        .get("expiresAt")
        .and_then(Value::as_str)
        .ok_or_else(|| "Verified-action expiry is missing".to_string())?;
    let expiry = chrono::DateTime::parse_from_rfc3339(expires_at)
        .map_err(|_| "Verified-action expiry is malformed".to_string())?;
    let valid = payload.pointer("/user/id").and_then(Value::as_str) == Some("user-jun")
        && payload.pointer("/user/type").and_then(Value::as_str) == Some("human")
        && trust.get("level").and_then(Value::as_str) == Some("verified_action")
        && trust.get("needsReverification").and_then(Value::as_bool) != Some(true)
        && trust.pointer("/action/key").and_then(Value::as_str)
            == Some(PHASE5H_LEGACY_TOKEN_MIGRATION_ACTION)
        && expiry.with_timezone(&Utc) > Utc::now();
    if !valid {
        return Err("Exact current Jun migration authorization is required".to_string());
    }
    Ok(())
}

async fn run_phase5h_legacy_token_migration(
    core_url: &str,
    settings_path: &Path,
) -> Result<LegacyControlTokenMigration, String> {
    let mut authorization = String::new();
    io::stdin()
        .read_line(&mut authorization)
        .map_err(|error| error.to_string())?;
    let authorization = authorization
        .trim()
        .trim_start_matches('\u{feff}')
        .trim_start();
    if authorization.is_empty() {
        return Err("Runtime verification input is required".to_string());
    }
    let response = reqwest::Client::new()
        .get(format!(
            "{}/api/auth/session",
            core_url.trim_end_matches('/')
        ))
        .bearer_auth(authorization)
        .send()
        .await
        .map_err(|error| format!("Runtime authorization check failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Runtime authorization was rejected (HTTP {})",
            response.status().as_u16()
        ));
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|error| format!("Runtime authorization response was invalid: {error}"))?;
    validate_phase5h_migration_session(&payload)?;
    migrate_legacy_control_token(settings_path, &WindowsControlTokenStore)
}

fn try_run_phase5h_migration_cli() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    let position = args
        .iter()
        .position(|arg| arg == "--phase5h-migrate-legacy-agent-control-token")?;
    let core_url = args.get(position + 1).map(String::as_str).unwrap_or("");
    let settings_path = args.get(position + 2).map(PathBuf::from);
    if core_url.is_empty() || settings_path.is_none() {
        eprintln!("Migration requires a Core URL and settings path");
        return Some(2);
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return Some(2),
    };
    match runtime.block_on(run_phase5h_legacy_token_migration(
        core_url,
        settings_path.as_deref().expect("checked above"),
    )) {
        Ok(outcome) => {
            let state = match outcome {
                LegacyControlTokenMigration::AlreadySanitized => "already-sanitized",
                LegacyControlTokenMigration::ExistingReplacementProven => {
                    "existing-replacement-proven"
                }
                LegacyControlTokenMigration::ReplacementMigrated => "replacement-migrated",
            };
            println!("{{\"status\":\"ok\",\"migration\":\"{state}\"}}");
            Some(0)
        }
        Err(error) => {
            eprintln!("Legacy control credential migration failed: {error}");
            Some(1)
        }
    }
}

fn tailscale_cache_ttl(value: &Option<String>) -> Duration {
    if value.is_some() {
        TAILSCALE_IP_SUCCESS_CACHE_TTL
    } else {
        TAILSCALE_IP_MISS_CACHE_TTL
    }
}

fn is_tailscale_ipv4_address(ip: Ipv4Addr) -> bool {
    let [first, second, _, _] = ip.octets();
    first == 100 && (64..=127).contains(&second)
}

fn parse_tailscale_ipv4_output(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find_map(|line| line.parse::<Ipv4Addr>().ok())
        .filter(|ip| is_tailscale_ipv4_address(*ip))
        .map(|ip| ip.to_string())
}

fn tailscale_ipv4_address_from_interfaces() -> Option<String> {
    for (name, ip) in list_afinet_netifas().ok()? {
        let IpAddr::V4(ipv4) = ip else {
            continue;
        };
        if name.to_ascii_lowercase().contains("tailscale") && is_tailscale_ipv4_address(ipv4) {
            return Some(ipv4.to_string());
        }
    }

    None
}

fn tailscale_ipv4_address_from_cli() -> Option<String> {
    let mut child = Command::new("tailscale")
        .args(["ip", "-4"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started_at = Instant::now();

    loop {
        if let Some(status) = child.try_wait().ok()? {
            if !status.success() {
                return None;
            }

            let mut stdout = String::new();
            child.stdout.as_mut()?.read_to_string(&mut stdout).ok()?;
            return parse_tailscale_ipv4_output(&stdout);
        }

        if started_at.elapsed() >= TAILSCALE_IP_LOOKUP_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }

        thread::sleep(TAILSCALE_IP_LOOKUP_POLL);
    }
}

fn lookup_tailscale_ipv4_address() -> Option<String> {
    tailscale_ipv4_address_from_interfaces().or_else(tailscale_ipv4_address_from_cli)
}

fn tailscale_ipv4_address() -> Option<String> {
    let cache = TAILSCALE_IPV4_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref() {
            if cached.checked_at.elapsed() <= tailscale_cache_ttl(&cached.value) {
                return cached.value.clone();
            }
        }
    }

    let value = lookup_tailscale_ipv4_address();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(TailscaleLookupCache {
            value: value.clone(),
            checked_at: Instant::now(),
        });
    }
    value
}

fn agent_control_bind_host(bind_all: bool) -> String {
    if bind_all {
        "0.0.0.0".to_string()
    } else {
        tailscale_ipv4_address().unwrap_or_else(|| "127.0.0.1".to_string())
    }
}

fn agent_control_address(port: u16, bind_all: bool) -> String {
    let host = agent_control_bind_host(bind_all);
    if host == "0.0.0.0" {
        tailscale_ipv4_address()
            .map(|ip| format!("{ip}:{port}"))
            .unwrap_or_else(|| format!("127.0.0.1:{port}"))
    } else {
        format!("{host}:{port}")
    }
}

fn capability_machine_id() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "local-machine".to_string());
    let normalized = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '/' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let normalized = normalized.trim_matches('-').to_string();
    if normalized.is_empty() {
        "local-machine".to_string()
    } else {
        normalized
    }
}

fn installation_instance_id_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join("capability-provider-instance-id"))
}

fn is_valid_provider_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '/' | '-'))
}

fn installation_instance_id(app: &AppHandle) -> Result<String, String> {
    let cache = INSTALLATION_INSTANCE_ID.get_or_init(|| Mutex::new(None));
    let mut cached = cache
        .lock()
        .map_err(|_| "Installation instance ID lock is unavailable".to_string())?;
    if let Some(value) = cached.as_ref() {
        return Ok(value.clone());
    }

    let path = installation_instance_id_path(app)?;
    if let Ok(value) = fs::read_to_string(&path) {
        let value = value.trim();
        if is_valid_provider_id(value) {
            let value = value.to_string();
            *cached = Some(value.clone());
            return Ok(value);
        }
    }

    let instance_id = format!(
        "vml-{}",
        generate_agent_control_token().trim_start_matches("vl-")
    );
    fs::write(&path, format!("{instance_id}\n")).map_err(|err| err.to_string())?;
    *cached = Some(instance_id.clone());
    Ok(instance_id)
}

fn capability_base_url(address: &str) -> String {
    format!("http://{address}")
}

fn capability_manifest(app: &AppHandle, address: &str) -> Result<Value, String> {
    let mut manifest: Value = serde_json::from_str(include_str!("capability-manifest.v1.json"))
        .expect("embedded capability manifest must be valid JSON");
    let base_url = capability_base_url(address);
    manifest["provider"]["instanceId"] = json!(installation_instance_id(app)?);
    manifest["provider"]["machineId"] = json!(capability_machine_id());
    manifest["provider"]["version"] = json!(app.package_info().version.to_string());
    manifest["transport"]["baseUrl"] = json!(base_url);
    manifest["transport"]["manifestUrl"] = json!(format!(
        "{}/api/v1/capabilities",
        capability_base_url(address)
    ));
    Ok(manifest)
}

fn model_cache_has_usable_models(cache: &ModelCache) -> bool {
    [
        &cache.image_models,
        &cache.edit_models,
        &cache.video_models,
        &cache.music_models,
        &cache.sfx_models,
        &cache.voice_models,
        &cache.transcribe_models,
    ]
    .into_iter()
    .any(|models| models.iter().any(|model| !model.id.trim().is_empty()))
}

fn capability_health(app: &AppHandle) -> Result<Value, String> {
    let key_configured = has_api_key();
    let cached = model_cache_path(app)
        .ok()
        .filter(|path| path.is_file())
        .map(|path| read_json_file(&path, fallback_model_cache()));
    let models_loaded = cached.as_ref().is_some_and(model_cache_has_usable_models);
    let model_source = cached
        .as_ref()
        .map(|cache| match cache.catalog_source.as_str() {
            "live" => "live",
            "cached" => "cached",
            _ if models_loaded => "cached",
            _ => "fallback",
        })
        .unwrap_or("fallback");
    let operation_health = provider::operation_health(app);
    let activity = combined_activity_projection(app);
    let direct_compatibility_count = activity["activeCompatibilityDirectCount"]
        .as_u64()
        .unwrap_or(0);
    let provider_operation_count = activity["activeProviderOperationCount"]
        .as_u64()
        .unwrap_or(0);
    let total_active_count = activity["activeOperationCount"].as_u64().unwrap_or(0);
    let ledger_ready = operation_health["ready"].as_bool().unwrap_or(false);
    let artifact_writable = operation_health["artifactWritable"]
        .as_bool()
        .unwrap_or(false);
    let operations_ready = key_configured && models_loaded && ledger_ready && artifact_writable;
    let status = if operations_ready {
        "ready"
    } else if key_configured && ledger_ready && artifact_writable {
        "degraded"
    } else {
        "unavailable"
    };
    let mut degraded_reasons = Vec::new();
    if !key_configured {
        degraded_reasons.push("VENICE_CREDENTIAL_NOT_CONFIGURED");
    }
    if model_source == "fallback" {
        degraded_reasons.push("MODEL_CATALOG_FALLBACK_ONLY");
    }
    if !ledger_ready {
        degraded_reasons.push("OPERATION_LEDGER_UNAVAILABLE");
    }
    if !artifact_writable {
        degraded_reasons.push("ARTIFACT_STORE_UNAVAILABLE");
    }
    Ok(json!({
        "schemaVersion": CAPABILITY_SCHEMA_VERSION,
        "provider": {
            "id": "venice-media-local",
            "instanceId": installation_instance_id(app)?,
            "version": app.package_info().version.to_string(),
            "machineId": capability_machine_id()
        },
        "status": status,
        "observedAt": Utc::now().to_rfc3339(),
        "checks": {
            "agentControl": { "status": "ready" },
            "veniceCredential": {
                "status": if key_configured { "ready" } else { "unavailable" },
                "configured": key_configured,
                "verification": if key_configured { "configured" } else { "unknown" },
                "lastVerifiedAt": null
            },
            "models": {
                "status": if models_loaded { "ready" } else { "degraded" },
                "loaded": models_loaded || model_source == "fallback",
                "source": model_source,
                "refreshedAt": cached.as_ref().and_then(|cache| if cache.last_fetched.is_empty() { None } else { Some(cache.last_fetched.clone()) })
            },
            "operations": {
                "status": if operations_ready { "ready" } else { "unavailable" },
                "ready": operations_ready
            },
            "operationLedger": { "status": if ledger_ready { "ready" } else { "unavailable" } },
            "callbackOutbox": { "status": if operation_health["callbackDegradedCount"].as_u64().unwrap_or(0) > 0 { "degraded" } else { "ready" }, "pendingCount": operation_health["pendingCallbackCount"] },
            "artifactStore": { "status": if artifact_writable { "ready" } else { "unavailable" }, "writable": artifact_writable },
            "disk": { "status": "degraded", "availableBytes": null, "reason": "DISK_CAPACITY_UNKNOWN" }
        },
        "lifecycle": provider::lifecycle_health(app),
        "activeOperationCount": total_active_count,
        "activeProviderOperationCount": provider_operation_count,
        "activeCompatibilityDirectCount": direct_compatibility_count,
        "degradedReasons": degraded_reasons,
        "version": app.package_info().version.to_string()
    }))
}

fn combined_activity_projection(app: &AppHandle) -> Value {
    let provider = provider::operation_health(app)["activeOperationCount"]
        .as_u64()
        .unwrap_or(0);
    let compatibility_direct = app
        .state::<AgentControlHandle>()
        .admission
        .active_work_count() as u64;
    json!({
        "activeOperationCount": provider.saturating_add(compatibility_direct),
        "activeProviderOperationCount": provider,
        "activeCompatibilityDirectCount": compatibility_direct
    })
}

/// Write the control-api.json discovery file so agents / the skill can auto-discover
/// the address, port and token without manual config.
fn write_agent_control_discovery(
    app: &AppHandle,
    token: &str,
    port: u16,
    bind_all: bool,
    generation: u64,
) -> Result<(), String> {
    let dir = app_data_dir(app)?;
    let tailscale_ip = tailscale_ipv4_address();
    let bind_host = agent_control_bind_host(bind_all);
    let address = if bind_host == "0.0.0.0" {
        tailscale_ip
            .as_ref()
            .map(|ip| format!("{ip}:{port}"))
            .unwrap_or_else(|| format!("127.0.0.1:{port}"))
    } else {
        format!("{bind_host}:{port}")
    };
    let discovery = serde_json::json!({
        "address": address,
        "bindAddress": format!("{bind_host}:{port}"),
        "bindAll": bind_all,
        "tailscaleIp": tailscale_ip,
        "port": port,
        "credentialId": format!("sha256:{}", &provider::sha256_hex(token.as_bytes())[..16]),
        "version": app.package_info().version.to_string(),
        "manifestUrl": format!("{}/api/v1/capabilities", capability_base_url(&address)),
        "healthUrl": format!("{}/api/v1/health", capability_base_url(&address)),
        "schemaVersions": [CAPABILITY_SCHEMA_VERSION],
        "note": "Connect using the address and a separately provisioned credential. Same Tailscale network recommended."
    });
    venice_provider_kernel::GenerationOwnedJsonFile::new(dir, "control-api.json")
        .publish(generation, discovery)?;
    Ok(())
}

fn remove_agent_control_discovery(app: &AppHandle, generation: u64) -> Result<(), String> {
    venice_provider_kernel::GenerationOwnedJsonFile::new(app_data_dir(app)?, "control-api.json")
        .remove_if_generation(generation)?;
    Ok(())
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(dir) = TEST_APP_DATA_DIR.get() {
        fs::create_dir_all(dir).map_err(|err| err.to_string())?;
        return Ok(dir.clone());
    }
    let dir = app.path().app_data_dir().map_err(|err| err.to_string())?;
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    Ok(dir)
}

fn default_output_dir(app: &AppHandle) -> Result<String, String> {
    #[cfg(test)]
    if let Some(dir) = TEST_APP_DATA_DIR.get() {
        return Ok(dir.join("output").to_string_lossy().to_string());
    }
    let desktop = app.path().desktop_dir().map_err(|err| err.to_string())?;
    Ok(desktop.join("VeniceMedia").to_string_lossy().to_string())
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join("settings.json"))
}

fn model_cache_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join("venice-models.json"))
}

fn read_json_file<T>(path: &Path, fallback: T) -> T
where
    T: for<'de> Deserialize<'de>,
{
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or(fallback),
        Err(_) => fallback,
    }
}

fn write_json_file<T>(path: &Path, value: &T) -> Result<(), String>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let raw = serde_json::to_string_pretty(value).map_err(|err| err.to_string())?;
    fs::write(path, raw).map_err(|err| err.to_string())
}

fn read_settings(app: &AppHandle) -> AppSettings {
    let fallback = default_settings(app);
    if let Ok(root) = app_data_dir(app) {
        let storage = venice_provider_kernel::FileStorage::new(root);
        let _ = venice_provider_kernel::Storage::recover_atomic(&storage, "settings.json");
    }
    let mut settings = settings_path(app)
        .map(|path| read_json_file(&path, fallback.clone()))
        .unwrap_or_else(|_| fallback.clone());
    if settings.output_dir.trim().is_empty() {
        settings.output_dir = fallback.output_dir;
    }
    if settings.agent_control_port == 0 {
        settings.agent_control_port = default_agent_control_port();
    }
    settings
}

fn update_settings_file(
    app: &AppHandle,
    update: impl FnOnce(&mut AppSettings),
) -> Result<AppSettings, String> {
    let _guard = SETTINGS_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Settings write lock is unavailable".to_string())?;
    let mut settings = read_settings(app);
    update(&mut settings);
    save_settings_file_unlocked(app, &settings)?;
    Ok(settings)
}

async fn persist_agent_control_disabled_for_generation(
    app: &AppHandle,
    generation: u64,
) -> Result<(), String> {
    let handle = app.state::<AgentControlHandle>();
    // A synchronous Settings caller may still own this barrier and will perform
    // its own rollback. Detached startup cancellation uses this path once the
    // caller has released the barrier.
    let Ok(_transaction) = handle.transaction.try_lock() else {
        return Ok(());
    };
    if !handle
        .control
        .ownership
        .may_persist_stopped_generation(generation)
    {
        return Ok(());
    }
    update_settings_file(app, |settings| settings.enable_agent_control = false).map(|_| ())
}

fn save_settings_file(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let _guard = SETTINGS_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Settings write lock is unavailable".to_string())?;
    save_settings_file_unlocked(app, settings)
}

fn save_settings_file_unlocked(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let root = app_data_dir(app)?;
    let mut persisted = settings.clone();
    if keyring_has_control_token() {
        persisted.agent_control_token = None;
    }
    let bytes = serde_json::to_vec_pretty(&persisted).map_err(|error| error.to_string())?;
    let storage = venice_provider_kernel::FileStorage::new(root);
    venice_provider_kernel::Storage::recover_atomic(&storage, "settings.json")?;
    venice_provider_kernel::Storage::write_atomic(&storage, "settings.json", &bytes)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn force_private_session_off_on_launch(app: &AppHandle) {
    if let Err(err) = update_settings_file(app, |settings| settings.private_session = false) {
        eprintln!("[settings] Failed to reset launch state: {err}");
    }
}

fn start_saved_agent_control_on_launch(app: AppHandle, _handle: &AgentControlHandle) {
    tauri::async_runtime::spawn(async move {
        let handle = app.state::<AgentControlHandle>();
        let _transaction = handle.transaction.lock().await;
        if handle.control.terminal.ensure_open().is_err() {
            return;
        }
        let mut settings = read_settings(&app);
        if !settings.enable_agent_control {
            return;
        }
        let token = match read_control_token(&settings) {
            Ok(Some(token)) => token,
            Ok(None) => {
                let token = generate_agent_control_token();
                if store_control_token(&token).is_err() {
                    settings.enable_agent_control = false;
                    let _ = save_settings_file(&app, &settings);
                    return;
                }
                token
            }
            Err(_) => {
                settings.enable_agent_control = false;
                let _ = save_settings_file(&app, &settings);
                return;
            }
        };
        let port = settings.agent_control_port;
        let bind_all = settings.agent_control_bind_all;
        if let Err(err) = start_agent_control_server(app.clone(), token, None, port, bind_all).await
        {
            let _ = update_settings_file(&app, |settings| settings.enable_agent_control = false);
            eprintln!("[agent-control] Failed to restore saved Agent Control state: {err}");
        }
    });
}

fn clamp_window_dimension(value: u32, min: u32, monitor_max: u32) -> u32 {
    let max = monitor_max.max(1);
    value.max(min.min(max)).min(max)
}

fn preferred_window_size(
    settings: &AppSettings,
    monitor_size: Option<PhysicalSize<u32>>,
) -> PhysicalSize<u32> {
    let monitor_width = monitor_size
        .as_ref()
        .map(|size| size.width)
        .unwrap_or(FALLBACK_WINDOW_WIDTH);
    let monitor_height = monitor_size
        .as_ref()
        .map(|size| size.height)
        .unwrap_or(FALLBACK_WINDOW_HEIGHT);

    let width = settings
        .window_width
        .unwrap_or_else(|| monitor_width.saturating_div(2).max(1));
    let height = settings
        .window_height
        .unwrap_or_else(|| monitor_height.saturating_div(2).max(1));

    PhysicalSize::new(
        clamp_window_dimension(width, MIN_WINDOW_WIDTH, monitor_width),
        clamp_window_dimension(height, MIN_WINDOW_HEIGHT, monitor_height),
    )
}

fn apply_initial_window_size(app: &AppHandle, window: &WebviewWindow) -> Result<(), String> {
    let settings = read_settings(app);
    let monitor_size = window
        .current_monitor()
        .map_err(|err| err.to_string())?
        .or_else(|| window.primary_monitor().ok().flatten())
        .map(|monitor| *monitor.size());
    let size = preferred_window_size(&settings, monitor_size);

    window
        .set_size(Size::Physical(size))
        .map_err(|err| err.to_string())?;
    let _ = window.center();

    Ok(())
}

async fn persist_window_size(app: &AppHandle, size: PhysicalSize<u32>) -> Result<(), String> {
    if size.width == 0 || size.height == 0 {
        return Ok(());
    }

    let handle = app.state::<AgentControlHandle>();
    let _transaction = handle.transaction.lock().await;
    update_settings_file(app, |settings| {
        settings.window_width = Some(size.width);
        settings.window_height = Some(size.height);
    })
    .map(|_| ())
}

fn fallback_model_cache() -> ModelCache {
    let model = |id: &str, name: &str, kind: &str, mode: &str, controls: Value| ModelRecord {
        id: id.to_string(),
        name: name.to_string(),
        kind: kind.to_string(),
        modes: vec![mode.to_string()],
        controls,
        raw: Value::Null,
    };

    ModelCache {
        last_fetched: String::new(),
        catalog_source: "fallback".to_string(),
        category_errors: Vec::new(),
        image_models: vec![
            model(
                "gpt-image-2",
                "GPT Image 2",
                "image",
                "generate-image",
                image_controls_with_resolutions(&["1K", "2K", "4K"]),
            ),
            model(
                "flux-2-max",
                "Flux 2 Max",
                "image",
                "generate-image",
                image_controls(),
            ),
            model(
                "qwen-image-2",
                "Qwen Image 2",
                "image",
                "generate-image",
                image_controls(),
            ),
        ],
        edit_models: vec![
            model(
                "firered-image-edit",
                "Firered Image Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("firered-image-edit", "Firered Image Edit"),
            ),
            model(
                "qwen-edit",
                "Qwen Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("qwen-edit", "Qwen Edit"),
            ),
            model(
                "grok-imagine-edit",
                "Grok Imagine Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("grok-imagine-edit", "Grok Imagine Edit"),
            ),
            model(
                "grok-imagine-quality-edit",
                "Grok Imagine Quality Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("grok-imagine-quality-edit", "Grok Imagine Quality Edit"),
            ),
            model(
                "qwen-image-2-edit",
                "Qwen Image 2 Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("qwen-image-2-edit", "Qwen Image 2 Edit"),
            ),
            model(
                "qwen-image-2-pro-edit",
                "Qwen Image 2 Pro Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("qwen-image-2-pro-edit", "Qwen Image 2 Pro Edit"),
            ),
            model(
                "wan-2-7-pro-edit",
                "Wan 2.7 Pro Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("wan-2-7-pro-edit", "Wan 2.7 Pro Edit"),
            ),
            model(
                "flux-2-max-edit",
                "Flux 2 Max Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("flux-2-max-edit", "Flux 2 Max Edit"),
            ),
            model(
                "gpt-image-2-edit",
                "GPT Image 2 Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("gpt-image-2-edit", "GPT Image 2 Edit"),
            ),
            model(
                "gpt-image-1-5-edit",
                "GPT Image 1.5 Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("gpt-image-1-5-edit", "GPT Image 1.5 Edit"),
            ),
            model(
                "nano-banana-2-edit",
                "Nano Banana 2 Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("nano-banana-2-edit", "Nano Banana 2 Edit"),
            ),
            model(
                "nano-banana-pro-edit",
                "Nano Banana Pro Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("nano-banana-pro-edit", "Nano Banana Pro Edit"),
            ),
            model(
                "seedream-v5-lite-edit",
                "Seedream v5 Lite Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("seedream-v5-lite-edit", "Seedream v5 Lite Edit"),
            ),
            model(
                "seedream-v4-edit",
                "Seedream v4 Edit",
                "edit",
                "edit-image",
                edit_controls_for_model("seedream-v4-edit", "Seedream v4 Edit"),
            ),
        ],
        video_models: vec![
            model(
                "seedance-2-0-image-to-video",
                "Seedance 2.0 (I2V)",
                "video",
                "generate-video",
                video_controls(),
            ),
            model(
                "seedance-2-0-text-to-video",
                "Seedance 2.0 (T2V)",
                "video",
                "generate-video",
                video_controls(),
            ),
            model(
                "wan-2-7-image-to-video",
                "Wan 2.7 (I2V)",
                "video",
                "generate-video",
                video_controls(),
            ),
        ],
        music_models: vec![
            model(
                "elevenlabs-music",
                "ElevenLabs Music",
                "music",
                "generate-music",
                audio_controls("music"),
            ),
            model(
                "stable-audio-25",
                "Stable Audio 2.5",
                "music",
                "generate-music",
                audio_controls_with_support("music", false, false, false, true),
            ),
        ],
        sfx_models: vec![model(
            "elevenlabs-sound-effects-v2",
            "ElevenLabs Sound Effects",
            "sfx",
            "generate-sfx",
            audio_controls("sfx"),
        )],
        voice_models: vec![
            model(
                "tts-kokoro",
                "Kokoro TTS",
                "voice",
                "generate-voice",
                voice_controls(Value::Array(vec![])),
            ),
            model(
                "tts-chatterbox-hd",
                "Chatterbox HD",
                "voice",
                "generate-voice",
                voice_controls(Value::Array(vec![])),
            ),
            model(
                "tts-xai-v1",
                "xAI TTS",
                "voice",
                "generate-voice",
                voice_controls(Value::Array(vec![])),
            ),
        ],
        transcribe_models: vec![
            model(
                "fal-ai/wizper",
                "fal.ai Wizper",
                "transcribe",
                "transcribe-audio",
                transcribe_controls(true, true),
            ),
            model(
                "nvidia/parakeet-tdt-0.6b-v3",
                "NVIDIA Parakeet TDT 0.6B v3",
                "transcribe",
                "transcribe-audio",
                transcribe_controls(false, true),
            ),
            model(
                "openai/whisper-large-v3",
                "Whisper Large v3",
                "transcribe",
                "transcribe-audio",
                transcribe_controls(true, true),
            ),
            model(
                "stt-xai-v1",
                "xAI STT v1",
                "transcribe",
                "transcribe-audio",
                transcribe_controls(true, true),
            ),
            model(
                "elevenlabs/scribe-v2",
                "ElevenLabs Scribe v2",
                "transcribe",
                "transcribe-audio",
                transcribe_controls(true, true),
            ),
        ],
    }
}

fn read_model_cache(app: &AppHandle) -> ModelCache {
    let mut cache = match model_cache_path(app) {
        Ok(path) => read_json_file(&path, fallback_model_cache()),
        Err(_) => fallback_model_cache(),
    };
    apply_model_fallbacks(&mut cache);
    cache
}

fn apply_model_fallbacks(cache: &mut ModelCache) {
    let fallback = fallback_model_cache();

    if cache.image_models.is_empty() {
        cache.image_models = fallback.image_models;
    }
    if cache.edit_models.is_empty() {
        cache.edit_models = fallback.edit_models.clone();
    } else {
        append_missing_models(&mut cache.edit_models, &fallback.edit_models);
    }
    if cache.video_models.is_empty() {
        cache.video_models = fallback.video_models;
    }
    if cache.music_models.is_empty() {
        cache.music_models = fallback.music_models;
    }
    if cache.sfx_models.is_empty() {
        cache.sfx_models = fallback.sfx_models;
    }
    if cache.voice_models.is_empty() {
        cache.voice_models = fallback.voice_models;
    }
    if cache.transcribe_models.is_empty() {
        cache.transcribe_models = fallback.transcribe_models;
    }

    for model in &mut cache.image_models {
        apply_known_image_resolution_controls(model);
    }
    for model in &mut cache.edit_models {
        apply_known_image_resolution_controls(model);
    }
    for model in &mut cache.video_models {
        apply_video_constraint_controls(model);
    }
    for model in &mut cache.music_models {
        apply_audio_support_controls(model);
    }
    for model in &mut cache.sfx_models {
        apply_audio_support_controls(model);
    }
}

fn append_missing_models(target: &mut Vec<ModelRecord>, fallback: &[ModelRecord]) {
    let mut seen = target
        .iter()
        .map(|model| model.id.clone())
        .collect::<HashSet<_>>();
    for model in fallback {
        if seen.insert(model.id.clone()) {
            target.push(model.clone());
        }
    }
}

fn save_model_cache(app: &AppHandle, cache: &ModelCache) -> Result<(), String> {
    let path = model_cache_path(app)?;
    write_json_file(&path, cache)
}

fn image_controls() -> Value {
    json!({
        "negativePrompt": true,
        "steps": true,
        "cfg": true,
        "seed": true,
        "hideWatermark": true,
        "variantCount": { "min": 1, "max": 4 },
        "sizeOptions": ["1:1", "4:3", "3:4", "16:9", "9:16"]
    })
}

fn image_controls_with_resolutions(resolutions: &[&str]) -> Value {
    let mut controls = image_controls();
    controls["resolutionOptions"] = json!(resolutions);
    controls
}

fn edit_controls_for_model(id: &str, name: &str) -> Value {
    let mut controls = json!({
        "variantCount": { "min": 1, "max": 1 }
    });
    let resolution_options = known_image_resolution_options(id, name);
    if !resolution_options.is_empty() {
        controls["resolutionOptions"] = json!(resolution_options);
    }
    controls
}

fn video_controls() -> Value {
    json!({
        "durationOptions": ["5s", "10s"],
        "resolutionOptions": ["480p", "720p", "1080p"],
        "aspectRatioOptions": ["16:9", "9:16", "1:1"],
        "supportsSourceImage": true,
        "supportsTextToVideo": true
    })
}

fn audio_controls(kind: &str) -> Value {
    audio_controls_with_support(
        kind,
        kind == "music",
        kind == "music",
        kind == "music",
        true,
    )
}

fn audio_controls_with_support(
    kind: &str,
    supports_lyrics: bool,
    supports_instrumental: bool,
    supports_lyrics_optimizer: bool,
    supports_duration_seconds: bool,
) -> Value {
    let duration_max = if kind == "sfx" { 22 } else { 180 };
    let duration_default = if kind == "sfx" { 2 } else { 30 };
    json!({
        "audioKind": kind,
        "durationSeconds": { "min": 1, "max": duration_max, "default": duration_default },
        "supportsDurationSeconds": supports_duration_seconds,
        "supportsLyrics": supports_lyrics,
        "supportsInstrumental": supports_instrumental,
        "supportsLyricsOptimizer": supports_lyrics_optimizer
    })
}

fn voice_controls(voices: Value) -> Value {
    json!({
        "supportsVoice": true,
        "supportsSpeed": true,
        "supportsLanguage": true,
        "supportsStylePrompt": true,
        "supportsResponseFormat": true,
        "responseFormats": ["mp3", "opus", "aac", "flac", "wav", "pcm"],
        "voices": voices
    })
}

fn transcribe_controls(supports_language: bool, supports_timestamps: bool) -> Value {
    json!({
        "supportsLanguage": supports_language,
        "supportsTimestamps": supports_timestamps,
        "responseFormats": ["json", "text"],
        "defaultResponseFormat": "json"
    })
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("venice-media-local/0.1")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| format!("Failed to initialize hardened HTTP client: {err}"))
}

async fn venice_get(path: &str) -> Result<reqwest::Response, String> {
    let key = read_api_key().map_err(|_| "Venice API key is not configured".to_string())?;
    let url = format!("{}{path}", venice_base_url());
    let response = client()?
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    ensure_success(response).await
}

async fn venice_post_json(path: &str, body: Value) -> Result<reqwest::Response, String> {
    let key = read_api_key().map_err(|_| "Venice API key is not configured".to_string())?;
    let url = format!("{}{path}", venice_base_url());
    let response = client()?
        .post(url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    ensure_success(response).await
}

fn venice_base_url() -> &'static str {
    #[cfg(test)]
    if let Some(value) = TEST_VENICE_BASE_URL.get() {
        return value;
    }
    VENICE_BASE_URL
}

async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response, String> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let text = bounded_response_bytes(response, 64 * 1024)
        .await
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    Err(format!(
        "Venice API returned {status}: {}",
        trim_error_text(&text)
    ))
}

async fn bounded_response_bytes(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err("HTTP response exceeds its configured bound".to_string());
    }
    let mut bytes =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err("HTTP response exceeds its configured bound".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn bounded_response_json<T: DeserializeOwned>(
    response: reqwest::Response,
    limit: usize,
) -> Result<T, String> {
    serde_json::from_slice(&bounded_response_bytes(response, limit).await?)
        .map_err(|error| error.to_string())
}

fn trim_error_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() > 500 {
        format!("{}...", &trimmed[..500])
    } else if trimmed.is_empty() {
        "request failed".to_string()
    } else {
        trimmed.to_string()
    }
}

fn version_parts(value: &str) -> Vec<u64> {
    value
        .trim()
        .trim_start_matches('v')
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

fn compare_versions(current: &str, latest: &str) -> Ordering {
    let current = version_parts(current);
    let latest = version_parts(latest);
    let len = current.len().max(latest.len()).max(1);
    for index in 0..len {
        let current_part = *current.get(index).unwrap_or(&0);
        let latest_part = *latest.get(index).unwrap_or(&0);
        match current_part.cmp(&latest_part) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    Ordering::Equal
}

fn clean_release_version(value: &str) -> String {
    value.trim().trim_start_matches('v').trim().to_string()
}

fn update_asset_from_github(asset: &GithubReleaseAsset) -> UpdateAsset {
    UpdateAsset {
        name: asset.name.clone(),
        url: asset.browser_download_url.clone(),
        size: asset.size,
    }
}

#[cfg(target_os = "windows")]
fn has_sibling_uninstaller(exe_path: &Path) -> bool {
    exe_path
        .parent()
        .and_then(|parent| fs::read_dir(parent).ok())
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                name.ends_with(".exe") && name.contains("uninstall")
            })
        })
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn is_running_portable_windows() -> bool {
    let exe_path = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return true,
    };
    let file_name = exe_path
        .file_name()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if file_name.contains("portable") {
        return true;
    }
    if has_sibling_uninstaller(&exe_path) {
        return false;
    }

    let normalized_path = exe_path.to_string_lossy().replace('/', "\\").to_lowercase();
    if normalized_path.contains("\\target\\release\\")
        || normalized_path.contains("\\bundle\\nsis\\")
    {
        return true;
    }
    if normalized_path.contains("\\program files\\")
        || normalized_path.contains("\\program files (x86)\\")
        || normalized_path.contains("\\appdata\\local\\programs\\")
    {
        return false;
    }

    true
}

fn current_update_target() -> UpdateTarget {
    #[cfg(target_os = "windows")]
    {
        if is_running_portable_windows() {
            UpdateTarget::WindowsPortable
        } else {
            UpdateTarget::WindowsSetup
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        UpdateTarget::ReleasePage
    }
}

fn format_number_for_message(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        value.to_string()
    }
}

fn as_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn number_like(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn bool_like(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::String(text)) => match text.trim().to_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn bool_field(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| bool_like(value.get(*key)))
}

fn model_bool_field(
    entry: &Value,
    spec: &Value,
    constraints: &Value,
    capabilities: &Value,
    keys: &[&str],
) -> Option<bool> {
    [entry, spec, constraints, capabilities]
        .into_iter()
        .find_map(|value| bool_field(value, keys))
}

fn number_field(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| number_like(value.get(*key)))
}

fn model_number_field(
    entry: &Value,
    spec: &Value,
    constraints: &Value,
    capabilities: &Value,
    keys: &[&str],
) -> Option<f64> {
    [entry, spec, constraints, capabilities]
        .into_iter()
        .find_map(|value| number_field(value, keys))
}

fn has_duration_metadata(value: &Value) -> bool {
    number_like(value.get("default_duration")).is_some()
        || number_like(value.get("min_duration")).is_some()
        || number_like(value.get("max_duration")).is_some()
        || matches!(value.get("duration_options"), Some(Value::Array(items)) if !items.is_empty())
        || matches!(value.get("durations"), Some(Value::Array(items)) if !items.is_empty())
        || value.get("duration_seconds").is_some()
        || value
            .get("pricing")
            .and_then(|pricing| pricing.get("durations"))
            .is_some()
}

fn model_supports_duration_seconds(
    entry: &Value,
    spec: &Value,
    constraints: &Value,
    capabilities: &Value,
) -> bool {
    model_bool_field(
        entry,
        spec,
        constraints,
        capabilities,
        &["supports_duration_seconds", "supports_duration"],
    )
    .unwrap_or_else(|| {
        [entry, spec, constraints, capabilities]
            .into_iter()
            .any(has_duration_metadata)
    })
}

#[derive(Debug, Clone, Copy)]
struct DurationControls {
    min: Option<f64>,
    max: Option<f64>,
    default: Option<f64>,
}

fn audio_duration_controls_from_raw(raw: &Value) -> DurationControls {
    if raw.is_null() {
        return DurationControls {
            min: None,
            max: None,
            default: None,
        };
    }

    let spec = raw.get("model_spec").unwrap_or(&Value::Null);
    let constraints = spec.get("constraints").unwrap_or(&Value::Null);
    let capabilities = spec.get("capabilities").unwrap_or(&Value::Null);
    DurationControls {
        min: model_number_field(
            raw,
            spec,
            constraints,
            capabilities,
            &[
                "min_duration",
                "min_duration_seconds",
                "minimum_duration_seconds",
            ],
        ),
        max: model_number_field(
            raw,
            spec,
            constraints,
            capabilities,
            &[
                "max_duration",
                "max_duration_seconds",
                "maximum_duration_seconds",
            ],
        ),
        default: model_number_field(
            raw,
            spec,
            constraints,
            capabilities,
            &["default_duration", "default_duration_seconds"],
        ),
    }
}

fn audio_support_controls_from_raw(
    kind: &str,
    id: &str,
    name: &str,
    raw: &Value,
) -> (bool, bool, bool, bool) {
    let spec = raw.get("model_spec").unwrap_or(&Value::Null);
    let constraints = spec.get("constraints").unwrap_or(&Value::Null);
    let capabilities = spec.get("capabilities").unwrap_or(&Value::Null);
    let label_id = if id.trim().is_empty() {
        as_string(raw, "id")
    } else {
        id.to_string()
    };
    let label_name = if name.trim().is_empty() {
        as_string(spec, "name")
    } else {
        name.to_string()
    };
    let label = format!("{} {}", label_id.to_lowercase(), label_name.to_lowercase());
    let fallback_music_lyrics = kind == "music" && !label.contains("stable-audio");
    let supports_lyrics = model_bool_field(
        raw,
        spec,
        constraints,
        capabilities,
        &["supports_lyrics", "lyrics_supported"],
    )
    .unwrap_or(fallback_music_lyrics);
    let supports_instrumental = model_bool_field(
        raw,
        spec,
        constraints,
        capabilities,
        &["supports_force_instrumental", "supports_instrumental"],
    )
    .unwrap_or(kind == "music" && !label.contains("stable-audio"));
    let supports_lyrics_optimizer = model_bool_field(
        raw,
        spec,
        constraints,
        capabilities,
        &["supports_lyrics_optimizer", "lyrics_optimizer_supported"],
    )
    .unwrap_or(kind == "music" && !label.contains("stable-audio"));
    let supports_duration_seconds = if raw.is_null() {
        true
    } else {
        model_supports_duration_seconds(raw, spec, constraints, capabilities)
    };

    (
        supports_lyrics,
        supports_instrumental,
        supports_lyrics_optimizer,
        supports_duration_seconds,
    )
}

fn apply_audio_support_controls(model: &mut ModelRecord) {
    if model.kind != "music" && model.kind != "sfx" {
        return;
    }

    let (supports_lyrics, supports_instrumental, supports_lyrics_optimizer, supports_duration) =
        audio_support_controls_from_raw(&model.kind, &model.id, &model.name, &model.raw);
    let duration_controls = audio_duration_controls_from_raw(&model.raw);

    if !model.controls.is_object() {
        model.controls = json!({});
    }
    if let Some(controls) = model.controls.as_object_mut() {
        controls.insert("audioKind".to_string(), json!(model.kind));
        controls.insert("supportsLyrics".to_string(), json!(supports_lyrics));
        controls.insert(
            "supportsInstrumental".to_string(),
            json!(supports_instrumental),
        );
        controls.insert(
            "supportsLyricsOptimizer".to_string(),
            json!(supports_lyrics_optimizer),
        );
        controls.insert(
            "supportsDurationSeconds".to_string(),
            json!(supports_duration),
        );
        if supports_duration {
            let duration = controls
                .entry("durationSeconds".to_string())
                .or_insert_with(|| json!({}));
            if let Some(duration) = duration.as_object_mut() {
                if let Some(min) = duration_controls.min {
                    duration.insert("min".to_string(), json!(min));
                }
                if let Some(max) = duration_controls.max {
                    duration.insert("max".to_string(), json!(max));
                }
                if let Some(default) = duration_controls.default {
                    duration.insert("default".to_string(), json!(default));
                }
            }
        }
    }
}

fn audio_model_controls<'a>(cache: &'a ModelCache, model_id: &str) -> Option<&'a Value> {
    cache
        .music_models
        .iter()
        .chain(cache.sfx_models.iter())
        .find(|model| model.id == model_id)
        .map(|model| &model.controls)
}

fn controls_duration_number(controls: &Value, key: &str) -> Option<f64> {
    controls
        .get("durationSeconds")
        .and_then(|duration| number_like(duration.get(key)))
}

fn audio_model_parameter_support(app: &AppHandle, model_id: &str) -> (bool, bool, bool, bool) {
    let cache = read_model_cache(app);
    audio_model_controls(&cache, model_id)
        .map(|controls| {
            (
                bool_like(controls.get("supportsDurationSeconds")).unwrap_or(true),
                bool_like(controls.get("supportsLyrics")).unwrap_or(false),
                bool_like(controls.get("supportsInstrumental")).unwrap_or(false),
                bool_like(controls.get("supportsLyricsOptimizer")).unwrap_or(false),
            )
        })
        .unwrap_or((true, true, true, true))
}

fn audio_model_duration_limits(app: &AppHandle, model_id: &str) -> (Option<f64>, Option<f64>) {
    let cache = read_model_cache(app);
    audio_model_controls(&cache, model_id)
        .map(|controls| {
            (
                controls_duration_number(controls, "min"),
                controls_duration_number(controls, "max"),
            )
        })
        .unwrap_or((None, None))
}

fn response_data(payload: &Value) -> &Value {
    payload
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(payload)
}

fn balance_number(balances: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| number_like(balances.get(*key)))
        .filter(|value| value.is_finite())
}

fn option_value_string(value: &Value) -> Option<String> {
    match value {
        Value::String(entry) => {
            let trimmed = entry.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Number(number) => Some(number.to_string()),
        Value::Object(map) => ["value", "label", "id", "name"]
            .into_iter()
            .find_map(|key| option_value_string(map.get(key)?)),
        _ => None,
    }
}

fn unique_options(options: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    options
        .into_iter()
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .filter(|entry| seen.insert(entry.to_lowercase()))
        .collect()
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(option_value_string).collect(),
        Some(Value::Object(map)) => {
            for key in ["options", "values", "allowed", "enum", "items"] {
                let entries = string_array(map.get(key));
                if !entries.is_empty() {
                    return entries;
                }
            }
            Vec::new()
        }
        Some(value) => option_value_string(value).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn first_string_array(value: &Value, keys: &[&str]) -> Vec<String> {
    for key in keys {
        let entries = string_array(value.get(key));
        if !entries.is_empty() {
            return entries;
        }
    }
    Vec::new()
}

fn first_options_from_sources(sources: &[&Value], keys: &[&str]) -> Vec<String> {
    sources
        .iter()
        .find_map(|source| {
            let entries = first_string_array(source, keys);
            (!entries.is_empty()).then_some(entries)
        })
        .unwrap_or_default()
}

fn first_option_from_sources(sources: &[&Value], keys: &[&str]) -> Option<String> {
    sources.iter().find_map(|source| {
        keys.iter()
            .find_map(|key| source.get(*key).and_then(option_value_string))
    })
}

fn bool_from_sources(sources: &[&Value], keys: &[&str]) -> Option<bool> {
    sources.iter().find_map(|source| bool_field(source, keys))
}

fn normalize_duration_option(entry: String) -> Option<String> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_lowercase();
    if lower == "auto" {
        return Some("Auto".to_string());
    }

    let number_text = [" seconds", " second", " secs", " sec"]
        .into_iter()
        .find_map(|suffix| {
            lower
                .strip_suffix(suffix)
                .map(|_| &trimmed[..trimmed.len() - suffix.len()])
        })
        .or_else(|| {
            if lower.ends_with('s') && !lower.ends_with("ms") {
                Some(&trimmed[..trimmed.len() - 1])
            } else {
                None
            }
        })
        .unwrap_or(trimmed)
        .trim();

    if let Ok(seconds) = number_text.parse::<f64>() {
        if seconds.is_finite() && seconds >= 0.0 {
            return Some(format!("{}s", format_number_for_message(seconds)));
        }
    }

    Some(trimmed.to_string())
}

fn normalize_duration_options(options: Vec<String>) -> Vec<String> {
    unique_options(
        options
            .into_iter()
            .filter_map(normalize_duration_option)
            .collect(),
    )
}

fn duration_range_options(value: &Value) -> Vec<String> {
    let min = number_field(
        value,
        &[
            "min",
            "minimum",
            "min_seconds",
            "minSeconds",
            "min_duration",
            "minDuration",
            "min_duration_seconds",
            "minDurationSeconds",
            "minimum_duration_seconds",
            "minimumDurationSeconds",
        ],
    );
    let max = number_field(
        value,
        &[
            "max",
            "maximum",
            "max_seconds",
            "maxSeconds",
            "max_duration",
            "maxDuration",
            "max_duration_seconds",
            "maxDurationSeconds",
            "maximum_duration_seconds",
            "maximumDurationSeconds",
        ],
    );
    let Some(min) = min else {
        return Vec::new();
    };
    let Some(max) = max else {
        return Vec::new();
    };
    if !min.is_finite() || !max.is_finite() || min < 0.0 || max < min {
        return Vec::new();
    }

    let step = number_field(value, &["step", "step_seconds", "stepSeconds"])
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(1.0);
    let count = ((max - min) / step).floor() as usize + 1;
    if count == 0 || count > 120 {
        return Vec::new();
    }

    let mut options = Vec::with_capacity(count);
    let mut current = min;
    while current <= max + f64::EPSILON && options.len() < count {
        options.push(format!("{}s", format_number_for_message(current)));
        current += step;
    }
    options
}

fn first_duration_options_from_sources(sources: &[&Value], keys: &[&str]) -> Vec<String> {
    for source in sources {
        for key in keys {
            let Some(value) = source.get(*key) else {
                continue;
            };
            let entries = normalize_duration_options(string_array(Some(value)));
            if !entries.is_empty() {
                return entries;
            }
            let range_entries = duration_range_options(value);
            if !range_entries.is_empty() {
                return range_entries;
            }
        }
        let range_entries = duration_range_options(source);
        if !range_entries.is_empty() {
            return range_entries;
        }
    }
    Vec::new()
}

fn normalize_resolution_options(options: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    options
        .into_iter()
        .map(|entry| entry.trim().to_uppercase())
        .filter(|entry| !entry.is_empty())
        .filter(|entry| seen.insert(entry.clone()))
        .collect()
}

fn known_image_resolution_options(id: &str, name: &str) -> Vec<String> {
    let label = format!("{} {}", id.to_lowercase(), name.to_lowercase());
    if label.contains("gpt-image-2") || label.contains("nano-banana") {
        vec!["1K".to_string(), "2K".to_string(), "4K".to_string()]
    } else {
        Vec::new()
    }
}

fn image_resolution_options(
    id: &str,
    name: &str,
    constraints: &Value,
    capabilities: &Value,
) -> Vec<String> {
    let from_constraints = first_string_array(
        constraints,
        &[
            "resolutions",
            "resolutionOptions",
            "resolution_options",
            "supported_resolutions",
            "supportedResolutions",
            "resolutionTiers",
            "resolution_tiers",
        ],
    );
    if !from_constraints.is_empty() {
        return normalize_resolution_options(from_constraints);
    }

    let from_capabilities = first_string_array(
        capabilities,
        &[
            "resolutions",
            "resolutionOptions",
            "resolution_options",
            "supported_resolutions",
            "supportedResolutions",
            "resolutionTiers",
            "resolution_tiers",
        ],
    );
    if !from_capabilities.is_empty() {
        return normalize_resolution_options(from_capabilities);
    }

    known_image_resolution_options(id, name)
}

fn apply_known_image_resolution_controls(model: &mut ModelRecord) {
    if !string_array(model.controls.get("resolutionOptions")).is_empty() {
        return;
    }

    let options = known_image_resolution_options(&model.id, &model.name);
    if options.is_empty() {
        return;
    }

    if !model.controls.is_object() {
        model.controls = json!({});
    }
    model.controls["resolutionOptions"] = json!(options);
}

fn text_contains_edit_signal(value: &str) -> bool {
    let normalized = value.to_lowercase();
    normalized
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| matches!(part, "edit" | "edits" | "editing"))
        || EDIT_MODEL_PATTERNS
            .iter()
            .any(|pattern| normalized.contains(pattern))
}

fn value_contains_edit_signal(value: &Value) -> bool {
    match value {
        Value::String(text) => text_contains_edit_signal(text),
        Value::Array(items) => items.iter().any(value_contains_edit_signal),
        Value::Object(map) => map
            .iter()
            .any(|(key, item)| text_contains_edit_signal(key) || value_contains_edit_signal(item)),
        _ => false,
    }
}

fn model_name(entry: &Value) -> String {
    let spec = entry.get("model_spec").unwrap_or(&Value::Null);
    as_string(spec, "name")
        .is_empty()
        .then(|| as_string(entry, "id"))
        .unwrap_or_else(|| as_string(spec, "name"))
}

fn normalized_model_id(entry: &Value) -> String {
    as_string(entry, "id")
}

fn video_mode_suffix(id: &str, name: &str, constraints: &Value) -> &'static str {
    let model_type =
        first_option_from_sources(&[constraints], &["model_type", "modelType"]).unwrap_or_default();
    let model_type_lower = model_type.to_lowercase();
    let haystack = format!("{id} {name} {model_type}").to_lowercase();
    let video_input = bool_field(constraints, &["video_input", "videoInput"]).unwrap_or(false);
    if haystack.contains("video-to-video") || (model_type_lower == "video" && video_input) {
        "V2V"
    } else if haystack.contains("image-to-video") || haystack.contains("reference-to-video") {
        "I2V"
    } else if haystack.contains("text-to-video") {
        "T2V"
    } else {
        ""
    }
}

fn append_mode_suffix(name: &str, suffix: &str) -> String {
    if suffix.is_empty()
        || name
            .to_lowercase()
            .contains(&format!("({})", suffix.to_lowercase()))
    {
        name.to_string()
    } else {
        format!("{name} ({suffix})")
    }
}

fn video_duration_options(constraints: &Value, capabilities: &Value) -> Vec<String> {
    first_duration_options_from_sources(
        &[constraints, capabilities],
        &[
            "durations",
            "durationOptions",
            "duration_options",
            "supportedDurations",
            "supported_durations",
            "durationSeconds",
            "duration_seconds",
            "seconds",
        ],
    )
}

fn video_resolution_options(constraints: &Value, capabilities: &Value) -> Vec<String> {
    unique_options(first_options_from_sources(
        &[constraints, capabilities],
        &[
            "resolutions",
            "resolutionOptions",
            "resolution_options",
            "supportedResolutions",
            "supported_resolutions",
        ],
    ))
}

fn video_aspect_ratio_options(constraints: &Value, capabilities: &Value) -> Vec<String> {
    unique_options(first_options_from_sources(
        &[constraints, capabilities],
        &[
            "aspect_ratios",
            "aspectRatios",
            "aspectRatioOptions",
            "aspect_ratio_options",
            "supportedAspectRatios",
            "supported_aspect_ratios",
            "ratios",
        ],
    ))
}

fn video_default_duration(constraints: &Value, capabilities: &Value) -> Option<String> {
    first_option_from_sources(
        &[constraints, capabilities],
        &[
            "default_duration",
            "defaultDuration",
            "default_duration_seconds",
            "defaultDurationSeconds",
            "default_seconds",
            "defaultSeconds",
        ],
    )
    .and_then(normalize_duration_option)
}

fn video_controls_from_constraints(
    id: &str,
    name: &str,
    haystack: &str,
    constraints: &Value,
    capabilities: &Value,
) -> Value {
    let sources = [constraints, capabilities];
    let model_type =
        first_option_from_sources(&sources, &["model_type", "modelType"]).unwrap_or_default();
    let model_type_lower = model_type.to_lowercase();
    let supports_source_image = bool_from_sources(
        &sources,
        &[
            "image_input",
            "imageInput",
            "reference_image_input",
            "referenceImageInput",
            "supports_image_input",
            "supportsImageInput",
        ],
    )
    .unwrap_or_else(|| {
        model_type_lower == "image-to-video"
            || model_type_lower == "reference-to-video"
            || haystack.contains("image-to-video")
            || haystack.contains("reference-to-video")
    });
    let supports_source_video = bool_from_sources(
        &sources,
        &[
            "video_input",
            "videoInput",
            "source_video",
            "sourceVideo",
            "supports_video_input",
            "supportsVideoInput",
        ],
    )
    .unwrap_or_else(|| {
        model_type_lower == "video"
            || model_type_lower == "video-to-video"
            || haystack.contains("video-to-video")
    });
    let supports_text_to_video = bool_from_sources(
        &sources,
        &[
            "text_input",
            "textInput",
            "supports_text_input",
            "supportsTextInput",
            "supports_text_to_video",
            "supportsTextToVideo",
        ],
    )
    .unwrap_or_else(|| model_type_lower == "text-to-video" || haystack.contains("text-to-video"));
    let prompt_character_limit = model_number_field(
        &Value::Null,
        &Value::Null,
        constraints,
        capabilities,
        &["prompt_character_limit", "promptCharacterLimit"],
    );

    let mut controls = json!({
        "durationOptions": video_duration_options(constraints, capabilities),
        "resolutionOptions": video_resolution_options(constraints, capabilities),
        "aspectRatioOptions": video_aspect_ratio_options(constraints, capabilities),
        "modelType": model_type,
        "supportsSourceImage": supports_source_image,
        "supportsSourceVideo": supports_source_video,
        "supportsTextToVideo": supports_text_to_video,
        "rawConstraints": constraints,
        "rawCapabilities": capabilities
    });

    if let Some(default_duration) = video_default_duration(constraints, capabilities) {
        controls["defaultDuration"] = json!(default_duration);
    }
    if let Some(default_resolution) =
        first_option_from_sources(&sources, &["default_resolution", "defaultResolution"])
    {
        controls["defaultResolution"] = json!(default_resolution);
    }
    if let Some(default_aspect_ratio) =
        first_option_from_sources(&sources, &["default_aspect_ratio", "defaultAspectRatio"])
    {
        controls["defaultAspectRatio"] = json!(default_aspect_ratio);
    }
    if let Some(limit) = prompt_character_limit {
        controls["promptCharacterLimit"] = json!(limit);
    }
    if let Some(supports_audio) =
        bool_from_sources(&sources, &["audio", "supports_audio", "supportsAudio"])
    {
        controls["supportsAudio"] = json!(supports_audio);
    }
    if let Some(audio_configurable) =
        bool_from_sources(&sources, &["audio_configurable", "audioConfigurable"])
    {
        controls["audioConfigurable"] = json!(audio_configurable);
    }
    if let Some(supports_audio_input) = bool_from_sources(&sources, &["audio_input", "audioInput"])
    {
        controls["supportsAudioInput"] = json!(supports_audio_input);
    }

    let model_label = format!("{id} {name}").trim().to_string();
    if !model_label.is_empty() {
        controls["modelLabel"] = json!(model_label);
    }

    controls
}

fn apply_video_constraint_controls(model: &mut ModelRecord) {
    if model.kind != "video" {
        return;
    }

    let spec = model.raw.get("model_spec").unwrap_or(&Value::Null);
    let constraints = spec
        .get("constraints")
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| model.controls.get("rawConstraints").cloned())
        .unwrap_or(Value::Null);
    let capabilities = spec
        .get("capabilities")
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| model.controls.get("rawCapabilities").cloned())
        .unwrap_or(Value::Null);
    if constraints.is_null() && capabilities.is_null() {
        return;
    }

    let description = as_string(spec, "description").to_lowercase();
    let haystack = format!(
        "{} {} {}",
        model.id.to_lowercase(),
        model.name.to_lowercase(),
        description
    );
    let normalized = video_controls_from_constraints(
        &model.id,
        &model.name,
        &haystack,
        &constraints,
        &capabilities,
    );

    if !model.controls.is_object() {
        model.controls = json!({});
    }
    if let (Some(target), Some(source)) = (model.controls.as_object_mut(), normalized.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn is_deprecated_or_offline(entry: &Value) -> bool {
    let spec = entry.get("model_spec").unwrap_or(&Value::Null);
    if spec
        .get("offline")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    let deprecation = spec.get("deprecation").unwrap_or(&Value::Null);
    !as_string(deprecation, "date").is_empty()
}

fn bounded_model_raw_summary(entry: &Value) -> Value {
    let mut summary = serde_json::Map::new();
    for key in [
        "id",
        "name",
        "type",
        "model_type",
        "owned_by",
        "description",
    ] {
        if let Some(value) = entry.get(key) {
            if let Some(text) = value.as_str() {
                summary.insert(
                    key.to_string(),
                    json!(text.chars().take(512).collect::<String>()),
                );
            } else if value.is_boolean() || value.is_number() || value.is_null() {
                summary.insert(key.to_string(), value.clone());
            }
        }
    }
    summary.insert(
        "sourceDigest".to_string(),
        json!(provider::sha256_hex(
            serde_json::to_string(entry).unwrap_or_default().as_bytes()
        )),
    );
    Value::Object(summary)
}

fn normalize_model(entry: Value, model_type: &str) -> Option<ModelRecord> {
    if is_deprecated_or_offline(&entry) {
        return None;
    }

    let id = normalized_model_id(&entry);
    if id.is_empty() {
        return None;
    }

    let spec = entry.get("model_spec").cloned().unwrap_or(Value::Null);
    let constraints = spec.get("constraints").cloned().unwrap_or(Value::Null);
    let capabilities = spec.get("capabilities").cloned().unwrap_or(Value::Null);
    let name = {
        let candidate = model_name(&entry);
        if candidate.is_empty() {
            id.clone()
        } else {
            candidate
        }
    };
    let haystack = format!(
        "{} {} {}",
        id.to_lowercase(),
        name.to_lowercase(),
        as_string(&spec, "description").to_lowercase()
    );

    match model_type {
        "image" => {
            let is_edit = text_contains_edit_signal(&haystack)
                || value_contains_edit_signal(&constraints)
                || value_contains_edit_signal(&capabilities)
                || value_contains_edit_signal(&entry);
            let kind = if is_edit { "edit" } else { "image" };
            let mode = if is_edit {
                "edit-image"
            } else {
                "generate-image"
            };
            let size_options = unique_options(first_options_from_sources(
                &[&constraints, &capabilities],
                &[
                    "aspect_ratios",
                    "aspectRatios",
                    "aspectRatioOptions",
                    "aspect_ratio_options",
                    "supportedAspectRatios",
                    "supported_aspect_ratios",
                ],
            ));
            let resolution_options =
                image_resolution_options(&id, &name, &constraints, &capabilities);
            let controls = if is_edit {
                let mut controls = edit_controls_for_model(&id, &name);
                controls["aspectRatioOptions"] = json!(if size_options.is_empty() {
                    vec![
                        "1:1".to_string(),
                        "4:3".to_string(),
                        "3:4".to_string(),
                        "16:9".to_string(),
                        "9:16".to_string(),
                    ]
                } else {
                    size_options
                });
                if !resolution_options.is_empty() {
                    controls["resolutionOptions"] = json!(resolution_options);
                }
                controls["rawConstraints"] = constraints.clone();
                controls["rawCapabilities"] = capabilities.clone();
                controls
            } else {
                json!({
                    "negativePrompt": true,
                    "steps": true,
                    "cfg": true,
                    "seed": true,
                    "hideWatermark": true,
                    "variantCount": { "min": 1, "max": 4 },
                    "sizeOptions": if size_options.is_empty() {
                        vec!["1:1".to_string(), "4:3".to_string(), "3:4".to_string(), "16:9".to_string(), "9:16".to_string()]
                    } else {
                        size_options
                    },
                    "resolutionOptions": resolution_options,
                    "rawConstraints": constraints,
                    "rawCapabilities": capabilities
                })
            };
            Some(ModelRecord {
                id,
                name,
                kind: kind.to_string(),
                modes: vec![mode.to_string()],
                controls,
                raw: bounded_model_raw_summary(&entry),
            })
        }
        "video" => {
            let suffix = video_mode_suffix(&id, &name, &constraints);
            let display_name = append_mode_suffix(&name, suffix);
            let controls =
                video_controls_from_constraints(&id, &name, &haystack, &constraints, &capabilities);
            Some(ModelRecord {
                id,
                name: display_name,
                kind: "video".to_string(),
                modes: vec!["generate-video".to_string()],
                controls,
                raw: bounded_model_raw_summary(&entry),
            })
        }
        "music" => {
            let is_sfx = haystack.contains("sound effect")
                || haystack.contains("sound-effects")
                || haystack.contains("sfx")
                || haystack.contains("foley");
            let kind = if is_sfx { "sfx" } else { "music" };
            let mode = if is_sfx {
                "generate-sfx"
            } else {
                "generate-music"
            };
            let (
                supports_lyrics,
                supports_instrumental,
                supports_lyrics_optimizer,
                supports_duration_seconds,
            ) = audio_support_controls_from_raw(kind, &id, &name, &entry);
            Some(ModelRecord {
                id,
                name,
                kind: kind.to_string(),
                modes: vec![mode.to_string()],
                controls: json!({
                    "audioKind": kind,
                    "supportsDurationSeconds": supports_duration_seconds,
                    "supportsLyrics": supports_lyrics,
                    "supportsInstrumental": supports_instrumental,
                    "supportsLyricsOptimizer": supports_lyrics_optimizer,
                    "rawConstraints": constraints,
                    "rawCapabilities": capabilities
                }),
                raw: bounded_model_raw_summary(&entry),
            })
        }
        "tts" => {
            let voices = constraints
                .get("voices")
                .cloned()
                .or_else(|| capabilities.get("voices").cloned())
                .unwrap_or(Value::Array(vec![]));
            Some(ModelRecord {
                id,
                name,
                kind: "voice".to_string(),
                modes: vec!["generate-voice".to_string()],
                controls: voice_controls(voices),
                raw: bounded_model_raw_summary(&entry),
            })
        }
        "asr" => {
            let supports_language = haystack.contains("whisper")
                || haystack.contains("scribe")
                || haystack.contains("wizper")
                || haystack.contains("xai");
            let supports_timestamps = constraints
                .get("supports_timestamps")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            Some(ModelRecord {
                id,
                name,
                kind: "transcribe".to_string(),
                modes: vec!["transcribe-audio".to_string()],
                controls: transcribe_controls(supports_language, supports_timestamps),
                raw: bounded_model_raw_summary(&entry),
            })
        }
        _ => None,
    }
}

async fn fetch_model_type(model_type: &str) -> Result<Vec<Value>, String> {
    let response = venice_get(&format!("/models?type={model_type}")).await?;
    let payload: Value = bounded_response_json(response, 8 * 1024 * 1024).await?;
    Ok(payload
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

async fn refresh_models_inner(app: &AppHandle) -> Result<ModelCache, String> {
    let previous = read_model_cache(app);
    let (image_result, video_result, music_result, tts_result, asr_result) = tokio::join!(
        fetch_model_type("image"),
        fetch_model_type("video"),
        fetch_model_type("music"),
        fetch_model_type("tts"),
        fetch_model_type("asr")
    );
    let any_success = image_result.is_ok()
        || video_result.is_ok()
        || music_result.is_ok()
        || tts_result.is_ok()
        || asr_result.is_ok();
    let previous_fetched = previous.last_fetched.clone();
    let previous_source = previous.catalog_source.clone();
    let mut category_errors = Vec::new();
    let mut category = |name: &str, result: Result<Vec<Value>, String>| match result {
        Ok(entries) => entries,
        Err(error) => {
            category_errors.push(json!({
                "category": name,
                "code": "UPSTREAM_UNAVAILABLE",
                "message": trim_error_text(&error)
            }));
            Vec::new()
        }
    };
    let image_entries = category("image", image_result);
    let video_entries = category("video", video_result);
    let music_entries = category("music", music_result);
    let tts_entries = category("tts", tts_result);
    let asr_entries = category("asr", asr_result);

    let mut seen = HashSet::new();
    let mut push_unique = |records: Vec<ModelRecord>| -> Vec<ModelRecord> {
        records
            .into_iter()
            .filter(|entry| seen.insert(format!("{}:{}", entry.kind, entry.id)))
            .collect()
    };

    let image_like: Vec<ModelRecord> = image_entries
        .into_iter()
        .filter_map(|entry| normalize_model(entry, "image"))
        .collect();
    let video_models = push_unique(
        video_entries
            .into_iter()
            .filter_map(|entry| normalize_model(entry, "video"))
            .collect(),
    );
    let audio_like = music_entries
        .into_iter()
        .filter_map(|entry| normalize_model(entry, "music"))
        .collect::<Vec<_>>();
    let voice_models = push_unique(
        tts_entries
            .into_iter()
            .filter_map(|entry| normalize_model(entry, "tts"))
            .collect(),
    );
    let transcribe_models = push_unique(
        asr_entries
            .into_iter()
            .filter_map(|entry| normalize_model(entry, "asr"))
            .collect(),
    );

    let mut cache = ModelCache {
        last_fetched: if any_success {
            Utc::now().to_rfc3339()
        } else {
            previous_fetched
        },
        catalog_source: if category_errors.is_empty() {
            "live"
        } else if any_success {
            "cached"
        } else {
            previous_source.as_str()
        }
        .to_string(),
        category_errors,
        image_models: image_like
            .iter()
            .filter(|entry| entry.kind == "image")
            .cloned()
            .collect(),
        edit_models: image_like
            .iter()
            .filter(|entry| entry.kind == "edit")
            .cloned()
            .collect(),
        video_models,
        music_models: audio_like
            .iter()
            .filter(|entry| entry.kind == "music")
            .cloned()
            .collect(),
        sfx_models: audio_like
            .iter()
            .filter(|entry| entry.kind == "sfx")
            .cloned()
            .collect(),
        voice_models,
        transcribe_models,
    };

    if cache.image_models.is_empty() {
        cache.image_models = previous.image_models;
    }
    if cache.edit_models.is_empty() {
        cache.edit_models = previous.edit_models;
    }
    if cache.video_models.is_empty() {
        cache.video_models = previous.video_models;
    }
    if cache.music_models.is_empty() {
        cache.music_models = previous.music_models;
    }
    if cache.sfx_models.is_empty() {
        cache.sfx_models = previous.sfx_models;
    }
    if cache.voice_models.is_empty() {
        cache.voice_models = previous.voice_models;
    }
    if cache.transcribe_models.is_empty() {
        cache.transcribe_models = previous.transcribe_models;
    }

    apply_model_fallbacks(&mut cache);

    save_model_cache(app, &cache)?;
    Ok(cache)
}

fn optional_safe_stem(value: &str) -> Option<String> {
    let mut out = String::new();
    for ch in value.chars().take(80) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch.is_whitespace() || matches!(ch, '-' | '_' | '.') {
            if !out.ends_with('-') {
                out.push('-');
            }
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn safe_stem(value: &str) -> String {
    if let Some(stem) = optional_safe_stem(value) {
        stem
    } else {
        "venice-media".to_string()
    }
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/wav" => "wav",
        "audio/flac" => "flac",
        "audio/opus" => "opus",
        "audio/aac" => "aac",
        "text/plain" => "txt",
        _ if mime.starts_with("audio/") => "mp3",
        _ => "bin",
    }
}

fn sniff_media_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some("video/mp4");
    }
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some("video/webm");
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1A\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        return Some("audio/wav");
    }
    None
}

fn effective_mime_type<'a>(declared: &'a str, bytes: &'a [u8]) -> &'a str {
    let normalized = declared.trim();
    if normalized.is_empty()
        || normalized == "application/octet-stream"
        || normalized == "binary/octet-stream"
    {
        sniff_media_mime(bytes).unwrap_or("application/octet-stream")
    } else {
        normalized
    }
}

fn mime_for_image_format(format: &str) -> &'static str {
    match format.trim().to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

fn normalize_image_format(format: &str) -> &'static str {
    match format.trim().to_lowercase().as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpeg",
        "webp" => "webp",
        _ => "webp",
    }
}

fn output_root(app: &AppHandle, settings: &AppSettings) -> Result<PathBuf, String> {
    if !settings.output_dir.trim().is_empty() {
        return Ok(PathBuf::from(settings.output_dir.trim()));
    }
    Ok(PathBuf::from(default_output_dir(app)?))
}

fn ensure_output_folders_for_settings(
    app: &AppHandle,
    settings: &AppSettings,
) -> Result<PathBuf, String> {
    let root = output_root(app, settings)?;
    fs::create_dir_all(&root).map_err(|err| err.to_string())?;
    fs::create_dir_all(root.join("burn")).map_err(|err| err.to_string())?;
    Ok(root)
}

fn ensure_output_folders(app: &AppHandle) -> Result<PathBuf, String> {
    let settings = read_settings(app);
    ensure_output_folders_for_settings(app, &settings)
}

fn ensure_private_root(root: &Path) -> Result<PathBuf, String> {
    let private_root = root.join("private");
    fs::create_dir_all(&private_root).map_err(|err| err.to_string())?;
    let marker = private_root.join(".nekoignore");
    if !marker.exists() {
        fs::write(&marker, b"").map_err(|err| err.to_string())?;
    }
    Ok(private_root)
}

fn output_dir_for_kind(root: &Path, kind: &str, settings: &AppSettings) -> Result<PathBuf, String> {
    if settings.private_session {
        Ok(ensure_private_root(root)?.join(kind))
    } else {
        Ok(root.join(kind))
    }
}

fn metadata_number(metadata: &Value, key: &str) -> Option<u64> {
    metadata.get(key).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
    })
}

fn metadata_title_suffix(metadata: &Value) -> String {
    let Some(title) = metadata.get("title").and_then(Value::as_str) else {
        return String::new();
    };
    if let Some(stem) = optional_safe_stem(title) {
        format!("_{stem}")
    } else {
        String::new()
    }
}

fn image_file_stem(metadata: &Value) -> Option<String> {
    let seed = metadata_number(metadata, "seed")?;
    let variant = metadata_number(metadata, "variantIndex").unwrap_or(1);
    let date = Local::now().format("%Y-%m-%d").to_string();
    let title_suffix = metadata_title_suffix(metadata);
    Some(format!("{date}_seed-{seed}_v{variant}{title_suffix}"))
}

fn generic_file_stem(metadata: &Value, timestamp: &str) -> String {
    metadata_number(metadata, "seed")
        .map(|seed| format!("{timestamp}-seed-{seed}"))
        .unwrap_or_else(|| timestamp.to_string())
}

fn unique_file_path(dir: &Path, stem: &str, ext: &str) -> (String, PathBuf) {
    let mut name = format!("{stem}.{ext}");
    let mut path = dir.join(&name);
    let mut attempt = 2;
    while path.exists() {
        name = format!("{stem}_{attempt}.{ext}");
        path = dir.join(&name);
        attempt += 1;
    }
    (name, path)
}

fn sidecar_kind_for_folder(kind: &str) -> &'static str {
    match kind {
        "images" => "image",
        "edits" => "edit",
        "videos" => "video",
        "audio" => "audio",
        "sfx" => "sfx",
        "voice" => "voice",
        "transcripts" => "transcript",
        _ => "image",
    }
}

fn media_sidecar_path(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?.to_str()?;
    Some(path.with_file_name(format!("{file_name}.json")))
}

fn companion_sidecar_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(full_suffix) = media_sidecar_path(path) {
        paths.push(full_suffix);
    }
    let legacy = path.with_extension("json");
    if !paths.iter().any(|existing| existing == &legacy) {
        paths.push(legacy);
    }
    paths
}

fn is_companion_sidecar(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
        && path.with_extension("").is_file()
}

fn sidecar_value_string(metadata: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        metadata.get(*key).and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.as_u64().map(|number| number.to_string()))
                .or_else(|| value.as_i64().map(|number| number.to_string()))
                .or_else(|| value.as_f64().map(|number| number.to_string()))
        })
    })
}

fn sidecar_value_i64(metadata: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        metadata
            .get(*key)
            .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
    })
}

fn sidecar_value_f64(metadata: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        metadata
            .get(*key)
            .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
    })
}

fn sidecar_value_u64(metadata: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        metadata
            .get(*key)
            .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
    })
}

fn source_files_from_metadata(metadata: &Value) -> Vec<String> {
    if let Some(files) = metadata.get("sourceFiles").and_then(Value::as_array) {
        return files
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|value| {
                Path::new(value)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .collect();
    }
    metadata
        .get("fileName")
        .and_then(Value::as_str)
        .and_then(|file| Path::new(file).file_name().and_then(|name| name.to_str()))
        .map(|file| vec![file.to_string()])
        .unwrap_or_default()
}

fn tags_from_metadata(metadata: &Value) -> Vec<String> {
    metadata
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn safe_sidecar_summary_value(value: &Value) -> Option<Value> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Some(value.clone()),
        Value::String(text)
            if text.len() <= 256
                && !text.starts_with("data:")
                && !text.to_ascii_lowercase().starts_with("bearer ") =>
        {
            Some(value.clone())
        }
        _ => None,
    }
}

fn media_sidecar_json(
    app: &AppHandle,
    kind: &str,
    prompt: &str,
    mime_type: &str,
    metadata: &Value,
) -> Value {
    let raw = metadata.get("raw").unwrap_or(metadata);
    let raw_digest =
        provider::sha256_hex(serde_json::to_string(raw).unwrap_or_default().as_bytes());
    let raw_summary = ["id", "request_id", "queue_id", "status", "state", "model"]
        .into_iter()
        .filter_map(|key| {
            raw.get(key)
                .and_then(safe_sidecar_summary_value)
                .map(|value| (key.to_string(), value))
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "schema": "nekolegends.media-sidecar",
        "schemaVersion": 1,
        "app": "venice-media-local",
        "appVersion": app.package_info().version.to_string(),
        "kind": sidecar_kind_for_folder(kind),
        "createdAt": Utc::now().to_rfc3339(),
        "mimeType": mime_type,
        "prompt": sidecar_value_string(metadata, &["prompt"]).unwrap_or_else(|| prompt.to_string()),
        "negativePrompt": sidecar_value_string(metadata, &["negativePrompt", "negative_prompt"]),
        "model": sidecar_value_string(metadata, &["model", "modelId"]),
        "seed": sidecar_value_string(metadata, &["seed"]),
        "sampler": sidecar_value_string(metadata, &["sampler"]),
        "steps": sidecar_value_i64(metadata, &["steps"]),
        "cfgScale": sidecar_value_f64(metadata, &["cfgScale", "cfg_scale", "cfg"]),
        "variantIndex": sidecar_value_u64(metadata, &["variantIndex"]),
        "title": sidecar_value_string(metadata, &["title"]),
        "durationSeconds": sidecar_value_f64(metadata, &["durationSeconds", "duration_seconds"]),
        "sourceFiles": source_files_from_metadata(metadata),
        "tags": tags_from_metadata(metadata),
        "raw": {
            "summary": raw_summary,
            "omittedMetadataSha256": raw_digest
        }
    })
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Output path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    AtomicFile::new(path, AllowOverwrite)
        .write(|file| file.write_all(bytes).and_then(|_| file.sync_all()))
        .map_err(|err| err.to_string())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaCommitMarker {
    schema_version: u32,
    media_path: String,
    sidecar_path: Option<String>,
    media_sha256: String,
    media_byte_size: u64,
    sidecar: Option<Value>,
    created_at: String,
}

fn media_commit_marker_path(path: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "Media commit path is invalid".to_string())?;
    Ok(path.with_file_name(format!(".{name}.vml-commit.json")))
}

fn validate_media_sidecar_schema(sidecar: &Value) -> Result<(), String> {
    let schema: Value =
        serde_json::from_str(include_str!("../../schemas/media-sidecar.v1.schema.json"))
            .map_err(|error| error.to_string())?;
    let validator = jsonschema::JSONSchema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .compile(&schema)
        .map_err(|error| error.to_string())?;
    if let Err(errors) = validator.validate(sidecar) {
        let message = errors
            .take(3)
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("Media sidecar schema validation failed: {message}"));
    }
    Ok(())
}

fn write_media_commit(path: &Path, bytes: &[u8], sidecar: Option<Value>) -> Result<(), String> {
    if let Some(value) = sidecar.as_ref() {
        validate_media_sidecar_schema(value)?;
    }
    let sidecar_path = sidecar.as_ref().and_then(|_| media_sidecar_path(path));
    let marker_path = media_commit_marker_path(path)?;
    let marker = MediaCommitMarker {
        schema_version: 1,
        media_path: path.to_string_lossy().to_string(),
        sidecar_path: sidecar_path
            .as_ref()
            .map(|value| value.to_string_lossy().to_string()),
        media_sha256: provider::sha256_hex(bytes),
        media_byte_size: bytes.len() as u64,
        sidecar: sidecar.clone(),
        created_at: Utc::now().to_rfc3339(),
    };
    atomic_write_bytes(
        &marker_path,
        &serde_json::to_vec_pretty(&marker).map_err(|error| error.to_string())?,
    )?;
    atomic_write_bytes(path, bytes)?;
    if let (Some(sidecar_path), Some(sidecar)) = (sidecar_path, sidecar) {
        atomic_write_bytes(
            &sidecar_path,
            &serde_json::to_vec_pretty(&sidecar).map_err(|error| error.to_string())?,
        )?;
    }
    let _ = fs::remove_file(marker_path);
    Ok(())
}

fn hash_file_bounded(path: &Path) -> Result<(u64, String), String> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        size = size.saturating_add(count as u64);
        hasher.update(&buffer[..count]);
    }
    Ok((size, provider::hex(&hasher.finalize())))
}

fn collect_media_commit_markers(dir: &Path, depth: usize, output: &mut Vec<PathBuf>) {
    if depth > 6 || output.len() >= 1024 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_media_commit_markers(&path, depth + 1, output);
        } else if path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.ends_with(".vml-commit.json"))
        {
            output.push(path);
        }
    }
}

fn recover_media_commits(app: &AppHandle) -> Result<(), String> {
    let settings = read_settings(app);
    let root = ensure_output_folders_for_settings(app, &settings)?;
    let mut markers = Vec::new();
    collect_media_commit_markers(&root, 0, &mut markers);
    for marker_path in markers {
        if fs::metadata(&marker_path)
            .map_err(|error| error.to_string())?
            .len()
            > 128 * 1024
        {
            return Err("Media commit marker exceeds its safety bound".to_string());
        }
        let marker: MediaCommitMarker =
            serde_json::from_slice(&fs::read(&marker_path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        if marker.schema_version != 1 {
            return Err("Media commit marker version is unsupported".to_string());
        }
        let media_path = PathBuf::from(&marker.media_path);
        if media_path.parent() != marker_path.parent() {
            return Err("Media commit marker escaped its output directory".to_string());
        }
        if !media_path.exists() {
            fs::remove_file(&marker_path).map_err(|error| error.to_string())?;
            continue;
        }
        ensure_under_output(app, &media_path)?;
        let (media_size, media_hash) = hash_file_bounded(&media_path)?;
        if media_size != marker.media_byte_size || media_hash != marker.media_sha256 {
            return Err("Recovered media does not match its commit marker".to_string());
        }
        if let (Some(sidecar_path), Some(sidecar)) =
            (marker.sidecar_path.as_ref(), marker.sidecar.as_ref())
        {
            let sidecar_path = PathBuf::from(sidecar_path);
            if sidecar_path.parent() != media_path.parent() {
                return Err("Media sidecar commit escaped its output directory".to_string());
            }
            validate_media_sidecar_schema(sidecar)?;
            atomic_write_bytes(
                &sidecar_path,
                &serde_json::to_vec_pretty(sidecar).map_err(|error| error.to_string())?,
            )?;
        }
        fs::remove_file(&marker_path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn save_media_bytes(
    app: &AppHandle,
    kind: &str,
    prompt: &str,
    mime_type: &str,
    bytes: &[u8],
    metadata: Value,
) -> Result<MediaResult, String> {
    let settings = read_settings(app);
    let mime_type = effective_mime_type(mime_type, bytes);
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S-%3f").to_string();
    let ext = extension_for_mime(mime_type);
    let root = ensure_output_folders_for_settings(app, &settings)?;
    let dir = output_dir_for_kind(&root, kind, &settings)?;
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let file_stem = if settings.private_session || settings.generic_filenames {
        generic_file_stem(&metadata, &timestamp)
    } else {
        let stem = safe_stem(prompt);
        let variant_suffix = metadata
            .get("variantIndex")
            .and_then(|value| value.as_u64())
            .map(|index| format!("-v{index}"))
            .unwrap_or_default();
        image_file_stem(&metadata).unwrap_or_else(|| format!("{timestamp}-{stem}{variant_suffix}"))
    };
    let (name, path) = unique_file_path(&dir, &file_stem, ext);
    let sidecar = if settings.write_metadata_sidecars && !settings.private_session {
        Some(media_sidecar_json(app, kind, prompt, mime_type, &metadata))
    } else {
        None
    };
    write_media_commit(&path, bytes, sidecar)?;
    let encoded = general_purpose::STANDARD.encode(bytes);
    Ok(MediaResult {
        id: format!("{kind}-{name}"),
        kind: kind.to_string(),
        name,
        mime_type: mime_type.to_string(),
        data_url: format!("data:{mime_type};base64,{encoded}"),
        file_path: path.to_string_lossy().to_string(),
        metadata,
        text: None,
    })
}

fn save_text_result(
    app: &AppHandle,
    kind: &str,
    prompt: &str,
    text: &str,
    metadata: Value,
) -> Result<MediaResult, String> {
    let mut result = save_media_bytes(app, kind, prompt, "text/plain", text.as_bytes(), metadata)?;
    result.text = Some(text.to_string());
    Ok(result)
}

fn burn_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let settings = read_settings(app);
    Ok(output_root(app, &settings)?.join("burn"))
}

fn ensure_burn_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = burn_dir(app)?;
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    Ok(dir)
}

fn canonical_output_root(app: &AppHandle) -> Result<PathBuf, String> {
    let settings = read_settings(app);
    let root = ensure_output_folders_for_settings(app, &settings)?;
    root.canonicalize().map_err(|err| err.to_string())
}

fn ensure_under_output(app: &AppHandle, path: &Path) -> Result<(), String> {
    let root = canonical_output_root(app)?;
    let canonical = path.canonicalize().map_err(|err| err.to_string())?;
    if canonical.starts_with(root) {
        Ok(())
    } else {
        Err(format!(
            "Refusing to move a file outside the output folder: {}",
            path.to_string_lossy()
        ))
    }
}

fn unique_burn_path(dir: &Path, original_name: &str, index: usize) -> PathBuf {
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S-%3f").to_string();
    let clean_name = original_name
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            _ => ch,
        })
        .collect::<String>();
    let name = if clean_name.trim().is_empty() {
        format!("{timestamp}-{index}.bin")
    } else {
        format!("{timestamp}-{index}-{clean_name}")
    };
    let mut candidate = dir.join(&name);
    let mut attempt = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{timestamp}-{index}-{attempt}-{clean_name}"));
        attempt += 1;
    }
    candidate
}

#[tauri::command]
fn move_media_files_to_burn(app: AppHandle, paths: Vec<String>) -> Result<Vec<String>, String> {
    let _permit = claim_direct_work(&app)?;
    let mut moved = Vec::new();
    let burn_dir = ensure_burn_dir(&app)?;

    for (index, raw_path) in paths.into_iter().enumerate() {
        let trimmed = raw_path.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }

        let path = PathBuf::from(&trimmed);
        if !path.exists() {
            return Err(format!(
                "Generated file no longer exists, so nothing was moved to burn: {trimmed}"
            ));
        }
        if !path.is_file() {
            return Err(format!("Refusing to move non-file path: {trimmed}"));
        }

        ensure_under_output(&app, &path)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("media.bin");
        let sidecars = companion_sidecar_paths(&path)
            .into_iter()
            .filter(|sidecar| sidecar.is_file())
            .collect::<Vec<_>>();
        let target = unique_burn_path(&burn_dir, file_name, index);
        fs::rename(&path, &target)
            .map_err(|err| format!("Failed to move {trimmed} to burn folder: {err}"))?;
        for (sidecar_index, sidecar) in sidecars.into_iter().enumerate() {
            ensure_under_output(&app, &sidecar)?;
            let sidecar_target = if sidecar_index == 0 {
                media_sidecar_path(&target).unwrap_or_else(|| {
                    let name = sidecar
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("media.json");
                    unique_burn_path(&burn_dir, name, index)
                })
            } else {
                let name = sidecar
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("media.json");
                unique_burn_path(&burn_dir, name, index + sidecar_index)
            };
            fs::rename(&sidecar, &sidecar_target).map_err(|err| {
                format!(
                    "Failed to move sidecar {} to burn folder: {err}",
                    sidecar.to_string_lossy()
                )
            })?;
        }
        if !target.is_file() {
            return Err(format!(
                "Move finished but the burn folder file was not found: {}",
                target.to_string_lossy()
            ));
        }
        moved.push(trimmed);
    }

    Ok(moved)
}

#[tauri::command]
fn move_private_session_to_burn(app: AppHandle) -> Result<BurnFolderStats, String> {
    let _permit = claim_direct_work(&app)?;
    let settings = read_settings(&app);
    let root = output_root(&app, &settings)?;
    let private_root = root.join("private");
    if !private_root.exists() {
        return burn_folder_stats_for_dir(&ensure_burn_dir(&app)?);
    }
    ensure_under_output(&app, &private_root)?;
    let burn_dir = ensure_burn_dir(&app)?;
    let target = unique_burn_path(&burn_dir, "private", 0);
    fs::rename(&private_root, &target)
        .map_err(|err| format!("Failed to move private session to burn folder: {}", err))?;
    if settings.private_session {
        let _ = ensure_private_root(&root);
    }
    burn_folder_stats_for_dir(&burn_dir)
}

#[tauri::command]
fn copy_media_file(
    app: AppHandle,
    source_path: String,
    destination_path: String,
) -> Result<String, String> {
    let _permit = claim_direct_work(&app)?;
    let source = PathBuf::from(source_path.trim());
    let destination = PathBuf::from(destination_path.trim());

    if source.as_os_str().is_empty() {
        return Err("Source file path is empty".to_string());
    }
    if destination.as_os_str().is_empty() {
        return Err("Save location is empty".to_string());
    }
    if !source.exists() {
        return Err(format!(
            "Generated file no longer exists: {}",
            source.to_string_lossy()
        ));
    }
    if !source.is_file() {
        return Err(format!(
            "Refusing to copy non-file path: {}",
            source.to_string_lossy()
        ));
    }
    if destination.exists() && destination.is_dir() {
        return Err(format!(
            "Save location is a folder, not a file: {}",
            destination.to_string_lossy()
        ));
    }
    ensure_under_output(&app, &source)?;

    let source_canonical = source.canonicalize().map_err(|err| err.to_string())?;
    if destination.exists() {
        let destination_canonical = destination.canonicalize().map_err(|err| err.to_string())?;
        if source_canonical == destination_canonical {
            return Ok(destination.to_string_lossy().to_string());
        }
    }

    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "Failed to create save folder {}: {err}",
                    parent.to_string_lossy()
                )
            })?;
        }
    }

    fs::copy(&source, &destination).map_err(|err| {
        format!(
            "Failed to save copy from {} to {}: {err}",
            source.to_string_lossy(),
            destination.to_string_lossy()
        )
    })?;

    Ok(destination.to_string_lossy().to_string())
}

#[tauri::command]
fn save_data_url_file(app: AppHandle, request: SaveDataUrlRequest) -> Result<String, String> {
    let _permit = claim_direct_work(&app)?;
    let destination = PathBuf::from(request.destination_path.trim());
    if destination.as_os_str().is_empty() {
        return Err("Save location is empty".to_string());
    }
    if destination.exists() && destination.is_dir() {
        return Err(format!(
            "Save location is a folder, not a file: {}",
            destination.to_string_lossy()
        ));
    }

    let (bytes, _) = decode_data_url(&request.data_url)?;
    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "Failed to create save folder {}: {err}",
                    parent.to_string_lossy()
                )
            })?;
        }
    }

    fs::write(&destination, bytes).map_err(|err| {
        format!(
            "Failed to save converted file to {}: {err}",
            destination.to_string_lossy()
        )
    })?;

    Ok(destination.to_string_lossy().to_string())
}

fn collect_burn_entries(
    dir: &Path,
    files: &mut Vec<PathBuf>,
    dirs: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(dir).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|err| err.to_string())?;
        if metadata.file_type().is_symlink() || metadata.is_file() {
            files.push(path);
        } else if metadata.is_dir() {
            collect_burn_entries(&path, files, dirs)?;
            dirs.push(path);
        }
    }

    Ok(())
}

fn burn_folder_stats_for_dir(dir: &Path) -> Result<BurnFolderStats, String> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    collect_burn_entries(dir, &mut files, &mut dirs)?;

    let mut total_bytes = 0;
    for path in &files {
        if is_companion_sidecar(path) {
            continue;
        }
        total_bytes += fs::symlink_metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
    }

    Ok(BurnFolderStats {
        file_count: files
            .iter()
            .filter(|path| !is_companion_sidecar(path))
            .count(),
        total_bytes,
        burn_dir: dir.to_string_lossy().to_string(),
    })
}

fn fill_corruption_buffer(buffer: &mut [u8], seed: &mut u64, pass: u8) {
    for byte in buffer.iter_mut() {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *byte = ((*seed >> 24) as u8) ^ pass.wrapping_mul(0xa5);
    }
}

fn mix_seed(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58476d1ce4e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn hash_seed_text(value: &str) -> u64 {
    let mut hash = 14695981039346656037u64;
    for byte in value.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    mix_seed(hash)
}

fn corrupt_regular_file(path: &Path, burn_seed: u64) -> Result<u64, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|err| format!("Failed to open {}: {err}", path.to_string_lossy()))?;
    let len = file
        .metadata()
        .map_err(|err| format!("Failed to inspect {}: {err}", path.to_string_lossy()))?
        .len();
    if len == 0 {
        file.sync_data()
            .map_err(|err| format!("Failed to flush {}: {err}", path.to_string_lossy()))?;
        return Ok(0);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let mut seed = now
        ^ len
        ^ burn_seed
        ^ path.to_string_lossy().bytes().fold(0u64, |hash, byte| {
            hash.wrapping_mul(1099511628211).wrapping_add(byte as u64)
        });
    seed = mix_seed(seed);
    let mut buffer = vec![0u8; 1024 * 1024];

    for pass in 0..2u8 {
        file.seek(SeekFrom::Start(0))
            .map_err(|err| format!("Failed to seek {}: {err}", path.to_string_lossy()))?;
        let mut remaining = len;
        while remaining > 0 {
            let write_len = remaining.min(buffer.len() as u64) as usize;
            fill_corruption_buffer(&mut buffer[..write_len], &mut seed, pass);
            file.write_all(&buffer[..write_len])
                .map_err(|err| format!("Failed to overwrite {}: {err}", path.to_string_lossy()))?;
            remaining -= write_len as u64;
        }
        file.flush()
            .map_err(|err| format!("Failed to flush {}: {err}", path.to_string_lossy()))?;
        file.sync_data()
            .map_err(|err| format!("Failed to sync {}: {err}", path.to_string_lossy()))?;
    }

    Ok(len)
}

#[tauri::command]
fn get_burn_folder_stats(app: AppHandle) -> Result<BurnFolderStats, String> {
    let dir = ensure_burn_dir(&app)?;
    burn_folder_stats_for_dir(&dir)
}

fn diem_percent_remaining(balance: Option<f64>, allocation: Option<f64>) -> Option<f64> {
    let balance = balance?;
    let allocation = allocation?;
    if !balance.is_finite() || !allocation.is_finite() || allocation <= 0.0 {
        return None;
    }
    Some((balance / allocation * 100.0).clamp(0.0, 100.0))
}

fn diem_snapshot_from_billing(payload: Value) -> Result<DiemBalanceSnapshot, String> {
    let data = response_data(&payload);
    let balances = data.get("balances").unwrap_or(&Value::Null);
    let diem_balance = balance_number(balances, &["diem", "DIEM"]);
    let usd_balance = balance_number(balances, &["usd", "USD"]);
    let allocation = number_like(data.get("diemEpochAllocation"));

    if diem_balance.is_none() && allocation.is_none() {
        return Err(
            "Venice billing balance response did not include DIEM balance data".to_string(),
        );
    }

    Ok(DiemBalanceSnapshot {
        success: true,
        diem_balance,
        usd_balance,
        diem_epoch_allocation: allocation,
        percent_remaining: diem_percent_remaining(diem_balance, allocation),
        consumption_currency: data
            .get("consumptionCurrency")
            .and_then(Value::as_str)
            .map(str::to_string),
        can_consume: bool_like(data.get("canConsume")),
        source: "billing_balance".to_string(),
        timestamp: Utc::now().to_rfc3339(),
        warning: None,
        raw: payload,
    })
}

fn diem_snapshot_from_rate_limits(
    payload: Value,
    warning: Option<String>,
) -> Result<DiemBalanceSnapshot, String> {
    let data = response_data(&payload);
    let balances = data.get("balances").unwrap_or(&Value::Null);
    let diem_balance = balance_number(balances, &["diem", "DIEM"]);
    let usd_balance = balance_number(balances, &["usd", "USD"]);

    if diem_balance.is_none() {
        return Err("Venice rate limits response did not include a DIEM balance".to_string());
    }

    Ok(DiemBalanceSnapshot {
        success: true,
        diem_balance,
        usd_balance,
        diem_epoch_allocation: None,
        percent_remaining: None,
        consumption_currency: Some("DIEM".to_string()),
        can_consume: bool_like(data.get("accessPermitted")),
        source: "api_key_rate_limits".to_string(),
        timestamp: Utc::now().to_rfc3339(),
        warning,
        raw: payload,
    })
}

async fn fetch_diem_billing_balance() -> Result<DiemBalanceSnapshot, String> {
    let response = venice_get("/billing/balance").await?;
    let payload: Value = bounded_response_json(response, 8 * 1024 * 1024).await?;
    diem_snapshot_from_billing(payload)
}

async fn fetch_diem_rate_limits(warning: Option<String>) -> Result<DiemBalanceSnapshot, String> {
    let response = venice_get("/api_keys/rate_limits").await?;
    let payload: Value = bounded_response_json(response, 8 * 1024 * 1024).await?;
    diem_snapshot_from_rate_limits(payload, warning)
}

#[tauri::command]
async fn get_diem_balance() -> Result<DiemBalanceSnapshot, String> {
    match fetch_diem_billing_balance().await {
        Ok(snapshot) => Ok(snapshot),
        Err(billing_err) => fetch_diem_rate_limits(Some(billing_err.clone()))
            .await
            .map_err(|rate_err| {
                format!(
                    "Failed to read DIEM balance. Billing balance: {billing_err}. Rate limits: {rate_err}"
                )
            }),
    }
}

fn open_folder_path(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("explorer");
        command.arg(path);
        command
    };

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(path);
        command
    };

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("Failed to open {}: {err}", path.to_string_lossy()))
}

fn open_url(url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        return Err("Only http/https URLs can be opened".to_string());
    }

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", trimmed]);
        command
    };

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(trimmed);
        command
    };

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(trimmed);
        command
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("Failed to open update URL: {err}"))
}

fn run_file(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!(
            "Update installer was not found: {}",
            path.to_string_lossy()
        ));
    }

    Command::new(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("Failed to run {}: {err}", path.to_string_lossy()))
}

#[tauri::command]
fn open_output_folder(app: AppHandle) -> Result<String, String> {
    let _permit = claim_direct_work(&app)?;
    let root = ensure_output_folders(&app)?;
    open_folder_path(&root)?;
    Ok(root.to_string_lossy().to_string())
}

#[tauri::command]
fn open_burn_folder(app: AppHandle) -> Result<String, String> {
    let _permit = claim_direct_work(&app)?;
    let dir = ensure_burn_dir(&app)?;
    open_folder_path(&dir)?;
    Ok(dir.to_string_lossy().to_string())
}

#[tauri::command]
fn open_file_folder(app: AppHandle, path: String) -> Result<String, String> {
    let _permit = claim_direct_work(&app)?;
    let file_path = PathBuf::from(path.trim());
    let folder = file_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "Saved file has no parent folder".to_string())?;
    open_folder_path(folder)?;
    Ok(folder.to_string_lossy().to_string())
}

#[tauri::command]
fn burn_folder(app: AppHandle, seed: Option<String>) -> Result<BurnFolderStats, String> {
    let _permit = claim_direct_work(&app)?;
    let dir = ensure_burn_dir(&app)?;
    let burn_seed = seed
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(hash_seed_text)
        .unwrap_or(0);

    let mut files = Vec::new();
    let mut dirs = Vec::new();
    collect_burn_entries(&dir, &mut files, &mut dirs)?;
    let stats = burn_folder_stats_for_dir(&dir)?;

    for path in files {
        let metadata = fs::symlink_metadata(&path).map_err(|err| err.to_string())?;
        if metadata.file_type().is_symlink() {
            fs::remove_file(&path)
                .map_err(|err| format!("Failed to delete {}: {err}", path.to_string_lossy()))?;
            continue;
        }

        corrupt_regular_file(&path, burn_seed)?;
        let burned_name =
            path.with_file_name(format!("burned-{}", Utc::now().format("%Y%m%d-%H%M%S-%3f")));
        let delete_path = if fs::rename(&path, &burned_name).is_ok() {
            burned_name
        } else {
            path
        };
        fs::remove_file(&delete_path)
            .map_err(|err| format!("Failed to delete {}: {err}", delete_path.to_string_lossy()))?;
    }

    for dir in dirs.into_iter().rev() {
        let _ = fs::remove_dir(&dir);
    }

    Ok(stats)
}

fn decode_base64_payload(value: &str) -> Result<Vec<u8>, String> {
    let payload = value
        .split_once(',')
        .map(|(_, right)| right)
        .unwrap_or(value)
        .trim();
    general_purpose::STANDARD
        .decode(payload)
        .map_err(|err| format!("Failed to decode base64 media: {err}"))
}

fn image_input_body(source_image: &str) -> Result<Value, String> {
    let trimmed = source_image.trim();
    if trimmed.is_empty() {
        return Err("Choose a source image first".to_string());
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(json!({ "image_url": trimmed }));
    }

    let payload = trimmed
        .split_once(',')
        .map(|(_, right)| right)
        .unwrap_or(trimmed)
        .trim();

    if payload.is_empty() {
        return Err("Source image data is empty".to_string());
    }

    Ok(json!({ "image": payload }))
}

fn multi_edit_image_input(source_image: &str) -> Result<String, String> {
    let trimmed = source_image.trim();
    if trimmed.is_empty() {
        return Err("Choose at least one image first".to_string());
    }
    Ok(trimmed.to_string())
}

fn decode_data_url(value: &str) -> Result<(Vec<u8>, String), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("Choose a file first".to_string());
    }

    let (mime, payload) = if let Some((left, right)) = trimmed.split_once(',') {
        let mime = left
            .strip_prefix("data:")
            .and_then(|header| header.split(';').next())
            .unwrap_or("application/octet-stream")
            .trim()
            .to_string();
        (mime, right.trim())
    } else {
        ("application/octet-stream".to_string(), trimmed)
    };

    let bytes = general_purpose::STANDARD
        .decode(payload)
        .map_err(|err| format!("Failed to decode file data: {err}"))?;
    Ok((bytes, mime))
}

fn first_string_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for key in keys {
        if let Some(found) = value.get(*key).and_then(Value::as_str) {
            if !found.trim().is_empty() {
                return Some(found);
            }
        }
    }
    None
}

fn json_status_label(value: &Value) -> String {
    first_string_field(value, &["status", "state"])
        .or_else(|| {
            value
                .get("data")
                .and_then(|data| first_string_field(data, &["status", "state"]))
        })
        .unwrap_or("queued")
        .to_string()
}

fn is_done_status(status: &str) -> bool {
    matches!(
        status.trim().to_lowercase().as_str(),
        "complete" | "completed" | "done" | "success" | "succeeded" | "finished"
    )
}

async fn save_binary_response(
    app: &AppHandle,
    response: reqwest::Response,
    kind: &str,
    prompt: &str,
    metadata: Value,
) -> Result<MediaResult, String> {
    let mime = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .trim()
        .to_string();
    let limit = match mime.as_str() {
        value if value.starts_with("image/") => 32 * 1024 * 1024,
        value if value.starts_with("audio/") => 64 * 1024 * 1024,
        value if value.starts_with("video/") => 256 * 1024 * 1024,
        "text/plain" | "application/json" => 4 * 1024 * 1024,
        _ => return Err("Upstream response media type is unsupported".to_string()),
    };
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err("Upstream response exceeds the bounded media output size".to_string());
    }
    let mut bytes =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| err.to_string())?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err("Upstream response exceeds the bounded media output size".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    save_media_bytes(app, kind, prompt, &mime, &bytes, metadata)
}

fn collect_app_state(
    app: AppHandle,
    setup_sections: Vec<StartupTimingEntry>,
) -> Result<AppState, String> {
    let total_started_at = Instant::now();
    let mut sections = Vec::new();

    let started_at = Instant::now();
    let mut settings = read_settings(&app);
    settings.agent_control_token = read_control_token(&settings).ok().flatten();
    sections.push(StartupTimingEntry {
        name: "read settings".to_string(),
        ms: elapsed_ms(started_at),
    });

    let started_at = Instant::now();
    ensure_output_folders_for_settings(&app, &settings)?;
    sections.push(StartupTimingEntry {
        name: "ensure output folders".to_string(),
        ms: elapsed_ms(started_at),
    });

    let started_at = Instant::now();
    let models = read_model_cache(&app);
    sections.push(StartupTimingEntry {
        name: "read model cache".to_string(),
        ms: elapsed_ms(started_at),
    });

    let started_at = Instant::now();
    let build_version = app.package_info().version.to_string();
    sections.push(StartupTimingEntry {
        name: "read build version".to_string(),
        ms: elapsed_ms(started_at),
    });

    let app_state_total_ms = elapsed_ms(total_started_at);
    let agent_control_port = settings.agent_control_port;

    Ok(AppState {
        agent_control_address: agent_control_address(
            agent_control_port,
            settings.agent_control_bind_all,
        ),
        settings,
        key_configured: has_api_key(),
        models,
        build_version,
        startup_timings: StartupTimings {
            setup_sections,
            app_state_sections: sections,
            app_state_total_ms,
        },
    })
}

#[tauri::command]
fn get_app_state(
    app: AppHandle,
    metrics: tauri::State<StartupMetricsHandle>,
) -> Result<AppState, String> {
    collect_app_state(app, metrics.sections())
}

#[tauri::command]
fn get_key_configured() -> Result<bool, String> {
    Ok(has_api_key())
}

#[tauri::command]
fn get_agent_control_address(app: AppHandle) -> Result<String, String> {
    let settings = read_settings(&app);
    Ok(agent_control_address(
        settings.agent_control_port,
        settings.agent_control_bind_all,
    ))
}

#[tauri::command]
async fn save_settings(
    app: AppHandle,
    request: SaveSettingsRequest,
    handle: tauri::State<'_, AgentControlHandle>,
) -> Result<AppSettings, String> {
    let transaction = handle.transaction.clone();
    let transaction_guard = transaction.lock().await;
    handle
        .control
        .terminal
        .ensure_open()
        .map_err(str::to_string)?;
    let mut settings = read_settings(&app);
    if let Some(theme) = request.theme {
        settings.theme = theme;
    }
    if let Some(output_dir) = request.output_dir {
        settings.output_dir = output_dir.trim().to_string();
    }
    if let Some(write_metadata_sidecars) = request.write_metadata_sidecars {
        settings.write_metadata_sidecars = write_metadata_sidecars;
    }
    if let Some(private_session) = request.private_session {
        settings.private_session = private_session;
    }
    if let Some(generic_filenames) = request.generic_filenames {
        settings.generic_filenames = generic_filenames;
    }
    if let Some(show_diem_balance) = request.show_diem_balance {
        settings.show_diem_balance = show_diem_balance;
    }
    if let Some(selected_models) = request.selected_models {
        settings.selected_models = selected_models
            .into_iter()
            .filter_map(|(kind, model)| {
                let kind = kind.trim().to_string();
                let model = model.trim().to_string();
                if kind.is_empty() || model.is_empty() {
                    None
                } else {
                    Some((kind, model))
                }
            })
            .collect();
    }
    let was_enabled = settings.enable_agent_control;
    let previous_port = settings.agent_control_port;
    let previous_bind_all = settings.agent_control_bind_all;

    if let Some(port) = request.agent_control_port {
        settings.agent_control_port = validate_agent_control_port(port)?;
    }
    if let Some(bind_all) = request.agent_control_bind_all {
        settings.agent_control_bind_all = bind_all;
    }

    let mut stop_ownership = None;
    let mut restart_after_stop = false;
    if let Some(enable) = request.enable_agent_control {
        settings.enable_agent_control = enable;
        // Live start / stop when the user toggles in Settings (no restart needed)
        if enable && !was_enabled {
            let token = read_control_token(&settings)?.unwrap_or_else(generate_agent_control_token);
            store_control_token(&token)?;
            settings.agent_control_token = Some(token.clone());
            save_settings_file(&app, &settings)?;
            if let Err(error) = start_agent_control_server(
                app.clone(),
                token,
                None,
                settings.agent_control_port,
                settings.agent_control_bind_all,
            )
            .await
            {
                settings.enable_agent_control = false;
                save_settings_file(&app, &settings)?;
                return Err(format!(
                    "Agent Control startup failed and enable was rolled back: {error}"
                ));
            }
        } else if !enable && was_enabled {
            save_settings_file(&app, &settings)?;
            stop_ownership = capture_agent_control_stop(&handle)?;
        }
    }

    if settings.enable_agent_control
        && was_enabled
        && (settings.agent_control_port != previous_port
            || settings.agent_control_bind_all != previous_bind_all)
    {
        save_settings_file(&app, &settings)?;
        stop_ownership = capture_agent_control_stop(&handle)?;
        restart_after_stop = true;
    }

    let stopped_generation = stop_ownership.as_ref().map(|(generation, _)| *generation);
    drop(transaction_guard);
    if let Err(error) = await_agent_control_stop(stop_ownership).await {
        let _guard = transaction.lock().await;
        handle
            .control
            .terminal
            .ensure_open()
            .map_err(str::to_string)?;
        settings.enable_agent_control = false;
        save_settings_file(&app, &settings)?;
        return Err(format!(
            "Agent Control stop failed; persisted disabled: {error}"
        ));
    }
    let _transaction_guard = transaction.lock().await;
    handle
        .control
        .terminal
        .ensure_open()
        .map_err(str::to_string)?;
    if let Some(generation) = stopped_generation {
        let (_, current_generation, _) = handle.control.ownership.snapshot();
        if current_generation != generation || handle.control.terminal.is_set() {
            return Err("APPLICATION_SHUTTING_DOWN".to_string());
        }
    }
    if restart_after_stop {
        let token = read_control_token(&settings)?
            .ok_or_else(|| "Agent Control credential is unavailable".to_string())?;
        settings.agent_control_token = Some(token.clone());
        if let Err(error) = start_agent_control_server(
            app.clone(),
            token,
            None,
            settings.agent_control_port,
            settings.agent_control_bind_all,
        )
        .await
        {
            settings.enable_agent_control = false;
            save_settings_file(&app, &settings)?;
            return Err(format!(
                "Agent Control restart failed and enable was rolled back: {error}"
            ));
        }
    }

    ensure_output_folders_for_settings(&app, &settings)?;
    save_settings_file(&app, &settings)?;
    Ok(settings)
}

#[tauri::command]
async fn rotate_agent_control_token(
    app: AppHandle,
    handle: tauri::State<'_, AgentControlHandle>,
) -> Result<AppSettings, String> {
    let transaction = handle.transaction.clone();
    let transaction_guard = transaction.lock().await;
    handle
        .control
        .terminal
        .ensure_open()
        .map_err(str::to_string)?;
    let mut settings = read_settings(&app);
    let previous_token = read_control_token(&settings)?
        .ok_or_else(|| "Agent Control credential is unavailable".to_string())?;
    let token = generate_agent_control_token();
    store_control_token(&token)?;
    settings.agent_control_token = Some(token.clone());

    if settings.enable_agent_control {
        let stop = capture_agent_control_stop(&handle)?;
        let stopped_generation = stop.as_ref().map(|(generation, _)| *generation);
        drop(transaction_guard);
        if let Err(error) = await_agent_control_stop(stop).await {
            let _guard = transaction.lock().await;
            handle
                .control
                .terminal
                .ensure_open()
                .map_err(str::to_string)?;
            settings.enable_agent_control = false;
            save_settings_file(&app, &settings)?;
            store_control_token(&previous_token)?;
            return Err(format!(
                "Credential rotation stop failed; persisted disabled: {error}"
            ));
        }
        let _guard = transaction.lock().await;
        if stopped_generation.is_some_and(|generation| {
            handle.control.ownership.snapshot().1 != generation || handle.control.terminal.is_set()
        }) {
            store_control_token(&previous_token)?;
            return Err("APPLICATION_SHUTTING_DOWN".to_string());
        }
        if let Err(error) = start_agent_control_server(
            app.clone(),
            token.clone(),
            Some(previous_token.clone()),
            settings.agent_control_port,
            settings.agent_control_bind_all,
        )
        .await
        {
            store_control_token(&previous_token)?;
            let restored = start_agent_control_server(
                app.clone(),
                previous_token,
                None,
                settings.agent_control_port,
                settings.agent_control_bind_all,
            )
            .await;
            if restored.is_err() {
                settings.enable_agent_control = false;
                save_settings_file(&app, &settings)?;
            }
            return Err(format!("Credential rotation was rolled back: {error}"));
        }
    } else {
        drop(transaction_guard);
    }

    let _transaction_guard = transaction.lock().await;
    handle
        .control
        .terminal
        .ensure_open()
        .map_err(str::to_string)?;
    ensure_output_folders_for_settings(&app, &settings)?;
    save_settings_file(&app, &settings)?;
    Ok(settings)
}

#[tauri::command]
fn save_api_key(app: AppHandle, api_key: String) -> Result<bool, String> {
    let _permit = claim_direct_work(&app)?;
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err("API key cannot be empty".to_string());
    }
    let entry = keyring_entry()?;
    entry.set_password(trimmed).map_err(|err| err.to_string())?;
    Ok(true)
}

#[tauri::command]
fn clear_api_key(app: AppHandle) -> Result<bool, String> {
    let _permit = claim_direct_work(&app)?;
    let entry = keyring_entry()?;
    match entry.delete_credential() {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

#[tauri::command]
async fn configure_provider_lifecycle(
    app: AppHandle,
    handle: tauri::State<'_, AgentControlHandle>,
    core_origin: String,
    credential_id: String,
    credential: String,
) -> Result<bool, String> {
    let _transaction = handle.transaction.lock().await;
    handle
        .control
        .terminal
        .ensure_open()
        .map_err(str::to_string)?;
    let (phase, generation, _) = handle.control.ownership.snapshot();
    if phase != venice_provider_kernel::AgentControlPhase::Running {
        return Err("Agent Control must be running before lifecycle starts".to_string());
    }
    provider::configure_lifecycle_transactional(
        &app,
        generation,
        &core_origin,
        &credential_id,
        &credential,
    )
    .await?;
    Ok(true)
}

#[tauri::command]
async fn clear_provider_lifecycle(
    app: AppHandle,
    handle: tauri::State<'_, AgentControlHandle>,
) -> Result<bool, String> {
    let _transaction = handle.transaction.lock().await;
    handle
        .control
        .terminal
        .ensure_open()
        .map_err(str::to_string)?;
    provider::clear_lifecycle(&app, handle.control.ownership.snapshot().1).await?;
    Ok(true)
}

#[tauri::command]
async fn refresh_models(app: AppHandle) -> Result<ModelCache, String> {
    let _permit = claim_direct_work(&app)?;
    refresh_models_inner(&app).await
}

#[tauri::command]
fn get_models(app: AppHandle) -> Result<ModelCache, String> {
    Ok(read_model_cache(&app))
}

fn preferred_update_assets(
    assets: &[GithubReleaseAsset],
) -> (Option<UpdateAsset>, Option<UpdateAsset>) {
    let setup = assets
        .iter()
        .find(|asset| {
            let name = asset.name.to_lowercase();
            name.ends_with(".exe") && name.contains("setup") && name.contains("x64")
        })
        .or_else(|| {
            assets.iter().find(|asset| {
                let name = asset.name.to_lowercase();
                name.ends_with(".exe") && name.contains("setup")
            })
        })
        .map(update_asset_from_github);

    let portable = assets
        .iter()
        .find(|asset| {
            let name = asset.name.to_lowercase();
            name.ends_with(".exe") && name.contains("portable") && name.contains("x64")
        })
        .or_else(|| {
            assets.iter().find(|asset| {
                let name = asset.name.to_lowercase();
                name.ends_with(".exe") && name.contains("portable")
            })
        })
        .map(update_asset_from_github);

    (setup, portable)
}

fn recommended_update_asset(
    target: UpdateTarget,
    setup_asset: &Option<UpdateAsset>,
    portable_asset: &Option<UpdateAsset>,
) -> Option<UpdateAsset> {
    match target {
        UpdateTarget::WindowsSetup => setup_asset.clone().or_else(|| portable_asset.clone()),
        UpdateTarget::WindowsPortable => portable_asset.clone().or_else(|| setup_asset.clone()),
        UpdateTarget::ReleasePage => None,
    }
}

#[tauri::command]
async fn check_for_update(app: AppHandle) -> Result<UpdateCheckResult, String> {
    let response = client()?
        .get(GITHUB_LATEST_RELEASE_URL)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let response = ensure_success(response).await?;
    let release: GithubRelease = bounded_response_json(response, 1024 * 1024).await?;
    let current_version = app.package_info().version.to_string();
    let latest_version = clean_release_version(&release.tag_name);
    let (setup_asset, portable_asset) = preferred_update_assets(&release.assets);
    let update_target = current_update_target();
    let recommended_asset = recommended_update_asset(update_target, &setup_asset, &portable_asset);

    Ok(UpdateCheckResult {
        checked_at: Utc::now().to_rfc3339(),
        current_version: current_version.clone(),
        latest_version: latest_version.clone(),
        update_available: compare_versions(&current_version, &latest_version) == Ordering::Less,
        release_name: release.name.unwrap_or_else(|| release.tag_name.clone()),
        release_url: release.html_url,
        setup_asset,
        portable_asset,
        recommended_asset,
        update_target,
    })
}

#[tauri::command]
fn open_update_release(app: AppHandle, url: String) -> Result<bool, String> {
    let _permit = claim_direct_work(&app)?;
    open_url(&url)?;
    Ok(true)
}

fn safe_download_name(name: &str) -> String {
    let clean = name
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            _ => ch,
        })
        .collect::<String>();
    if clean.trim().is_empty() {
        "VeniceMediaLocal-update.exe".to_string()
    } else {
        clean
    }
}

fn update_download_dirs(app: &AppHandle, asset: &UpdateAsset) -> Result<Vec<PathBuf>, String> {
    let mut dirs = Vec::new();

    #[cfg(target_os = "windows")]
    {
        let name = asset.name.to_lowercase();
        if current_update_target() == UpdateTarget::WindowsPortable && name.contains("portable") {
            if let Ok(exe_path) = std::env::current_exe() {
                if let Some(parent) = exe_path.parent() {
                    dirs.push(parent.to_path_buf());
                }
            }
        }
    }

    dirs.push(app_data_dir(app)?.join("updates"));
    Ok(dirs)
}

#[tauri::command]
async fn download_update_installer(app: AppHandle, asset: UpdateAsset) -> Result<String, String> {
    let _permit = claim_direct_work(&app)?;
    if !asset.url.starts_with("https://github.com/") {
        return Err("Update asset URL did not come from GitHub releases".to_string());
    }

    let response = client()?
        .get(&asset.url)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let response = ensure_success(response).await?;
    let bytes = response.bytes().await.map_err(|err| err.to_string())?;
    let name = safe_download_name(&asset.name);
    let mut last_error = None;

    for dir in update_download_dirs(&app, &asset)? {
        if let Err(err) = fs::create_dir_all(&dir) {
            last_error = Some(err.to_string());
            continue;
        }
        let path = dir.join(&name);
        match fs::write(&path, &bytes) {
            Ok(_) => return Ok(path.to_string_lossy().to_string()),
            Err(err) => last_error = Some(err.to_string()),
        }
    }

    Err(format!(
        "Failed to save update: {}",
        last_error.unwrap_or_else(|| "no writable update folder".to_string())
    ))
}

#[tauri::command]
fn run_update_installer(app: AppHandle, path: String) -> Result<bool, String> {
    let _permit = claim_direct_work(&app)?;
    run_file(Path::new(path.trim()))?;
    Ok(true)
}

#[tauri::command]
async fn generate_image(
    app: AppHandle,
    request: ImageGenerationRequest,
) -> Result<Vec<MediaResult>, String> {
    let _permit = claim_direct_work(&app)?;
    let variant_count = request.variants.unwrap_or(1).clamp(1, 4);
    let format = normalize_image_format(request.format.as_deref().unwrap_or("webp"));
    let title = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let mut body = json!({
        "model": request.model.clone(),
        "prompt": request.prompt.clone(),
        "variants": variant_count,
        "format": format,
        "return_binary": false,
    });

    if let Some(value) = request
        .negative_prompt
        .filter(|value| !value.trim().is_empty())
    {
        body["negative_prompt"] = json!(value);
    }
    if let Some(value) = request
        .aspect_ratio
        .filter(|value| !value.trim().is_empty())
    {
        body["aspect_ratio"] = json!(value);
    }
    if let Some(value) = request.resolution.filter(|value| !value.trim().is_empty()) {
        body["resolution"] = json!(value);
    }
    if let Some(value) = request.steps {
        body["steps"] = json!(value);
    }
    if let Some(value) = request.cfg_scale {
        body["cfg_scale"] = json!(value);
    }
    if let Some(value) = request.seed {
        body["seed"] = json!(value);
    }
    if let Some(value) = request.hide_watermark {
        body["hide_watermark"] = json!(value);
    }
    if let Some(value) = request.safe_mode {
        body["safe_mode"] = json!(value);
    }

    let response = venice_post_json("/image/generate", body.clone()).await?;
    let payload: Value = bounded_response_json(response, 8 * 1024 * 1024).await?;
    let mut encoded_images = Vec::new();

    if let Some(items) = payload.get("images").and_then(Value::as_array) {
        for item in items {
            if let Some(raw) = item.as_str() {
                encoded_images.push(raw.to_string());
            } else if let Some(raw) =
                first_string_field(item, &["b64_json", "base64", "image", "url"])
            {
                encoded_images.push(raw.to_string());
            }
        }
    }

    if let Some(items) = payload.get("data").and_then(Value::as_array) {
        for item in items {
            if let Some(raw) = first_string_field(item, &["b64_json", "base64", "image"]) {
                encoded_images.push(raw.to_string());
            }
        }
    }

    if encoded_images.is_empty() {
        return Err(format!(
            "Venice image response did not include image data: {payload}"
        ));
    }

    let mime = mime_for_image_format(format);
    let mut results = Vec::new();
    for (index, encoded) in encoded_images.iter().enumerate() {
        let bytes = decode_base64_payload(encoded)?;
        let metadata = json!({
            "model": body.get("model"),
            "title": title,
            "prompt": body.get("prompt"),
            "seed": body.get("seed"),
            "variantIndex": index + 1,
            "raw": payload
        });
        results.push(save_media_bytes(
            &app,
            "images",
            &request.prompt,
            mime,
            &bytes,
            metadata,
        )?);
    }

    Ok(results)
}

#[tauri::command]
async fn remove_background(
    app: AppHandle,
    request: BackgroundRemoveRequest,
) -> Result<MediaResult, String> {
    let _permit = claim_direct_work(&app)?;
    let body = image_input_body(&request.source_image)?;
    let response = venice_post_json("/image/background-remove", body.clone()).await?;
    save_binary_response(
        &app,
        response,
        "edits",
        "background-removed",
        json!({ "operation": "background-remove", "request": body }),
    )
    .await
}

#[tauri::command]
async fn upscale_image(
    app: AppHandle,
    request: ImageUpscaleRequest,
) -> Result<MediaResult, String> {
    let _permit = claim_direct_work(&app)?;
    let scale = match request.scale {
        2 | 4 => request.scale,
        _ => return Err("Scale must be 2x or 4x".to_string()),
    };
    let mut body = image_input_body(&request.source_image)?;
    body["scale"] = json!(scale);

    let response = venice_post_json("/image/upscale", body.clone()).await?;
    save_binary_response(
        &app,
        response,
        "edits",
        &format!("upscaled-{scale}x"),
        json!({
            "operation": "image-upscale",
            "scale": scale,
            "request": body
        }),
    )
    .await
}

#[tauri::command]
async fn multi_edit_image(
    app: AppHandle,
    request: ImageMultiEditRequest,
) -> Result<MediaResult, String> {
    let _permit = claim_direct_work(&app)?;
    let prompt = request.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err("Enter an edit prompt first".to_string());
    }

    let images = request
        .images
        .iter()
        .filter(|image| !image.trim().is_empty())
        .map(|image| multi_edit_image_input(image))
        .collect::<Result<Vec<_>, _>>()?;

    if images.is_empty() {
        return Err("Choose at least one image first".to_string());
    }
    if images.len() > 3 {
        return Err("Edit/combine supports up to 3 images".to_string());
    }

    let mut body = json!({
        "modelId": request.model.clone(),
        "prompt": prompt,
        "images": images,
    });

    if let Some(value) = request.resolution.filter(|value| !value.trim().is_empty()) {
        body["resolution"] = json!(value);
    }
    if let Some(value) = request
        .aspect_ratio
        .filter(|value| !value.trim().is_empty())
    {
        body["aspect_ratio"] = json!(value);
    }
    if let Some(value) = request.safe_mode {
        body["safe_mode"] = json!(value);
    }

    let response = venice_post_json("/image/multi-edit", body.clone()).await?;
    save_binary_response(
        &app,
        response,
        "edits",
        &request.prompt,
        json!({
            "operation": "multi-edit",
            "model": request.model,
            "request": body
        }),
    )
    .await
}

fn video_control_option_supported(controls: Option<&Value>, key: &str, value: &str) -> bool {
    let Some(options) = controls.and_then(|controls| controls.get(key)) else {
        return true;
    };
    let Some(items) = options.as_array() else {
        return true;
    };

    items
        .iter()
        .filter_map(Value::as_str)
        .any(|option| option == value)
}

#[tauri::command]
async fn queue_video(app: AppHandle, request: QueueMediaRequest) -> Result<QueueResult, String> {
    let _permit = claim_direct_work(&app)?;
    queue_video_inner(&app, request).await
}

async fn queue_video_inner(
    app: &AppHandle,
    request: QueueMediaRequest,
) -> Result<QueueResult, String> {
    let cached_controls = read_model_cache(app)
        .video_models
        .into_iter()
        .find(|model| model.id == request.model)
        .map(|model| model.controls);

    let mut body = json!({
        "model": request.model.clone(),
        "prompt": request.prompt,
    });
    if let Some(value) = request
        .negative_prompt
        .filter(|value| !value.trim().is_empty())
    {
        body["negative_prompt"] = json!(value);
    }
    if let Some(value) = request
        .source_image
        .filter(|value| !value.trim().is_empty())
    {
        body["image_url"] = json!(value);
    }
    if let Some(value) = request
        .source_video
        .filter(|value| !value.trim().is_empty())
    {
        body["video_url"] = json!(value);
    }
    if let Some(value) = request
        .duration
        .filter(|value| !value.trim().is_empty())
        .filter(|value| {
            video_control_option_supported(cached_controls.as_ref(), "durationOptions", value)
        })
    {
        body["duration"] = json!(value);
    }
    if let Some(value) = request
        .resolution
        .filter(|value| !value.trim().is_empty())
        .filter(|value| {
            video_control_option_supported(cached_controls.as_ref(), "resolutionOptions", value)
        })
    {
        body["resolution"] = json!(value);
    }
    if let Some(value) = request
        .aspect_ratio
        .filter(|value| !value.trim().is_empty())
        .filter(|value| {
            video_control_option_supported(cached_controls.as_ref(), "aspectRatioOptions", value)
        })
    {
        body["aspect_ratio"] = json!(value);
    }
    if let Some(value) = request.upscale_factor {
        body["upscale_factor"] = json!(value);
    }

    let response = venice_post_json("/video/queue", body).await?;
    let payload: Value = bounded_response_json(response, 8 * 1024 * 1024).await?;
    let queue_id = first_string_field(&payload, &["id", "queue_id", "request_id"])
        .or_else(|| {
            payload
                .get("data")
                .and_then(|data| first_string_field(data, &["id", "queue_id", "request_id"]))
        })
        .unwrap_or("")
        .to_string();
    if queue_id.is_empty() {
        return Err(format!(
            "Venice video queue response did not include a queue id: {payload}"
        ));
    }
    let status = json_status_label(&payload);
    let download_url = first_string_field(&payload, &["download_url", "url"])
        .or_else(|| {
            payload
                .get("data")
                .and_then(|data| first_string_field(data, &["download_url", "url"]))
        })
        .unwrap_or("")
        .to_string();
    Ok(QueueResult {
        queue_id,
        status,
        progress_label: "Queued".to_string(),
        download_url,
        raw: payload,
    })
}

#[tauri::command]
async fn retrieve_video(
    app: AppHandle,
    request: RetrieveRequest,
) -> Result<RetrieveResult, String> {
    let _permit = claim_direct_work(&app)?;
    retrieve_queued_media(app, request, "/video/retrieve", "videos").await
}

#[tauri::command]
async fn queue_audio(app: AppHandle, request: QueueMediaRequest) -> Result<QueueResult, String> {
    let _permit = claim_direct_work(&app)?;
    let (
        supports_duration_seconds,
        supports_lyrics,
        supports_instrumental,
        supports_lyrics_optimizer,
    ) = audio_model_parameter_support(&app, &request.model);
    let (min_duration_seconds, max_duration_seconds) =
        audio_model_duration_limits(&app, &request.model);
    let mut body = json!({
        "model": request.model,
        "prompt": request.prompt,
    });
    if supports_duration_seconds {
        if let Some(value) = request
            .duration_seconds
            .or(request.duration)
            .filter(|value| !value.trim().is_empty())
        {
            let duration_seconds = value
                .trim()
                .parse::<f64>()
                .map_err(|_| "Duration seconds must be a number".to_string())?;
            if duration_seconds <= 0.0 {
                return Err("Duration seconds must be greater than 0".to_string());
            }
            if let Some(min) = min_duration_seconds {
                if duration_seconds < min {
                    return Err(format!(
                        "Duration seconds must be at least {} for {}",
                        format_number_for_message(min),
                        request.model
                    ));
                }
            }
            if let Some(max) = max_duration_seconds {
                if duration_seconds > max {
                    return Err(format!(
                        "Duration seconds must be {} or less for {}",
                        format_number_for_message(max),
                        request.model
                    ));
                }
            }
            body["duration_seconds"] = json!(duration_seconds);
        }
    }
    if supports_instrumental && request.force_instrumental.unwrap_or(false) {
        body["force_instrumental"] = json!(true);
    }
    if supports_lyrics {
        if let Some(value) = request
            .lyrics_prompt
            .filter(|value| !value.trim().is_empty())
        {
            body["lyrics_prompt"] = json!(value);
        }
    }
    if supports_lyrics_optimizer && request.lyrics_optimizer.unwrap_or(false) {
        body["lyrics_optimizer"] = json!(true);
    }

    let response = venice_post_json("/audio/queue", body).await?;
    let payload: Value = bounded_response_json(response, 8 * 1024 * 1024).await?;
    let queue_id = first_string_field(&payload, &["id", "queue_id", "request_id"])
        .or_else(|| {
            payload
                .get("data")
                .and_then(|data| first_string_field(data, &["id", "queue_id", "request_id"]))
        })
        .unwrap_or("")
        .to_string();
    if queue_id.is_empty() {
        return Err(format!(
            "Venice audio queue response did not include a queue id: {payload}"
        ));
    }
    let status = json_status_label(&payload);
    let download_url = first_string_field(&payload, &["download_url", "url"])
        .or_else(|| {
            payload
                .get("data")
                .and_then(|data| first_string_field(data, &["download_url", "url"]))
        })
        .unwrap_or("")
        .to_string();
    Ok(QueueResult {
        queue_id,
        status,
        progress_label: "Queued".to_string(),
        download_url,
        raw: payload,
    })
}

#[tauri::command]
async fn retrieve_audio(
    app: AppHandle,
    request: RetrieveRequest,
) -> Result<RetrieveResult, String> {
    let _permit = claim_direct_work(&app)?;
    let default_kind = if matches!(request.kind.as_deref(), Some("sfx")) {
        "sfx"
    } else {
        "audio"
    };
    retrieve_queued_media(app, request, "/audio/retrieve", default_kind).await
}

async fn retrieve_queued_media(
    app: AppHandle,
    request: RetrieveRequest,
    endpoint: &str,
    default_kind: &str,
) -> Result<RetrieveResult, String> {
    let mut body = json!({
        "queue_id": request.queue_id,
        "delete_media_on_completion": false,
    });
    if let Some(model) = request
        .model
        .clone()
        .filter(|value| !value.trim().is_empty())
    {
        body["model"] = json!(model);
    }
    let response = venice_post_json(endpoint, body).await?;
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    if !content_type.contains("json") {
        let result = save_binary_response(
            &app,
            response,
            default_kind,
            request.model.as_deref().unwrap_or(default_kind),
            json!({ "queueId": request.queue_id, "kind": request.kind, "model": request.model }),
        )
        .await?;
        return Ok(RetrieveResult {
            status: "completed".to_string(),
            progress_label: "Completed".to_string(),
            result: Some(result),
            raw: Value::Null,
        });
    }

    let payload: Value = bounded_response_json(response, 8 * 1024 * 1024).await?;
    let status = json_status_label(&payload);
    let prompt = request.model.as_deref().unwrap_or(default_kind);
    let download_url = request
        .download_url
        .filter(|value| !value.trim().is_empty())
        .or_else(|| first_string_field(&payload, &["download_url", "url"]).map(ToString::to_string))
        .or_else(|| {
            payload
                .get("data")
                .and_then(|data| first_string_field(data, &["download_url", "url"]))
                .map(ToString::to_string)
        });

    if is_done_status(&status) {
        if let Some(url) = download_url {
            let parsed = reqwest::Url::parse(&url)
                .map_err(|_| "Venice download URL is invalid".to_string())?;
            if parsed.scheme() != "https" || parsed.host_str() != Some("api.venice.ai") {
                return Err(
                    "Refusing to forward Venice credentials to an untrusted download origin"
                        .to_string(),
                );
            }
            let key = read_api_key().map_err(|_| "Venice API key is not configured".to_string())?;
            let response = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(120))
                .build()
                .map_err(|err| err.to_string())?
                .get(url)
                .bearer_auth(key)
                .send()
                .await
                .map_err(|err| err.to_string())?;
            let response = ensure_success(response).await?;
            let result = save_binary_response(
                &app,
                response,
                default_kind,
                prompt,
                json!({ "raw": payload }),
            )
            .await?;
            return Ok(RetrieveResult {
                status,
                progress_label: "Completed".to_string(),
                result: Some(result),
                raw: payload,
            });
        }
    }

    Ok(RetrieveResult {
        status: status.clone(),
        progress_label: if is_done_status(&status) {
            "Completed"
        } else {
            "Processing"
        }
        .to_string(),
        result: None,
        raw: payload,
    })
}

#[tauri::command]
async fn transcribe_audio(
    app: AppHandle,
    request: TranscriptionRequest,
) -> Result<MediaResult, String> {
    let _permit = claim_direct_work(&app)?;
    let key = read_api_key().map_err(|_| "Venice API key is not configured".to_string())?;
    let (bytes, detected_mime) = decode_data_url(&request.audio)?;
    let file_name = request
        .file_name
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "audio".to_string());
    let mime = request
        .mime_type
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(detected_mime);

    let file_part = reqwest::multipart::Part::bytes(bytes)
        .file_name(file_name.clone())
        .mime_str(&mime)
        .map_err(|err| err.to_string())?;
    let mut form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("model", request.model.clone());

    let response_format = request
        .response_format
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "json".to_string());
    form = form.text("response_format", response_format.clone());

    if request.timestamps.unwrap_or(false) {
        form = form.text("timestamps", "true".to_string());
    }
    if let Some(language) = request.language.filter(|value| !value.trim().is_empty()) {
        form = form.text("language", language);
    }

    let response = client()?
        .post(format!("{}/audio/transcriptions", venice_base_url()))
        .bearer_auth(key)
        .multipart(form)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let response = ensure_success(response).await?;
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let (text, raw) = if content_type.contains("json") {
        let payload: Value = bounded_response_json(response, 8 * 1024 * 1024).await?;
        let text = first_string_field(&payload, &["text", "transcript"])
            .unwrap_or("")
            .to_string();
        (text, payload)
    } else {
        let text = String::from_utf8(bounded_response_bytes(response, 8 * 1024 * 1024).await?)
            .map_err(|err| err.to_string())?;
        (text.trim().to_string(), json!({ "text": text }))
    };

    if text.trim().is_empty() {
        return Err(format!(
            "Venice transcription response did not include transcript text: {raw}"
        ));
    }

    save_text_result(
        &app,
        "transcripts",
        &file_name,
        &text,
        json!({
            "model": request.model,
            "fileName": file_name,
            "mimeType": mime,
            "responseFormat": response_format,
            "raw": raw
        }),
    )
}

#[tauri::command]
async fn generate_speech(app: AppHandle, request: SpeechRequest) -> Result<MediaResult, String> {
    let _permit = claim_direct_work(&app)?;
    let response_format = request
        .response_format
        .clone()
        .unwrap_or_else(|| "mp3".to_string());
    let mut body = json!({
        "model": request.model.clone(),
        "input": request.input.clone(),
        "response_format": response_format,
        "streaming": false
    });
    if let Some(value) = request.voice.filter(|value| !value.trim().is_empty()) {
        body["voice"] = json!(value);
    }
    if let Some(value) = request.speed {
        body["speed"] = json!(value);
    }
    if let Some(value) = request.language.filter(|value| !value.trim().is_empty()) {
        body["language"] = json!(value);
    }
    if let Some(value) = request
        .style_prompt
        .filter(|value| !value.trim().is_empty())
    {
        body["style_prompt"] = json!(value);
    }
    if let Some(value) = request.temperature {
        body["temperature"] = json!(value);
    }
    if let Some(value) = request.top_p {
        body["top_p"] = json!(value);
    }

    let response = venice_post_json("/audio/speech", body.clone()).await?;
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    if content_type.contains("json") {
        let payload: Value = bounded_response_json(response, 8 * 1024 * 1024).await?;
        if let Some(encoded) = first_string_field(&payload, &["audio", "base64", "b64_json"]) {
            let mime = format!("audio/{}", response_format.trim().trim_start_matches('.'));
            let bytes = decode_base64_payload(encoded)?;
            return save_media_bytes(
                &app,
                "voice",
                &request.input,
                &mime,
                &bytes,
                json!({ "raw": payload }),
            );
        }
        return Err(format!(
            "Venice speech response did not include audio data: {payload}"
        ));
    }

    save_binary_response(
        &app,
        response,
        "voice",
        &request.input,
        json!({ "request": body }),
    )
    .await
}

#[derive(Clone)]
struct TauriMediaExecutor {
    app: AppHandle,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharedExecutionCache {
    results: Vec<MediaResult>,
    result: Value,
}

fn shared_execution_cache_path(app: &AppHandle, request_id: &str) -> Result<PathBuf, String> {
    let root = app_data_dir(app)?.join("provider-v2-execution");
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    Ok(root.join(format!("{request_id}.json")))
}

fn shared_execution_input(input: &venice_provider_kernel::ExecutionInput) -> Result<Value, String> {
    let mut value = input.operation.input.clone();
    let object = value
        .as_object_mut()
        .ok_or_else(|| "Operation input must be an object".to_string())?;
    let mut data_urls = input
        .artifacts
        .iter()
        .map(|(reference, bytes)| {
            format!(
                "data:{};base64,{}",
                reference.mime_type,
                general_purpose::STANDARD.encode(bytes)
            )
        })
        .collect::<Vec<_>>();
    match input.operation.capability.id.as_str() {
        "media.image.edit" => {
            object.insert("images".into(), json!(data_urls));
        }
        "media.image.background-remove" | "media.image.upscale" => {
            object.insert(
                "sourceImage".into(),
                json!(data_urls
                    .into_iter()
                    .next()
                    .ok_or_else(|| "A sealed source image is required".to_string())?),
            );
        }
        "media.video.generate" if !data_urls.is_empty() => {
            object.insert("sourceImage".into(), json!(data_urls.remove(0)));
        }
        "media.transcribe" => {
            object.insert(
                "audio".into(),
                json!(data_urls
                    .into_iter()
                    .next()
                    .ok_or_else(|| "A sealed source audio artifact is required".to_string())?),
            );
        }
        _ => {}
    }
    if let Some(duration) = object.get("durationSeconds").and_then(Value::as_f64) {
        object.insert("durationSeconds".into(), json!(duration.to_string()));
    }
    Ok(value)
}

fn shared_execution_result(
    input: &venice_provider_kernel::ExecutionInput,
    cache: SharedExecutionCache,
) -> Result<venice_provider_kernel::ExecutionResult, String> {
    let controls = Value::Object(
        input
            .operation
            .input
            .as_object()
            .into_iter()
            .flat_map(|object| object.iter())
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "model" | "prompt" | "title" | "catalogRevision"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    let model = input
        .operation
        .input
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| match input.operation.capability.id.as_str() {
            "media.image.background-remove" => "background-remove".into(),
            "media.image.upscale" => "upscale".into(),
            "media.models.list" => "model-list".into(),
            "media.models.refresh" => "model-refresh".into(),
            _ => "provider-default".into(),
        });
    let artifacts = cache
        .results
        .into_iter()
        .map(|result| {
            let bytes = fs::read(&result.file_path).map_err(|error| error.to_string())?;
            let kind = match result.kind.as_str() {
                "images" | "edits" => "image",
                "videos" => "video",
                "audio" => "music",
                "sfx" => "sound-effect",
                "transcripts" => "transcript",
                other => other,
            }
            .to_string();
            Ok(venice_provider_kernel::ExecutionArtifact {
                kind,
                mime_type: result.mime_type,
                bytes,
                media: result.metadata,
                model: json!({"id":model}),
                controls: controls.clone(),
                recipe: json!({"prompt":input.operation.input.get("prompt")}),
                source_evidence: None,
                source_path: Some(PathBuf::from(result.file_path)),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(venice_provider_kernel::ExecutionResult {
        artifacts,
        output: cache.result.clone(),
        result: cache.result,
    })
}

#[async_trait::async_trait]
impl venice_provider_kernel::Executor for TauriMediaExecutor {
    async fn validate(
        &self,
        capability: &venice_provider_kernel::CapabilityRef,
        input: &Value,
    ) -> Result<(), String> {
        validate_operation_model_controls_for_admission(&self.app, &capability.id, input)
    }

    async fn submit(
        &self,
        execution: venice_provider_kernel::ExecutionInput,
        provider_request_id: &str,
    ) -> Result<venice_provider_kernel::SubmissionReceipt, String> {
        WORK_ALREADY_ADMITTED
            .scope(true, async {
                let input = shared_execution_input(&execution)?;
                validate_typed_provider_input(&execution.operation.capability.id, &input)?;
                let (results, result, upstream_id) =
                    match execution.operation.capability.id.as_str() {
                        "media.models.list" => (
                            vec![],
                            model_catalog(&self.app),
                            provider_request_id.to_string(),
                        ),
                        "media.models.refresh" => {
                            refresh_models_inner(&self.app).await?;
                            (
                                vec![],
                                model_catalog(&self.app),
                                provider_request_id.to_string(),
                            )
                        }
                        "media.image.generate" => {
                            let request: ImageGenerationRequest =
                                serde_json::from_value(input).map_err(|error| error.to_string())?;
                            (
                                generate_image(self.app.clone(), request).await?,
                                Value::Null,
                                provider_request_id.to_string(),
                            )
                        }
                        "media.image.edit" => {
                            let request: ImageMultiEditRequest =
                                serde_json::from_value(input).map_err(|error| error.to_string())?;
                            (
                                vec![multi_edit_image(self.app.clone(), request).await?],
                                Value::Null,
                                provider_request_id.to_string(),
                            )
                        }
                        "media.image.background-remove" => {
                            let request: BackgroundRemoveRequest =
                                serde_json::from_value(input).map_err(|error| error.to_string())?;
                            (
                                vec![remove_background(self.app.clone(), request).await?],
                                Value::Null,
                                provider_request_id.to_string(),
                            )
                        }
                        "media.image.upscale" => {
                            let request: ImageUpscaleRequest =
                                serde_json::from_value(input).map_err(|error| error.to_string())?;
                            (
                                vec![upscale_image(self.app.clone(), request).await?],
                                Value::Null,
                                provider_request_id.to_string(),
                            )
                        }
                        "media.voice.generate" => {
                            let request: SpeechRequest =
                                serde_json::from_value(input).map_err(|error| error.to_string())?;
                            (
                                vec![generate_speech(self.app.clone(), request).await?],
                                Value::Null,
                                provider_request_id.to_string(),
                            )
                        }
                        "media.transcribe" => {
                            let request: TranscriptionRequest =
                                serde_json::from_value(input).map_err(|error| error.to_string())?;
                            (
                                vec![transcribe_audio(self.app.clone(), request).await?],
                                Value::Null,
                                provider_request_id.to_string(),
                            )
                        }
                        "media.video.generate"
                        | "media.audio.music.generate"
                        | "media.audio.sfx.generate" => {
                            let request: QueueMediaRequest =
                                serde_json::from_value(input).map_err(|error| error.to_string())?;
                            let queued =
                                if execution.operation.capability.id == "media.video.generate" {
                                    queue_video_inner(&self.app, request).await?
                                } else {
                                    queue_audio(self.app.clone(), request).await?
                                };
                            (vec![], Value::Null, queued.queue_id)
                        }
                        _ => return Err("Capability is unavailable".into()),
                    };
                if !results.is_empty() || !result.is_null() {
                    let cache = SharedExecutionCache { results, result };
                    atomic_write_bytes(
                        &shared_execution_cache_path(&self.app, provider_request_id)?,
                        &serde_json::to_vec(&cache).map_err(|error| error.to_string())?,
                    )?;
                }
                Ok(venice_provider_kernel::SubmissionReceipt {
                    upstream_id,
                    certainty: "submitted_confirmed".into(),
                })
            })
            .await
    }

    async fn resume(
        &self,
        execution: venice_provider_kernel::ExecutionInput,
        upstream_id: &str,
    ) -> Result<venice_provider_kernel::ExecutionResult, String> {
        WORK_ALREADY_ADMITTED
            .scope(true, async {
                if matches!(
                    execution.operation.capability.id.as_str(),
                    "media.video.generate"
                        | "media.audio.music.generate"
                        | "media.audio.sfx.generate"
                ) {
                    loop {
                        tokio::time::sleep(Duration::from_secs(7)).await;
                        let kind = execution.operation.capability.id.as_str();
                        let request = RetrieveRequest {
                            queue_id: upstream_id.to_string(),
                            model: execution
                                .operation
                                .input
                                .get("model")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            kind: if kind == "media.audio.sfx.generate" {
                                Some("sfx".into())
                            } else {
                                None
                            },
                            download_url: None,
                        };
                        let retrieved = if kind == "media.video.generate" {
                            retrieve_queued_media(
                                self.app.clone(),
                                request,
                                "/video/retrieve",
                                "videos",
                            )
                            .await?
                        } else {
                            retrieve_queued_media(
                                self.app.clone(),
                                request,
                                "/audio/retrieve",
                                if kind == "media.audio.sfx.generate" {
                                    "sfx"
                                } else {
                                    "audio"
                                },
                            )
                            .await?
                        };
                        if let Some(result) = retrieved.result {
                            return shared_execution_result(
                                &execution,
                                SharedExecutionCache {
                                    results: vec![result],
                                    result: Value::Null,
                                },
                            );
                        }
                    }
                }
                let request_id = execution
                    .operation
                    .provider_request_id
                    .as_deref()
                    .ok_or_else(|| "Provider request identity is missing".to_string())?;
                let cache: SharedExecutionCache = serde_json::from_slice(
                    &fs::read(shared_execution_cache_path(&self.app, request_id)?)
                        .map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
                shared_execution_result(&execution, cache)
            })
            .await
    }

    async fn finalize_artifact(
        &self,
        operation: &venice_provider_kernel::ProviderOperation,
        artifact_id: &str,
        sha256: &str,
        byte_size: u64,
        mut artifact: venice_provider_kernel::ExecutionArtifact,
    ) -> Result<venice_provider_kernel::ExecutionArtifact, String> {
        let media_path = artifact
            .source_path
            .as_ref()
            .ok_or_else(|| "Provider media path is missing".to_string())?;
        let sidecar_path = media_sidecar_path(media_path)
            .ok_or_else(|| "Provider sidecar path is invalid".to_string())?;
        let sidecar_bytes = fs::read(&sidecar_path).map_err(|error| error.to_string())?;
        if sidecar_bytes.len() > 64 * 1024 {
            return Err("Media sidecar exceeds the 64 KiB safety limit".to_string());
        }
        let mut sidecar: Value =
            serde_json::from_slice(&sidecar_bytes).map_err(|error| error.to_string())?;
        validate_media_sidecar_schema(&sidecar)?;
        sidecar["providerArtifactId"] = json!(artifact_id);
        sidecar["providerOperationId"] = json!(operation.provider_operation_id);
        sidecar["sha256"] = json!(sha256);
        sidecar["byteSize"] = json!(byte_size);
        sidecar["controls"] = artifact.controls.clone();
        sidecar["recipe"] = artifact.recipe.clone();
        sidecar["sourceArtifacts"]=json!(operation.input_artifacts.iter().map(|source|json!({"coreArtifactId":source.core_artifact_id,"relationship":source.relationship,"sha256":source.sha256})).collect::<Vec<_>>());
        atomic_write_bytes(
            &sidecar_path,
            &serde_json::to_vec_pretty(&sidecar).map_err(|error| error.to_string())?,
        )?;
        let verified: Value =
            serde_json::from_slice(&fs::read(&sidecar_path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        for (key, expected) in [
            ("providerArtifactId", json!(artifact_id)),
            (
                "providerOperationId",
                json!(operation.provider_operation_id),
            ),
            ("sha256", json!(sha256)),
            ("byteSize", json!(byte_size)),
        ] {
            if verified.get(key) != Some(&expected) {
                return Err(format!("Provider sidecar {key} verification failed"));
            }
        }
        let sanitized = venice_provider_kernel::canonical_json(&verified);
        artifact.source_evidence = Some(
            json!({"schemaIdentity":"nekolegends.media-sidecar","schemaVersion":1,"sanitizedSha256":venice_provider_kernel::canonical_digest(&sanitized).map_err(|error|error.to_string())?,"sanitizedSidecar":sanitized}),
        );
        Ok(artifact)
    }
}

fn model_catalog(app: &AppHandle) -> Value {
    let cache = read_model_cache(app);
    let source = match cache.catalog_source.as_str() {
        "live" => "live",
        "cached" => "cached",
        _ if cache.last_fetched.trim().is_empty() => "fallback",
        _ => "cached",
    };
    let refreshed_at = cache.last_fetched.clone();
    let errors = cache.category_errors.clone();
    let groups = [
        ("image", cache.image_models),
        ("image-edit", cache.edit_models),
        ("video", cache.video_models),
        ("music", cache.music_models),
        ("sfx", cache.sfx_models),
        ("voice", cache.voice_models),
        ("transcription", cache.transcribe_models),
    ];
    let models = groups
        .into_iter()
        .flat_map(|(kind, models)| {
            models.into_iter().map(move |model| {
                let raw_digest = provider::sha256_hex(
                    serde_json::to_string(&model.raw)
                        .unwrap_or_default()
                        .as_bytes(),
                );
                let capability_ids = match kind {
                    "image" => vec!["media.image.generate"],
                    "image-edit" => vec!["media.image.edit"],
                    "video" => vec!["media.video.generate"],
                    "music" => vec!["media.audio.music.generate"],
                    "sfx" => vec!["media.audio.sfx.generate"],
                    "voice" => vec!["media.voice.generate"],
                    "transcription" => vec!["media.transcribe"],
                    _ => vec![],
                };
                json!({
                    "id": model.id,
                    "displayName": model.name,
                    "mediaKind": kind,
                    "modes": model.modes,
                    "capabilityIds": capability_ids,
                    "available": source != "fallback",
                    "controlsSchema": controls_to_schema(kind, &model.controls),
                    "resourceEstimateSchema": {"type":"object","properties":{},"additionalProperties":false},
                    "rawProviderMetadataDigest": raw_digest
                })
            })
        })
        .chain([
            json!({"id":"background-remove","displayName":"Background removal","mediaKind":"image","capabilityIds":["media.image.background-remove"],"available":true,"controlsSchema":{"type":"object","properties":{},"additionalProperties":false}}),
            json!({"id":"upscale","displayName":"Image upscale","mediaKind":"image","capabilityIds":["media.image.upscale"],"available":true,"controlsSchema":{"type":"object","properties":{"scale":{"enum":[2,4]}},"required":["scale"],"additionalProperties":false}}),
            json!({"id":"model-list","displayName":"Model catalog","mediaKind":"models","capabilityIds":["media.models.list"],"available":true,"controlsSchema":{"type":"object","properties":{},"additionalProperties":false}}),
            json!({"id":"model-refresh","displayName":"Model catalog refresh","mediaKind":"models","capabilityIds":["media.models.refresh"],"available":true,"controlsSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        ])
        .collect::<Vec<_>>();
    let normalized = serde_json::to_vec(&models).unwrap_or_default();
    json!({
        "schemaVersion": "1.0",
        "catalogRevision": format!("sha256:{}", provider::sha256_hex(&normalized)),
        "source": source,
        "refreshedAt": if refreshed_at.is_empty() { Value::Null } else { json!(refreshed_at) },
        "partial": !errors.is_empty(),
        "errors": if source == "fallback" { json!([{"code":"MODEL_CATALOG_FALLBACK_ONLY","message":"No live or cached catalog is available"}]) } else { json!(errors) },
        "models": models
    })
}

fn controls_to_schema(media_kind: &str, controls: &Value) -> Value {
    let mut properties = serde_json::Map::new();
    let option = |key: &str| controls.get(key).and_then(Value::as_array).cloned();
    match media_kind {
        "image" => {
            if controls
                .get("negativePrompt")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                properties.insert("negativePrompt".into(), json!({"type":"string"}));
            }
            if controls
                .get("steps")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                properties.insert("steps".into(), json!({"type":"integer","minimum":1}));
            }
            if controls
                .get("cfg")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                properties.insert("cfgScale".into(), json!({"type":"number"}));
            }
            if controls
                .get("seed")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                properties.insert("seed".into(), json!({"type":"integer","minimum":0}));
            }
            if controls
                .get("hideWatermark")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                properties.insert("hideWatermark".into(), json!({"type":"boolean"}));
            }
            properties.insert("safeMode".into(), json!({"type":"boolean"}));
            properties.insert(
                "format".into(),
                json!({"type":"string","enum":["png","webp"]}),
            );
            properties.insert("navigate".into(), json!({"type":"boolean"}));
            if let Some(values) = option("sizeOptions") {
                properties.insert("aspectRatio".into(), json!({"type":"string","enum":values}));
            }
            if let Some(values) = option("resolutionOptions") {
                properties.insert("resolution".into(), json!({"type":"string","enum":values}));
            }
            if let Some(bounds) = controls.get("variantCount") {
                properties.insert("variants".into(), json!({"type":"integer","minimum":bounds.get("min").and_then(Value::as_u64).unwrap_or(1),"maximum":bounds.get("max").and_then(Value::as_u64).unwrap_or(4)}));
            }
        }
        "image-edit" => {
            properties.insert("safeMode".into(), json!({"type":"boolean"}));
            if let Some(values) = option("sizeOptions") {
                properties.insert("aspectRatio".into(), json!({"type":"string","enum":values}));
            }
            if let Some(values) = option("resolutionOptions") {
                properties.insert("resolution".into(), json!({"type":"string","enum":values}));
            }
        }
        "video" => {
            if let Some(values) = option("durationOptions") {
                properties.insert("duration".into(), json!({"type":"string","enum":values}));
            }
            if let Some(values) = option("resolutionOptions") {
                properties.insert("resolution".into(), json!({"type":"string","enum":values}));
            }
            if let Some(values) = option("aspectRatioOptions") {
                properties.insert("aspectRatio".into(), json!({"type":"string","enum":values}));
            }
            properties.insert("negativePrompt".into(), json!({"type":"string"}));
            properties.insert(
                "upscaleFactor".into(),
                json!({"type":"integer","minimum":1,"maximum":4}),
            );
        }
        "music" | "sfx" => {
            if controls
                .get("supportsDurationSeconds")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let bounds = controls.get("durationSeconds").unwrap_or(&Value::Null);
                properties.insert("durationSeconds".into(), json!({"type":"number","minimum":bounds.get("min").and_then(Value::as_f64).unwrap_or(1.0),"maximum":bounds.get("max").and_then(Value::as_f64).unwrap_or(180.0)}));
            }
            if controls
                .get("supportsLyrics")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                properties.insert("lyricsPrompt".into(), json!({"type":"string"}));
            }
            if controls
                .get("supportsInstrumental")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                properties.insert("forceInstrumental".into(), json!({"type":"boolean"}));
            }
            if controls
                .get("supportsLyricsOptimizer")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                properties.insert("lyricsOptimizer".into(), json!({"type":"boolean"}));
            }
        }
        "voice" => {
            if let Some(values) = option("responseFormats") {
                properties.insert(
                    "responseFormat".into(),
                    json!({"type":"string","enum":values}),
                );
            }
            for (key, support) in [
                ("voice", "supportsVoice"),
                ("language", "supportsLanguage"),
                ("stylePrompt", "supportsStylePrompt"),
            ] {
                if controls.get(support).and_then(Value::as_bool) == Some(true) {
                    properties.insert(key.into(), json!({"type":"string"}));
                }
            }
            for (key, support) in [
                ("speed", "supportsSpeed"),
                ("temperature", "supportsTemperature"),
                ("topP", "supportsTopP"),
            ] {
                if controls.get(support).and_then(Value::as_bool) == Some(true) {
                    properties.insert(key.into(), json!({"type":"number"}));
                }
            }
        }
        "transcription" => {
            if let Some(values) = option("responseFormats") {
                properties.insert(
                    "responseFormat".into(),
                    json!({"type":"string","enum":values}),
                );
            }
            if controls
                .get("supportsLanguage")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                properties.insert("language".into(), json!({"type":"string"}));
            }
            if controls
                .get("supportsTimestamps")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                properties.insert("timestamps".into(), json!({"type":"boolean"}));
            }
        }
        _ => {}
    }
    json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":properties,"additionalProperties":false})
}

fn validate_operation_model_controls_for_admission(
    app: &AppHandle,
    capability_id: &str,
    input: &Value,
) -> Result<(), String> {
    let Some(model_id) = input.get("model").and_then(Value::as_str) else {
        return Ok(());
    };
    let cache = read_model_cache(app);
    if cache.last_fetched.trim().is_empty() {
        return Err(
            "CAPABILITY_NOT_AVAILABLE: live or cached model catalog is required".to_string(),
        );
    }
    let models = match capability_id {
        "media.image.generate" => &cache.image_models,
        "media.image.edit" => &cache.edit_models,
        "media.video.generate" => &cache.video_models,
        "media.audio.music.generate" => &cache.music_models,
        "media.audio.sfx.generate" => &cache.sfx_models,
        "media.voice.generate" => &cache.voice_models,
        "media.transcribe" => &cache.transcribe_models,
        _ => return Ok(()),
    };
    let model = models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or_else(|| {
            format!("MODEL_NOT_FOUND: model {model_id} is not in the selected catalog")
        })?;
    let controls = &model.controls;
    let check_option = |input_key: &str, controls_key: &str| -> Result<(), String> {
        let Some(value) = input.get(input_key).and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(options) = controls.get(controls_key).and_then(Value::as_array) else {
            return Err(format!(
                "CONTROL_NOT_SUPPORTED: {input_key} is not supported by {model_id}"
            ));
        };
        if options.iter().any(|option| option.as_str() == Some(value)) {
            Ok(())
        } else {
            Err(format!(
                "CONTROL_NOT_SUPPORTED: {input_key} is not supported by {model_id}"
            ))
        }
    };
    check_option("resolution", "resolutionOptions")?;
    if controls.get("aspectRatioOptions").is_some() {
        check_option("aspectRatio", "aspectRatioOptions")?;
    } else {
        check_option("aspectRatio", "sizeOptions")?;
    }
    check_option("duration", "durationOptions")?;
    check_option("responseFormat", "responseFormats")?;

    for (input_key, support_key) in [
        ("durationSeconds", "supportsDurationSeconds"),
        ("lyricsPrompt", "supportsLyrics"),
        ("forceInstrumental", "supportsInstrumental"),
        ("lyricsOptimizer", "supportsLyricsOptimizer"),
        ("voice", "supportsVoice"),
        ("speed", "supportsSpeed"),
        ("language", "supportsLanguage"),
        ("stylePrompt", "supportsStylePrompt"),
        ("timestamps", "supportsTimestamps"),
        ("temperature", "supportsTemperature"),
        ("topP", "supportsTopP"),
        ("negativePrompt", "negativePrompt"),
        ("steps", "steps"),
        ("cfgScale", "cfg"),
        ("seed", "seed"),
        ("hideWatermark", "hideWatermark"),
    ] {
        if input.get(input_key).is_some()
            && controls.get(support_key).and_then(Value::as_bool) != Some(true)
        {
            return Err(format!(
                "CONTROL_NOT_SUPPORTED: {input_key} is not supported by {model_id}"
            ));
        }
    }
    if let Some(duration) = input.get("durationSeconds").and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    }) {
        if let Some(bounds) = controls.get("durationSeconds") {
            let minimum = bounds.get("min").and_then(Value::as_f64).unwrap_or(0.0);
            let maximum = bounds
                .get("max")
                .and_then(Value::as_f64)
                .unwrap_or(f64::MAX);
            if duration < minimum || duration > maximum {
                return Err(format!(
                    "CONTROL_NOT_SUPPORTED: durationSeconds is outside the model range"
                ));
            }
        }
    }
    if let Some(variants) = input.get("variants").and_then(Value::as_u64) {
        let bounds = controls.get("variantCount").ok_or_else(|| {
            format!("CONTROL_NOT_SUPPORTED: variants is not supported by {model_id}")
        })?;
        let minimum = bounds.get("min").and_then(Value::as_u64).unwrap_or(1);
        let maximum = bounds.get("max").and_then(Value::as_u64).unwrap_or(1);
        if variants < minimum || variants > maximum {
            return Err("CONTROL_NOT_SUPPORTED: variants is outside the model range".to_string());
        }
    }
    Ok(())
}

fn validate_typed_provider_input(capability: &str, input: &Value) -> Result<(), String> {
    let invalid = |error: serde_json::Error| format!("CONTROL_NOT_SUPPORTED: {error}");
    match capability {
        "media.image.generate" => serde_json::from_value::<ImageGenerationRequest>(input.clone())
            .map(|_| ())
            .map_err(invalid),
        "media.image.edit" => serde_json::from_value::<ImageMultiEditRequest>(input.clone())
            .map(|_| ())
            .map_err(invalid),
        "media.image.background-remove" => {
            serde_json::from_value::<BackgroundRemoveRequest>(input.clone())
                .map(|_| ())
                .map_err(invalid)
        }
        "media.image.upscale" => serde_json::from_value::<ImageUpscaleRequest>(input.clone())
            .map(|_| ())
            .map_err(invalid),
        "media.video.generate" | "media.audio.music.generate" | "media.audio.sfx.generate" => {
            serde_json::from_value::<QueueMediaRequest>(input.clone())
                .map(|_| ())
                .map_err(invalid)
        }
        "media.voice.generate" => serde_json::from_value::<SpeechRequest>(input.clone())
            .map(|_| ())
            .map_err(invalid),
        "media.transcribe" => serde_json::from_value::<TranscriptionRequest>(input.clone())
            .map(|_| ())
            .map_err(invalid),
        "media.models.list" | "media.models.refresh"
            if input.as_object().is_some_and(|object| object.is_empty()) =>
        {
            Ok(())
        }
        "media.models.list" | "media.models.refresh" => {
            Err("CONTROL_NOT_SUPPORTED: model operations accept no input fields".to_string())
        }
        _ => Err("CAPABILITY_NOT_AVAILABLE: capability is unavailable".to_string()),
    }
}

// === AI Agent Remote Control HTTP Server ===
// Supports live toggle: when the user turns "AI Agent Control" on in Settings,
// the server starts immediately. When turned off, it shuts down gracefully.
// Off by default.

#[derive(Clone)]
struct AgentControlState {
    app: AppHandle,
    token: String,
    previous_token: Option<(String, SystemTime)>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentResultGroupPayload {
    title: String,
    results: Vec<MediaResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentRequest<T> {
    #[serde(flatten)]
    request: T,
    navigate: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentNavigateRequest {
    mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentMoveToBurnRequest {
    paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentBurnFolderRequest {
    seed: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentNavigatePayload {
    mode: String,
    status: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentQueuePayload {
    kind: String,
    queue_id: String,
    status: String,
    progress_label: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentRemoveResultPathsPayload {
    paths: Vec<String>,
    status: String,
}

type AgentApiError = (StatusCode, String);

tokio::task_local! {
    static WORK_ALREADY_ADMITTED: bool;
}

fn claim_direct_work(
    app: &AppHandle,
) -> Result<Option<venice_provider_kernel::CompatibilityPermit>, String> {
    if WORK_ALREADY_ADMITTED
        .try_with(|value| *value)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    app.state::<AgentControlHandle>()
        .admission
        .claim_compatibility()
        .map(Some)
        .map_err(str::to_string)
}

async fn reject_mutation_while_shutting_down(
    State(admission): State<venice_provider_kernel::AdmissionController>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() == axum::http::Method::GET {
        return next.run(request).await;
    }
    let _permit = match admission.claim_compatibility() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": {
                        "code": "APPLICATION_SHUTTING_DOWN",
                        "message": "Application shutdown has already been accepted",
                        "retryable": false,
                        "submissionCertainty": "not_submitted",
                        "details": {}
                    }
                })),
            )
                .into_response()
        }
    };
    WORK_ALREADY_ADMITTED.scope(true, next.run(request)).await
}

fn check_agent_token(state: &AgentControlState, headers: &HeaderMap) -> Result<(), AgentApiError> {
    let Some(auth) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return Err((StatusCode::UNAUTHORIZED, "Missing bearer token".to_string()));
    };
    let presented = auth.strip_prefix("Bearer ").unwrap_or("").as_bytes();
    let current_matches = provider::constant_time_eq(presented, state.token.as_bytes());
    let overlap_matches = state
        .previous_token
        .as_ref()
        .is_some_and(|(token, expires_at)| {
            SystemTime::now() <= *expires_at
                && provider::constant_time_eq(presented, token.as_bytes())
        });
    if current_matches || overlap_matches {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "Invalid bearer token".to_string()))
    }
}

fn agent_error(error: String) -> AgentApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, error)
}

fn validate_agent_mode(mode: &str) -> Result<String, AgentApiError> {
    match mode {
        "image" | "edit" | "video" | "music" | "sfx" | "voice" | "transcribe" | "models"
        | "settings" => Ok(mode.to_string()),
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("Invalid mode '{mode}'. Expected image, edit, video, music, sfx, voice, transcribe, models, or settings."),
        )),
    }
}

fn emit_agent_navigate(app: &AppHandle, mode: &str, status: &str) {
    let payload = AgentNavigatePayload {
        mode: mode.to_string(),
        status: status.to_string(),
    };
    if let Err(error) = app.emit("agent:navigate", payload) {
        eprintln!("[agent-control] Failed to emit navigation: {error}");
    }
}

fn emit_agent_results(app: &AppHandle, title: &str, results: Vec<MediaResult>) {
    let payload = AgentResultGroupPayload {
        title: title.to_string(),
        results,
    };
    if let Err(error) = app.emit("agent:result-group", payload) {
        eprintln!("[agent-control] Failed to emit result group: {error}");
    }
}

fn emit_agent_queue(app: &AppHandle, kind: &str, queue: &QueueResult) {
    let payload = AgentQueuePayload {
        kind: kind.to_string(),
        queue_id: queue.queue_id.clone(),
        status: queue.status.clone(),
        progress_label: queue.progress_label.clone(),
    };
    if let Err(error) = app.emit("agent:queue", payload) {
        eprintln!("[agent-control] Failed to emit queue: {error}");
    }
}

fn emit_agent_queue_status(
    app: &AppHandle,
    kind: &str,
    queue_id: &str,
    status: &str,
    progress_label: &str,
) {
    let payload = AgentQueuePayload {
        kind: kind.to_string(),
        queue_id: queue_id.to_string(),
        status: status.to_string(),
        progress_label: progress_label.to_string(),
    };
    if let Err(error) = app.emit("agent:queue", payload) {
        eprintln!("[agent-control] Failed to emit queue status: {error}");
    }
}

fn emit_agent_remove_result_paths(app: &AppHandle, paths: Vec<String>, status: String) {
    let payload = AgentRemoveResultPathsPayload { paths, status };
    if let Err(error) = app.emit("agent:remove-result-paths", payload) {
        eprintln!("[agent-control] Failed to emit result removal: {error}");
    }
}

fn should_agent_navigate<T>(request: &AgentRequest<T>) -> bool {
    request.navigate.unwrap_or(true)
}

fn sanitize_agent_media_result(app: &AppHandle, mut result: MediaResult) -> MediaResult {
    if read_settings(app).private_session {
        result.file_path.clear();
    }
    result
}

fn sanitize_agent_media_results(app: &AppHandle, results: Vec<MediaResult>) -> Vec<MediaResult> {
    if !read_settings(app).private_session {
        return results;
    }
    results
        .into_iter()
        .map(|mut result| {
            result.file_path.clear();
            result
        })
        .collect()
}

fn sanitize_agent_retrieve_result(app: &AppHandle, mut result: RetrieveResult) -> RetrieveResult {
    if read_settings(app).private_session {
        if let Some(media) = result.result.as_mut() {
            media.file_path.clear();
        }
    }
    result
}

async fn agent_get_state(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let setup_sections = state.app.state::<StartupMetricsHandle>().sections();
    let app_state = collect_app_state(state.app.clone(), setup_sections).map_err(agent_error)?;
    let mut value =
        serde_json::to_value(app_state).map_err(|error| agent_error(error.to_string()))?;
    if let Some(settings) = value.get_mut("settings").and_then(Value::as_object_mut) {
        settings.remove("agentControlToken");
    }
    Ok(Json(value))
}

async fn agent_get_capabilities(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let settings = read_settings(&state.app);
    let address =
        agent_control_address(settings.agent_control_port, settings.agent_control_bind_all);
    Ok(Json(
        capability_manifest(&state.app, &address).map_err(agent_error)?,
    ))
}

async fn agent_get_health(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    Ok(Json(capability_health(&state.app).map_err(agent_error)?))
}

async fn agent_navigate(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentNavigateRequest>,
) -> Result<Json<serde_json::Value>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let mode = validate_agent_mode(payload.mode.trim())?;
    emit_agent_navigate(&state.app, &mode, &format!("Remote opened {mode}"));
    Ok(Json(json!({ "ok": true, "mode": mode })))
}

async fn agent_generate_image(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<ImageGenerationRequest>>,
) -> Result<Json<Vec<MediaResult>>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, "image", "Remote image generation started");
    }
    let results = generate_image(state.app.clone(), payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_results(&state.app, "Images · Remote", results.clone());
    Ok(Json(sanitize_agent_media_results(&state.app, results)))
}

async fn agent_refresh_models(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
) -> Result<Json<ModelCache>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let cache = refresh_models(state.app.clone())
        .await
        .map_err(agent_error)?;
    if let Err(error) = state.app.emit("agent:models", cache.clone()) {
        eprintln!("[agent-control] Failed to emit model cache: {error}");
    }
    Ok(Json(cache))
}

async fn agent_edit_image(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<ImageMultiEditRequest>>,
) -> Result<Json<MediaResult>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, "edit", "Remote image edit started");
    }
    let result = multi_edit_image(state.app.clone(), payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_results(&state.app, "Edit / Combine · Remote", vec![result.clone()]);
    Ok(Json(sanitize_agent_media_result(&state.app, result)))
}

async fn agent_remove_background(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<BackgroundRemoveRequest>>,
) -> Result<Json<MediaResult>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, "edit", "Remote background removal started");
    }
    let result = remove_background(state.app.clone(), payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_results(
        &state.app,
        "Background Removed · Remote",
        vec![result.clone()],
    );
    Ok(Json(sanitize_agent_media_result(&state.app, result)))
}

async fn agent_upscale_image(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<ImageUpscaleRequest>>,
) -> Result<Json<MediaResult>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, "edit", "Remote image upscale started");
    }
    let result = upscale_image(state.app.clone(), payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_results(&state.app, "Upscaled Image · Remote", vec![result.clone()]);
    Ok(Json(sanitize_agent_media_result(&state.app, result)))
}

async fn agent_queue_video(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<QueueMediaRequest>>,
) -> Result<Json<QueueResult>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, "video", "Remote video queue started");
    }
    let queue = queue_video_inner(&state.app, payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_queue(&state.app, "video", &queue);
    Ok(Json(queue))
}

async fn agent_retrieve_video(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<RetrieveRequest>>,
) -> Result<Json<RetrieveResult>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, "video", "Remote video retrieval started");
    }
    let queue_id = payload.request.queue_id.clone();
    let output = retrieve_video(state.app.clone(), payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_queue_status(
        &state.app,
        "video",
        &queue_id,
        &output.status,
        &output.progress_label,
    );
    if let Some(result) = output.result.clone() {
        emit_agent_results(&state.app, "Video · Remote", vec![result]);
    }
    Ok(Json(sanitize_agent_retrieve_result(&state.app, output)))
}

async fn agent_queue_music(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<QueueMediaRequest>>,
) -> Result<Json<QueueResult>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, "music", "Remote music queue started");
    }
    let queue = queue_audio(state.app.clone(), payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_queue(&state.app, "music", &queue);
    Ok(Json(queue))
}

async fn agent_queue_sfx(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<QueueMediaRequest>>,
) -> Result<Json<QueueResult>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, "sfx", "Remote SFX queue started");
    }
    let queue = queue_audio(state.app.clone(), payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_queue(&state.app, "sfx", &queue);
    Ok(Json(queue))
}

async fn agent_retrieve_audio(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<RetrieveRequest>>,
) -> Result<Json<RetrieveResult>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let mode = payload
        .request
        .kind
        .as_deref()
        .filter(|kind| *kind == "sfx")
        .unwrap_or("music")
        .to_string();
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, &mode, "Remote audio retrieval started");
    }
    let title = if mode == "sfx" {
        "SFX · Remote"
    } else {
        "Music · Remote"
    };
    let queue_id = payload.request.queue_id.clone();
    let output = retrieve_audio(state.app.clone(), payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_queue_status(
        &state.app,
        &mode,
        &queue_id,
        &output.status,
        &output.progress_label,
    );
    if let Some(result) = output.result.clone() {
        emit_agent_results(&state.app, title, vec![result]);
    }
    Ok(Json(sanitize_agent_retrieve_result(&state.app, output)))
}

async fn agent_generate_speech(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<SpeechRequest>>,
) -> Result<Json<MediaResult>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, "voice", "Remote voice generation started");
    }
    let result = generate_speech(state.app.clone(), payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_results(&state.app, "Voice · Remote", vec![result.clone()]);
    Ok(Json(sanitize_agent_media_result(&state.app, result)))
}

async fn agent_transcribe_audio(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentRequest<TranscriptionRequest>>,
) -> Result<Json<MediaResult>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if should_agent_navigate(&payload) {
        emit_agent_navigate(&state.app, "transcribe", "Remote transcription started");
    }
    let result = transcribe_audio(state.app.clone(), payload.request)
        .await
        .map_err(agent_error)?;
    emit_agent_results(&state.app, "Speech -> Text · Remote", vec![result.clone()]);
    Ok(Json(sanitize_agent_media_result(&state.app, result)))
}

async fn agent_open_output_folder(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let path = open_output_folder(state.app.clone()).map_err(agent_error)?;
    Ok(Json(json!({ "path": path })))
}

async fn agent_open_burn_folder(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let path = open_burn_folder(state.app.clone()).map_err(agent_error)?;
    Ok(Json(json!({ "path": path })))
}

async fn agent_clear_results(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    if let Err(error) = state.app.emit(
        "agent:clear-results",
        json!({ "status": "Remote cleared results" }),
    ) {
        eprintln!("[agent-control] Failed to emit clear results: {error}");
    }
    Ok(Json(json!({ "ok": true })))
}

async fn agent_move_to_burn(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentMoveToBurnRequest>,
) -> Result<Json<Vec<String>>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let moved = move_media_files_to_burn(state.app.clone(), payload.paths).map_err(agent_error)?;
    emit_agent_remove_result_paths(
        &state.app,
        moved.clone(),
        format!(
            "Remote moved {} file{} to the burn folder",
            moved.len(),
            if moved.len() == 1 { "" } else { "s" }
        ),
    );
    Ok(Json(moved))
}

async fn agent_get_burn_folder_stats(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
) -> Result<Json<BurnFolderStats>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let stats = get_burn_folder_stats(state.app.clone()).map_err(agent_error)?;
    Ok(Json(stats))
}

async fn agent_burn_folder(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
    Json(payload): Json<AgentBurnFolderRequest>,
) -> Result<Json<BurnFolderStats>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let stats = burn_folder(state.app.clone(), payload.seed).map_err(agent_error)?;
    Ok(Json(stats))
}

struct TauriShutdownHooks {
    app: AppHandle,
    kernel: venice_provider_kernel::Kernel,
    generation: u64,
}

async fn rollback_agent_control_startup(
    app: &AppHandle,
    kernel: &venice_provider_kernel::Kernel,
    generation: u64,
    server_task: Option<tokio::task::JoinHandle<Result<(), &'static str>>>,
) {
    if let Some(task) = server_task {
        task.abort();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }
    let _ = kernel.shutdown_resources().await;
    let _ = provider::unregister_lifecycle(app, generation).await;
    let _ = remove_agent_control_discovery(app, generation);
    let handle = app.state::<AgentControlHandle>();
    match handle.control.ownership.snapshot().0 {
        venice_provider_kernel::AgentControlPhase::Starting => {
            handle.control.ownership.fail_start(generation);
        }
        venice_provider_kernel::AgentControlPhase::Running => {
            let _ = handle.control.ownership.begin_stop();
            handle.control.ownership.finish_stop(generation);
        }
        venice_provider_kernel::AgentControlPhase::Stopping => {
            handle.control.ownership.finish_stop(generation);
        }
        venice_provider_kernel::AgentControlPhase::Stopped => {}
    }
    let _ = persist_agent_control_disabled_for_generation(app, generation).await;
}

#[async_trait::async_trait]
impl venice_provider_kernel::ShutdownHooks for TauriShutdownHooks {
    async fn release_resources(&self) -> Result<(), &'static str> {
        self.kernel.shutdown_resources().await
    }
    async fn unregister_lifecycle(&self) -> Result<&'static str, &'static str> {
        let outcome = provider::unregister_lifecycle(&self.app, self.generation).await;
        outcome.failure_code.map_or(Ok(outcome.outcome), Err)
    }
    async fn request_exit(&self) {
        self.app.exit(0);
    }
}

async fn start_agent_control_server(
    app: AppHandle,
    token: String,
    previous_token: Option<String>,
    port: u16,
    bind_all: bool,
) -> Result<(), String> {
    if token.is_empty() {
        return Err("No token configured".to_string());
    }
    let port = validate_agent_control_port(port)?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let (stopped_tx, stopped_rx) = oneshot::channel::<Result<(), String>>();
    let generation = app
        .state::<AgentControlHandle>()
        .control
        .reserve_start(
            port,
            AgentControlOwner {
                shutdown: shutdown_tx,
                completion: stopped_rx,
            },
        )
        .map_err(str::to_string)?;
    provider::set_lifecycle_generation(generation);

    let bind_host = agent_control_bind_host(bind_all);
    let addr: SocketAddr = format!("{bind_host}:{port}")
        .parse()
        .map_err(|error| format!("Invalid agent control bind address: {error}"))?;
    let std_listener = match StdTcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(error) => {
            app.state::<AgentControlHandle>()
                .control
                .ownership
                .fail_start(generation);
            return Err(if error.kind() == ErrorKind::AddrInUse {
                format!(
                "AI Agent Remote Control is already running on {addr}. Close the other Venice Media Local window or disable AI Agent Control there, then try again."
                )
            } else {
                format!("Failed to bind agent control server on {addr}: {error}")
            });
        }
    };
    if let Err(error) = std_listener.set_nonblocking(true) {
        app.state::<AgentControlHandle>()
            .control
            .ownership
            .fail_start(generation);
        return Err(format!(
            "Failed to configure agent control listener: {error}"
        ));
    }

    let discovery_token = token.clone();
    let state = AgentControlState {
        app: app.clone(),
        token,
        previous_token: previous_token
            .map(|token| (token, SystemTime::now() + Duration::from_secs(5 * 60))),
    };

    let server_app = app.clone();
    let (startup_tx, startup_rx) = oneshot::channel::<Result<(), String>>();
    tauri::async_runtime::spawn(async move {
        let app = server_app;
        let handle = app.state::<AgentControlHandle>();
        // Agent Control is not a browser API. A trusted browser origin can be
        // added explicitly later without restoring wildcard CORS.
        let cors = CorsLayer::new();

        let core_origin = provider::configured_core_origin(&app).unwrap_or_else(|| {
            eprintln!("[agent-control] Core callback origin is not configured; revision-2 callback admission remains closed");
            "http://127.0.0.1:0".to_string()
        });
        let manifest_digest = match provider::shared_manifest_digest(&app) {
            Ok(value) => value,
            Err(error) => {
                let _ = startup_tx.send(Err(format!("Provider manifest digest failed: {error}")));
                eprintln!("[agent-control] Provider manifest digest failed: {error}");
                handle.control.ownership.fail_start(generation);
                return;
            }
        };
        let (shutdown_action_tx, mut shutdown_action_rx) = mpsc::unbounded_channel();
        let kernel =
            match venice_provider_kernel::Kernel::open(venice_provider_kernel::KernelConfig {
                storage: std::sync::Arc::new(venice_provider_kernel::FileStorage::new(
                    match app_data_dir(&app) {
                        Ok(path) => path.join("provider-v2"),
                        Err(error) => {
                            let _ = startup_tx.send(Err(format!("Provider root failed: {error}")));
                            eprintln!("[agent-control] Provider root failed: {error}");
                            handle.control.ownership.fail_start(generation);
                            return;
                        }
                    },
                )),
                token: state.token.clone(),
                manifest_digest,
                trusted_callback_origin: core_origin,
                executor: std::sync::Arc::new(TauriMediaExecutor { app: app.clone() }),
                secret_protector: std::sync::Arc::new(provider::TauriSecretProtector::new(
                    app.clone(),
                )),
                callback_retry_base_ms: 2000,
                terminal_replay_window_ms: 5 * 60 * 1000,
                maintenance_interval_ms: 60 * 1000,
                provider_id: "venice-media-local".into(),
                instance_id: match installation_instance_id(&app) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = startup_tx
                            .send(Err(format!("Provider instance identity failed: {error}")));
                        eprintln!("[agent-control] Provider instance identity failed: {error}");
                        handle.control.ownership.fail_start(generation);
                        return;
                    }
                },
                shutdown_tx: Some(shutdown_action_tx),
                token_scopes: std::collections::BTreeSet::from([
                    venice_provider_kernel::SHUTDOWN_SCOPE.to_string(),
                ]),
                admission: handle.admission.clone(),
                ownership_generation: generation,
                terminal_shutdown: handle.control.terminal.clone(),
                shutdown_transaction: handle.transaction.clone(),
            })
            .await
            {
                Ok(kernel) => kernel,
                Err(error) => {
                    let _ = startup_tx.send(Err(format!("Provider kernel failed: {error}")));
                    eprintln!("[agent-control] Provider kernel failed: {error}");
                    handle.control.ownership.fail_start(generation);
                    return;
                }
            };

        let maintenance_kernel = kernel.clone();
        let compatibility_router = Router::new()
            .route("/api/v1/state", get(agent_get_state))
            .route("/api/v1/capabilities", get(agent_get_capabilities))
            .route("/api/v1/health", get(agent_get_health))
            .route("/api/v1/navigate", post(agent_navigate))
            .route("/api/v1/generate-image", post(agent_generate_image))
            .route("/api/v1/edit-image", post(agent_edit_image))
            .route("/api/v1/remove-background", post(agent_remove_background))
            .route("/api/v1/upscale-image", post(agent_upscale_image))
            .route("/api/v1/queue-video", post(agent_queue_video))
            .route("/api/v1/retrieve-video", post(agent_retrieve_video))
            .route("/api/v1/queue-music", post(agent_queue_music))
            .route("/api/v1/queue-sfx", post(agent_queue_sfx))
            .route("/api/v1/retrieve-audio", post(agent_retrieve_audio))
            .route("/api/v1/generate-speech", post(agent_generate_speech))
            .route("/api/v1/transcribe-audio", post(agent_transcribe_audio))
            .route("/api/v1/refresh-models", post(agent_refresh_models))
            .route("/api/v1/open-output-folder", post(agent_open_output_folder))
            .route("/api/v1/open-burn-folder", post(agent_open_burn_folder))
            .route("/api/v1/clear-results", post(agent_clear_results))
            .route("/api/v1/move-to-burn", post(agent_move_to_burn))
            .route(
                "/api/v1/burn-folder-stats",
                get(agent_get_burn_folder_stats),
            )
            .route("/api/v1/burn-folder", post(agent_burn_folder))
            .with_state(state)
            .layer(middleware::from_fn_with_state(
                kernel.admission(),
                reject_mutation_while_shutting_down,
            ));
        let router = compatibility_router
            .merge(kernel.clone().router())
            .layer(DefaultBodyLimit::max(100 * 1024 * 1024)) // 100 MB — supports 4K image payloads
            .layer(cors);

        println!(
            "[agent-control] Starting HTTP server on {} (for AI agents over Tailscale)",
            addr
        );

        let listener_result = TcpListener::from_std(std_listener)
            .map_err(|error| format!("Async listener failed: {error}"));
        let listener = match listener_result {
            Ok(listener) => listener,
            Err(error) => {
                let reported = error.clone();
                let app_for_rollback = app.clone();
                let kernel_for_rollback = maintenance_kernel.clone();
                let _ = venice_provider_kernel::run_post_kernel_startup(
                    || async { Err("LISTENER_CONVERSION_FAILED") },
                    || async move {
                        rollback_agent_control_startup(
                            &app_for_rollback,
                            &kernel_for_rollback,
                            generation,
                            None,
                        )
                        .await;
                    },
                )
                .await;
                let _ = startup_tx.send(Err(reported));
                return;
            }
        };

        if handle.control.ownership.snapshot().0
            != venice_provider_kernel::AgentControlPhase::Starting
            || handle.control.ownership.snapshot().1 != generation
        {
            let _ = startup_tx.send(Err("Agent Control startup ownership became stale".into()));
            rollback_agent_control_startup(&app, &maintenance_kernel, generation, None).await;
            return;
        }
        if let Err(code) = provider::start_lifecycle(app.clone(), generation).await {
            let _ = startup_tx.send(Err(format!("Lifecycle worker start failed: {code}")));
            eprintln!("[agent-control] Lifecycle worker start failed: {code}");
            rollback_agent_control_startup(&app, &maintenance_kernel, generation, None).await;
            return;
        }
        // Graceful shutdown when the user turns the toggle off
        let (drain_tx, drain_rx) = oneshot::channel::<()>();
        let server = axum::serve(listener, router).with_graceful_shutdown(async move {
            let _ = drain_rx.await;
        });
        let signal = async move {
            tokio::select! {
                _ = shutdown_rx => {
                    println!("[agent-control] Settings shutdown signal received, stopping HTTP server");
                }
                receipt = shutdown_action_rx.recv() => {
                    if let Some(receipt) = receipt {
                        let _ = receipt;
                        println!("[agent-control] Authenticated application shutdown accepted");
                    }
                }
            }
            let _ = drain_tx.send(());
        };

        let server_task =
            tokio::spawn(async move { server.await.map_err(|_| "SERVER_DRAIN_FAILED") });
        if let Err(error) = handle.control.ownership.publish_running(generation) {
            let _ = startup_tx.send(Err(error.to_string()));
            rollback_agent_control_startup(
                &app,
                &maintenance_kernel,
                generation,
                Some(server_task),
            )
            .await;
            return;
        }
        if let Err(error) = handle
            .control
            .terminal
            .ensure_open()
            .map_err(str::to_string)
            .and_then(|_| {
                write_agent_control_discovery(&app, &discovery_token, port, bind_all, generation)
            })
        {
            let _ = startup_tx.send(Err(format!("Discovery publication failed: {error}")));
            rollback_agent_control_startup(
                &app,
                &maintenance_kernel,
                generation,
                Some(server_task),
            )
            .await;
            return;
        }
        if startup_tx.send(Ok(())).is_err() {
            rollback_agent_control_startup(
                &app,
                &maintenance_kernel,
                generation,
                Some(server_task),
            )
            .await;
            return;
        }
        let server_result = venice_provider_kernel::run_until_shutdown_then_drain(
            signal,
            server_task,
            Duration::from_secs(20),
        )
        .await;
        let owned_generation = handle.control.ownership.snapshot().1 == generation;
        let completion = if maintenance_kernel.accepted_shutdown().is_some() {
            let hooks = TauriShutdownHooks {
                app: app.clone(),
                kernel: maintenance_kernel.clone(),
                generation: maintenance_kernel
                    .accepted_shutdown()
                    .map(|receipt| receipt.ownership_generation)
                    .unwrap_or(generation),
            };
            match maintenance_kernel
                .orchestrate_shutdown(server_result, &hooks)
                .await
            {
                venice_provider_kernel::TeardownOutcome::Exited => Ok(()),
                outcome => Err(format!("Authenticated shutdown did not exit: {outcome:?}")),
            }
        } else {
            match maintenance_kernel.shutdown_resources().await {
                Ok(()) => {
                    if owned_generation {
                        let outcome = provider::unregister_lifecycle(&app, generation).await;
                        if let Some(code) = outcome.failure_code {
                            Err(format!("Lifecycle unregister failed: {code}"))
                        } else {
                            Ok(())
                        }
                    } else {
                        Ok(())
                    }
                }
                Err(code) => Err(format!("Agent Control resource stop failed: {code}")),
            }
        };
        let _ = remove_agent_control_discovery(&app, generation);
        handle.control.ownership.finish_stop(generation);
        let _ = stopped_tx.send(completion);
    });
    match tokio::time::timeout(Duration::from_secs(20), startup_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("Agent Control startup owner exited before acknowledgment".into()),
        Err(_) => {
            let handle = app.state::<AgentControlHandle>();
            let _ = stop_agent_control_server(&handle).await;
            Err("Agent Control startup acknowledgment timed out".into())
        }
    }
}

async fn stop_agent_control_server(handle: &AgentControlHandle) -> Result<(), String> {
    let ownership = capture_agent_control_stop(handle)?;
    await_agent_control_stop(ownership).await
}

fn capture_agent_control_stop(
    handle: &AgentControlHandle,
) -> Result<Option<(u64, AgentControlOwner)>, String> {
    handle
        .control
        .ownership
        .begin_stop()
        .map_err(str::to_string)
}

async fn await_agent_control_stop(
    ownership: Option<(u64, AgentControlOwner)>,
) -> Result<(), String> {
    if let Some((_generation, owner)) = ownership {
        let _ = owner.shutdown.send(());
        println!("[agent-control] Shutdown requested for agent control server");
        return match tokio::time::timeout(Duration::from_secs(24), owner.completion).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Agent Control stop owner exited without completion".into()),
            Err(_) => Err("Agent Control stop completion timed out".into()),
        };
    }
    Ok(())
}

fn main() {
    if let Some(exit_code) = try_run_phase5h_migration_cli() {
        std::process::exit(exit_code);
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(AgentControlHandle::default())
        .manage(StartupMetricsHandle::default())
        .setup(|app| {
            let metrics = app.state::<StartupMetricsHandle>();
            let started_at = Instant::now();
            if let Err(err) = ensure_output_folders(app.handle()) {
                eprintln!("Failed to initialize output folders: {err}");
            }
            if let Err(err) = recover_media_commits(app.handle()) {
                eprintln!("Failed to recover media commits: {err}");
            }
            metrics.push("ensure output folders", started_at);

            let app_handle = app.handle().clone();
            let started_at = Instant::now();
            force_private_session_off_on_launch(&app_handle);
            metrics.push("reset private session launch state", started_at);

            let started_at = Instant::now();
            let agent_handle = app.state::<AgentControlHandle>();
            start_saved_agent_control_on_launch(app_handle.clone(), &*agent_handle);
            metrics.push("restore agent control launch state", started_at);

            let started_at = Instant::now();
            if let Some(window) = app.get_webview_window("main") {
                if let Err(err) = apply_initial_window_size(&app_handle, &window) {
                    eprintln!("Failed to initialize window size: {err}");
                }

                let resize_app = app_handle.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::Resized(size) = event {
                        let resize_app = resize_app.clone();
                        let size = *size;
                        tauri::async_runtime::spawn(async move {
                            if let Err(err) = persist_window_size(&resize_app, size).await {
                                eprintln!("Failed to save window size: {err}");
                            }
                        });
                    }
                });
            }
            metrics.push("apply window size and hooks", started_at);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_state,
            get_key_configured,
            get_agent_control_address,
            save_settings,
            rotate_agent_control_token,
            save_api_key,
            clear_api_key,
            configure_provider_lifecycle,
            clear_provider_lifecycle,
            get_models,
            move_media_files_to_burn,
            move_private_session_to_burn,
            copy_media_file,
            save_data_url_file,
            get_burn_folder_stats,
            get_diem_balance,
            open_output_folder,
            open_burn_folder,
            open_file_folder,
            burn_folder,
            refresh_models,
            check_for_update,
            open_update_release,
            download_update_installer,
            run_update_installer,
            generate_image,
            remove_background,
            upscale_image,
            multi_edit_image,
            queue_video,
            retrieve_video,
            queue_audio,
            retrieve_audio,
            generate_speech,
            transcribe_audio,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Venice Media Local");
    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::ExitRequested { .. }) {
            let generation = app_handle
                .state::<AgentControlHandle>()
                .control
                .ownership
                .snapshot()
                .1;
            tauri::async_runtime::block_on(provider::unregister_lifecycle(app_handle, generation));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct TestControlTokenStore {
        value: RefCell<Option<String>>,
        fail_write: bool,
        fail_read_after_write: bool,
        wrote: RefCell<bool>,
    }

    impl TestControlTokenStore {
        fn empty() -> Self {
            Self {
                value: RefCell::new(None),
                fail_write: false,
                fail_read_after_write: false,
                wrote: RefCell::new(false),
            }
        }
    }

    impl ControlTokenStore for TestControlTokenStore {
        fn read(&self) -> Result<Option<String>, String> {
            if self.fail_read_after_write && *self.wrote.borrow() {
                return Err("injected read failure".to_string());
            }
            Ok(self.value.borrow().clone())
        }

        fn write(&self, value: &str) -> Result<(), String> {
            if self.fail_write {
                return Err("injected write failure".to_string());
            }
            *self.wrote.borrow_mut() = true;
            *self.value.borrow_mut() = Some(value.to_string());
            Ok(())
        }
    }

    fn migration_settings(contents: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "venice-phase5h-migration-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&root).expect("create temp migration root");
        let path = root.join("settings.json");
        fs::write(&path, contents).expect("write migration settings");
        (root, path)
    }

    #[test]
    fn legacy_control_token_migrates_before_json_removal_without_disclosure() {
        let secret = "legacy-secret-value-that-must-not-leak";
        let (root, path) = migration_settings(&format!(
            "{{\"theme\":\"eva-dark\",\"agentControlToken\":\"{secret}\",\"future\":7}}"
        ));
        let store = TestControlTokenStore::empty();
        let outcome = migrate_legacy_control_token(&path, &store).expect("migration succeeds");
        assert_eq!(outcome, LegacyControlTokenMigration::ReplacementMigrated);
        assert_eq!(store.value.borrow().as_deref(), Some(secret));
        let sanitized = fs::read_to_string(&path).expect("read sanitized settings");
        assert!(!sanitized.contains(secret));
        assert!(!sanitized.contains("agentControlToken"));
        assert!(sanitized.contains("\"future\": 7"));
        fs::remove_dir_all(root).expect("remove temp migration root");
    }

    #[test]
    fn existing_secure_replacement_removes_obsolete_legacy_without_overwrite() {
        let (root, path) = migration_settings(
            "{\"agentControlToken\":\"obsolete-legacy\",\"theme\":\"eva-dark\"}",
        );
        let store = TestControlTokenStore {
            value: RefCell::new(Some("existing-replacement".to_string())),
            ..TestControlTokenStore::empty()
        };
        let outcome = migrate_legacy_control_token(&path, &store).expect("migration succeeds");
        assert_eq!(
            outcome,
            LegacyControlTokenMigration::ExistingReplacementProven
        );
        assert_eq!(
            store.value.borrow().as_deref(),
            Some("existing-replacement")
        );
        assert!(!*store.wrote.borrow());
        fs::remove_dir_all(root).expect("remove temp migration root");
    }

    #[test]
    fn failed_secure_replacement_preserves_legacy_settings_exactly() {
        let original = b"{\"agentControlToken\":\"still-needed\",\"theme\":\"eva-dark\"}";
        for store in [
            TestControlTokenStore {
                fail_write: true,
                ..TestControlTokenStore::empty()
            },
            TestControlTokenStore {
                fail_read_after_write: true,
                ..TestControlTokenStore::empty()
            },
        ] {
            let (root, path) = migration_settings(std::str::from_utf8(original).unwrap());
            assert!(migrate_legacy_control_token(&path, &store).is_err());
            assert_eq!(fs::read(&path).expect("read preserved settings"), original);
            fs::remove_dir_all(root).expect("remove temp migration root");
        }
    }

    #[test]
    fn migration_authorization_is_exact_current_jun_action() {
        let valid = json!({
            "user": {"id": "user-jun", "type": "human"},
            "trust": {
                "level": "verified_action",
                "needsReverification": false,
                "expiresAt": (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
                "action": {"key": PHASE5H_LEGACY_TOKEN_MIGRATION_ACTION}
            }
        });
        assert!(validate_phase5h_migration_session(&valid).is_ok());
        for pointer in ["/user/id", "/trust/level", "/trust/action/key"] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).expect("test pointer") = json!("wrong");
            assert!(validate_phase5h_migration_session(&invalid).is_err());
        }
        let mut expired = valid;
        expired["trust"]["expiresAt"] = json!("2000-01-01T00:00:00Z");
        assert!(validate_phase5h_migration_session(&expired).is_err());
    }

    #[test]
    fn migration_authorization_accepts_only_transport_bom_and_whitespace_prefix() {
        let authorization = "\u{feff}  synthetic-bearer\r\n"
            .trim()
            .trim_start_matches('\u{feff}')
            .trim_start();
        assert_eq!(authorization, "synthetic-bearer");
    }

    #[test]
    fn generic_file_stem_uses_timestamp_and_seed_only() {
        let metadata = json!({
            "seed": "12345",
            "title": "secret prompt title",
            "variantIndex": 9
        });
        assert_eq!(
            generic_file_stem(&metadata, "20260612-120000-000"),
            "20260612-120000-000-seed-12345"
        );
    }

    #[test]
    fn prompt_stem_is_not_used_for_empty_generic_metadata() {
        let metadata = json!({});
        assert_eq!(
            generic_file_stem(&metadata, "20260612-120000-000"),
            "20260612-120000-000"
        );
    }

    #[test]
    fn image_generation_request_accepts_camel_case_and_snake_case() {
        let camel_case: ImageGenerationRequest = serde_json::from_value(json!({
            "model": "gpt-image-2",
            "prompt": "cat knight",
            "negativePrompt": "text",
            "aspectRatio": "1:1",
            "resolution": "1K",
            "cfgScale": 7.0,
            "hideWatermark": true,
            "safeMode": false
        }))
        .expect("camelCase request should deserialize");
        assert_eq!(camel_case.aspect_ratio.as_deref(), Some("1:1"));
        assert_eq!(camel_case.negative_prompt.as_deref(), Some("text"));
        assert_eq!(camel_case.cfg_scale, Some(7.0));
        assert_eq!(camel_case.hide_watermark, Some(true));
        assert_eq!(camel_case.safe_mode, Some(false));

        let snake_case: ImageGenerationRequest = serde_json::from_value(json!({
            "model": "gpt-image-2",
            "prompt": "cat knight",
            "negative_prompt": "text",
            "aspect_ratio": "1:1",
            "resolution": "1K",
            "cfg_scale": 7.0,
            "hide_watermark": true,
            "safe_mode": false
        }))
        .expect("snake_case aliases should deserialize");
        assert_eq!(snake_case.aspect_ratio.as_deref(), Some("1:1"));
        assert_eq!(snake_case.negative_prompt.as_deref(), Some("text"));
        assert_eq!(snake_case.cfg_scale, Some(7.0));
        assert_eq!(snake_case.hide_watermark, Some(true));
        assert_eq!(snake_case.safe_mode, Some(false));
    }

    #[test]
    fn agent_media_requests_accept_snake_case_aliases() {
        let background: BackgroundRemoveRequest = serde_json::from_value(json!({
            "source_image": "data:image/webp;base64,abc"
        }))
        .expect("background remove snake_case alias should deserialize");
        assert_eq!(background.source_image, "data:image/webp;base64,abc");

        let edit: ImageMultiEditRequest = serde_json::from_value(json!({
            "model": "gpt-image-2-edit",
            "prompt": "make it square",
            "images": ["data:image/webp;base64,abc"],
            "aspect_ratio": "1:1",
            "safe_mode": false
        }))
        .expect("edit snake_case aliases should deserialize");
        assert_eq!(edit.aspect_ratio.as_deref(), Some("1:1"));
        assert_eq!(edit.safe_mode, Some(false));

        let retrieve: RetrieveRequest = serde_json::from_value(json!({
            "queue_id": "queue-123",
            "download_url": "https://example.com/result"
        }))
        .expect("retrieve snake_case aliases should deserialize");
        assert_eq!(retrieve.queue_id, "queue-123");
        assert_eq!(
            retrieve.download_url.as_deref(),
            Some("https://example.com/result")
        );
    }

    #[test]
    fn sidecar_schema_requires_complete_recipe_evidence() {
        let valid = json!({
            "schema":"nekolegends.media-sidecar","schemaVersion":1,"app":"venice-media-local",
            "kind":"image","createdAt":"2026-07-12T00:00:00Z",
            "recipe":{"prompt":"test","model":"model","controlsDigest":"a".repeat(64)}
        });
        assert!(validate_media_sidecar_schema(&valid).is_ok());
        let mut invalid = valid;
        invalid["recipe"] = json!({"prompt":"test"});
        assert!(validate_media_sidecar_schema(&invalid).is_err());
    }
}
