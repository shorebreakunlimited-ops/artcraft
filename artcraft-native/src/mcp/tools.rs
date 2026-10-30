//! The MCP tools: list models, generate images (PhotoCraft) and videos (FilmCraft), check jobs.
//! Requests are built exactly like the Create Image / Create Video pages build theirs, so the
//! server sees the same OmniGen fields either way.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use artcraft_client::endpoints::credits::get_session_credits::get_session_credits;
use artcraft_client::endpoints::media_files::upload_image_media_file_from_file::{UploadImageFromFileArgs, upload_image_media_file_from_file};
use artcraft_client::endpoints::omni_gen::models::image::omni_gen_list_image_models::{OmniGenListImageModelsArgs, omni_gen_list_image_models};
use artcraft_client::endpoints::omni_gen::models::video::omni_gen_list_video_models::{OmniGenListVideoModelsArgs, omni_gen_list_video_models};
use artcraft_client::utils::api_host::ApiHost;
use artcraft_client::utils::basic_json_post_request::basic_json_post_request;
use enums::common::payments_namespace::PaymentsNamespace;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::Scope;
use super::timeline;
use crate::backend::wire::{BatchMedia, Job, JobPhase, MediaFile, SessionInfo, SessionJobs};
use crate::backend::{Api, Modality, api_message, http_client, job_tokens, session};
use crate::models::{self, ModelInfo};

/// How long a generate call waits for its results by default before handing back job tokens.
const DEFAULT_WAIT_SECS: u64 = 120;
/// The longest any single call will wait.
const MAX_WAIT_SECS: u64 = 1800;
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const NOT_SIGNED_IN: &str = "Not signed in to ArtCraft. Open the ArtCraft app and sign in, then try again.";

/// The tools and the account they act for. Credentials are read per call, so signing in to the
/// app takes effect without restarting the server.
pub struct Server {
  scope: Scope,
  root: PathBuf,
  host: ApiHost,
  http: reqwest::Client,
}

/// `generate_image`'s arguments.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GenerateImageArgs {
  prompt: String,
  model: Option<String>,
  aspect_ratio: Option<String>,
  resolution: Option<String>,
  quality: Option<String>,
  count: Option<u16>,
  reference_images: Vec<String>,
  estimate_only: bool,
  wait_seconds: Option<u64>,
  save_dir: Option<String>,
}

/// `generate_video`'s arguments.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GenerateVideoArgs {
  prompt: String,
  model: Option<String>,
  aspect_ratio: Option<String>,
  resolution: Option<String>,
  duration_seconds: Option<u16>,
  generate_audio: Option<bool>,
  count: Option<u16>,
  start_frame: Option<String>,
  end_frame: Option<String>,
  reference_images: Vec<String>,
  estimate_only: bool,
  wait_seconds: Option<u64>,
  save_dir: Option<String>,
}

/// `get_job`'s arguments.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GetJobArgs {
  job_tokens: Vec<String>,
  job_token: Option<String>,
  wait_seconds: Option<u64>,
  save_dir: Option<String>,
}

/// `GET /v1/jobs/job/{token}` (no batch token or failure message; `wait_for`'s fallback).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct JobStateResponse {
  success: bool,
  state: Job,
}

impl Server {
  pub fn new(scope: Scope) -> Self {
    let root = session::data_root();
    let host = session::api_host(&root);
    Self { scope, root, host, http: http_client() }
  }

  pub fn scope(&self) -> Scope {
    self.scope
  }

  /// The `tools/list` entries for this scope.
  pub fn tool_definitions(&self) -> Vec<Value> {
    let mut tools = vec![account_tool()];
    if self.scope.images() {
      tools.push(list_models_tool(Modality::Image));
      tools.push(generate_image_tool());
      tools.push(timeline::split_thumbnail_tool());
    }
    if self.scope.videos() {
      tools.push(list_models_tool(Modality::Video));
      tools.push(generate_video_tool());
      tools.push(timeline::assemble_sequence_tool());
    }
    tools.push(get_job_tool());
    tools
  }

