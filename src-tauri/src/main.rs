#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::{engine::general_purpose, Engine as _};
use chrono::Utc;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Manager};

const VENICE_BASE_URL: &str = "https://api.venice.ai/api/v1";
const KEYRING_SERVICE: &str = "venice-media-local";
const KEYRING_ACCOUNT: &str = "venice-api-key";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    theme: String,
    output_dir: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: "eva-dark".to_string(),
            output_dir: String::new(),
        }
    }
}

fn default_settings(app: &AppHandle) -> AppSettings {
    AppSettings {
        theme: "eva-dark".to_string(),
        output_dir: default_output_dir(app).unwrap_or_default(),
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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppState {
    settings: AppSettings,
    key_configured: bool,
    models: ModelCache,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveSettingsRequest {
    theme: Option<String>,
    output_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageGenerationRequest {
    model: String,
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
struct QueueMediaRequest {
    model: String,
    prompt: String,
    negative_prompt: Option<String>,
    source_image: Option<String>,
    source_video: Option<String>,
    duration: Option<String>,
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaResult {
    id: String,
    kind: String,
    name: String,
    mime_type: String,
    data_url: String,
    file_path: String,
    metadata: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QueueResult {
    queue_id: String,
    status: String,
    progress_label: String,
    download_url: String,
    raw: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RetrieveResult {
    status: String,
    progress_label: String,
    result: Option<MediaResult>,
    raw: Value,
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
    read_api_key().map(|key| !key.trim().is_empty()).unwrap_or(false)
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
    settings
}

fn save_settings_file(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    write_json_file(&path, settings)
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
            model("gpt-image-2", "GPT Image 2", "image", "generate-image", image_controls()),
            model("flux-2-max", "Flux 2 Max", "image", "generate-image", image_controls()),
            model("qwen-image-2", "Qwen Image 2", "image", "generate-image", image_controls()),
        ],
        edit_models: vec![
            model("gpt-image-2-edit", "GPT Image 2 Edit", "edit", "edit-image", json!({ "variantCount": { "min": 1, "max": 4 } })),
            model("qwen-image-2-edit", "Qwen Image 2 Edit", "edit", "edit-image", json!({ "variantCount": { "min": 1, "max": 4 } })),
        ],
        video_models: vec![
            model("seedance-2-0-image-to-video", "Seedance 2.0", "video", "generate-video", video_controls()),
            model("seedance-2-0-text-to-video", "Seedance 2.0 Text", "video", "generate-video", video_controls()),
            model("wan-2-7-image-to-video", "Wan 2.7", "video", "generate-video", video_controls()),
        ],
        music_models: vec![
            model("elevenlabs-music", "ElevenLabs Music", "music", "generate-music", audio_controls("music")),
            model("stable-audio-25", "Stable Audio 2.5", "music", "generate-music", audio_controls("music")),
        ],
        sfx_models: vec![
            model("elevenlabs-sound-effects-v2", "ElevenLabs Sound Effects", "sfx", "generate-sfx", audio_controls("sfx")),
        ],
        voice_models: vec![
            model("tts-kokoro", "Kokoro TTS", "voice", "generate-voice", voice_controls(Value::Array(vec![]))),
            model("tts-chatterbox-hd", "Chatterbox HD", "voice", "generate-voice", voice_controls(Value::Array(vec![]))),
            model("tts-xai-v1", "xAI TTS", "voice", "generate-voice", voice_controls(Value::Array(vec![]))),
        ],
    }
}

fn read_model_cache(app: &AppHandle) -> ModelCache {
    match model_cache_path(app) {
        Ok(path) => read_json_file(&path, fallback_model_cache()),
        Err(_) => fallback_model_cache(),
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
    json!({
        "audioKind": kind,
        "durationSeconds": { "min": 1, "max": 180 },
        "supportsLyrics": kind == "music",
        "supportsInstrumental": kind == "music",
        "supportsLyricsOptimizer": kind == "music"
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
    Err(format!("Venice API returned {status}: {}", trim_error_text(&text)))
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

fn as_string(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
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
        _ => Vec::new(),
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

fn is_deprecated_or_offline(entry: &Value) -> bool {
    let spec = entry.get("model_spec").unwrap_or(&Value::Null);
    if spec.get("offline").and_then(Value::as_bool).unwrap_or(false) {
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
        if candidate.is_empty() { id.clone() } else { candidate }
    };
    let haystack = format!(
        "{} {} {}",
        id.to_lowercase(),
        name.to_lowercase(),
        as_string(&spec, "description").to_lowercase()
    );

    match model_type {
        "image" => {
            let is_edit = haystack.contains("edit") || haystack.contains("inpaint");
            let kind = if is_edit { "edit" } else { "image" };
            let mode = if is_edit { "edit-image" } else { "generate-image" };
            let size_options = string_array(constraints.get("aspect_ratios"));
            let controls = if is_edit {
                json!({ "variantCount": { "min": 1, "max": 4 } })
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
                    "rawConstraints": constraints,
                    "rawCapabilities": capabilities
                })
            };
            Some(ModelRecord { id, name, kind: kind.to_string(), modes: vec![mode.to_string()], controls, raw: entry })
        }
        "video" => Some(ModelRecord {
            id,
            name,
            kind: "video".to_string(),
            modes: vec!["generate-video".to_string()],
            controls: json!({
                "durationOptions": string_array(constraints.get("durations")),
                "resolutionOptions": string_array(constraints.get("resolutions")),
                "aspectRatioOptions": string_array(constraints.get("aspect_ratios")),
                "modelType": as_string(&constraints, "model_type"),
                "supportsSourceImage": haystack.contains("image-to-video") || as_string(&constraints, "model_type") == "image-to-video",
                "supportsTextToVideo": haystack.contains("text-to-video") || as_string(&constraints, "model_type") == "text-to-video",
                "rawConstraints": constraints,
                "rawCapabilities": capabilities
            }),
            raw: entry,
        }),
        "music" => {
            let is_sfx = haystack.contains("sound effect")
                || haystack.contains("sound-effects")
                || haystack.contains("sfx")
                || haystack.contains("foley");
            let kind = if is_sfx { "sfx" } else { "music" };
            let mode = if is_sfx { "generate-sfx" } else { "generate-music" };
            Some(ModelRecord {
                id,
                name,
                kind: kind.to_string(),
                modes: vec![mode.to_string()],
                controls: json!({
                    "audioKind": kind,
                    "supportsLyrics": !is_sfx,
                    "supportsInstrumental": !is_sfx,
                    "supportsLyricsOptimizer": !is_sfx,
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
    let video_models = push_unique(video_entries.into_iter().filter_map(|entry| normalize_model(entry, "video")).collect());
    let audio_like = music_entries
        .into_iter()
        .filter_map(|entry| normalize_model(entry, "music"))
        .collect::<Vec<_>>();
    let voice_models = push_unique(tts_entries.into_iter().filter_map(|entry| normalize_model(entry, "tts")).collect());

    let mut cache = ModelCache {
        last_fetched: Utc::now().to_rfc3339(),
        image_models: image_like.iter().filter(|entry| entry.kind == "image").cloned().collect(),
        edit_models: image_like.iter().filter(|entry| entry.kind == "edit").cloned().collect(),
        video_models,
        music_models: audio_like.iter().filter(|entry| entry.kind == "music").cloned().collect(),
        sfx_models: audio_like.iter().filter(|entry| entry.kind == "sfx").cloned().collect(),
        voice_models,
    };

    if cache.image_models.is_empty() {
        cache.image_models = fallback_model_cache().image_models;
    }
    if cache.video_models.is_empty() {
        cache.video_models = fallback_model_cache().video_models;
    }
    if cache.music_models.is_empty() {
        cache.music_models = fallback_model_cache().music_models;
    }
    if cache.sfx_models.is_empty() {
        cache.sfx_models = fallback_model_cache().sfx_models;
    }
    if cache.voice_models.is_empty() {
        cache.voice_models = fallback_model_cache().voice_models;
    }

    save_model_cache(app, &cache)?;
    Ok(cache)
}

fn safe_stem(value: &str) -> String {
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
    if trimmed.is_empty() { "venice-media".to_string() } else { trimmed }
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
        _ if mime.starts_with("audio/") => "mp3",
        _ => "bin",
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

fn save_media_bytes(
    app: &AppHandle,
    kind: &str,
    prompt: &str,
    mime_type: &str,
    bytes: &[u8],
    metadata: Value,
) -> Result<MediaResult, String> {
    let settings = read_settings(app);
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S-%3f").to_string();
    let stem = safe_stem(prompt);
    let ext = extension_for_mime(mime_type);
    let dir = output_root(app, &settings)?.join(kind);
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let variant_suffix = metadata
        .get("variantIndex")
        .and_then(|value| value.as_u64())
        .map(|index| format!("-v{index}"))
        .unwrap_or_default();
    let name = format!("{timestamp}-{stem}{variant_suffix}.{ext}");
    let path = dir.join(&name);
    fs::write(&path, bytes).map_err(|err| err.to_string())?;
    let encoded = general_purpose::STANDARD.encode(bytes);
    Ok(MediaResult {
        id: format!("{kind}-{timestamp}-{stem}{variant_suffix}"),
        kind: kind.to_string(),
        name,
        mime_type: mime_type.to_string(),
        data_url: format!("data:{mime_type};base64,{encoded}"),
        file_path: path.to_string_lossy().to_string(),
        metadata,
    })
}

#[tauri::command]
fn delete_media_files(paths: Vec<String>) -> Result<Vec<String>, String> {
    let mut deleted = Vec::new();

    for raw_path in paths {
        let trimmed = raw_path.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }

        let path = PathBuf::from(&trimmed);
        if !path.exists() {
            deleted.push(trimmed);
            continue;
        }
        if !path.is_file() {
            return Err(format!("Refusing to delete non-file path: {trimmed}"));
        }

        fs::remove_file(&path).map_err(|err| format!("Failed to delete {trimmed}: {err}"))?;
        deleted.push(trimmed);
    }

    Ok(deleted)
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

#[tauri::command]
fn get_app_state(app: AppHandle) -> Result<AppState, String> {
    Ok(AppState {
        settings: read_settings(&app),
        key_configured: has_api_key(),
        models: read_model_cache(&app),
    })
}

#[tauri::command]
fn save_settings(app: AppHandle, request: SaveSettingsRequest) -> Result<AppSettings, String> {
    let mut settings = read_settings(&app);
    if let Some(theme) = request.theme {
        settings.theme = theme;
    }
    if let Some(output_dir) = request.output_dir {
        settings.output_dir = output_dir.trim().to_string();
    }
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

#[tauri::command]
async fn generate_image(app: AppHandle, request: ImageGenerationRequest) -> Result<Vec<MediaResult>, String> {
    let variant_count = request.variants.unwrap_or(1).clamp(1, 4);
    let format = normalize_image_format(request.format.as_deref().unwrap_or("webp"));
    let mut body = json!({
        "model": request.model.clone(),
        "prompt": request.prompt.clone(),
        "variants": variant_count,
        "format": format,
        "return_binary": false,
    });

    if let Some(value) = request.negative_prompt.filter(|value| !value.trim().is_empty()) {
        body["negative_prompt"] = json!(value);
    }
    if let Some(value) = request.aspect_ratio.filter(|value| !value.trim().is_empty()) {
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
            } else if let Some(raw) = first_string_field(item, &["b64_json", "base64", "image", "url"]) {
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
        return Err(format!("Venice image response did not include image data: {payload}"));
    }

    let mime = mime_for_image_format(format);
    let mut results = Vec::new();
    for (index, encoded) in encoded_images.iter().enumerate() {
        let bytes = decode_base64_payload(encoded)?;
        let metadata = json!({
            "model": body.get("model"),
            "prompt": body.get("prompt"),
            "variantIndex": index + 1,
            "raw": payload
        });
        results.push(save_media_bytes(&app, "images", &request.prompt, mime, &bytes, metadata)?);
    }

    Ok(results)
}

#[tauri::command]
async fn queue_video(request: QueueMediaRequest) -> Result<QueueResult, String> {
    let mut body = json!({
        "model": request.model,
        "prompt": request.prompt,
    });
    if let Some(value) = request.negative_prompt.filter(|value| !value.trim().is_empty()) {
        body["negative_prompt"] = json!(value);
    }
    if let Some(value) = request.source_image.filter(|value| !value.trim().is_empty()) {
        body["image"] = json!(value);
    }
    if let Some(value) = request.source_video.filter(|value| !value.trim().is_empty()) {
        body["video"] = json!(value);
    }
    if let Some(value) = request.duration.filter(|value| !value.trim().is_empty()) {
        body["duration"] = json!(value);
    }
    if let Some(value) = request.resolution.filter(|value| !value.trim().is_empty()) {
        body["resolution"] = json!(value);
    }
    if let Some(value) = request.aspect_ratio.filter(|value| !value.trim().is_empty()) {
        body["aspect_ratio"] = json!(value);
    }
    if let Some(value) = request.upscale_factor {
        body["upscale_factor"] = json!(value);
    }

    let response = venice_post_json("/video/queue", body).await?;
    let payload: Value = response.json().await.map_err(|err| err.to_string())?;
    let queue_id = first_string_field(&payload, &["id", "queue_id", "request_id"])
        .or_else(|| payload.get("data").and_then(|data| first_string_field(data, &["id", "queue_id", "request_id"])))
        .unwrap_or("")
        .to_string();
    if queue_id.is_empty() {
        return Err(format!("Venice video queue response did not include a queue id: {payload}"));
    }
    let status = json_status_label(&payload);
    let download_url = first_string_field(&payload, &["download_url", "url"])
        .or_else(|| payload.get("data").and_then(|data| first_string_field(data, &["download_url", "url"])))
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
async fn retrieve_video(app: AppHandle, request: RetrieveRequest) -> Result<RetrieveResult, String> {
    retrieve_queued_media(app, request, "/video/retrieve", "videos").await
}

#[tauri::command]
async fn queue_audio(request: QueueMediaRequest) -> Result<QueueResult, String> {
    let mut body = json!({
        "model": request.model,
        "prompt": request.prompt,
    });
    if let Some(value) = request.duration.filter(|value| !value.trim().is_empty()) {
        body["duration"] = json!(value);
    }
    if let Some(value) = request.force_instrumental {
        body["force_instrumental"] = json!(value);
    }
    if let Some(value) = request.lyrics_prompt.filter(|value| !value.trim().is_empty()) {
        body["lyrics_prompt"] = json!(value);
    }
    if let Some(value) = request.lyrics_optimizer {
        body["lyrics_optimizer"] = json!(value);
    }

    let response = venice_post_json("/audio/queue", body).await?;
    let payload: Value = response.json().await.map_err(|err| err.to_string())?;
    let queue_id = first_string_field(&payload, &["id", "queue_id", "request_id"])
        .or_else(|| payload.get("data").and_then(|data| first_string_field(data, &["id", "queue_id", "request_id"])))
        .unwrap_or("")
        .to_string();
    if queue_id.is_empty() {
        return Err(format!("Venice audio queue response did not include a queue id: {payload}"));
    }
    let status = json_status_label(&payload);
    let download_url = first_string_field(&payload, &["download_url", "url"])
        .or_else(|| payload.get("data").and_then(|data| first_string_field(data, &["download_url", "url"])))
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
async fn retrieve_audio(app: AppHandle, request: RetrieveRequest) -> Result<RetrieveResult, String> {
    retrieve_queued_media(app, request, "/audio/retrieve", "audio").await
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
    if let Some(model) = request.model.clone().filter(|value| !value.trim().is_empty()) {
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
        .or_else(|| payload.get("data").and_then(|data| first_string_field(data, &["download_url", "url"])).map(ToString::to_string));

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
            let result = save_binary_response(&app, response, default_kind, prompt, json!({ "raw": payload })).await?;
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
        progress_label: if is_done_status(&status) { "Completed" } else { "Processing" }.to_string(),
        result: None,
        raw: payload,
    })
}

#[tauri::command]
async fn generate_speech(app: AppHandle, request: SpeechRequest) -> Result<MediaResult, String> {
    let response_format = request.response_format.clone().unwrap_or_else(|| "mp3".to_string());
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
    if let Some(value) = request.style_prompt.filter(|value| !value.trim().is_empty()) {
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
            return save_media_bytes(&app, "voice", &request.input, &mime, &bytes, json!({ "raw": payload }));
        }
        return Err(format!("Venice speech response did not include audio data: {payload}"));
    }

    save_binary_response(&app, response, "voice", &request.input, json!({ "request": body })).await
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_app_state,
            save_settings,
            save_api_key,
            clear_api_key,
            get_models,
            delete_media_files,
            refresh_models,
            generate_image,
            queue_video,
            retrieve_video,
            queue_audio,
            retrieve_audio,
            generate_speech,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Venice Media Local");
}
