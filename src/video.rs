//! Asynchronous video generation on the ComfyUI workflows in `active-workflows/video/`, shaped like
//! OpenRouter's video API: `POST /api/v1/videos` only queues the job and answers `202` with its id,
//! the client polls `GET /api/v1/videos/{id}` and downloads `GET /api/v1/videos/{id}/content`.
//!
//! Nothing is stored here: the job id is ComfyUI's `prompt_id`, its status is read from `/queue` and
//! `/history`, and the video is streamed from `/view`. Reference files come as multipart parts and are
//! uploaded to ComfyUI's input folder.
//!
//! A workflow is served when `config.toml` has `[video_workflows.<name>]` with `active = true`
//! (`VideoWorkflowConfig`, whose defaults are the MiniMax H3 conventions). It carries two generator
//! variants, `i2v_node` (text/first+last frame to video) and `r2v_node` (reference images/audio/videos
//! to video); the one a request needs becomes `generator_node`, which the rest of the graph links to.
//! When the variants need different model chains (UNET, LoRA, sampler...), the nodes of a variant are
//! keyed `i2v:<id>` / `r2v:<id>`: the chosen variant's nodes lose the prefix, the other's are dropped,
//! so shared nodes link to `<id>`. `i2v:<generator_node>` / `r2v:<generator_node>` work as generators too.

use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::VideoWorkflowConfig;
use crate::AppState;

const VIDEO_DIR: &str = "active-workflows/video";
const VIDEO_EXTENSIONS: [&str; 4] = ["mp4", "webm", "mov", "mkv"];
/// Prefixes of the node ids that belong to one generator variant only
const I2V_PREFIX: &str = "i2v:";
const R2V_PREFIX: &str = "r2v:";

type ApiError = (StatusCode, String);

fn bad_request(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn config_error(msg: impl Into<String>) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.into())
}

fn error_response((status, message): ApiError) -> Response {
    (status, Json(json!({ "error": { "code": status.as_u16(), "message": message } }))).into_response()
}

/// The JSON part `request` (or the whole body when it is `application/json`).
#[derive(Deserialize, Debug)]
struct VideoParams {
    model: String,
    prompt: String,
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    aspect_ratio: Option<String>,
    /// `WIDTHxHEIGHT`, instead of `resolution` + `aspect_ratio`
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    seed: Option<u64>,
    /// Accepted for compatibility; the workflows always generate sound.
    #[serde(default)]
    #[allow(dead_code)]
    generate_audio: Option<bool>,
    /// The sound of the n-th `video` part (from 0) as the next audio reference.
    #[serde(default)]
    video_sound: Vec<usize>,
    #[serde(default)]
    frame_images: Option<Value>,
    #[serde(default)]
    input_references: Option<Value>,
}

fn parse_params(json: &[u8]) -> Result<VideoParams, ApiError> {
    let params: VideoParams = serde_json::from_slice(json).map_err(|e| bad_request(format!("Invalid request JSON: {}", e)))?;
    if params.frame_images.is_some() || params.input_references.is_some() {
        return Err(bad_request(
            "frame_images / input_references are not supported: send the files as multipart parts \
             first_frame, last_frame, image, audio or video",
        ));
    }
    Ok(params)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    FirstFrame,
    LastFrame,
    Image,
    Audio,
    Video,
}

impl Role {
    fn from_field(name: &str) -> Option<Self> {
        match name.trim_end_matches("[]") {
            "first_frame" => Some(Self::FirstFrame),
            "last_frame" => Some(Self::LastFrame),
            "image" => Some(Self::Image),
            "audio" => Some(Self::Audio),
            "video" => Some(Self::Video),
            _ => None,
        }
    }

    fn default_extension(self) -> &'static str {
        match self {
            Self::FirstFrame | Self::LastFrame | Self::Image => "png",
            Self::Audio => "wav",
            Self::Video => "mp4",
        }
    }
}

struct FilePart {
    role: Role,
    filename: Option<String>,
    data: Vec<u8>,
}

/// Splits multipart parts into the `request` JSON and the files, by field name.
fn split_parts(parts: Vec<(String, Option<String>, Vec<u8>)>) -> Result<(Vec<u8>, Vec<FilePart>), ApiError> {
    let mut request = None;
    let mut files = Vec::new();
    for (name, filename, data) in parts {
        if name == "request" {
            request = Some(data);
        } else if let Some(role) = Role::from_field(&name) {
            if matches!(role, Role::FirstFrame | Role::LastFrame) && files.iter().any(|f: &FilePart| f.role == role) {
                return Err(bad_request(format!("Only one {} is allowed", name)));
            }
            files.push(FilePart { role, filename, data });
        } else {
            return Err(bad_request(format!(
                "Unknown multipart field '{}' (expected request, first_frame, last_frame, image, audio, video)",
                name
            )));
        }
    }
    let request = request.ok_or_else(|| bad_request("Missing multipart field 'request' with the JSON parameters"))?;
    Ok((request, files))
}

/// ComfyUI input filenames of the uploaded files, by role and in the order they were sent.
#[derive(Default, Debug)]
struct VideoInputs {
    first_frame: Option<String>,
    last_frame: Option<String>,
    images: Vec<String>,
    audios: Vec<String>,
    videos: Vec<String>,
}