  /// Runs tool `name`, or `None` when this scope has no such tool.
  pub async fn call(&self, name: &str, arguments: Value) -> Option<Value> {
    let result = match name {
      "account" => self.account().await,
      "list_image_models" if self.scope.images() => self.list_models(Modality::Image).await,
      "list_video_models" if self.scope.videos() => self.list_models(Modality::Video).await,
      "generate_image" if self.scope.images() => match parse_args::<GenerateImageArgs>(arguments) {
        Ok(args) => self.generate_image(args).await,
        Err(err) => Err(err),
      },
      "generate_video" if self.scope.videos() => match parse_args::<GenerateVideoArgs>(arguments) {
        Ok(args) => self.generate_video(args).await,
        Err(err) => Err(err),
      },
      "split_thumbnail" if self.scope.images() => match parse_args::<timeline::SplitThumbnailArgs>(arguments) {
        Ok(args) => timeline::split_thumbnail(args).await,
        Err(err) => Err(err),
      },
      "assemble_sequence" if self.scope.videos() => match parse_args::<timeline::AssembleSequenceArgs>(arguments) {
        Ok(args) => timeline::assemble_sequence(args).await,
        Err(err) => Err(err),
      },
      "get_job" => match parse_args::<GetJobArgs>(arguments) {
        Ok(args) => self.get_job(args).await,
        Err(err) => Err(err),
      },
      _ => return None,
    };
    Some(tool_result(result))
  }

  // --- Tools ---------------------------------------------------------------------------------

  async fn account(&self) -> Result<Value, String> {
    let api = self.api();
    if api.creds.is_none() {
      return Ok(json!({ "signed_in": false, "message": NOT_SIGNED_IN }));
    }
    let info = api.get_json::<SessionInfo>("/v1/session").await?;
    let Some(user) = info.user.filter(|_| info.logged_in) else {
      return Ok(json!({ "signed_in": false, "message": "The saved ArtCraft session has expired. Open the ArtCraft app and sign in again." }));
    };
    let credits = get_session_credits(&api.host, api.creds.as_ref(), PaymentsNamespace::Artcraft).await.map(|c| c.sum_total_credits).map_err(|e| api_message(&e))?;
    Ok(json!({ "signed_in": true, "username": user.username, "display_name": user.display_name, "credits": credits }))
  }

  async fn list_models(&self, modality: Modality) -> Result<Value, String> {
    let models = self.models(modality).await?;
    let summaries: Vec<Value> = models.iter().filter(|m| !m.disabled).map(|m| model_summary(m, modality)).collect();
    Ok(json!({ "models": summaries }))
  }

  async fn generate_image(&self, args: GenerateImageArgs) -> Result<Value, String> {
    let api = self.api();
    if api.creds.is_none() && !args.estimate_only {
      return Err(NOT_SIGNED_IN.to_owned());
    }
    let models = self.models(Modality::Image).await?;
    let model = pick_model(&models, args.model.as_deref(), |_| true)?;
    check_prompt(model, &args.prompt, args.estimate_only)?;
    if args.reference_images.len() > model.image_refs_max {
      return Err(too_many_refs(model, args.reference_images.len()));
    }

    let mut f = Map::new();
    f.insert("model".into(), json!(model.id));
    if !args.estimate_only {
      f.insert("prompt".into(), json!(args.prompt));
    }
    f.insert("image_batch_count".into(), json!(pick_count(model, args.count)?));
    if let Some(v) = pick_option(model, "aspect_ratio", args.aspect_ratio.as_deref(), &model.aspect_ratios, model.aspect_default.as_ref())? {
      f.insert("aspect_ratio".into(), json!(v));
    }
    if let Some(v) = pick_option(model, "resolution", args.resolution.as_deref(), &model.resolutions, model.resolution_default.as_ref())?.filter(|_| !model.resolutions.is_empty()) {
      f.insert("resolution".into(), json!(v));
    }
    if let Some(v) = pick_option(model, "quality", args.quality.as_deref(), &model.qualities, model.quality_default.as_ref())?.filter(|_| !model.qualities.is_empty()) {
      f.insert("quality".into(), json!(v));
    }
    if !args.reference_images.is_empty() {
      f.insert("image_media_tokens".into(), json!(self.reference_tokens(&api, &args.reference_images, args.estimate_only).await?));
    }

    if args.estimate_only {
      return self.estimate(&api, Modality::Image, f).await;
    }
    self.submit(&api, Modality::Image, f, args.wait_seconds, args.save_dir.as_deref()).await
  }

