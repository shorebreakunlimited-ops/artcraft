//! Local editing with FFmpeg: `assemble_sequence` cuts clips into one rendered sequence (video
//! track, each clip's own audio, a foley track under it, crossfades), and `split_thumbnail` puts
//! two frames side by side. Nothing here calls the ArtCraft API or spends credits.
//!
//! The graph is one `ffmpeg` run: every clip is trimmed to its probed length and conformed
//! (scale/pad, rate, pixel format), its audio and foley are leveled and mixed, then clips are
//! chained with `xfade`/`acrossfade` (or `concat` without crossfades).

use std::env;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::tools::expand_home;

const DEFAULT_AMBIENT_GAIN_DB: f64 = -12.0;
const DEFAULT_SFX_GAIN_DB: f64 = -3.0;
const DEFAULT_CROSSFADE_SECS: f64 = 0.5;
const DEFAULT_FPS: u32 = 24;
const DEFAULT_THUMBNAIL_SIZE: (u32, u32) = (1280, 720);
const AUDIO_RATE: u32 = 48_000;
/// Places FFmpeg usually lives, for servers started from a GUI app (which gets a minimal PATH).
const EXTRA_TOOL_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "~/.local/bin"];
const NO_FFMPEG: &str = "FFmpeg isn't installed (ffmpeg and ffprobe weren't found on the PATH, in /opt/homebrew/bin, /usr/local/bin or ~/.local/bin). Install it, or set ARTCRAFT_FFMPEG_DIR to the folder that holds both.";

/// `assemble_sequence`'s arguments.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AssembleSequenceArgs {
  name: String,
  clips: Vec<ClipArgs>,
  output_path: Option<String>,
  resolution: Option<String>,
  orientation: Option<String>,
  fps: Option<u32>,
  crossfade_seconds: Option<f64>,
  ambient_gain_db: Option<f64>,
  sfx_gain_db: Option<f64>,
  keep_ambient: Option<bool>,
  overwrite: bool,
}

/// One clip on the timeline, with the foley that plays under it.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClipArgs {
  video: String,
  sfx: Option<String>,
  sfx_gain_db: Option<f64>,
}

/// `split_thumbnail`'s arguments.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SplitThumbnailArgs {
  left: String,
  right: String,
  output_path: String,
  width: Option<u32>,
  height: Option<u32>,
  left_at_seconds: Option<f64>,
  right_at_seconds: Option<f64>,
  divider: Option<bool>,
  overwrite: bool,
}

/// The render settings shared by every clip.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Sequence {
  width: u32,
  height: u32,
  fps: u32,
  crossfade: f64,
  ambient_gain_db: f64,
  keep_ambient: bool,
}

/// A clip as probed: its length and whether it has its own audio.
#[derive(Clone, Debug, PartialEq)]
struct Clip {
  video: PathBuf,
  duration: f64,
  has_audio: bool,
  /// Mono audio is copied to both channels instead of taking the upmix's −3 dB.
  ambient_mono: bool,
  sfx: Option<PathBuf>,
  sfx_mono: bool,
  sfx_gain_db: f64,
}