impl VideoInputs {
    fn push(&mut self, role: Role, name: String) {
        match role {
            Role::FirstFrame => self.first_frame = Some(name),
            Role::LastFrame => self.last_frame = Some(name),
            Role::Image => self.images.push(name),
            Role::Audio => self.audios.push(name),
            Role::Video => self.videos.push(name),
        }
    }
}

fn check_duration(params: &VideoParams, cfg: &VideoWorkflowConfig) -> Result<u64, ApiError> {
    let duration = params.duration.unwrap_or(cfg.default_duration);
    if !(cfg.min_duration..=cfg.max_duration).contains(&duration) {
        return Err(bad_request(format!(
            "duration must be a whole number of seconds from {} to {}",
            cfg.min_duration, cfg.max_duration
        )));
    }
    Ok(duration)
}

/// Frames of the video: the model takes `frame_step * k + frame_offset` of them, so the count is rounded up.
fn calc_length(duration: u64, cfg: &VideoWorkflowConfig) -> u64 {
    let frames = (duration * cfg.fps).max(cfg.frame_offset);
    if cfg.frame_step == 0 {
        return frames;
    }
    frames + (cfg.frame_step - (frames - cfg.frame_offset) % cfg.frame_step) % cfg.frame_step
}

fn round_to_multiple(v: f64, multiple: u64) -> u64 {
    let multiple = multiple.max(1);
    ((v / multiple as f64).round() as u64).max(1) * multiple
}

/// Width and height (multiples of `size_multiple`) with the resolution as the shorter side, or from `size`.
fn calc_resolution(params: &VideoParams, cfg: &VideoWorkflowConfig) -> Result<(u64, u64), ApiError> {
    if let Some(size) = &params.size {
        if params.resolution.is_some() || params.aspect_ratio.is_some() {
            return Err(bad_request("Use either size or resolution + aspect_ratio"));
        }
        let parsed = size
            .split_once(['x', 'X'])
            .and_then(|(w, h)| Some((w.trim().parse::<u64>().ok()?, h.trim().parse::<u64>().ok()?)))
            .filter(|&(w, h)| w > 0 && h > 0);
        let (w, h) = parsed.ok_or_else(|| bad_request(format!("Invalid size '{}', expected WIDTHxHEIGHT", size)))?;
        return Ok((round_to_multiple(w as f64, cfg.size_multiple), round_to_multiple(h as f64, cfg.size_multiple)));
    }

    let resolution = params.resolution.as_deref().unwrap_or(&cfg.default_resolution);
    let aspect_ratio = params.aspect_ratio.as_deref().unwrap_or(&cfg.default_aspect_ratio);
    if !cfg.resolutions.iter().any(|r| r == resolution) {
        return Err(bad_request(format!("Unsupported resolution '{}' (supported: {})", resolution, cfg.resolutions.join(", "))));
    }
    if !cfg.aspect_ratios.iter().any(|a| a == aspect_ratio) {
        return Err(bad_request(format!("Unsupported aspect_ratio '{}' (supported: {})", aspect_ratio, cfg.aspect_ratios.join(", "))));
    }
    let short_side: f64 = resolution
        .trim_end_matches(['p', 'P'])
        .parse()
        .map_err(|_| config_error(format!("Resolution '{}' in config is not like '480p'", resolution)))?;
    let (rw, rh) = aspect_ratio
        .split_once(':')
        .and_then(|(w, h)| Some((w.parse::<f64>().ok()?, h.parse::<f64>().ok()?)))
        .filter(|&(w, h)| w > 0.0 && h > 0.0)
        .ok_or_else(|| config_error(format!("Aspect ratio '{}' in config is not like '16:9'", aspect_ratio)))?;
    let scale = short_side / rw.min(rh);
    Ok((round_to_multiple(rw * scale, cfg.size_multiple), round_to_multiple(rh * scale, cfg.size_multiple)))
}

fn set_input(wf: &mut serde_json::Map<String, Value>, node: &str, field: &str, value: Value) -> Result<(), ApiError> {
    let inputs = wf
        .get_mut(node)
        .and_then(|n| n.get_mut("inputs"))
        .and_then(|i| i.as_object_mut())
        .ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, format!("Video workflow has no node '{}' with inputs", node)))?;
    inputs.insert(field.to_string(), value);
    Ok(())
}

