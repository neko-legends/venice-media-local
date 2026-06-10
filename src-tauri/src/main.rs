#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::{engine::general_purpose, Engine as _};
use chrono::{Local, Utc};
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    cmp::Ordering,
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{ErrorKind, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager, PhysicalSize, Size, WebviewWindow, WindowEvent};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower_http::cors::{Any, CorsLayer};

const VENICE_BASE_URL: &str = "https://api.venice.ai/api/v1";
const GITHUB_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/neko-legends/VeniceMediaLocal/releases/latest";
const KEYRING_SERVICE: &str = "venice-media-local";
const KEYRING_ACCOUNT: &str = "venice-api-key";
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    theme: String,
    output_dir: String,
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
    agent_control_token: Option<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: "eva-dark".to_string(),
            output_dir: String::new(),
            show_diem_balance: false,
            window_width: None,
            window_height: None,
            enable_agent_control: false,
            agent_control_port: default_agent_control_port(),
            agent_control_token: None,
        }
    }
}

fn default_settings(app: &AppHandle) -> AppSettings {
    AppSettings {
        theme: "eva-dark".to_string(),
        output_dir: default_output_dir(app).unwrap_or_default(),
        show_diem_balance: false,
        window_width: None,
        window_height: None,
        enable_agent_control: false,
        agent_control_port: default_agent_control_port(),
        agent_control_token: None,
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
    show_diem_balance: Option<bool>,
    enable_agent_control: Option<bool>,
    agent_control_port: Option<u16>,
    // We do not allow the frontend to set the token directly for security
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveDataUrlRequest {
    data_url: String,
    destination_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageGenerationRequest {
    model: String,
    title: Option<String>,
    prompt: String,
    negative_prompt: Option<String>,
    aspect_ratio: Option<String>,
    resolution: Option<String>,
    variants: Option<u8>,
    steps: Option<u32>,
    cfg_scale: Option<f32>,
    seed: Option<u64>,
    hide_watermark: Option<bool>,
    safe_mode: Option<bool>,
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundRemoveRequest {
    source_image: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageUpscaleRequest {
    source_image: String,
    scale: u8,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageMultiEditRequest {
    model: String,
    prompt: String,
    images: Vec<String>,
    aspect_ratio: Option<String>,
    resolution: Option<String>,
    safe_mode: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueMediaRequest {
    model: String,
    prompt: String,
    negative_prompt: Option<String>,
    source_image: Option<String>,
    source_video: Option<String>,
    duration: Option<String>,
    duration_seconds: Option<String>,
    resolution: Option<String>,
    aspect_ratio: Option<String>,
    upscale_factor: Option<u8>,
    force_instrumental: Option<bool>,
    lyrics_prompt: Option<String>,
    lyrics_optimizer: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetrieveRequest {
    queue_id: String,
    model: Option<String>,
    kind: Option<String>,
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
    response_format: Option<String>,
    style_prompt: Option<String>,
    temperature: Option<f32>,
    top_p: Option<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptionRequest {
    model: String,
    audio: String,
    file_name: Option<String>,
    mime_type: Option<String>,
    response_format: Option<String>,
    timestamps: Option<bool>,
    language: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaResult {
    id: String,
    kind: String,
    name: String,
    mime_type: String,
    data_url: String,
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
    shutdown_tx: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}

impl Default for AgentControlHandle {
    fn default() -> Self {
        Self {
            shutdown_tx: std::sync::Mutex::new(None),
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
    // Simple but sufficient token for Tailscale/local use (no extra deps)
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let pid = std::process::id();
    let mixed = ts.rotate_left(17) ^ (pid as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    format!("vl-{mixed:016x}")
}

fn tailscale_ipv4_address() -> Option<String> {
    let output = Command::new("tailscale").args(["ip", "-4"]).output().ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && line.chars().all(|ch| ch.is_ascii_digit() || ch == '.'))
        .map(str::to_string)
}

fn agent_control_address(port: u16) -> String {
    tailscale_ipv4_address()
        .map(|ip| format!("{ip}:{port}"))
        .unwrap_or_else(|| format!("0.0.0.0:{port}"))
}

/// Write the control-api.json discovery file so agents / the skill can auto-discover
/// the address, port and token without manual config.
fn write_agent_control_discovery(app: &AppHandle, token: &str, port: u16) -> Result<(), String> {
    let dir = app_data_dir(app)?;
    let tailscale_ip = tailscale_ipv4_address();
    let address = tailscale_ip
        .as_ref()
        .map(|ip| format!("{ip}:{port}"))
        .unwrap_or_else(|| format!("0.0.0.0:{port}"));
    let discovery = serde_json::json!({
        "address": address,
        "bindAddress": format!("0.0.0.0:{port}"),
        "tailscaleIp": tailscale_ip,
        "port": port,
        "token": token,
        "version": app.package_info().version.to_string(),
        "note": "Connect using the address and token. Same Tailscale network recommended."
    });
    let path = dir.join("control-api.json");
    std::fs::write(&path, serde_json::to_string_pretty(&discovery).unwrap())
        .map_err(|e| e.to_string())?;
    println!("[agent-control] Wrote discovery file: {:?}", path);
    Ok(())
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|err| err.to_string())?;
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    Ok(dir)
}

fn default_output_dir(app: &AppHandle) -> Result<String, String> {
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

fn save_settings_file(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    write_json_file(&path, settings)
}

fn force_agent_control_off_on_launch(app: &AppHandle) {
    let mut settings = read_settings(app);
    if !settings.enable_agent_control {
        return;
    }

    settings.enable_agent_control = false;
    if let Err(err) = save_settings_file(app, &settings) {
        eprintln!("[agent-control] Failed to reset launch state: {err}");
    }
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

fn persist_window_size(app: &AppHandle, size: PhysicalSize<u32>) -> Result<(), String> {
    if size.width == 0 || size.height == 0 {
        return Ok(());
    }

    let mut settings = read_settings(app);
    settings.window_width = Some(size.width);
    settings.window_height = Some(size.height);
    save_settings_file(app, &settings)
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

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("venice-media-local/0.1")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn venice_get(path: &str) -> Result<reqwest::Response, String> {
    let key = read_api_key().map_err(|_| "Venice API key is not configured".to_string())?;
    let url = format!("{VENICE_BASE_URL}{path}");
    let response = client()
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    ensure_success(response).await
}

async fn venice_post_json(path: &str, body: Value) -> Result<reqwest::Response, String> {
    let key = read_api_key().map_err(|_| "Venice API key is not configured".to_string())?;
    let url = format!("{VENICE_BASE_URL}{path}");
    let response = client()
        .post(url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    ensure_success(response).await
}

async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response, String> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    Err(format!(
        "Venice API returned {status}: {}",
        trim_error_text(&text)
    ))
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
            &["min_duration", "min_duration_seconds", "minimum_duration_seconds"],
        ),
        max: model_number_field(
            raw,
            spec,
            constraints,
            capabilities,
            &["max_duration", "max_duration_seconds", "maximum_duration_seconds"],
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

fn string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(ToString::to_string)
            .collect(),
        Some(Value::Object(map)) => {
            for key in ["options", "values", "allowed", "enum"] {
                let entries = string_array(map.get(key));
                if !entries.is_empty() {
                    return entries;
                }
            }
            Vec::new()
        }
        Some(Value::String(entry)) => {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            }
        }
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
            "resolution_options",
            "supported_resolutions",
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
            "resolution_options",
            "supported_resolutions",
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
    let model_type = as_string(constraints, "model_type");
    let haystack = format!("{id} {name} {model_type}").to_lowercase();
    if haystack.contains("video-to-video") {
        "V2V"
    } else if haystack.contains("image-to-video") {
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
            let size_options = string_array(constraints.get("aspect_ratios"));
            let resolution_options =
                image_resolution_options(&id, &name, &constraints, &capabilities);
            let controls = if is_edit {
                let mut controls = edit_controls_for_model(&id, &name);
                controls["aspectRatioOptions"] = json!(if size_options.is_empty() {
                    vec!["1:1".to_string(), "4:3".to_string(), "3:4".to_string(), "16:9".to_string(), "9:16".to_string()]
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
                raw: entry,
            })
        }
        "video" => {
            let model_type = as_string(&constraints, "model_type");
            let model_type_lower = model_type.to_lowercase();
            let suffix = video_mode_suffix(&id, &name, &constraints);
            Some(ModelRecord {
                id,
                name: append_mode_suffix(&name, suffix),
                kind: "video".to_string(),
                modes: vec!["generate-video".to_string()],
                controls: json!({
                    "durationOptions": string_array(constraints.get("durations")),
                    "resolutionOptions": string_array(constraints.get("resolutions")),
                    "aspectRatioOptions": string_array(constraints.get("aspect_ratios")),
                    "modelType": model_type,
                    "supportsSourceImage": haystack.contains("image-to-video") || model_type_lower == "image-to-video",
                    "supportsSourceVideo": haystack.contains("video-to-video") || model_type_lower == "video-to-video",
                    "supportsTextToVideo": haystack.contains("text-to-video") || model_type_lower == "text-to-video",
                    "rawConstraints": constraints,
                    "rawCapabilities": capabilities
                }),
                raw: entry,
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
                raw: entry,
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
                raw: entry,
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
                raw: entry,
            })
        }
        _ => None,
    }
}

async fn fetch_model_type(model_type: &str) -> Result<Vec<Value>, String> {
    let response = venice_get(&format!("/models?type={model_type}")).await?;
    let payload: Value = response.json().await.map_err(|err| err.to_string())?;
    Ok(payload
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

async fn refresh_models_inner(app: &AppHandle) -> Result<ModelCache, String> {
    let image_entries = fetch_model_type("image").await?;
    let video_entries = fetch_model_type("video").await?;
    let music_entries = fetch_model_type("music").await?;
    let tts_entries = fetch_model_type("tts").await?;
    let asr_entries = fetch_model_type("asr").await?;

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
        last_fetched: Utc::now().to_rfc3339(),
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
    if normalized.is_empty() || normalized == "application/octet-stream" || normalized == "binary/octet-stream" {
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
    let stem = safe_stem(prompt);
    let ext = extension_for_mime(mime_type);
    let dir = ensure_output_folders_for_settings(app, &settings)?.join(kind);
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let variant_suffix = metadata
        .get("variantIndex")
        .and_then(|value| value.as_u64())
        .map(|index| format!("-v{index}"))
        .unwrap_or_default();
    let file_stem =
        image_file_stem(&metadata).unwrap_or_else(|| format!("{timestamp}-{stem}{variant_suffix}"));
    let (name, path) = unique_file_path(&dir, &file_stem, ext);
    fs::write(&path, bytes).map_err(|err| err.to_string())?;
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
        let target = unique_burn_path(&burn_dir, file_name, index);
        fs::rename(&path, &target)
            .map_err(|err| format!("Failed to move {trimmed} to burn folder: {err}"))?;
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
fn copy_media_file(
    app: AppHandle,
    source_path: String,
    destination_path: String,
) -> Result<String, String> {
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
fn save_data_url_file(request: SaveDataUrlRequest) -> Result<String, String> {
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
        total_bytes += fs::symlink_metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
    }

    Ok(BurnFolderStats {
        file_count: files.len(),
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
    let payload: Value = response.json().await.map_err(|err| err.to_string())?;
    diem_snapshot_from_billing(payload)
}

async fn fetch_diem_rate_limits(warning: Option<String>) -> Result<DiemBalanceSnapshot, String> {
    let response = venice_get("/api_keys/rate_limits").await?;
    let payload: Value = response.json().await.map_err(|err| err.to_string())?;
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
        return Err(format!("Update installer was not found: {}", path.to_string_lossy()));
    }

    Command::new(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("Failed to run {}: {err}", path.to_string_lossy()))
}

#[tauri::command]
fn open_output_folder(app: AppHandle) -> Result<String, String> {
    let root = ensure_output_folders(&app)?;
    open_folder_path(&root)?;
    Ok(root.to_string_lossy().to_string())
}

#[tauri::command]
fn open_burn_folder(app: AppHandle) -> Result<String, String> {
    let dir = ensure_burn_dir(&app)?;
    open_folder_path(&dir)?;
    Ok(dir.to_string_lossy().to_string())
}

#[tauri::command]
fn open_file_folder(path: String) -> Result<String, String> {
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
    let bytes = response.bytes().await.map_err(|err| err.to_string())?;
    save_media_bytes(app, kind, prompt, &mime, &bytes, metadata)
}

fn collect_app_state(
    app: AppHandle,
    setup_sections: Vec<StartupTimingEntry>,
) -> Result<AppState, String> {
    let total_started_at = Instant::now();
    let mut sections = Vec::new();

    let started_at = Instant::now();
    let settings = read_settings(&app);
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

    Ok(AppState {
        settings,
        key_configured: false,
        models,
        build_version,
        agent_control_address: format!("0.0.0.0:{}", settings.agent_control_port),
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
    Ok(agent_control_address(settings.agent_control_port))
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    request: SaveSettingsRequest,
    handle: tauri::State<AgentControlHandle>,
) -> Result<AppSettings, String> {
    let mut settings = read_settings(&app);
    if let Some(theme) = request.theme {
        settings.theme = theme;
    }
    if let Some(output_dir) = request.output_dir {
        settings.output_dir = output_dir.trim().to_string();
    }
    if let Some(show_diem_balance) = request.show_diem_balance {
        settings.show_diem_balance = show_diem_balance;
    }
    let was_enabled = settings.enable_agent_control;
    let previous_port = settings.agent_control_port;

    if let Some(port) = request.agent_control_port {
        settings.agent_control_port = validate_agent_control_port(port)?;
    }

    if let Some(enable) = request.enable_agent_control {
        settings.enable_agent_control = enable;
        // Generate a token the first time the user enables the feature
        if enable && settings.agent_control_token.is_none() {
            settings.agent_control_token = Some(generate_agent_control_token());
        }

        // Live start / stop when the user toggles in Settings (no restart needed)
        if enable && !was_enabled {
            if let Some(token) = settings.agent_control_token.clone() {
                start_agent_control_server(app.clone(), token, settings.agent_control_port, &handle)?;
            }
        } else if !enable && was_enabled {
            stop_agent_control_server(&handle);
        }
    }

    if settings.enable_agent_control
        && was_enabled
        && settings.agent_control_port != previous_port
    {
        stop_agent_control_server(&handle);
        if let Some(token) = settings.agent_control_token.clone() {
            start_agent_control_server(app.clone(), token, settings.agent_control_port, &handle)?;
        }
    }

    ensure_output_folders_for_settings(&app, &settings)?;
    save_settings_file(&app, &settings)?;
    Ok(settings)
}

#[tauri::command]
fn rotate_agent_control_token(
    app: AppHandle,
    handle: tauri::State<AgentControlHandle>,
) -> Result<AppSettings, String> {
    let mut settings = read_settings(&app);
    settings.agent_control_token = Some(generate_agent_control_token());

    if settings.enable_agent_control {
        stop_agent_control_server(&handle);
        if let Some(token) = settings.agent_control_token.clone() {
            start_agent_control_server(app.clone(), token, settings.agent_control_port, &handle)?;
        }
    }

    ensure_output_folders_for_settings(&app, &settings)?;
    save_settings_file(&app, &settings)?;
    Ok(settings)
}

#[tauri::command]
fn save_api_key(api_key: String) -> Result<bool, String> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err("API key cannot be empty".to_string());
    }
    let entry = keyring_entry()?;
    entry.set_password(trimmed).map_err(|err| err.to_string())?;
    Ok(true)
}

#[tauri::command]
fn clear_api_key() -> Result<bool, String> {
    let entry = keyring_entry()?;
    match entry.delete_credential() {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

#[tauri::command]
async fn refresh_models(app: AppHandle) -> Result<ModelCache, String> {
    refresh_models_inner(&app).await
}

#[tauri::command]
fn get_models(app: AppHandle) -> Result<ModelCache, String> {
    Ok(read_model_cache(&app))
}

fn preferred_update_assets(assets: &[GithubReleaseAsset]) -> (Option<UpdateAsset>, Option<UpdateAsset>) {
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
    let response = client()
        .get(GITHUB_LATEST_RELEASE_URL)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let response = ensure_success(response).await?;
    let release: GithubRelease = response.json().await.map_err(|err| err.to_string())?;
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
fn open_update_release(url: String) -> Result<bool, String> {
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
    if !asset.url.starts_with("https://github.com/") {
        return Err("Update asset URL did not come from GitHub releases".to_string());
    }

    let response = client()
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
fn run_update_installer(path: String) -> Result<bool, String> {
    run_file(Path::new(path.trim()))?;
    Ok(true)
}

#[tauri::command]
async fn generate_image(
    app: AppHandle,
    request: ImageGenerationRequest,
) -> Result<Vec<MediaResult>, String> {
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
    let payload: Value = response.json().await.map_err(|err| err.to_string())?;
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
    if let Some(value) = request.aspect_ratio.filter(|value| !value.trim().is_empty()) {
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

    items.iter().filter_map(Value::as_str).any(|option| option == value)
}

#[tauri::command]
async fn queue_video(app: AppHandle, request: QueueMediaRequest) -> Result<QueueResult, String> {
    queue_video_inner(&app, request).await
}

async fn queue_video_inner(app: &AppHandle, request: QueueMediaRequest) -> Result<QueueResult, String> {
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
    let payload: Value = response.json().await.map_err(|err| err.to_string())?;
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
    retrieve_queued_media(app, request, "/video/retrieve", "videos").await
}

#[tauri::command]
async fn queue_audio(app: AppHandle, request: QueueMediaRequest) -> Result<QueueResult, String> {
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
    let payload: Value = response.json().await.map_err(|err| err.to_string())?;
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

    let payload: Value = response.json().await.map_err(|err| err.to_string())?;
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
            let key = read_api_key().map_err(|_| "Venice API key is not configured".to_string())?;
            let response = client()
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

    let response = client()
        .post(format!("{VENICE_BASE_URL}/audio/transcriptions"))
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
        let payload: Value = response.json().await.map_err(|err| err.to_string())?;
        let text = first_string_field(&payload, &["text", "transcript"])
            .unwrap_or("")
            .to_string();
        (text, payload)
    } else {
        let text = response.text().await.map_err(|err| err.to_string())?;
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
        let payload: Value = response.json().await.map_err(|err| err.to_string())?;
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


// === AI Agent Remote Control HTTP Server ===
// Supports live toggle: when the user turns "AI Agent Control" on in Settings,
// the server starts immediately. When turned off, it shuts down gracefully.
// Off by default.

#[derive(Clone)]
struct AgentControlState {
    app: AppHandle,
    token: String,
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

fn check_agent_token(state: &AgentControlState, headers: &HeaderMap) -> Result<(), AgentApiError> {
    let Some(auth) = headers.get(AUTHORIZATION).and_then(|value| value.to_str().ok()) else {
        return Err((StatusCode::UNAUTHORIZED, "Missing bearer token".to_string()));
    };
    if auth == format!("Bearer {}", state.token) {
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

fn emit_agent_queue_status(app: &AppHandle, kind: &str, queue_id: &str, status: &str, progress_label: &str) {
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

async fn agent_get_state(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let setup_sections = state.app.state::<StartupMetricsHandle>().sections();
    let app_state = collect_app_state(state.app.clone(), setup_sections).map_err(agent_error)?;
    Ok(Json(serde_json::to_value(app_state).unwrap()))
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
    Ok(Json(results))
}

async fn agent_refresh_models(
    State(state): State<AgentControlState>,
    headers: HeaderMap,
) -> Result<Json<ModelCache>, AgentApiError> {
    check_agent_token(&state, &headers)?;
    let cache = refresh_models(state.app.clone()).await.map_err(agent_error)?;
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
    Ok(Json(result))
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
    emit_agent_results(&state.app, "Background Removed · Remote", vec![result.clone()]);
    Ok(Json(result))
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
    Ok(Json(result))
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
    Ok(Json(output))
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
    let title = if mode == "sfx" { "SFX · Remote" } else { "Music · Remote" };
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
    Ok(Json(output))
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
    Ok(Json(result))
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
    Ok(Json(result))
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
    if let Err(error) = state.app.emit("agent:clear-results", json!({ "status": "Remote cleared results" })) {
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

fn start_agent_control_server(
    app: AppHandle,
    token: String,
    port: u16,
    handle: &AgentControlHandle,
) -> Result<(), String> {
    if token.is_empty() {
        return Err("No token configured".to_string());
    }
    let port = validate_agent_control_port(port)?;

    // If already running, do nothing
    {
        let guard = handle.shutdown_tx.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
    }

    let addr: SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .map_err(|error| format!("Invalid agent control bind address: {error}"))?;
    let std_listener = StdTcpListener::bind(addr).map_err(|error| {
        if error.kind() == ErrorKind::AddrInUse {
            format!(
                "AI Agent Remote Control is already running on {addr}. Close the other Venice Media Local window or disable AI Agent Control there, then try again."
            )
        } else {
            format!("Failed to bind agent control server on {addr}: {error}")
        }
    })?;
    std_listener
        .set_nonblocking(true)
        .map_err(|error| format!("Failed to configure agent control listener: {error}"))?;

    // Write discovery only after bind succeeds, so agents never pick up a dead token.
    if let Err(e) = write_agent_control_discovery(&app, &token, port) {
        eprintln!("[agent-control] Could not write discovery file: {}", e);
    }

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    // Store the sender so we can shut down later when the toggle is turned off
    {
        let mut guard = handle.shutdown_tx.lock().unwrap();
        *guard = Some(shutdown_tx);
    }

    let state = AgentControlState {
        app: app.clone(),
        token,
    };

    tauri::async_runtime::spawn(async move {
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        let router = Router::new()
            .route("/api/v1/state", get(agent_get_state))
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
            .route("/api/v1/burn-folder-stats", get(agent_get_burn_folder_stats))
            .route("/api/v1/burn-folder", post(agent_burn_folder))
            .layer(DefaultBodyLimit::max(100 * 1024 * 1024)) // 100 MB — supports 4K image payloads
            .layer(cors)
            .with_state(state);

        println!("[agent-control] Starting HTTP server on {} (for AI agents over Tailscale)", addr);

        let listener = match TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[agent-control] Failed to create async listener: {}", e);
                return;
            }
        };

        // Graceful shutdown when the user turns the toggle off
        let server = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
                println!("[agent-control] Shutdown signal received, stopping HTTP server");
            });

        if let Err(e) = server.await {
            eprintln!("[agent-control] Server error: {}", e);
        }
    });

    Ok(())
}

fn stop_agent_control_server(handle: &AgentControlHandle) {
    let mut guard = handle.shutdown_tx.lock().unwrap();
    if let Some(tx) = guard.take() {
        let _ = tx.send(());
        println!("[agent-control] Shutdown requested for agent control server");
    }
}


fn main() {
    tauri::Builder::default()
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
            metrics.push("ensure output folders", started_at);

            let app_handle = app.handle().clone();
            let started_at = Instant::now();
            force_agent_control_off_on_launch(&app_handle);
            metrics.push("reset agent control launch state", started_at);

            let started_at = Instant::now();
            if let Some(window) = app.get_webview_window("main") {
                if let Err(err) = apply_initial_window_size(&app_handle, &window) {
                    eprintln!("Failed to initialize window size: {err}");
                }

                let resize_app = app_handle.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::Resized(size) = event {
                        if let Err(err) = persist_window_size(&resize_app, *size) {
                            eprintln!("Failed to save window size: {err}");
                        }
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
            get_models,
            move_media_files_to_burn,
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
        .run(tauri::generate_context!())
        .expect("error while running Venice Media Local");
}