/// Renders the sequence and writes its timeline next to it.
pub async fn assemble_sequence(args: AssembleSequenceArgs) -> Result<Value, String> {
  let tools = Tools::find()?;
  let name = args.name.trim();
  if name.is_empty() {
    return Err("Give the sequence a name.".to_owned());
  }
  if args.clips.is_empty() {
    return Err("Add at least one clip.".to_owned());
  }
  let (width, height) = frame_size(args.resolution.as_deref(), args.orientation.as_deref())?;
  let fps = args.fps.unwrap_or(DEFAULT_FPS);
  if !(1..=120).contains(&fps) {
    return Err(format!("fps {fps} is out of range (1–120)."));
  }
  let crossfade = args.crossfade_seconds.unwrap_or(DEFAULT_CROSSFADE_SECS);
  if !(0.0..=5.0).contains(&crossfade) {
    return Err(format!("crossfade_seconds {crossfade} is out of range (0–5)."));
  }
  let sequence = Sequence { width, height, fps, crossfade, ambient_gain_db: args.ambient_gain_db.unwrap_or(DEFAULT_AMBIENT_GAIN_DB), keep_ambient: args.keep_ambient.unwrap_or(true) };
  let default_sfx_gain = args.sfx_gain_db.unwrap_or(DEFAULT_SFX_GAIN_DB);

  let mut clips = Vec::with_capacity(args.clips.len());
  for (i, clip) in args.clips.iter().enumerate() {
    let video = existing_file(&clip.video, &format!("Clip {} video", i + 1))?;
    let sfx = clip.sfx.as_deref().map(|s| existing_file(s, &format!("Clip {} sfx", i + 1))).transpose()?;
    let sfx_mono = match &sfx {
      Some(path) => {
        let probe = tools.probe(path).await?;
        if !probe.has_audio {
          return Err(format!("{} has no audio stream.", path.display()));
        }
        probe.audio_mono
      },
      None => false,
    };
    let probe = tools.probe(&video).await?;
    if !probe.has_video {
      return Err(format!("{} has no video stream.", video.display()));
    }
    // A crossfade eats into both neighbours, so every clip must outlast it (twice in the middle).
    let needed = if args.clips.len() == 1 { 0.0 } else if i == 0 || i + 1 == args.clips.len() { crossfade } else { crossfade * 2.0 };
    if probe.duration <= needed {
      return Err(format!("{} is {:.2}s, too short for {crossfade}s crossfades.", video.display(), probe.duration));
    }
    clips.push(Clip { video, duration: probe.duration, has_audio: probe.has_audio, ambient_mono: probe.audio_mono, sfx, sfx_mono, sfx_gain_db: clip.sfx_gain_db.unwrap_or(default_sfx_gain) });
  }

  let output = match &args.output_path {
    Some(path) => expand_home(path),
    None => expand_home("~/Movies/ArtCraft").join(format!("{}.mp4", file_stem(name))),
  };
  prepare_output(&output, args.overwrite).await?;
  let mut command = tools.ffmpeg(args.overwrite);
  command.args(sequence_args(&sequence, &clips, &output));
  run(command, "Rendering the sequence").await?;

  let timeline = timeline(name, &sequence, &clips, &output);
  let timeline_path = output.with_extension("timeline.json");
  tokio::fs::write(&timeline_path, serde_json::to_vec_pretty(&timeline).unwrap_or_default()).await.map_err(|e| format!("Couldn't write {}: {e}", timeline_path.display()))?;
  Ok(json!({ "output_path": output.display().to_string(), "timeline_path": timeline_path.display().to_string(), "timeline": timeline }))
}

/// Saves the two frames side by side, each center-cropped to half the width.
pub async fn split_thumbnail(args: SplitThumbnailArgs) -> Result<Value, String> {
  let tools = Tools::find()?;
  let left = existing_file(&args.left, "left")?;
  let right = existing_file(&args.right, "right")?;
  let (width, height) = (args.width.unwrap_or(DEFAULT_THUMBNAIL_SIZE.0), args.height.unwrap_or(DEFAULT_THUMBNAIL_SIZE.1));
  if width < 16 || height < 16 || width > 7680 || height > 4320 {
    return Err(format!("{width}x{height} is out of range."));
  }
  if args.output_path.trim().is_empty() {
    return Err("Give an output_path (.png or .jpg).".to_owned());
  }
  let output = expand_home(&args.output_path);
  prepare_output(&output, args.overwrite).await?;
  let mut command = tools.ffmpeg(args.overwrite);
  command.args(thumbnail_args(&left, args.left_at_seconds, &right, args.right_at_seconds, width, height, args.divider.unwrap_or(true), &output));
  run(command, "Making the thumbnail").await?;
  Ok(json!({ "output_path": output.display().to_string(), "width": even(width), "height": height }))
}

// --- Tool definitions ----------------------------------------------------------------------