/// The workflow to queue: the generator variant the request needs, its parameters and a loader node
/// for every uploaded file, linked to the generator.
fn build_video_prompt(
    mut template: Value,
    cfg: &VideoWorkflowConfig,
    params: &VideoParams,
    inputs: &VideoInputs,
    seed: u64,
) -> Result<Value, ApiError> {
    let duration = check_duration(params, cfg)?;
    let (width, height) = calc_resolution(params, cfg)?;
    for (kind, count, max) in [
        ("image", inputs.images.len(), cfg.max_images),
        ("audio", inputs.audios.len() + params.video_sound.len(), cfg.max_audios),
        ("video", inputs.videos.len(), cfg.max_videos),
    ] {
        if let Some(max) = max {
            if count > max {
                return Err(bad_request(format!("Too many {} references: {} (max {})", kind, count, max)));
            }
        }
    }
    let wf = template
        .as_object_mut()
        .ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, "Video workflow is not a JSON object".to_string()))?;

    let has_frames = inputs.first_frame.is_some() || inputs.last_frame.is_some();
    let has_references = !inputs.images.is_empty() || !inputs.audios.is_empty() || !inputs.videos.is_empty() || !params.video_sound.is_empty();
    if has_frames && has_references {
        return Err(bad_request("Send either first_frame/last_frame or image/audio/video references, not both"));
    }

    let (keep, drop) = if has_references { (R2V_PREFIX, I2V_PREFIX) } else { (I2V_PREFIX, R2V_PREFIX) };
    let variant_ids: Vec<String> = wf.keys().filter(|k| k.starts_with(keep) || k.starts_with(drop)).cloned().collect();
    for id in variant_ids {
        let node = wf.remove(&id).unwrap_or_default();
        if let Some(stripped) = id.strip_prefix(keep) {
            if wf.contains_key(stripped) {
                return Err(config_error(format!("Video workflow has both '{}' and '{}'", id, stripped)));
            }
            wf.insert(stripped.to_string(), node);
        }
    }

    let i2v = wf.remove(&cfg.i2v_node);
    let r2v = wf.remove(&cfg.r2v_node);
    let (variant, generator) = if has_references { (&cfg.r2v_node, r2v) } else { (&cfg.i2v_node, i2v) };
    let generator_id = cfg.generator_node.as_str();
    match generator {
        Some(generator) => {
            wf.insert(generator_id.to_string(), generator);
        }
        None if wf.contains_key(generator_id) => {}
        None => return Err(bad_request(format!("This workflow has no '{}' node for this kind of request", variant))),
    }

    set_input(wf, generator_id, &cfg.prompt_field, json!(params.prompt))?;
    set_input(wf, generator_id, &cfg.width_field, json!(width))?;
    set_input(wf, generator_id, &cfg.height_field, json!(height))?;
    set_input(wf, generator_id, &cfg.length_field, json!(calc_length(duration, cfg)))?;
    set_input(wf, &cfg.seed_node, &cfg.seed_field, json!(seed))?;
    if !cfg.denoise_node.is_empty() && wf.contains_key(&cfg.denoise_node) {
        set_input(wf, &cfg.denoise_node, "denoise", json!(1))?;
    }
    set_input(wf, &cfg.output_node, "filename_prefix", json!("comfy-serve/video"))?;

    for (node, field, name) in [
        ("first_frame", &cfg.first_frame_field, &inputs.first_frame),
        ("last_frame", &cfg.last_frame_field, &inputs.last_frame),
    ] {
        if let Some(name) = name {
            wf.insert(node.to_string(), json!({ "class_type": "LoadImage", "inputs": { "image": name } }));
            set_input(wf, generator_id, field, json!([node, 0]))?;
        }
    }
    for (i, name) in inputs.images.iter().enumerate() {
        let node = format!("ref_image_{}", i);
        wf.insert(node.clone(), json!({ "class_type": "LoadImage", "inputs": { "image": name } }));
        set_input(wf, generator_id, &format!("{}{}", cfg.image_prefix, node), json!([node, 0]))?;
    }
    // A video is taken apart: its images go to the model here, its sound only via `video_sound`
    for (i, name) in inputs.videos.iter().enumerate() {
        let node = format!("ref_video_{}", i);
        let components = format!("ref_video_components_{}", i);
        wf.insert(node.clone(), json!({ "class_type": "LoadVideo", "inputs": { "file": name } }));
        wf.insert(components.clone(), json!({ "class_type": "GetVideoComponents", "inputs": { "video": [node, 0] } }));
        set_input(wf, generator_id, &format!("{}{}", cfg.video_prefix, node), json!([components, 0]))?;
    }
    for (i, name) in inputs.audios.iter().enumerate() {
        let node = format!("ref_audio_{}", i);
        wf.insert(node.clone(), json!({ "class_type": "LoadAudio", "inputs": { "audio": name } }));
        set_input(wf, generator_id, &format!("{}{}", cfg.audio_prefix, node), json!([node, 0]))?;
    }
    for (i, &video) in params.video_sound.iter().enumerate() {
        if video >= inputs.videos.len() {
            return Err(bad_request(format!("video_sound refers to video {}, but only {} were sent", video, inputs.videos.len())));
        }
        let node = format!("ref_audio_{}", inputs.audios.len() + i);
        set_input(wf, generator_id, &format!("{}{}", cfg.audio_prefix, node), json!([format!("ref_video_components_{}", video), 1]))?;
    }

    Ok(template)
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) && !name.starts_with('.')
}

/// The configuration and JSON of an active video workflow.
async fn load_video_workflow(state: &AppState, model: &str) -> Result<(VideoWorkflowConfig, Value), ApiError> {
    let not_found = || bad_request(format!("Unknown video model '{}'", model));
    if !is_valid_name(model) {
        return Err(not_found());
    }
    let cfg = match state.config.read().await.video_workflows.get(model) {
        Some(c) if c.active => c.clone(),
        _ => return Err(not_found()),
    };
    let content = std::fs::read_to_string(format!("{}/{}.json", VIDEO_DIR, model))
        .map_err(|_| config_error(format!("Video workflow file {}/{}.json not found", VIDEO_DIR, model)))?;
    let json = serde_json::from_str(&content).map_err(|e| config_error(format!("Invalid workflow JSON: {}", e)))?;
    Ok((cfg, json))
}