  async fn generate_video(&self, args: GenerateVideoArgs) -> Result<Value, String> {
    let api = self.api();
    if api.creds.is_none() && !args.estimate_only {
      return Err(NOT_SIGNED_IN.to_owned());
    }
    let reference = !args.reference_images.is_empty();
    if reference && (args.start_frame.is_some() || args.end_frame.is_some()) {
      return Err("Use either reference_images or start_frame/end_frame, not both.".to_owned());
    }
    if args.end_frame.is_some() && args.start_frame.is_none() {
      return Err("end_frame needs a start_frame too.".to_owned());
    }
    let has_frames = args.start_frame.is_some();
    let models = self.models(Modality::Video).await?;
    // Without an explicit model, take the first that can run this request as given.
    let model = pick_model(&models, args.model.as_deref(), |m| if reference { m.image_refs_max > 0 } else if has_frames { m.start_frame } else { m.text_to_video && !m.start_frame_required })?;
    check_prompt(model, &args.prompt, args.estimate_only)?;
    if reference && args.reference_images.len() > model.image_refs_max {
      return Err(too_many_refs(model, args.reference_images.len()));
    }
    if has_frames && !model.start_frame {
      return Err(format!("{} doesn't take a start_frame.", model.id));
    }
    if args.end_frame.is_some() && !model.end_frame {
      return Err(format!("{} doesn't take an end_frame.", model.id));
    }
    if !has_frames && (model.start_frame_required || (!model.text_to_video && !reference)) {
      return Err(format!("{} needs a start_frame image.", model.id));
    }

    let mut f = Map::new();
    f.insert("model".into(), json!(model.id));
    if !args.estimate_only {
      f.insert("prompt".into(), json!(args.prompt));
    }
    if let Some(v) = pick_option(model, "aspect_ratio", args.aspect_ratio.as_deref(), &model.aspect_ratios, model.aspect_default.as_ref())? {
      f.insert("aspect_ratio".into(), json!(v));
    }
    if let Some(v) = pick_option(model, "resolution", args.resolution.as_deref(), &model.resolutions, model.resolution_default.as_ref())?.filter(|_| !model.resolutions.is_empty()) {
      f.insert("resolution".into(), json!(v));
    }
    // Bitrate and format aren't exposed; send the model's defaults like the page does.
    if let Some(v) = ModelInfo::resolve(None, &model.bitrates, model.bitrate_default.as_ref()).filter(|_| !model.bitrates.is_empty()) {
      f.insert("bitrate".into(), json!(v));
    }
    if let Some(v) = ModelInfo::resolve(None, &model.output_formats, model.output_format_default.as_ref()).filter(|_| !model.output_formats.is_empty()) {
      f.insert("output_format".into(), json!(v));
    }
    if let Some(d) = model.resolve_duration(args.duration_seconds, reference) {
      f.insert("duration_seconds".into(), json!(d));
    }
    if model.sound_toggle {
      f.insert("generate_audio".into(), json!(args.generate_audio.unwrap_or(true)));
    }
    // Only models that list batch sizes take a count (older ones reject the field).
    if !model.batch_sizes.is_empty() {
      f.insert("video_batch_count".into(), json!(pick_count(model, args.count)?));
    } else if args.count.is_some_and(|c| c != 1) {
      return Err(format!("{} makes one video per request.", model.id));
    }
    if reference {
      f.insert("reference_image_media_tokens".into(), json!(self.reference_tokens(&api, &args.reference_images, args.estimate_only).await?));
      if args.estimate_only {
        f.insert("estimate_only".into(), json!({ "total_input_video_duration_millis": 0, "total_input_audio_duration_millis": 0 }));
      }
    } else if let Some(start) = &args.start_frame {
      let mut frames = vec![start.clone()];
      frames.extend(args.end_frame.clone());
      let tokens = self.reference_tokens(&api, &frames, args.estimate_only).await?;
      f.insert("start_frame_image_media_token".into(), json!(tokens[0]));
      if let Some(end) = tokens.get(1) {
        f.insert("end_frame_image_media_token".into(), json!(end));
      }
    }

    if args.estimate_only {
      return self.estimate(&api, Modality::Video, f).await;
    }
    self.submit(&api, Modality::Video, f, args.wait_seconds, args.save_dir.as_deref()).await
  }

  async fn get_job(&self, args: GetJobArgs) -> Result<Value, String> {
    let api = self.api();
    if api.creds.is_none() {
      return Err(NOT_SIGNED_IN.to_owned());
    }
    let mut tokens = args.job_tokens;
    tokens.extend(args.job_token);
    if tokens.is_empty() {
      return Err("Pass the job_tokens a generate call returned.".to_owned());
    }
    let jobs = self.wait_for(&api, &tokens, args.wait_seconds.unwrap_or(0)).await;
    self.report(&api, jobs, args.save_dir.as_deref()).await
  }

  // --- Generation plumbing -------------------------------------------------------------------