pub fn assemble_sequence_tool() -> Value {
  json!({
    "name": "assemble_sequence",
    "title": "Assemble sequence",
    "description": "Edits local video clips into one rendered sequence with FFmpeg: clips in order on the video track, each clip's own audio as an ambient track, optional foley/SFX files under their clips, and crossfades between clips. Writes the .mp4 and a .timeline.json describing every track. Runs locally; no credits.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "name": { "type": "string", "description": "Sequence name (also the default file name)." },
        "clips": {
          "type": "array",
          "minItems": 1,
          "description": "Clips in timeline order.",
          "items": {
            "type": "object",
            "properties": {
              "video": { "type": "string", "description": "Local video file." },
              "sfx": { "type": "string", "description": "Local audio file laid under this clip (trimmed or padded with silence to its length)." },
              "sfx_gain_db": { "type": "number", "description": "This clip's SFX level, overriding the sequence's sfx_gain_db." },
            },
            "required": ["video"],
            "additionalProperties": false,
          },
        },
        "output_path": { "type": "string", "description": "Where to write the .mp4. Defaults to ~/Movies/ArtCraft/<name>.mp4." },
        "resolution": { "type": "string", "enum": ["720p", "1080p", "1440p", "2160p"], "description": "Default 1080p." },
        "orientation": { "type": "string", "enum": ["landscape", "portrait", "square"], "description": "Default landscape (16:9)." },
        "fps": { "type": "integer", "minimum": 1, "maximum": 120, "description": format!("Default {DEFAULT_FPS}.") },
        "crossfade_seconds": { "type": "number", "minimum": 0, "maximum": 5, "description": format!("Video and audio crossfade between clips (default {DEFAULT_CROSSFADE_SECS}; 0 for hard cuts).") },
        "ambient_gain_db": { "type": "number", "description": format!("Level of each clip's own audio (default {DEFAULT_AMBIENT_GAIN_DB} dB).") },
        "sfx_gain_db": { "type": "number", "description": format!("Level of the SFX track (default {DEFAULT_SFX_GAIN_DB} dB).") },
        "keep_ambient": { "type": "boolean", "description": "Keep each clip's own audio (default true)." },
        "overwrite": { "type": "boolean", "description": "Replace an existing output file." },
      },
      "required": ["name", "clips"],
      "additionalProperties": false,
    },
    "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
  })
}

pub fn split_thumbnail_tool() -> Value {
  json!({
    "name": "split_thumbnail",
    "title": "Split-screen thumbnail",
    "description": "Makes a 50/50 split-screen image from two local images or video frames (e.g. before and after), each center-cropped to fill its half. Runs locally with FFmpeg; no credits.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "left": { "type": "string", "description": "Local image or video for the left half." },
        "right": { "type": "string", "description": "Local image or video for the right half." },
        "output_path": { "type": "string", "description": "Where to save it (.png or .jpg)." },
        "width": { "type": "integer", "description": format!("Default {}.", DEFAULT_THUMBNAIL_SIZE.0) },
        "height": { "type": "integer", "description": format!("Default {}.", DEFAULT_THUMBNAIL_SIZE.1) },
        "left_at_seconds": { "type": "number", "minimum": 0, "description": "For a video: which moment to use (default its first frame)." },
        "right_at_seconds": { "type": "number", "minimum": 0, "description": "For a video: which moment to use (default its first frame)." },
        "divider": { "type": "boolean", "description": "Draw a thin white line between the halves (default true)." },
        "overwrite": { "type": "boolean", "description": "Replace an existing output file." },
      },
      "required": ["left", "right", "output_path"],
      "additionalProperties": false,
    },
    "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
  })
}

// --- FFmpeg --------------------------------------------------------------------------------

/// Where `ffmpeg` and `ffprobe` are.
struct Tools {
  ffmpeg: PathBuf,
  ffprobe: PathBuf,
}

/// What `ffprobe` says about an input.
struct Probe {
  duration: f64,
  has_video: bool,
  has_audio: bool,
  audio_mono: bool,
}