/// The video files (`filename`, `subfolder`, `type`) a finished job saved. Loader nodes such as
/// `LoadVideo` also list their input as a preview (`type: "input"`), so only saved outputs count.
fn video_outputs(history: &Value) -> Vec<(String, String, String)> {
    let mut files = Vec::new();
    let Some(outputs) = history.get("outputs").and_then(|o| o.as_object()) else {
        return files;
    };
    for node_output in outputs.values() {
        for key in ["images", "videos", "video", "gifs"] {
            for item in node_output.get(key).and_then(|v| v.as_array()).into_iter().flatten() {
                let Some(filename) = item.get("filename").and_then(|f| f.as_str()) else { continue };
                let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
                let folder_type = item.get("type").and_then(|s| s.as_str()).unwrap_or("output");
                if VIDEO_EXTENSIONS.contains(&ext.as_str()) && folder_type == "output" {
                    files.push((
                        filename.to_string(),
                        item.get("subfolder").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                        folder_type.to_string(),
                    ));
                }
            }
        }
    }
    files
}

/// Status and error of a job that has a `/history` entry.
fn history_status(history: &Value) -> (&'static str, Option<String>) {
    let status = history.get("status");
    let messages = status.and_then(|s| s.get("messages")).and_then(|m| m.as_array());
    let message = |kind: &str| messages.into_iter().flatten().find(|m| m.get(0).and_then(|k| k.as_str()) == Some(kind)).and_then(|m| m.get(1));

    if status.and_then(|s| s.get("status_str")).and_then(|s| s.as_str()) == Some("error") {
        if message("execution_interrupted").is_some() {
            return ("cancelled", Some("The job was interrupted".to_string()));
        }
        let error = message("execution_error")
            .map(|e| {
                let node = e.get("node_type").and_then(|n| n.as_str()).unwrap_or("?");
                let msg = e.get("exception_message").and_then(|m| m.as_str()).unwrap_or("unknown error");
                format!("{}: {}", node, msg.trim())
            })
            .unwrap_or_else(|| "The job failed".to_string());
        return ("failed", Some(error));
    }
    if video_outputs(history).is_empty() {
        return ("failed", Some("The workflow finished without a video".to_string()));
    }
    ("completed", None)
}

/// Status of a job still in ComfyUI's queue.
fn queue_status(queue: &Value, prompt_id: &str) -> Option<&'static str> {
    let contains = |key: &str| {
        queue
            .get(key)
            .and_then(|q| q.as_array())
            .map(|jobs| jobs.iter().any(|j| j.get(1).and_then(|id| id.as_str()) == Some(prompt_id)))
            .unwrap_or(false)
    };
    if contains("queue_running") {
        Some("in_progress")
    } else if contains("queue_pending") {
        Some("pending")
    } else {
        None
    }
}

/// `http(s)://host` of this server as the client reached it.
fn public_base(headers: &HeaderMap) -> String {
    let get = |name: &str| headers.get(name).and_then(|h| h.to_str().ok()).map(|s| s.split(',').next().unwrap_or(s).trim().to_string());
    let proto = get("x-forwarded-proto").unwrap_or_else(|| "http".to_string());
    let host = get("x-forwarded-host").or_else(|| get("host")).unwrap_or_else(|| "127.0.0.1:3000".to_string());
    format!("{}://{}", proto, host)
}

fn check_job_id(id: &str) -> Result<(), ApiError> {
    if !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(())
    } else {
        Err((StatusCode::NOT_FOUND, "Video job not found".to_string()))
    }
}

/// History entry of a job, or its queue status; `None` when ComfyUI knows nothing about it.
enum JobState {
    Finished(Value),
    Queued(&'static str),
}

async fn job_state(state: &AppState, id: &str) -> Result<Option<JobState>, ApiError> {
    let comfy = &state.comfy_client;
    let upstream = |e: String| (StatusCode::BAD_GATEWAY, format!("ComfyUI: {}", e));
    if let Some(h) = comfy.get_history(id).await.map_err(upstream)? {
        return Ok(Some(JobState::Finished(h)));
    }
    let queue = comfy.get_queue().await.map_err(upstream)?;
    if let Some(status) = queue_status(&queue, id) {
        return Ok(Some(JobState::Queued(status)));
    }
    // It may have finished between the two calls
    Ok(comfy.get_history(id).await.map_err(upstream)?.map(JobState::Finished))
}

pub async fn create_video_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(res) = crate::check_api_key(&state, &headers) {
        return res;
    }
    match create_video(&state, &headers, &body).await {
        Ok(res) => res,
        Err(e) => error_response(e),
    }
}