  /// `POST /v1/omni_gen/cost/{modality}` for the request as built.
  async fn estimate(&self, api: &Api, modality: Modality, fields: Map<String, Value>) -> Result<Value, String> {
    let path = format!("/v1/omni_gen/cost/{}", modality_path(modality));
    let response = basic_json_post_request::<_, Value>(&api.host, &path, api.creds.as_ref(), fields.clone()).await.map_err(|e| api_message(&e))?;
    Ok(json!({ "cost_in_credits": response.get("cost_in_credits").cloned().unwrap_or(Value::Null), "request": fields }))
  }

  /// Starts the generation, then waits for it (up to `wait_seconds`) and reports the results.
  async fn submit(&self, api: &Api, modality: Modality, mut fields: Map<String, Value>, wait_seconds: Option<u64>, save_dir: Option<&str>) -> Result<Value, String> {
    fields.insert("idempotency_token".into(), json!(uuid::Uuid::new_v4().to_string()));
    let path = format!("/v1/omni_gen/generate/{}", modality_path(modality));
    let response = basic_json_post_request::<_, Value>(&api.host, &path, api.creds.as_ref(), fields.clone()).await.map_err(|e| api_message(&e))?;
    let tokens = job_tokens(&response);
    if tokens.is_empty() {
      return Err("The server didn't start a job.".to_owned());
    }
    fields.remove("idempotency_token");
    let jobs = self.wait_for(api, &tokens, wait_seconds.unwrap_or(DEFAULT_WAIT_SECS)).await;
    let mut report = self.report(api, jobs, save_dir).await?;
    report["request"] = Value::Object(fields);
    Ok(report)
  }

  /// Polls `tokens` until every job has finished or `wait_seconds` pass; the last state of each.
  /// The session's job list comes first: unlike `/v1/jobs/job/{token}` it has each result's
  /// batch token (so every image of a batch is found) and the failure message. It only covers
  /// the last 36 hours, so older tokens fall back to the single-job lookup.
  async fn wait_for(&self, api: &Api, tokens: &[String], wait_seconds: u64) -> Vec<Result<Job, (String, String)>> {
    let deadline = Instant::now() + Duration::from_secs(wait_seconds.min(MAX_WAIT_SECS));
    loop {
      let session_jobs = api.get_json::<SessionJobs>("/v1/jobs/session").await.map(|s| s.jobs).unwrap_or_default();
      let mut jobs = Vec::with_capacity(tokens.len());
      for token in tokens {
        if let Some(job) = session_jobs.iter().find(|j| &j.job_token == token) {
          jobs.push(Ok(job.clone()));
          continue;
        }
        let job = api.get_json::<JobStateResponse>(&format!("/v1/jobs/job/{token}")).await.and_then(|r| if r.success { Ok(r.state) } else { Err("The server couldn't find this job.".to_owned()) });
        jobs.push(job.map_err(|err| (token.clone(), err)));
      }
      let running = jobs.iter().any(|j| matches!(j, Ok(job) if job.status.phase() == JobPhase::Running));
      if !running || Instant::now() + POLL_INTERVAL > deadline {
        return jobs;
      }
      tokio::time::sleep(POLL_INTERVAL).await;
    }
  }

  /// Each job's status, plus its media (downloaded into `save_dir` when given) once finished.
  async fn report(&self, api: &Api, jobs: Vec<Result<Job, (String, String)>>, save_dir: Option<&str>) -> Result<Value, String> {
    let save_dir = save_dir.map(expand_home);
    if let Some(dir) = &save_dir {
      tokio::fs::create_dir_all(dir).await.map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    }
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    let mut all_done = true;
    for job in jobs {
      let job = match job {
        Ok(job) => job,
        Err((token, err)) => {
          entries.push(json!({ "job_token": token, "status": "unknown", "error": err }));
          continue;
        },
      };
      let mut entry = json!({ "job_token": job.job_token, "progress_percentage": job.status.progress_percentage });
      match job.status.phase() {
        JobPhase::Running => {
          all_done = false;
          entry["status"] = json!("running");
        },
        JobPhase::Failed => {
          entry["status"] = json!("failed");
          let detail = job.status.maybe_failure_message.clone().filter(|m| !m.is_empty());
          entry["failure_reason"] = json!(detail.map_or_else(|| job.status.failure_reason(), |d| format!("{}: {d}", job.status.failure_reason())));
        },
        JobPhase::Succeeded => {
          entry["status"] = json!("succeeded");
          let mut media = Vec::new();
          for file in self.job_media(api, &job).await {
            if !seen.insert(file.token.clone()) {
              continue;
            }
            let mut item = json!({ "media_token": file.token, "media_class": file.media_class, "url": file.media_links.cdn_url });
            if let Some(dir) = &save_dir {
              match self.download(&file, dir).await {
                Ok(path) => item["saved_path"] = json!(path.display().to_string()),
                Err(err) => item["save_error"] = json!(err),
              }
            }
            media.push(item);
          }
          entry["media"] = json!(media);
        },
      }
      entries.push(entry);
    }
    let note = if all_done { "All jobs have finished." } else { "Still running. Call get_job with these job_tokens (and wait_seconds) to collect the results." };
    Ok(json!({ "done": all_done, "jobs": entries, "note": note }))
  }