impl Tools {
  /// `ARTCRAFT_FFMPEG_DIR`, then the PATH, then the usual install folders.
  fn find() -> Result<Self, String> {
    let mut dirs: Vec<PathBuf> = env::var_os("ARTCRAFT_FFMPEG_DIR").map(PathBuf::from).into_iter().collect();
    dirs.extend(env::var_os("PATH").map(|p| env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default());
    dirs.extend(EXTRA_TOOL_DIRS.iter().map(|d| expand_home(d)));
    let find = |name: &str| dirs.iter().map(|d| d.join(executable(name))).find(|p| p.is_file());
    match (find("ffmpeg"), find("ffprobe")) {
      (Some(ffmpeg), Some(ffprobe)) => Ok(Self { ffmpeg, ffprobe }),
      _ => Err(NO_FFMPEG.to_owned()),
    }
  }

  fn ffmpeg(&self, overwrite: bool) -> Command {
    let mut command = Command::new(&self.ffmpeg);
    command.args(["-nostdin", "-hide_banner", "-loglevel", "error", if overwrite { "-y" } else { "-n" }]);
    command
  }

  async fn probe(&self, path: &Path) -> Result<Probe, String> {
    let output = Command::new(&self.ffprobe).args(["-v", "error", "-show_entries", "format=duration:stream=codec_type,channels", "-of", "json"]).arg(path).output().await.map_err(|e| format!("ffprobe: {e}"))?;
    if !output.status.success() {
      return Err(format!("Couldn't read {}: {}", path.display(), String::from_utf8_lossy(&output.stderr).trim()));
    }
    parse_probe(&output.stdout).ok_or_else(|| format!("Couldn't read the length of {}.", path.display()))
  }
}

fn parse_probe(stdout: &[u8]) -> Option<Probe> {
  let v: Value = serde_json::from_slice(stdout).ok()?;
  let duration = v.pointer("/format/duration")?.as_str()?.parse::<f64>().ok().filter(|d| *d > 0.0)?;
  let streams = v.get("streams")?.as_array()?;
  let of_kind = |kind: &str| streams.iter().find(|s| s.get("codec_type").and_then(Value::as_str) == Some(kind));
  let audio = of_kind("audio");
  Some(Probe { duration, has_video: of_kind("video").is_some(), has_audio: audio.is_some(), audio_mono: audio.and_then(|s| s.get("channels")?.as_u64()) == Some(1) })
}

async fn run(mut command: Command, what: &str) -> Result<(), String> {
  let output = command.output().await.map_err(|e| format!("{what} failed to start FFmpeg: {e}"))?;
  if output.status.success() {
    return Ok(());
  }
  let stderr = String::from_utf8_lossy(&output.stderr);
  let tail: Vec<&str> = stderr.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
  Err(format!("{what} failed: {}", tail.join("\n")))
}

/// The `ffmpeg` arguments (after the common flags) that render the sequence to `output`.
fn sequence_args(seq: &Sequence, clips: &[Clip], output: &Path) -> Vec<String> {
  let mut args = Vec::new();
  let mut inputs = 0usize;
  let mut graph = Vec::new();
  for (i, clip) in clips.iter().enumerate() {
    let video_input = inputs;
    args.extend(["-i".to_owned(), clip.video.display().to_string()]);
    inputs += 1;
    let d = secs(clip.duration);
    let (w, h, fps) = (seq.width, seq.height, seq.fps);
    graph.push(format!("[{video_input}:v]trim=duration={d},setpts=PTS-STARTPTS,scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:black,setsar=1,fps={fps},format=yuv420p[v{i}]"));

    if clip.has_audio && seq.keep_ambient {
      graph.push(format!("[{video_input}:a]atrim=duration={d},asetpts=PTS-STARTPTS,{},volume={}dB,apad,atrim=duration={d}[amb{i}]", stereo_input(clip.ambient_mono), db(seq.ambient_gain_db)));
    } else {
      graph.push(format!("anullsrc=r={AUDIO_RATE}:cl=stereo,atrim=duration={d},aformat=sample_fmts=fltp:channel_layouts=stereo[amb{i}]"));
    }
    match &clip.sfx {
      Some(sfx) => {
        let sfx_input = inputs;
        args.extend(["-i".to_owned(), sfx.display().to_string()]);
        inputs += 1;
        graph.push(format!("[{sfx_input}:a]{},volume={}dB,apad,atrim=duration={d},asetpts=PTS-STARTPTS[sfx{i}]", stereo_input(clip.sfx_mono), db(clip.sfx_gain_db)));
        graph.push(format!("[amb{i}][sfx{i}]amix=inputs=2:duration=first:normalize=0[a{i}]"));
      },
      None => graph.push(format!("[amb{i}]anull[a{i}]")),
    }
  }

  let last = clips.len() - 1;
  if clips.len() == 1 {
    graph.push("[v0]null[vout]".to_owned());
    graph.push("[a0]anull[aout]".to_owned());
  } else if seq.crossfade <= 0.0 {
    let pairs: String = (0..clips.len()).map(|i| format!("[v{i}][a{i}]")).collect();
    graph.push(format!("{pairs}concat=n={}:v=1:a=1[vout][aout]", clips.len()));
  } else {
    let xf = secs(seq.crossfade);
    let mut elapsed = 0.0;
    for k in 1..clips.len() {
      elapsed += clips[k - 1].duration;
      let offset = secs(elapsed - seq.crossfade * k as f64);
      let (vin, ain) = if k == 1 { ("v0".to_owned(), "a0".to_owned()) } else { (format!("vx{}", k - 1), format!("ax{}", k - 1)) };
      let (vout, aout) = if k == last { ("vout".to_owned(), "aout".to_owned()) } else { (format!("vx{k}"), format!("ax{k}")) };
      graph.push(format!("[{vin}][v{k}]xfade=transition=fade:duration={xf}:offset={offset}[{vout}]"));
      graph.push(format!("[{ain}][a{k}]acrossfade=d={xf}:c1=tri:c2=tri[{aout}]"));
    }
  }

  args.extend(["-filter_complex".to_owned(), graph.join(";")]);
  for arg in ["-map", "[vout]", "-map", "[aout]", "-c:v", "libx264", "-preset", "medium", "-crf", "18", "-pix_fmt", "yuv420p", "-r"] {
    args.push(arg.to_owned());
  }
  args.push(seq.fps.to_string());
  for arg in ["-c:a", "aac", "-b:a", "192k", "-ar"] {
    args.push(arg.to_owned());
  }
  args.push(AUDIO_RATE.to_string());
  args.extend(["-movflags".to_owned(), "+faststart".to_owned(), output.display().to_string()]);
  args
}

/// The `ffmpeg` arguments for a split-screen still.
#[allow(clippy::too_many_arguments)]
fn thumbnail_args(left: &Path, left_at: Option<f64>, right: &Path, right_at: Option<f64>, width: u32, height: u32, divider: bool, output: &Path) -> Vec<String> {
  let mut args = Vec::new();
  for (path, at) in [(left, left_at), (right, right_at)] {
    if let Some(at) = at.filter(|t| *t > 0.0) {
      args.extend(["-ss".to_owned(), secs(at)]);
    }
    args.extend(["-i".to_owned(), path.display().to_string()]);
  }
  let half = even(width) / 2;
  let crop = |i: usize, label: &str| format!("[{i}:v]scale={half}:{height}:force_original_aspect_ratio=increase,crop={half}:{height},setsar=1[{label}]");
  let mut graph = format!("{};{};[l][r]hstack=inputs=2", crop(0, "l"), crop(1, "r"));
  if divider {
    let line = (width / 320).max(2);
    graph.push_str(&format!(",drawbox=x={}:y=0:w={line}:h={height}:color=white:t=fill", half - line / 2));
  }
  graph.push_str("[out]");
  args.extend(["-filter_complex".to_owned(), graph, "-map".to_owned(), "[out]".to_owned(), "-frames:v".to_owned(), "1".to_owned()]);
  if output.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg")) {
    args.extend(["-q:v".to_owned(), "2".to_owned()]);
  }
  args.push(output.display().to_string());
  args
}