async fn create_video(state: &AppState, headers: &HeaderMap, body: &[u8]) -> Result<Response, ApiError> {
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|h| h.to_str().ok()).unwrap_or("");
    let (request_json, files) = if content_type.starts_with("multipart/") {
        let boundary = content_type
            .find("boundary=")
            .map(|idx| content_type[idx + 9..].trim().trim_matches('"').to_string())
            .ok_or_else(|| bad_request("Missing boundary in content-type"))?;
        split_parts(crate::parse_lenient_multipart(body, &boundary))?
    } else {
        (body.to_vec(), Vec::new())
    };

    let params = parse_params(&request_json)?;
    let (cfg, template) = load_video_workflow(state, &params.model).await?;
    let seed = params.seed.unwrap_or_else(|| rand::random::<u64>() >> 16);

    // Check the request before anything is uploaded
    let mut dry_inputs = VideoInputs::default();
    for f in &files {
        dry_inputs.push(f.role, String::new());
    }
    build_video_prompt(template.clone(), &cfg, &params, &dry_inputs, seed)?;

    let comfy = &state.comfy_client;
    let mut inputs = VideoInputs::default();
    let mut uploaded = Vec::new();
    for f in files {
        let ext = f
            .filename
            .as_deref()
            .map(|n| crate::comfy::derive_content_type(n).1)
            .filter(|e| e != "bin")
            .unwrap_or_else(|| f.role.default_extension().to_string());
        let filename = format!("comfy_serve_temp_{}.{}", uuid::Uuid::new_v4(), ext);
        match comfy.upload_file(f.data, &filename).await {
            Ok(name) => {
                uploaded.push(name.clone());
                inputs.push(f.role, name);
            }
            Err(e) => {
                comfy.cleanup_files(&uploaded).await;
                return Err((StatusCode::BAD_GATEWAY, format!("Upload to ComfyUI failed: {}", e)));
            }
        }
    }

    let prompt = build_video_prompt(template, &cfg, &params, &inputs, seed)?;
    let id = match comfy.queue_prompt(prompt).await {
        Ok(id) => id,
        Err(e) => {
            comfy.cleanup_files(&uploaded).await;
            return Err((StatusCode::BAD_GATEWAY, e));
        }
    };
    tracing::info!("Queued video job {} ({}, {} file(s))", id, params.model, uploaded.len());

    let polling_url = format!("{}/api/v1/videos/{}", public_base(headers), id);
    Ok((StatusCode::ACCEPTED, Json(json!({ "id": id, "polling_url": polling_url, "status": "pending" }))).into_response())
}

pub async fn get_video_handler(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(res) = crate::check_api_key(&state, &headers) {
        return res;
    }
    let result = async {
        check_job_id(&id)?;
        let polling_url = format!("{}/api/v1/videos/{}", public_base(&headers), id);
        let (status, error, count) = match job_state(&state, &id).await? {
            Some(JobState::Finished(h)) => {
                let (status, error) = history_status(&h);
                let count = if status == "completed" { video_outputs(&h).len() } else { 0 };
                (status, error, count)
            }
            Some(JobState::Queued(status)) => (status, None, 0),
            None => return Err((StatusCode::NOT_FOUND, "Video job not found".to_string())),
        };
        let unsigned_urls: Vec<String> = (0..count).map(|i| format!("{}/content?index={}", polling_url, i)).collect();
        Ok(Json(json!({
            "id": id,
            "polling_url": polling_url,
            "status": status,
            "unsigned_urls": unsigned_urls,
            "error": error,
        }))
        .into_response())
    }
    .await;
    result.unwrap_or_else(error_response)
}

#[derive(Deserialize)]
pub struct ContentQuery {
    index: Option<usize>,
}

pub async fn video_content_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<ContentQuery>,
) -> Response {
    if let Err(res) = crate::check_api_key(&state, &headers) {
        return res;
    }
    let result = async {
        check_job_id(&id)?;
        let history = match job_state(&state, &id).await? {
            Some(JobState::Finished(h)) => h,
            Some(JobState::Queued(status)) => return Err((StatusCode::CONFLICT, format!("Video job is {}", status))),
            None => return Err((StatusCode::NOT_FOUND, "Video job not found".to_string())),
        };
        let index = query.index.unwrap_or(0);
        let (filename, subfolder, folder_type) = video_outputs(&history)
            .into_iter()
            .nth(index)
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Video job has no output {}", index)))?;

        let upstream = state
            .comfy_client
            .view(&filename, &subfolder, &folder_type)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("ComfyUI: {}", e)))?;
        let (content_type, extension) = crate::comfy::derive_content_type(&filename);
        let mut builder = Response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_DISPOSITION, format!("inline; filename=\"{}.{}\"", id, extension));
        if let Some(len) = upstream.content_length() {
            builder = builder.header(header::CONTENT_LENGTH, len);
        }
        Ok(builder.body(Body::from_stream(upstream.bytes_stream())).unwrap())
    }
    .await;
    result.unwrap_or_else(error_response)
}

/// Not part of OpenRouter's API: removes a job from the queue or interrupts it while it runs.
pub async fn cancel_video_handler(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(res) = crate::check_api_key(&state, &headers) {
        return res;
    }
    let result = async {
        check_job_id(&id)?;
        match job_state(&state, &id).await? {
            Some(JobState::Queued(_)) => {
                state.comfy_client.cancel_prompt(&id).await;
                tracing::info!("Cancelled video job {}", id);
                Ok(Json(json!({ "id": id, "status": "cancelled" })).into_response())
            }
            Some(JobState::Finished(_)) => Err((StatusCode::CONFLICT, "Video job has already finished".to_string())),
            None => Err((StatusCode::NOT_FOUND, "Video job not found".to_string())),
        }
    }
    .await;
    result.unwrap_or_else(error_response)
}