  /// A finished job's files: the whole batch when it made several.
  async fn job_media(&self, api: &Api, job: &Job) -> Vec<MediaFile> {
    let Some(result) = &job.maybe_result else {
      return Vec::new();
    };
    if let Some(batch) = result.maybe_batch_token.as_deref().filter(|b| !b.is_empty()) {
      if let Ok(batch) = api.get_json::<BatchMedia>(&format!("/v1/media_files/batch_gen_redux/{batch}")).await {
        if !batch.media_files.is_empty() {
          return batch.media_files;
        }
      }
    }
    let mut file = MediaFile { token: result.entity_token.clone(), media_links: result.media_links.clone(), ..Default::default() };
    file.media_class = Some(job.request.inference_category.clone()).filter(|c| !c.is_empty());
    vec![file]
  }

  /// Saves `file` as `<dir>/<media token>.<ext>`.
  async fn download(&self, file: &MediaFile, dir: &Path) -> Result<PathBuf, String> {
    let url = &file.media_links.cdn_url;
    if url.is_empty() {
      return Err("The file has no download link.".to_owned());
    }
    let ext = url::Url::parse(url).ok().and_then(|u| Path::new(u.path()).extension().map(|e| e.to_string_lossy().into_owned())).unwrap_or_else(|| if file.media_class.as_deref() == Some("video") { "mp4".to_owned() } else { "png".to_owned() });
    let path = dir.join(format!("{}.{ext}", file.token));
    // Downloads get no overall timeout: videos can be large.
    let response = reqwest::get(url).await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
      return Err(format!("HTTP {} downloading {url}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    tokio::fs::write(&path, &bytes).await.map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    Ok(path)
  }

  /// Media tokens for references given as tokens (`mf_…`) or local image paths (uploaded). An
  /// estimate doesn't upload; placeholders price the same.
  async fn reference_tokens(&self, api: &Api, refs: &[String], estimate: bool) -> Result<Vec<String>, String> {
    let mut tokens = Vec::with_capacity(refs.len());
    for (i, reference) in refs.iter().enumerate() {
      if reference.starts_with("mf_") {
        tokens.push(reference.clone());
        continue;
      }
      let path = expand_home(reference);
      if !path.is_file() {
        return Err(format!("Reference image not found: {}", path.display()));
      }
      if estimate {
        tokens.push(format!("mf_estimate_{i}"));
        continue;
      }
      let upload = upload_image_media_file_from_file(UploadImageFromFileArgs { api_host: &api.host, maybe_creds: api.creds.as_ref(), path: &path, is_intermediate_system_file: true, maybe_prompt_token: None, maybe_generation_provider: None, maybe_batch_token: None }).await;
      let token = upload.map_err(|e| format!("Uploading {} failed: {}", path.display(), api_message(&e)))?.media_file_token.to_string();
      tokens.push(token);
    }
    Ok(tokens)
  }

  /// The OmniGen model listing (`Backend::load_models`).
  async fn models(&self, modality: Modality) -> Result<Vec<ModelInfo>, String> {
    let host = &self.host;
    match modality {
      Modality::Image => omni_gen_list_image_models(OmniGenListImageModelsArgs { api_host: host, maybe_creds: None, provider: None }).await.map(|r| r.models.iter().map(models::image_model).collect()).map_err(|e| api_message(&e)),
      Modality::Video => omni_gen_list_video_models(OmniGenListVideoModelsArgs { api_host: host, maybe_creds: None, provider: None }).await.map(|r| r.models.iter().map(models::video_model).collect()).map_err(|e| api_message(&e)),
      Modality::Audio => Err("Audio isn't available over MCP.".to_owned()),
    }
  }

  /// A client for this call, signed in with whatever session the app has saved right now.
  fn api(&self) -> Api {
    let creds = session::load(&self.root).map(|c| c.to_set());
    Api { host: self.host.clone(), creds, http: self.http.clone() }
  }
}

/// What the server tells the agent up front.
pub fn instructions(scope: Scope) -> String {
  let what = match scope {
    Scope::Photo => "images",
    Scope::Film => "videos",
    Scope::All => "images and videos",
  };
  format!(
    "Generates {what} with the user's ArtCraft account. Generating spends the user's ArtCraft credits: \
     use estimate_only to price a request first when cost matters, and `account` to see the balance. \
     Model options differ per model; call the list_*_models tool and pass only values a model offers. \
     assemble_sequence and split_thumbnail edit local files with FFmpeg and cost nothing. \
     Generate calls wait up to wait_seconds (default {DEFAULT_WAIT_SECS}) for results; if they're still \
     running, call get_job with the returned job_tokens. Pass save_dir to download results locally. \
     The session comes from the ArtCraft app: if a call says you're not signed in, ask the user to sign in there."
  )
}

// --- Tool definitions ----------------------------------------------------------------------

fn account_tool() -> Value {
  json!({
    "name": "account",
    "title": "ArtCraft account",
    "description": "Who is signed in to ArtCraft and how many credits they have.",
    "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
    "annotations": { "readOnlyHint": true, "openWorldHint": true },
  })
}

fn list_models_tool(modality: Modality) -> Value {
  let (name, title, what) = match modality {
    Modality::Video => ("list_video_models", "List video models", "video"),
    _ => ("list_image_models", "List image models", "image"),
  };
  json!({
    "name": name,
    "title": title,
    "description": format!("The {what} models available on ArtCraft and the options each one takes (aspect ratios, resolutions, counts, reference limits{}).", if modality == Modality::Video { ", durations, keyframes" } else { ", qualities" }),
    "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
    "annotations": { "readOnlyHint": true, "openWorldHint": true },
  })
}

fn generate_image_tool() -> Value {
  json!({
    "name": "generate_image",
    "title": "Generate image",
    "description": "Generates images on ArtCraft from a prompt, optionally guided by reference images. Spends credits unless estimate_only is set.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "prompt": { "type": "string", "description": "What to make." },
        "model": { "type": "string", "description": "A model id from list_image_models. Defaults to the first available model." },
        "aspect_ratio": { "type": "string", "description": "One of the model's aspect_ratios. Defaults to the model's default." },
        "resolution": { "type": "string", "description": "One of the model's resolutions, if it has any." },
        "quality": { "type": "string", "description": "One of the model's qualities, if it has any." },
        "count": { "type": "integer", "minimum": 1, "description": "How many images, from the model's batch_sizes (default 1 where offered)." },
        "reference_images": { "type": "array", "items": { "type": "string" }, "description": "Local image paths (uploaded for you) or ArtCraft media tokens (mf_…), up to the model's max_reference_images." },
        "estimate_only": { "type": "boolean", "description": "Only price the request; nothing is generated or uploaded." },
        "wait_seconds": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECS, "description": format!("How long to wait for the results (default {DEFAULT_WAIT_SECS}). 0 returns the job tokens at once.") },
        "save_dir": { "type": "string", "description": "A local folder to download the finished files into." },
      },
      "required": ["prompt"],
      "additionalProperties": false,
    },
    "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true },
  })
}