/// What sits where: V1 clips, A1 ambient, A2 SFX, and the transitions, in seconds.
fn timeline(name: &str, seq: &Sequence, clips: &[Clip], output: &Path) -> Value {
  let crossfade = if clips.len() > 1 { seq.crossfade.max(0.0) } else { 0.0 };
  let mut start = 0.0;
  let (mut v1, mut a1, mut a2, mut transitions) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
  for (i, clip) in clips.iter().enumerate() {
    let end = start + clip.duration;
    let source = clip.video.display().to_string();
    v1.push(json!({ "clip": i + 1, "source": source, "start": round(start), "end": round(end) }));
    if clip.has_audio && seq.keep_ambient {
      a1.push(json!({ "clip": i + 1, "source": source, "start": round(start), "end": round(end), "gain_db": seq.ambient_gain_db }));
    }
    if let Some(sfx) = &clip.sfx {
      a2.push(json!({ "clip": i + 1, "source": sfx.display().to_string(), "start": round(start), "end": round(end), "gain_db": clip.sfx_gain_db }));
    }
    if i + 1 < clips.len() && crossfade > 0.0 {
      transitions.push(json!({ "between": [i + 1, i + 2], "type": "crossfade", "start": round(end - crossfade), "duration": crossfade }));
    }
    start = end - crossfade;
  }
  let total = clips.iter().map(|c| c.duration).sum::<f64>() - crossfade * clips.len().saturating_sub(1) as f64;
  json!({
    "name": name,
    "output": output.display().to_string(),
    "width": seq.width,
    "height": seq.height,
    "fps": seq.fps,
    "duration_seconds": round(total),
    "tracks": { "V1": v1, "A1": a1, "A2": a2 },
    "transitions": transitions,
  })
}

