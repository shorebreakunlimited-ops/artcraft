//! `artcraft-native --mcp`: a headless Model Context Protocol server on stdin/stdout, so agents
//! (Claude Code, Claude Desktop, …) can generate images and videos with the user's ArtCraft
//! account. No window opens; the session is the one the app saved.
//!
//! The transport is MCP's stdio framing: one JSON-RPC 2.0 message per line. Stdout carries
//! protocol messages only (logs go to stderr). Requests run concurrently, so a long `wait` on a
//! generation doesn't hold up `ping` or other calls.

mod timeline;
mod tools;

use std::path::Path;
use std::sync::Arc;

use log::{info, warn};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use tools::Server;

/// The newest protocol revision we speak; older clients get their own version echoed back.
const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Which tools a server exposes: PhotoCraft is images, FilmCraft is video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
  Photo,
  Film,
  All,
}

impl Scope {
  /// `--scope photo|film|all` if given, else from the executable's name (`photocraft`,
  /// `filmcraft`), else everything.
  pub fn from_args(args: &[String]) -> Result<Self, String> {
    if let Some(i) = args.iter().position(|a| a == "--scope") {
      let value = args.get(i + 1).ok_or("--scope needs a value: photo, film or all")?;
      return Self::parse(value).ok_or_else(|| format!("Unknown --scope {value:?}: use photo, film or all"));
    }
    if let Some(value) = args.iter().find_map(|a| a.strip_prefix("--scope=")) {
      return Self::parse(value).ok_or_else(|| format!("Unknown --scope {value:?}: use photo, film or all"));
    }
    let exe = args.first().and_then(|a| Path::new(a).file_stem()).map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    Ok(if exe.contains("photo") {
      Scope::Photo
    } else if exe.contains("film") {
      Scope::Film
    } else {
      Scope::All
    })
  }

  fn parse(value: &str) -> Option<Self> {
    match value {
      "photo" | "image" => Some(Scope::Photo),
      "film" | "video" => Some(Scope::Film),
      "all" => Some(Scope::All),
      _ => None,
    }
  }

  pub fn images(self) -> bool {
    self != Scope::Film
  }

  pub fn videos(self) -> bool {
    self != Scope::Photo
  }

  fn server_name(self) -> &'static str {
    match self {
      Scope::Photo => "photocraft",
      Scope::Film => "filmcraft",
      Scope::All => "artcraft",
    }
  }
}

/// Serves MCP on stdio until stdin closes.
pub fn run(scope: Scope) -> anyhow::Result<()> {
  let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_name("artcraft-mcp").enable_all().build()?;
  rt.block_on(serve(scope));
  Ok(())
}

async fn serve(scope: Scope) {
  let server = Arc::new(Server::new(scope));
  info!("MCP server ({}) ready on stdio", scope.server_name());

  // One writer, so concurrent replies never interleave on stdout.
  let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
  let writer = tokio::spawn(async move {
    let mut stdout = tokio::io::stdout();
    while let Some(message) = out_rx.recv().await {
      let mut line = message.to_string();
      line.push('\n');
      if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
        break;
      }
    }
  });

  let mut lines = BufReader::new(tokio::io::stdin()).lines();
  loop {
    let line = match lines.next_line().await {
      Ok(Some(line)) => line,
      Ok(None) => break,
      Err(err) => {
        warn!("Reading stdin failed: {err}");
        break;
      },
    };
    if line.trim().is_empty() {
      continue;
    }
    let message: Value = match serde_json::from_str(&line) {
      Ok(message) => message,
      Err(err) => {
        let _ = out_tx.send(error_response(Value::Null, PARSE_ERROR, &format!("Parse error: {err}")));
        continue;
      },
    };
    let (server, out_tx) = (server.clone(), out_tx.clone());
    tokio::spawn(async move {
      if let Some(reply) = handle(&server, message).await {
        let _ = out_tx.send(reply);
      }
    });
  }

  drop(out_tx);
  let _ = writer.await;
  info!("stdin closed; MCP server exiting");
}