fn generate_video_tool() -> Value {
  json!({
    "name": "generate_video",
    "title": "Generate video",
    "description": "Generates videos on ArtCraft from a prompt, from start/end keyframes, or guided by reference images. Spends credits unless estimate_only is set.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "prompt": { "type": "string", "description": "What happens in the video." },
        "model": { "type": "string", "description": "A model id from list_video_models. Defaults to the first model that can run the request as given." },
        "aspect_ratio": { "type": "string", "description": "One of the model's aspect_ratios." },
        "resolution": { "type": "string", "description": "One of the model's resolutions, if it has any." },
        "duration_seconds": { "type": "integer", "minimum": 1, "description": "Length; snapped to the nearest duration the model offers." },
        "generate_audio": { "type": "boolean", "description": "Generate sound too (models with sound_toggle; default true)." },
        "count": { "type": "integer", "minimum": 1, "description": "How many videos, from the model's batch_sizes." },
        "start_frame": { "type": "string", "description": "First frame: a local image path or a media token (mf_…). Keyframe mode." },
        "end_frame": { "type": "string", "description": "Last frame, for models with end_frame. Needs start_frame." },
        "reference_images": { "type": "array", "items": { "type": "string" }, "description": "Reference images (paths or mf_… tokens) for models with max_reference_images. Not combined with keyframes." },
        "estimate_only": { "type": "boolean", "description": "Only price the request; nothing is generated or uploaded." },
        "wait_seconds": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECS, "description": format!("How long to wait for the results (default {DEFAULT_WAIT_SECS}; videos often take longer). 0 returns the job tokens at once.") },
        "save_dir": { "type": "string", "description": "A local folder to download the finished files into." },
      },
      "required": ["prompt"],
      "additionalProperties": false,
    },
    "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true },
  })
}