// --- Helpers -------------------------------------------------------------------------------

/// Conforms an input's audio to 48 kHz float stereo. Mono is copied to both channels: FFmpeg's
/// own mono-to-stereo upmix lowers each channel by 3 dB, which would make "−12 dB" mean −15.
fn stereo_input(mono: bool) -> String {
  let copy = if mono { "pan=stereo|c0=c0|c1=c0," } else { "" };
  format!("{copy}aresample={AUDIO_RATE},aformat=sample_fmts=fltp:channel_layouts=stereo")
}

/// The frame for a resolution name and orientation (16:9 landscape by default).
fn frame_size(resolution: Option<&str>, orientation: Option<&str>) -> Result<(u32, u32), String> {
  let short = match resolution.unwrap_or("1080p") {
    "720p" => 720,
    "1080p" => 1080,
    "1440p" => 1440,
    "2160p" | "4k" => 2160,
    other => return Err(format!("Unknown resolution {other:?}: use 720p, 1080p, 1440p or 2160p.")),
  };
  let long = short * 16 / 9;
  match orientation.unwrap_or("landscape") {
    "landscape" => Ok((long, short)),
    "portrait" => Ok((short, long)),
    "square" => Ok((short, short)),
    other => Err(format!("Unknown orientation {other:?}: use landscape, portrait or square.")),
  }
}

fn existing_file(path: &str, what: &str) -> Result<PathBuf, String> {
  let path = expand_home(path);
  if path.is_file() {
    Ok(path)
  } else {
    Err(format!("{what} not found: {}", path.display()))
  }
}

async fn prepare_output(output: &Path, overwrite: bool) -> Result<(), String> {
  if output.exists() && !overwrite {
    return Err(format!("{} already exists. Pass overwrite: true to replace it.", output.display()));
  }
  match output.parent().filter(|p| !p.as_os_str().is_empty()) {
    Some(dir) => tokio::fs::create_dir_all(dir).await.map_err(|e| format!("Couldn't create {}: {e}", dir.display())),
    None => Ok(()),
  }
}

/// A name safe to use as a file name.
fn file_stem(name: &str) -> String {
  let stem: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') { c } else { '_' }).collect();
  stem.trim_matches(['.', ' ']).to_owned()
}

fn executable(name: &str) -> String {
  if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() }
}

/// Seconds as FFmpeg reads them (millisecond precision, no exponent).
fn secs(value: f64) -> String {
  format!("{:.3}", value.max(0.0))
}

fn db(value: f64) -> String {
  format!("{value:.2}")
}

fn round(value: f64) -> f64 {
  (value * 1000.0).round() / 1000.0
}

fn even(value: u32) -> u32 {
  value & !1
}

