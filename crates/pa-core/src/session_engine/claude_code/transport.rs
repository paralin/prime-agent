use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use pa_agent::abort::{AbortController, AbortSignal};
use serde_json::{json, Value};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::process::{Child, Command};
use tokio::task::{JoinHandle, JoinSet};

use super::runtime::ClaudeCodeRuntime;
use super::{map_sdk_message, user_input, COORDINATION_PROMPT, DENIED_TOOLS};

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONTROL_REQUESTS: usize = 32;

pub type McpHandler = Arc<
    dyn Fn(Value, AbortSignal) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + Sync,
>;

pub struct QueryOptions {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub model: String,
    pub resume_session_id: Option<String>,
    pub effort: Option<String>,
    pub append_system_prompt: Option<String>,
    pub tools: Vec<String>,
    pub required_tools: Vec<String>,
    pub mcp_handler: Option<McpHandler>,
}

struct AbortTask<T>(JoinHandle<T>);

#[cfg(unix)]
struct ProcessGroupGuard(i32);

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.0),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn command(options: &QueryOptions) -> Command {
    let mut command = Command::new(&options.executable);
    command
        .current_dir(&options.cwd)
        .args([
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--setting-sources=",
            "--strict-mcp-config",
            "--permission-mode",
            "dontAsk",
        ])
        .arg("--model")
        .arg(&options.model)
        .arg("--tools")
        .arg(options.tools.join(","))
        .arg("--disallowedTools")
        .arg(DENIED_TOOLS.join(","))
        .arg("--append-system-prompt")
        .arg(
            options
                .append_system_prompt
                .as_deref()
                .unwrap_or(COORDINATION_PROMPT),
        )
        .env("CLAUDE_CODE_ENTRYPOINT", "sdk-rust")
        .env_remove("NODE_OPTIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut allowed = options.tools.clone();
    allowed.extend(options.required_tools.iter().cloned());
    if !allowed.is_empty() {
        command.arg("--allowedTools").arg(allowed.join(","));
    }
    if let Some(effort) = &options.effort {
        command.arg("--effort").arg(effort);
    }
    if let Some(session_id) = &options.resume_session_id {
        command.arg(format!("--resume={session_id}"));
    }
    if options.mcp_handler.is_some() {
        command
            .arg("--mcp-config")
            .arg(json!({"mcpServers":{"prime":{"type":"sdk","name":"prime"}}}).to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    command
}

/// Spawn the retained SDK-compatible stream transport. No provider work is submitted until initialization succeeds.
///
/// # Errors
/// Returns an error if the runtime already started, effort is invalid, or the executable cannot spawn.
pub fn start_query(
    runtime: Arc<ClaudeCodeRuntime>,
    options: QueryOptions,
) -> Result<JoinHandle<()>> {
    if let Some(session_id) = &options.resume_session_id {
        anyhow::ensure!(
            uuid::Uuid::parse_str(session_id).is_ok(),
            "invalid Claude Code resume session ID"
        );
    }
    if let Some(effort) = &options.effort {
        anyhow::ensure!(
            matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max"),
            "invalid Claude Code effort"
        );
    }
    runtime.begin_start()?;
    let child = match command(&options).spawn() {
        Ok(child) => child,
        Err(error) => {
            runtime.fail(format!("Failed to spawn Claude Code process: {error}"));
            return Err(error.into());
        }
    };
    #[cfg(unix)]
    let group = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .map(ProcessGroupGuard);
    Ok(tokio::spawn(async move {
        #[cfg(unix)]
        let _group = group;
        let mut child = child;
        let result = pump(&runtime, &options, &mut child).await;
        if let Err(error) = result {
            runtime.fail(format!("{error:#}"));
        } else if !runtime.snapshot().closed {
            runtime.fail("Claude Code query ended unexpectedly".into());
        }
        stop_child(&mut child).await;
    }))
}

async fn pump(
    runtime: &Arc<ClaudeCodeRuntime>,
    options: &QueryOptions,
    child: &mut Child,
) -> Result<()> {
    let mut stdin = child.stdin.take().context("Claude Code stdin missing")?;
    let mut stdout = BufReader::new(child.stdout.take().context("Claude Code stdout missing")?);
    let stderr = child.stderr.take().context("Claude Code stderr missing")?;
    let mut stderr_task = AbortTask(tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buffer = [0u8; 4096];
        let mut tail = Vec::new();
        loop {
            let read = stderr.read(&mut buffer).await?;
            if read == 0 {
                return Ok::<_, std::io::Error>(tail);
            }
            tail.extend_from_slice(&buffer[..read]);
            if tail.len() > 8192 {
                tail.drain(..tail.len() - 8192);
            }
        }
    }));
    let mut consumer = runtime.input().consumer()?;
    let signal = runtime.signal();
    let init_id = uuid::Uuid::new_v4().to_string();
    write_frame(&mut stdin,&json!({"type":"control_request", "request_id":init_id,
        "request":{"subtype":"initialize", "sdkMcpServers":if options.mcp_handler.is_some() {vec!["prime"]} else {vec![]}}})).await?;
    let init_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut initialized = false;
    let mut controllers: HashMap<String, AbortController> = HashMap::new();
    let mut pending: JoinSet<(String, Result<Value>)> = JoinSet::new();
    let mut partial_frame = Vec::new();
    let outcome: Result<()> = async {
        loop {
            tokio::select! {
                biased;
                () = signal.aborted() => return Ok(()),
                () = tokio::time::sleep_until(init_deadline), if !initialized => anyhow::bail!("Claude Code initialization timed out"),
                completion = pending.join_next(), if !pending.is_empty() => {
                    let (id,response) = completion.context("Claude Code control task missing")?.context("Claude Code control task failed")?;
                    controllers.remove(&id);
                    write_frame(&mut stdin,&control_response(&id,response)).await?;
                },
                frame = read_frame(&mut stdout, &mut partial_frame) => {
                    let Some(frame) = frame? else { anyhow::bail!("Claude Code query ended unexpectedly"); };
                    if frame["type"] == "control_response" && frame["response"]["request_id"] == init_id {
                        anyhow::ensure!(frame["response"]["subtype"] == "success", "Claude Code initialization failed: {}",frame["response"]["error"]);
                        initialized = true;
                    } else if frame["type"] == "control_cancel_request" {
                        if let Some(id) = frame["request_id"].as_str() {
                            if let Some(controller) = controllers.get(id) { controller.abort(); }
                        }
                    } else if frame["type"] == "control_request" {
                        let id = frame["request_id"].as_str().context("Claude Code control request omitted id")?.to_string();
                        if controllers.contains_key(&id) { continue; }
                        if controllers.len() >= MAX_CONTROL_REQUESTS {
                            write_frame(&mut stdin,&control_response(&id,Err(anyhow::anyhow!("Claude Code control request capacity reached")))).await?;
                            continue;
                        }
                        let request = frame["request"].clone();
                        let controller = AbortController::new();
                        controllers.insert(id.clone(),controller.clone());
                        let handler = options.mcp_handler.clone();
                        pending.spawn(async move { let response = handle_control(request,handler,controller.signal()).await; (id,response) });
                    } else if let Some(event) = map_sdk_message(&frame)? {
                        anyhow::ensure!(initialized, "Claude Code emitted an event before transport initialization");
                        runtime.handle_event(event)?;
                    }
                },
                text = consumer.next(), if initialized => {
                    let Some(text) = text else { return Ok(()); };
                    write_frame(&mut stdin,&user_input(&text)).await?;
                },
            }
        }
    }.await;
    for controller in controllers.values() {
        controller.abort();
    }
    pending.abort_all();
    while pending.join_next().await.is_some() {}
    drop(stdin);
    stderr_task.0.abort();
    let _ = (&mut stderr_task.0).await;
    outcome
}

async fn handle_control(
    request: Value,
    handler: Option<McpHandler>,
    signal: AbortSignal,
) -> Result<Value> {
    match request["subtype"].as_str() {
        Some("mcp_message") => {
            anyhow::ensure!(
                request["server_name"] == "prime",
                "unknown Claude Code SDK MCP server"
            );
            let handler = handler.context("Prime family MCP server unavailable")?;
            let message = request["message"].clone();
            let response = tokio::select! {
                biased;
                () = signal.aborted() => return Err(pa_agent::abort::aborted_error()),
                response = handler(message,signal.clone()) => response?,
            };
            Ok(json!({"mcp_response":response}))
        }
        Some("can_use_tool") => {
            Ok(json!({"behavior":"deny","message":"Prime Agent did not allow this tool"}))
        }
        _ => anyhow::bail!("unsupported Claude Code control request"),
    }
}

fn control_response(id: &str, response: Result<Value>) -> Value {
    match response {
        Ok(response) => {
            json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":response}})
        }
        Err(error) => {
            json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":error.to_string()}})
        }
    }
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), frame: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(frame)?;
    anyhow::ensure!(
        bytes.len() <= MAX_FRAME_BYTES,
        "Claude Code input frame exceeded its bound"
    );
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .context("write Claude Code stdin")?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut Vec<u8>,
) -> Result<Option<Value>> {
    loop {
        let chunk = reader.fill_buf().await.context("read Claude Code stdout")?;
        if chunk.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            let frame = serde_json::from_slice(line).context("parse Claude Code frame")?;
            line.clear();
            return Ok(Some(frame));
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(chunk.len(), |index| index + 1);
        anyhow::ensure!(
            line.len() + consumed <= MAX_FRAME_BYTES,
            "Claude Code output frame exceeded its bound"
        );
        line.extend_from_slice(&chunk[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            if line.iter().all(u8::is_ascii_whitespace) {
                line.clear();
                continue;
            }
            let frame = serde_json::from_slice(line).context("parse Claude Code frame")?;
            line.clear();
            return Ok(Some(frame));
        }
    }
}

async fn stop_child(child: &mut Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let mut command = Command::new("taskkill");
        command
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), command.status()).await;
    }
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    if tokio::time::timeout(Duration::from_secs(1), child.wait())
        .await
        .is_err()
    {
        #[cfg(unix)]
        if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[cfg(test)]
mod tests;