fn get_job_tool() -> Value {
  json!({
    "name": "get_job",
    "title": "Get generation job",
    "description": "Status and results of generation jobs, by the job_tokens a generate call returned. Can wait for them to finish and download the results.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "job_tokens": { "type": "array", "items": { "type": "string" }, "description": "The jobs to check." },
        "wait_seconds": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECS, "description": "Wait up to this long for running jobs to finish (default 0)." },
        "save_dir": { "type": "string", "description": "A local folder to download finished files into." },
      },
      "required": ["job_tokens"],
      "additionalProperties": false,
    },
    "annotations": { "readOnlyHint": true, "openWorldHint": true },
  })
}

// --- Helpers -------------------------------------------------------------------------------

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: Value) -> Result<T, String> {
  let arguments = if arguments.is_null() { json!({}) } else { arguments };
  serde_json::from_value(arguments).map_err(|e| format!("Invalid arguments: {e}"))
}

/// A `tools/call` result: the JSON as text (and structured), or the error flagged for the model.
fn tool_result(result: Result<Value, String>) -> Value {
  match result {
    Ok(value) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }], "structuredContent": value, "isError": false }),
    Err(message) => json!({ "content": [{ "type": "text", "text": message }], "isError": true }),
  }
}

/// `requested` if it names an enabled model, else the first enabled one `suits`.
fn pick_model<'a>(models: &'a [ModelInfo], requested: Option<&str>, suits: impl Fn(&ModelInfo) -> bool) -> Result<&'a ModelInfo, String> {
  let enabled = || models.iter().filter(|m| !m.disabled);
  match requested {
    Some(id) => enabled().find(|m| m.id == id).ok_or_else(|| format!("Unknown or unavailable model {id:?}. Available: {}.", enabled().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", "))),
    None => enabled().find(|m| suits(m)).ok_or_else(|| "No available model can run this request.".to_owned()),
  }
}

/// The value to send for an option: `given` if the model offers it (an error otherwise), else
/// the model's default.
fn pick_option(model: &ModelInfo, name: &str, given: Option<&str>, options: &[String], default: Option<&String>) -> Result<Option<String>, String> {
  match given {
    Some(_) if options.is_empty() => Err(format!("{} has no {name} option.", model.id)),
    Some(value) if !options.iter().any(|o| o == value) => Err(format!("{} doesn't offer {name} {value:?}. Choose one of: {}.", model.id, options.join(", "))),
    _ => Ok(ModelInfo::resolve(given, options, default)),
  }
}

fn pick_count(model: &ModelInfo, count: Option<u16>) -> Result<u16, String> {
  let options = model.batch_options();
  match count {
    Some(c) if !options.contains(&c) => Err(format!("{} can't make {c} at once. Choose one of: {}.", model.id, options.iter().map(u16::to_string).collect::<Vec<_>>().join(", "))),
    Some(c) => Ok(c),
    // One unless asked: an agent asked for "an image" shouldn't spend on a batch of four.
    None => Ok(model.valid_batch(1)),
  }
}

fn check_prompt(model: &ModelInfo, prompt: &str, estimate: bool) -> Result<(), String> {
  if !estimate && model.prompt_supported && prompt.trim().is_empty() {
    return Err("The prompt is empty.".to_owned());
  }
  match model.prompt_max {
    Some(max) if prompt.chars().count() > max => Err(format!("The prompt is {} characters; {} takes at most {max}.", prompt.chars().count(), model.id)),
    _ => Ok(()),
  }
}

fn too_many_refs(model: &ModelInfo, given: usize) -> String {
  match model.image_refs_max {
    0 => format!("{} doesn't take reference images.", model.id),
    max => format!("{} takes at most {max} reference images, not {given}.", model.id),
  }
}