/// The reply to one message, or `None` for notifications and responses.
async fn handle(server: &Server, message: Value) -> Option<Value> {
  let id = message.get("id").cloned();
  let Some(method) = message.get("method").and_then(Value::as_str) else {
    // A response to something we never send, or junk.
    return id.filter(|_| message.get("result").is_none() && message.get("error").is_none()).map(|id| error_response(id, INVALID_REQUEST, "Invalid request"));
  };
  let id = id?; // Notifications (`notifications/initialized`, cancellations) get no reply.
  let params = message.get("params").cloned().unwrap_or(Value::Null);
  let result = match method {
    "initialize" => Ok(initialize(server.scope(), &params)),
    "ping" => Ok(json!({})),
    "tools/list" => Ok(json!({ "tools": server.tool_definitions() })),
    "tools/call" => {
      let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Some(error_response(id, INVALID_PARAMS, "tools/call needs a tool name"));
      };
      let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
      match server.call(name, arguments).await {
        Some(result) => Ok(result),
        None => Err((INVALID_PARAMS, format!("Unknown tool: {name}"))),
      }
    },
    // Clients may probe these even though we don't advertise them.
    "resources/list" => Ok(json!({ "resources": [] })),
    "prompts/list" => Ok(json!({ "prompts": [] })),
    _ => Err((METHOD_NOT_FOUND, format!("Method not found: {method}"))),
  };
  Some(match result {
    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    Err((code, message)) => error_response(id, code, &message),
  })
}

fn initialize(scope: Scope, params: &Value) -> Value {
  let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
  let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) { requested } else { PROTOCOL_VERSION };
  json!({
    "protocolVersion": version,
    "capabilities": { "tools": { "listChanged": false } },
    "serverInfo": { "name": scope.server_name(), "version": env!("CARGO_PKG_VERSION") },
    "instructions": tools::instructions(scope),
  })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
  json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
  use super::*;

  mod scope_tests {
    use super::*;

    #[test]
    fn the_executable_name_picks_the_scope() {
      assert_eq!(Scope::from_args(&args(&["/Applications/PhotoCraft.app/Contents/MacOS/photocraft", "--mcp"])), Ok(Scope::Photo));
      assert_eq!(Scope::from_args(&args(&["/Applications/FilmCraft.app/Contents/MacOS/filmcraft", "--mcp"])), Ok(Scope::Film));
      assert_eq!(Scope::from_args(&args(&["target/release/artcraft-native", "--mcp"])), Ok(Scope::All));
    }

    #[test]
    fn the_scope_flag_wins_over_the_name() {
      assert_eq!(Scope::from_args(&args(&["photocraft", "--mcp", "--scope", "film"])), Ok(Scope::Film));
      assert_eq!(Scope::from_args(&args(&["artcraft-native", "--mcp", "--scope=photo"])), Ok(Scope::Photo));
      assert!(Scope::from_args(&args(&["artcraft-native", "--mcp", "--scope", "audio"])).is_err());
      assert!(Scope::from_args(&args(&["artcraft-native", "--mcp", "--scope"])).is_err());
    }
  }

  mod protocol_tests {
    use super::*;

    #[tokio::test]
    async fn initialize_echoes_a_supported_version_and_advertises_tools() {
      let server = Server::new(Scope::Photo);
      let reply = handle(&server, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2024-11-05" } })).await.unwrap();
      assert_eq!(reply["result"]["protocolVersion"], "2024-11-05");
      assert_eq!(reply["result"]["serverInfo"]["name"], "photocraft");
      assert!(reply["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn notifications_get_no_reply() {
      let server = Server::new(Scope::All);
      assert!(handle(&server, json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await.is_none());
    }

    #[tokio::test]
    async fn unknown_methods_and_tools_are_errors() {
      let server = Server::new(Scope::All);
      let reply = handle(&server, json!({ "jsonrpc": "2.0", "id": "a", "method": "nope" })).await.unwrap();
      assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
      let reply = handle(&server, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "nope" } })).await.unwrap();
      assert_eq!(reply["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn the_scope_decides_which_generators_are_listed() {
      let names = |scope| {
        let server = Server::new(scope);
        server.tool_definitions().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
      };
      let photo = names(Scope::Photo);
      assert!(photo.contains(&"generate_image".to_owned()) && photo.contains(&"split_thumbnail".to_owned()) && !photo.contains(&"generate_video".to_owned()));
      let film = names(Scope::Film);
      assert!(film.contains(&"generate_video".to_owned()) && film.contains(&"assemble_sequence".to_owned()) && !film.contains(&"generate_image".to_owned()));
      assert!(names(Scope::All).iter().any(|n| n == "get_job"));
    }
  }

  fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
  }
}