#[cfg(test)]
mod tests {
  use super::*;

  mod sequence_tests {
    use super::*;

    #[test]
    fn crossfades_chain_with_offsets_shortened_by_each_fade() {
      let clips = [clip("a.mp4", 8.0, true, Some("a.wav")), clip("b.mp4", 6.0, true, None), clip("c.mp4", 5.0, false, Some("c.wav"))];
      let args = sequence_args(&sequence(0.5), &clips, Path::new("out.mp4"));
      let graph = filter(&args);
      assert!(graph.contains("[v0][v1]xfade=transition=fade:duration=0.500:offset=7.500[vx1]"), "{graph}");
      assert!(graph.contains("[vx1][v2]xfade=transition=fade:duration=0.500:offset=13.000[vout]"), "{graph}");
      assert!(graph.contains("[a0][a1]acrossfade=d=0.500:c1=tri:c2=tri[ax1]"), "{graph}");
      assert!(graph.contains("[ax1][a2]acrossfade=d=0.500:c1=tri:c2=tri[aout]"), "{graph}");
      // Inputs: a.mp4, a.wav, b.mp4, c.mp4, c.wav.
      assert_eq!(inputs(&args), ["a.mp4", "a.wav", "b.mp4", "c.mp4", "c.wav"]);
      assert!(graph.contains("[1:a]aresample=48000"), "{graph}");
      assert!(graph.contains("[4:a]aresample=48000"), "{graph}");
    }

    #[test]
    fn ambient_and_sfx_are_leveled_and_mixed_without_normalizing() {
      let args = sequence_args(&sequence(0.5), &[clip("a.mp4", 8.0, true, Some("a.wav"))], Path::new("out.mp4"));
      let graph = filter(&args);
      assert!(graph.contains("volume=-12.00dB,apad,atrim=duration=8.000[amb0]"), "{graph}");
      assert!(graph.contains("volume=-3.00dB,apad,atrim=duration=8.000"), "{graph}");
      assert!(graph.contains("[amb0][sfx0]amix=inputs=2:duration=first:normalize=0[a0]"), "{graph}");
      assert!(graph.contains("[v0]null[vout]") && graph.contains("[a0]anull[aout]"), "{graph}");
    }

    #[test]
    fn mono_inputs_are_copied_to_both_channels() {
      let mut mono = clip("a.mp4", 8.0, true, Some("a.wav"));
      mono.ambient_mono = true;
      mono.sfx_mono = true;
      let graph = filter(&sequence_args(&sequence(0.5), &[mono, clip("b.mp4", 6.0, true, Some("b.wav"))], Path::new("out.mp4")));
      assert!(graph.contains("[0:a]atrim=duration=8.000,asetpts=PTS-STARTPTS,pan=stereo|c0=c0|c1=c0,aresample=48000"), "{graph}");
      assert!(graph.contains("[1:a]pan=stereo|c0=c0|c1=c0,aresample=48000"), "{graph}");
      // The second clip's inputs are stereo and untouched.
      assert!(graph.contains("[2:a]atrim=duration=6.000,asetpts=PTS-STARTPTS,aresample=48000"), "{graph}");
      assert!(graph.contains("[3:a]aresample=48000"), "{graph}");
    }

    #[test]
    fn silent_clips_get_a_silent_bed() {
      let args = sequence_args(&sequence(0.0), &[clip("a.mp4", 4.0, false, None), clip("b.mp4", 4.0, true, None)], Path::new("out.mp4"));
      let graph = filter(&args);
      assert!(graph.contains("anullsrc=r=48000:cl=stereo,atrim=duration=4.000"), "{graph}");
      assert!(graph.contains("[v0][a0][v1][a1]concat=n=2:v=1:a=1[vout][aout]"), "{graph}");
    }

    #[test]
    fn video_is_conformed_to_the_frame_and_rate() {
      let args = sequence_args(&sequence(0.5), &[clip("a.mp4", 4.0, true, None)], Path::new("out.mp4"));
      assert!(filter(&args).contains("scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black,setsar=1,fps=24,format=yuv420p[v0]"));
      let r = args.iter().position(|a| a == "-r").unwrap();
      assert_eq!(args[r + 1], "24");
      assert_eq!(args.last().map(String::as_str), Some("out.mp4"));
    }