/// What `list_*_models` says about a model.
fn model_summary(m: &ModelInfo, modality: Modality) -> Value {
  let mut v = json!({
    "id": m.id,
    "name": m.name,
    "creator": m.creator,
    "aspect_ratios": m.aspect_ratios,
    "aspect_ratio_default": m.aspect_default,
    "batch_sizes": m.batch_options(),
    "max_reference_images": m.image_refs_max,
  });
  if !m.description.is_empty() {
    v["description"] = json!(m.description);
  }
  if !m.resolutions.is_empty() {
    v["resolutions"] = json!(m.resolutions);
    v["resolution_default"] = json!(m.resolution_default);
  }
  if !m.qualities.is_empty() {
    v["qualities"] = json!(m.qualities);
    v["quality_default"] = json!(m.quality_default);
  }
  if let Some(max) = m.prompt_max {
    v["prompt_max_chars"] = json!(max);
  }
  if modality == Modality::Video {
    v["text_to_video"] = json!(m.text_to_video);
    v["start_frame"] = json!(m.start_frame);
    v["start_frame_required"] = json!(m.start_frame_required);
    v["end_frame"] = json!(m.end_frame);
    v["sound_toggle"] = json!(m.sound_toggle);
    if !m.duration_options.is_empty() {
      v["duration_seconds_options"] = json!(m.duration_options);
    } else if let (Some(min), Some(max)) = (m.duration_min, m.duration_max) {
      v["duration_seconds_range"] = json!([min, max]);
    }
    v["duration_seconds_default"] = json!(m.resolve_duration(None, false));
  }
  v
}

fn modality_path(modality: Modality) -> &'static str {
  match modality {
    Modality::Image => "image",
    Modality::Video => "video",
    Modality::Audio => "audio",
  }
}

/// `~/x` → the home folder's `x`.
pub(super) fn expand_home(path: &str) -> PathBuf {
  match path.strip_prefix("~/") {
    Some(rest) => directories::UserDirs::new().map_or_else(|| PathBuf::from(path), |d| d.home_dir().join(rest)),
    None => PathBuf::from(path),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  mod option_tests {
    use super::*;

    #[test]
    fn offered_options_pass_and_defaults_fill_in() {
      let m = model();
      assert_eq!(pick_option(&m, "aspect_ratio", Some("16:9"), &m.aspect_ratios, m.aspect_default.as_ref()), Ok(Some("16:9".to_owned())));
      assert_eq!(pick_option(&m, "aspect_ratio", None, &m.aspect_ratios, m.aspect_default.as_ref()), Ok(Some("1:1".to_owned())));
    }

    #[test]
    fn options_the_model_lacks_are_errors() {
      let m = model();
      let err = pick_option(&m, "aspect_ratio", Some("21:9"), &m.aspect_ratios, m.aspect_default.as_ref()).unwrap_err();
      assert!(err.contains("1:1, 16:9"), "{err}");
      assert!(pick_option(&m, "quality", Some("high"), &m.qualities, None).is_err());
    }

    #[test]
    fn counts_must_be_a_batch_size() {
      let mut m = model();
      m.batch_default = 4;
      assert_eq!(pick_count(&m, None), Ok(1));
      assert_eq!(pick_count(&m, Some(4)), Ok(4));
      assert!(pick_count(&m, Some(3)).is_err());
    }
  }

  mod model_tests {
    use super::*;

    #[test]
    fn a_named_model_must_be_enabled() {
      let mut off = model();
      off.id = "off".into();
      off.disabled = true;
      let models = [off, model()];
      assert_eq!(pick_model(&models, Some("test_model"), |_| true).map(|m| m.id.as_str()), Ok("test_model"));
      assert!(pick_model(&models, Some("off"), |_| true).unwrap_err().contains("Available: test_model"));
      assert_eq!(pick_model(&models, None, |_| true).map(|m| m.id.as_str()), Ok("test_model"));
      assert!(pick_model(&models, None, |_| false).is_err());
    }

    #[test]
    fn prompts_are_checked_against_the_model() {
      let m = model();
      assert!(check_prompt(&m, "  ", false).is_err());
      assert!(check_prompt(&m, "", true).is_ok());
      assert!(check_prompt(&m, &"x".repeat(11), false).is_err());
    }
  }

  #[test]
  fn errors_are_flagged_for_the_model() {
    let ok = tool_result(Ok(json!({ "done": true })));
    assert_eq!(ok["isError"], false);
    assert_eq!(ok["structuredContent"]["done"], true);
    let err = tool_result(Err("nope".into()));
    assert_eq!(err["isError"], true);
    assert_eq!(err["content"][0]["text"], "nope");
  }

  fn model() -> ModelInfo {
    ModelInfo { id: "test_model".into(), prompt_supported: true, prompt_max: Some(10), aspect_ratios: vec!["1:1".into(), "16:9".into()], aspect_default: Some("1:1".into()), batch_sizes: vec![1, 2, 4], batch_default: 1, ..Default::default() }
  }
}