pub async fn video_models_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(res) = crate::check_api_key(&state, &headers) {
        return res;
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let config = state.config.read().await;
    let mut names: Vec<&String> = config
        .video_workflows
        .iter()
        .filter(|(name, c)| c.active && is_valid_name(name) && std::path::Path::new(&format!("{}/{}.json", VIDEO_DIR, name)).is_file())
        .map(|(name, _)| name)
        .collect();
    names.sort();
    let data: Vec<Value> = names
        .into_iter()
        .map(|id| {
            let c = &config.video_workflows[id];
            json!({
                "id": id,
                "name": id,
                "created": now,
                "supported_durations": (c.min_duration..=c.max_duration).collect::<Vec<_>>(),
                "supported_resolutions": c.resolutions,
                "supported_aspect_ratios": c.aspect_ratios,
                "supported_inputs": ["first_frame", "last_frame", "image", "audio", "video"],
            })
        })
        .collect();
    Json(json!({ "data": data })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(json: Value) -> VideoParams {
        parse_params(json.to_string().as_bytes()).unwrap()
    }

    fn template() -> Value {
        let content = std::fs::read_to_string(format!("{}/minimax_h3.json", VIDEO_DIR)).unwrap();
        serde_json::from_str(&content).unwrap()
    }

    fn cfg() -> VideoWorkflowConfig {
        VideoWorkflowConfig { active: true, ..Default::default() }
    }

    #[test]
    fn test_calc_length() {
        let c = cfg();
        assert_eq!(calc_length(5, &c), 124); // 120 frames -> 17 * 7 + 5
        assert_eq!(calc_length(15, &c), 362); // 360 -> 17 * 21 + 5
        for d in c.min_duration..=c.max_duration {
            let l = calc_length(d, &c);
            assert_eq!((l - 5) % 17, 0);
            assert!(l >= d * 24 && l < d * 24 + 17);
        }
        let plain = VideoWorkflowConfig { fps: 30, frame_step: 1, frame_offset: 0, ..cfg() };
        assert_eq!(calc_length(5, &plain), 150);
        let step8 = VideoWorkflowConfig { fps: 16, frame_step: 4, frame_offset: 1, ..cfg() };
        assert_eq!(calc_length(5, &step8), 81); // 80 -> 4 * 20 + 1
    }

    #[test]
    fn test_duration_validation() {
        let parse = |d: Value| parse_params(json!({"model": "m", "prompt": "p", "duration": d}).to_string().as_bytes()).map(|p| p.duration);
        assert_eq!(parse(json!(5)).unwrap(), Some(5));
        for bad in [json!(5.5), json!("5"), json!(-5)] {
            assert_eq!(parse(bad).unwrap_err().0, StatusCode::BAD_REQUEST);
        }
        let check = |d: u64, c: &VideoWorkflowConfig| check_duration(&params(json!({"model": "m", "prompt": "p", "duration": d})), c);
        assert_eq!(check(5, &cfg()).unwrap(), 5);
        assert_eq!(check(15, &cfg()).unwrap(), 15);
        assert_eq!(check(3, &cfg()).unwrap(), 3);
        assert!(check(2, &cfg()).is_err());
        assert!(check(16, &cfg()).is_err());
        assert!(check(15, &VideoWorkflowConfig { max_duration: 10, ..cfg() }).is_err());
        assert_eq!(check_duration(&params(json!({"model": "m", "prompt": "p"})), &cfg()).unwrap(), 5);
        assert!(parse_params(br#"{"model": "m", "prompt": "p", "frame_images": []}"#).is_err());
        assert!(parse_params(br#"{"prompt": "p"}"#).is_err());
    }

    #[test]
    fn test_calc_resolution() {
        let c = cfg();
        let res = |j: Value| calc_resolution(&params(j), &c);
        assert_eq!(res(json!({"model": "m", "prompt": "p"})).unwrap(), (1344, 768));
        assert_eq!(res(json!({"model": "m", "prompt": "p", "resolution": "720p", "aspect_ratio": "9:16"})).unwrap(), (704, 1280));
        assert_eq!(res(json!({"model": "m", "prompt": "p", "size": "1280x720"})).unwrap(), (1280, 704));
        assert!(res(json!({"model": "m", "prompt": "p", "resolution": "4K"})).is_err());
        assert!(res(json!({"model": "m", "prompt": "p", "aspect_ratio": "5:4"})).is_err());
        assert!(res(json!({"model": "m", "prompt": "p", "size": "720p"})).is_err());
        assert!(res(json!({"model": "m", "prompt": "p", "size": "64x64", "resolution": "480p"})).is_err());

        let custom = VideoWorkflowConfig { resolutions: vec!["720p".into()], default_resolution: "720p".into(), size_multiple: 16, ..cfg() };
        assert_eq!(calc_resolution(&params(json!({"model": "m", "prompt": "p"})), &custom).unwrap(), (1280, 720));
        assert!(calc_resolution(&params(json!({"model": "m", "prompt": "p", "resolution": "480p"})), &custom).is_err());
    }

    #[test]
    fn test_build_i2v() {
        let p = params(json!({"model": "minimax_h3", "prompt": "a cat", "duration": 6}));
        let inputs = VideoInputs { first_frame: Some("f.png".into()), last_frame: Some("l.png".into()), ..Default::default() };
        let wf = build_video_prompt(template(), &cfg(), &p, &inputs, 42).unwrap();
        assert!(wf.as_object().unwrap().keys().all(|k| !k.starts_with(I2V_PREFIX) && !k.starts_with(R2V_PREFIX)));
        assert_eq!(wf["model_sd"]["inputs"]["unet_name"], "minimax_h3_fl2va_pruned_int8_convrot.safetensors");
        let g = &wf["generator"];
        assert_eq!(g["class_type"], "MiniMaxH3ImageToVideo");
        assert_eq!(g["inputs"]["prompt"], "a cat");
        assert_eq!(g["inputs"]["length"], calc_length(6, &cfg()));
        assert_eq!(g["inputs"]["first_frame"], json!(["first_frame", 0]));
        assert_eq!(g["inputs"]["last_frame"], json!(["last_frame", 0]));
        assert_eq!(wf["last_frame"]["inputs"]["image"], "l.png");
        assert_eq!(wf["rnd_seed"]["inputs"]["noise_seed"], 42);
        assert_eq!(wf["scheduler"]["inputs"]["denoise"], 1);
    }

    #[test]
    fn test_build_custom_nodes() {
        let tpl = json!({
            "t2v": {"class_type": "Gen", "inputs": {}},
            "seed": {"class_type": "Seed", "inputs": {}},
            "save": {"class_type": "SaveVideo", "inputs": {}}
        });
        let c = VideoWorkflowConfig {
            i2v_node: "t2v".into(),
            r2v_node: "missing".into(),
            generator_node: "gen".into(),
            prompt_field: "text".into(),
            image_prefix: "refs.".into(),
            seed_node: "seed".into(),
            seed_field: "seed".into(),
            output_node: "save".into(),
            ..cfg()
        };
        let wf = build_video_prompt(tpl.clone(), &c, &params(json!({"model": "m", "prompt": "hi"})), &VideoInputs::default(), 7).unwrap();
        assert_eq!(wf["gen"]["inputs"]["text"], "hi");
        assert_eq!(wf["seed"]["inputs"]["seed"], 7);
        assert!(wf.get("t2v").is_none() && wf.get("scheduler").is_none());

        // No r2v variant in this workflow
        let refs = VideoInputs { images: vec!["a.png".into()], ..Default::default() };
        assert_eq!(build_video_prompt(tpl, &c, &params(json!({"model": "m", "prompt": "hi"})), &refs, 7).unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_build_r2v() {
        let p = params(json!({"model": "minimax_h3", "prompt": "p", "video_sound": [0]}));
        let inputs = VideoInputs {
            images: vec!["a.png".into(), "b.png".into()],
            audios: vec!["s.wav".into()],
            videos: vec!["v.mp4".into()],
            ..Default::default()
        };
        let wf = build_video_prompt(template(), &cfg(), &p, &inputs, 1).unwrap();
        let g = &wf["generator"]["inputs"];
        assert_eq!(wf["generator"]["class_type"], "MiniMaxH3ReferenceToVideo");
        assert_eq!(wf["model_sd"]["inputs"]["unet_name"], "minimax_h3_ref2va_pruned_int8_convrot.safetensors");
        assert_eq!(g["ref_images.ref_image_1"], json!(["ref_image_1", 0]));
        assert_eq!(wf["ref_image_1"]["inputs"]["image"], "b.png");
        assert_eq!(g["ref_videos.ref_video_0"], json!(["ref_video_components_0", 0]));
        assert_eq!(wf["ref_video_0"]["inputs"]["file"], "v.mp4");
        assert_eq!(wf["ref_video_components_0"]["inputs"]["video"], json!(["ref_video_0", 0]));
        assert_eq!(g["ref_audios.ref_audio_0"], json!(["ref_audio_0", 0]));
        assert_eq!(g["ref_audios.ref_audio_1"], json!(["ref_video_components_0", 1]));

        let bad = params(json!({"model": "minimax_h3", "prompt": "p", "video_sound": [1]}));
        assert!(build_video_prompt(template(), &cfg(), &bad, &inputs, 1).is_err());

        let limited = VideoWorkflowConfig { max_audios: Some(1), ..cfg() };
        assert!(build_video_prompt(template(), &limited, &p, &inputs, 1).is_err()); // 1 file + 1 video_sound

        let mixed = VideoInputs { first_frame: Some("f.png".into()), images: vec!["a.png".into()], ..Default::default() };
        assert!(build_video_prompt(template(), &cfg(), &params(json!({"model": "m", "prompt": "p"})), &mixed, 1).is_err());
    }

    #[test]
    fn test_build_variant_nodes() {
        let tpl = json!({
            "i2v:unet": {"class_type": "UNETLoader", "inputs": {"unet_name": "fl2v"}},
            "r2v:unet": {"class_type": "UNETLoader", "inputs": {"unet_name": "ref2v"}},
            "i2v:scheduler": {"class_type": "BasicScheduler", "inputs": {"model": ["unet", 0]}},
            "r2v:scheduler": {"class_type": "BasicScheduler", "inputs": {"model": ["unet", 0]}},
            "i2v:generator": {"class_type": "MiniMaxH3ImageToVideo", "inputs": {}},
            "r2v:generator": {"class_type": "MiniMaxH3ReferenceToVideo", "inputs": {}},
            "guider": {"class_type": "BasicGuider", "inputs": {"model": ["unet", 0], "conditioning": ["generator", 0]}},
            "rnd_seed": {"class_type": "RandomNoise", "inputs": {}},
            "video_output": {"class_type": "SaveVideo", "inputs": {}}
        });
        let p = params(json!({"model": "m", "prompt": "p"}));
        let no_prefix = |wf: &Value| wf.as_object().unwrap().keys().all(|k| !k.starts_with(I2V_PREFIX) && !k.starts_with(R2V_PREFIX));

        let wf = build_video_prompt(tpl.clone(), &cfg(), &p, &VideoInputs::default(), 1).unwrap();
        assert!(no_prefix(&wf));
        assert_eq!(wf["unet"]["inputs"]["unet_name"], "fl2v");
        assert_eq!(wf["generator"]["class_type"], "MiniMaxH3ImageToVideo");
        assert_eq!(wf["generator"]["inputs"]["prompt"], "p");
        assert_eq!(wf["scheduler"]["inputs"]["denoise"], 1);

        let refs = VideoInputs { images: vec!["a.png".into()], ..Default::default() };
        let wf = build_video_prompt(tpl.clone(), &cfg(), &p, &refs, 1).unwrap();
        assert!(no_prefix(&wf));
        assert_eq!(wf["unet"]["inputs"]["unet_name"], "ref2v");
        assert_eq!(wf["generator"]["class_type"], "MiniMaxH3ReferenceToVideo");
        assert_eq!(wf["generator"]["inputs"]["ref_images.ref_image_0"], json!(["ref_image_0", 0]));

        // A variant node may not shadow a shared one
        let mut clash = tpl.clone();
        clash["unet"] = json!({"class_type": "UNETLoader", "inputs": {}});
        assert_eq!(build_video_prompt(clash, &cfg(), &p, &VideoInputs::default(), 1).unwrap_err().0, StatusCode::INTERNAL_SERVER_ERROR);

        // Only an i2v generator: references are rejected
        let mut i2v_only = tpl;
        i2v_only.as_object_mut().unwrap().remove("r2v:generator");
        assert_eq!(build_video_prompt(i2v_only, &cfg(), &p, &refs, 1).unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_split_parts() {
        let parts = vec![
            ("request".to_string(), None, b"{}".to_vec()),
            ("image[]".to_string(), Some("a.jpg".to_string()), vec![1]),
            ("audio".to_string(), Some("a.mp3".to_string()), vec![2]),
        ];
        let (req, files) = split_parts(parts).unwrap();
        assert_eq!(req, b"{}");
        assert_eq!(files.iter().map(|f| f.role).collect::<Vec<_>>(), vec![Role::Image, Role::Audio]);

        assert!(split_parts(vec![("image".into(), None, vec![])]).is_err()); // no request
        assert!(split_parts(vec![("request".into(), None, vec![]), ("photo".into(), None, vec![])]).is_err());
        let two_frames = vec![("request".into(), None, vec![]), ("first_frame".into(), None, vec![]), ("first_frame".into(), None, vec![])];
        assert!(split_parts(two_frames).is_err());
    }

    #[test]
    fn test_status_mapping() {
        let done = json!({
            "status": {"status_str": "success", "completed": true, "messages": []},
            "outputs": {
                "ref_video_0": {"images": [{"filename": "comfy_serve_temp_x.mp4", "subfolder": "", "type": "input"}], "animated": [true]},
                "video_output": {"images": [{"filename": "video_00001_.mp4", "subfolder": "comfy-serve", "type": "output"}], "animated": [true]}
            }
        });
        assert_eq!(history_status(&done), ("completed", None));
        assert_eq!(video_outputs(&done), vec![("video_00001_.mp4".into(), "comfy-serve".into(), "output".into())]);

        let failed = json!({"status": {"status_str": "error", "completed": false, "messages": [
            ["execution_start", {}],
            ["execution_error", {"node_type": "LoadVideo", "exception_message": "bad file\n"}]
        ]}, "outputs": {}});
        assert_eq!(history_status(&failed), ("failed", Some("LoadVideo: bad file".into())));

        let interrupted = json!({"status": {"status_str": "error", "messages": [["execution_interrupted", {}]]}, "outputs": {}});
        assert_eq!(history_status(&interrupted).0, "cancelled");

        let no_video = json!({"status": {"status_str": "success"}, "outputs": {"x": {"images": [{"filename": "a.png"}]}}});
        assert_eq!(history_status(&no_video).0, "failed");

        let queue = json!({"queue_running": [[0, "a", {}, {}, []]], "queue_pending": [[1, "b", {}, {}, []]]});
        assert_eq!(queue_status(&queue, "a"), Some("in_progress"));
        assert_eq!(queue_status(&queue, "b"), Some("pending"));
        assert_eq!(queue_status(&queue, "c"), None);
    }

    #[test]
    fn test_names() {
        assert!(is_valid_name("minimax_h3_fast"));
        assert!(!is_valid_name("../config"));
        assert!(!is_valid_name("video/x"));
        assert!(check_job_id("6f1c1d1e-0000-4000-8000-000000000000").is_ok());
        assert!(check_job_id("a/../b").is_err());
    }
}