    #[test]
    fn the_timeline_lists_tracks_and_overlapping_transitions() {
      let clips = [clip("a.mp4", 8.0, true, Some("a.wav")), clip("b.mp4", 6.0, false, None)];
      let t = timeline("Ep1", &sequence(0.5), &clips, Path::new("Ep1.mp4"));
      assert_eq!(t["duration_seconds"], 13.5);
      assert_eq!(t["tracks"]["V1"][1]["start"], 7.5);
      assert_eq!(t["tracks"]["A1"].as_array().unwrap().len(), 1);
      assert_eq!(t["tracks"]["A2"][0]["gain_db"], -3.0);
      assert_eq!(t["transitions"][0]["start"], 7.5);
    }
  }

  mod thumbnail_tests {
    use super::*;

    #[test]
    fn halves_are_cropped_and_stacked_with_a_divider() {
      let args = thumbnail_args(Path::new("1a.png"), None, Path::new("1c.mp4"), Some(2.0), 1280, 720, true, Path::new("thumb.jpg"));
      let graph = filter(&args);
      assert!(graph.contains("[0:v]scale=640:720:force_original_aspect_ratio=increase,crop=640:720"), "{graph}");
      assert!(graph.contains("[l][r]hstack=inputs=2,drawbox=x=638:y=0:w=4:h=720:color=white:t=fill[out]"), "{graph}");
      assert_eq!(&args[..2], ["-i", "1a.png"]);
      assert_eq!(&args[2..6], ["-ss", "2.000", "-i", "1c.mp4"]);
      assert!(args.windows(2).any(|w| w == ["-q:v", "2"]));
    }
  }

  mod helper_tests {
    use super::*;

    #[test]
    fn frame_sizes_follow_resolution_and_orientation() {
      assert_eq!(frame_size(None, None), Ok((1920, 1080)));
      assert_eq!(frame_size(Some("720p"), Some("portrait")), Ok((720, 1280)));
      assert_eq!(frame_size(Some("2160p"), Some("square")), Ok((2160, 2160)));
      assert!(frame_size(Some("8k"), None).is_err());
    }

    #[test]
    fn probes_read_length_and_streams() {
      let probe = parse_probe(br#"{"streams":[{"codec_type":"video"},{"codec_type":"audio"}],"format":{"duration":"8.041667"}}"#).unwrap();
      assert!(probe.has_video && probe.has_audio && (probe.duration - 8.041667).abs() < 1e-9);
      assert!(!probe.audio_mono);
      assert!(parse_probe(br#"{"streams":[],"format":{}}"#).is_none());
      let mono = parse_probe(br#"{"streams":[{"codec_type":"audio","channels":1}],"format":{"duration":"2.0"}}"#).unwrap();
      assert!(mono.has_audio && mono.audio_mono && !mono.has_video);
    }

    #[test]
    fn names_become_safe_file_names() {
      assert_eq!(file_stem("Ep1_SilverZephyr"), "Ep1_SilverZephyr");
      assert_eq!(file_stem("../a/b:c"), "_a_b_c");
    }
  }

  fn sequence(crossfade: f64) -> Sequence {
    Sequence { width: 1920, height: 1080, fps: 24, crossfade, ambient_gain_db: -12.0, keep_ambient: true }
  }

  fn clip(video: &str, duration: f64, has_audio: bool, sfx: Option<&str>) -> Clip {
    Clip { video: PathBuf::from(video), duration, has_audio, ambient_mono: false, sfx: sfx.map(PathBuf::from), sfx_mono: false, sfx_gain_db: -3.0 }
  }

  fn filter(args: &[String]) -> String {
    let i = args.iter().position(|a| a == "-filter_complex").unwrap();
    args[i + 1].clone()
  }

  fn inputs(args: &[String]) -> Vec<&str> {
    args.windows(2).filter(|w| w[0] == "-i").map(|w| w[1].as_str()).collect()
  }
}
