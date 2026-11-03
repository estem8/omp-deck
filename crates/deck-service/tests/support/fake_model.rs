//! A scripted OpenAI-compatible model server on loopback: streaming `/v1/chat/completions`
//! (SSE), for omp's custom-provider config (`models.yml`, `api: openai-completions`). Every
//! completion request is recorded; a script answers each one with text or a tool call, paced
//! so a turn can stay busy for as long as a test needs.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

/// omp's provider and model id of the fake (`fake/fake-1`).
pub const MODEL: &str = "fake/fake-1";

/// How a request is answered: SSE chunks written `pace` apart.
pub struct Reply {
    chunks: Vec<Value>,
    pace: Duration,
}

/// Pieces a slow reply is cut into.
const PIECES: usize = 10;

static CALLS: AtomicU64 = AtomicU64::new(1);

fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "fake-1",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    })
}

fn usage() -> Value {
    json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "fake-1",
        "choices": [],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 },
    })
}

/// `text` cut into at most `n` non-empty pieces (on char boundaries).
fn pieces(text: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let size = chars.len().div_ceil(n).max(1);
    chars.chunks(size).map(|c| c.iter().collect()).collect()
}

impl Reply {
    /// An assistant text, at once.
    pub fn text(text: &str) -> Reply {
        Reply::slow_text(text, Duration::ZERO)
    }

    /// An assistant text streamed over about `total`.
    pub fn slow_text(text: &str, total: Duration) -> Reply {
        let mut chunks = vec![chunk(json!({ "role": "assistant", "content": "" }), None)];
        let parts = pieces(text, PIECES);
        let pace = total / parts.len().max(1) as u32;
        chunks.extend(
            parts
                .into_iter()
                .map(|p| chunk(json!({ "content": p }), None)),
        );
        chunks.push(chunk(json!({}), Some("stop")));
        chunks.push(usage());
        Reply { chunks, pace }
    }

    /// One call of tool `name` with `args`, at once.
    pub fn tool_call(name: &str, args: &Value) -> Reply {
        Reply::slow_tool_call(name, args, Duration::ZERO)
    }

    /// One call of tool `name`, its arguments streamed over about `total`.
    pub fn slow_tool_call(name: &str, args: &Value, total: Duration) -> Reply {
        let id = format!("call_{}", CALLS.fetch_add(1, Ordering::Relaxed));
        let call = |args: String, first: bool| {
            let mut f = json!({ "arguments": args });
            if first {
                f["name"] = json!(name);
            }
            let mut c = json!({ "index": 0, "function": f });
            if first {
                c["id"] = json!(id);
                c["type"] = json!("function");
            }
            json!({ "tool_calls": [c] })
        };
        let mut chunks = vec![chunk(json!({ "role": "assistant", "content": null }), None)];
        let parts = pieces(&args.to_string(), PIECES);
        let pace = total / parts.len().max(1) as u32;
        chunks.push(chunk(call(String::new(), true), None));
        chunks.extend(parts.into_iter().map(|p| chunk(call(p, false), None)));
        chunks.push(chunk(json!({}), Some("tool_calls")));
        chunks.push(usage());
        Reply { chunks, pace }
    }
}

type Script = Box<dyn FnMut(&Value) -> Reply + Send>;

struct Shared {
    requests: Mutex<Vec<Value>>,
    script: Mutex<Script>,
    stop: AtomicBool,
}

/// The running server; stopped on drop.
pub struct FakeModel {
    port: u16,
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeModel {
    /// Serves on a free loopback port; `script` answers each completion request (the request
    /// body as JSON).
    pub fn start(script: impl FnMut(&Value) -> Reply + Send + 'static) -> FakeModel {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake model");
        let port = listener.local_addr().expect("fake model address").port();
        let shared = Arc::new(Shared {
            requests: Mutex::new(Vec::new()),
            script: Mutex::new(Box::new(script)),
            stop: AtomicBool::new(false),
        });
        let s = shared.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if s.stop.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let s = s.clone();
                std::thread::spawn(move || {
                    let _ = serve(stream, &s);
                });
            }
        });
        FakeModel {
            port,
            shared,
            thread: Some(thread),
        }
    }

    /// omp's `models.yml` naming this server as provider `fake` with model `fake-1` (text and
    /// image input).
    pub fn models_yml(&self) -> String {
        format!(
            "providers:\n  fake:\n    baseUrl: http://127.0.0.1:{}/v1\n    apiKey: sk-fake\n    \
             api: openai-completions\n    models:\n      - id: fake-1\n        name: Fake One\n        \
             contextWindow: 200000\n        maxTokens: 4096\n        input: [text, image]\n",
            self.port
        )
    }

    /// Completion requests received so far.
    pub fn count(&self) -> usize {
        lock(&self.shared.requests).len()
    }

    /// The first request from index `after` on that `f` accepts.
    pub fn find(&self, after: usize, f: impl Fn(&Value) -> bool) -> Option<Value> {
        lock(&self.shared.requests)
            .iter()
            .skip(after)
            .find(|r| f(r))
            .cloned()
    }

    /// Every request from index `after` on.
    pub fn since(&self, after: usize) -> Vec<Value> {
        lock(&self.shared.requests)
            .iter()
            .skip(after)
            .cloned()
            .collect()
    }
}

impl Drop for FakeModel {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One HTTP/1.1 exchange (the connection is closed after it).
fn serve(stream: TcpStream, shared: &Shared) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let mut length = 0usize;
    let mut chunked = false;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                length = v.parse().unwrap_or(0);
            } else if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }
    let body = if chunked {
        let mut body = Vec::new();
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)
                .unwrap_or(0);
            let mut part = vec![0; n + 2];
            reader.read_exact(&mut part)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&part[..n]);
        }
        body
    } else {
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        body
    };
    if method == "POST" && path.ends_with("/chat/completions") {
        let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        lock(&shared.requests).push(request.clone());
        let reply = (lock(&shared.script))(&request);
        out.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
              Connection: close\r\n\r\n",
        )?;
        for c in &reply.chunks {
            out.write_all(format!("data: {c}\n\n").as_bytes())?;
            out.flush()?;
            if !reply.pace.is_zero() {
                std::thread::sleep(reply.pace);
            }
        }
        out.write_all(b"data: [DONE]\n\n")?;
        out.flush()
    } else if method == "GET" && path.ends_with("/models") {
        let list = json!({ "object": "list", "data": [{ "id": "fake-1", "object": "model" }] });
        let body = list.to_string();
        write!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    } else {
        out.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }
}

// ---- reading requests ---------------------------------------------------------------------------

/// The request's messages.
pub fn messages(request: &Value) -> &[Value] {
    request["messages"].as_array().map_or(&[], Vec::as_slice)
}

/// A message's role.
pub fn role(message: &Value) -> &str {
    message["role"].as_str().unwrap_or("")
}

/// A message's text: its string content, or its text parts joined.
pub fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The `data:` URLs of a message's image parts.
pub fn images(message: &Value) -> Vec<String> {
    message["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p["type"] == "image_url")
                .filter_map(|p| p["image_url"]["url"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The request offers tools (a conversation turn, not a side request such as a title).
pub fn has_tools(request: &Value) -> bool {
    request["tools"].as_array().is_some_and(|t| !t.is_empty())
}

/// The newest user message.
pub fn last_user(request: &Value) -> Option<&Value> {
    messages(request).iter().rev().find(|m| role(m) == "user")
}

/// Messages after the newest user message.
pub fn after_last_user(request: &Value) -> &[Value] {
    let m = messages(request);
    let i = m
        .iter()
        .rposition(|m| role(m) == "user")
        .map_or(0, |i| i + 1);
    &m[i..]
}

/// The newest user message's text.
pub fn last_user_text(request: &Value) -> String {
    last_user(request).map(text).unwrap_or_default()
}

// ---- subagents ----------------------------------------------------------------------------------

/// The user text that makes the parent run one subagent through omp's `task` tool
/// (see [`task_script`]); the subagent ends by answering with text.
pub const TASK_MARK: &str = "TASK-MARK";

/// Like [`TASK_MARK`], but the task declares an output schema: the subagent must end through its
/// `yield` finish tool.
pub const TASK_YIELD_MARK: &str = "TASK-YIELD-MARK";

/// What the parent says once the subagent's result came back.
pub const TASK_PARENT_REPLY: &str = "The subagent has finished.";

/// What the subagent answers or submits.
pub const TASK_RESULT: &str = "Subagent result: all done.";

/// Names of the tools the request offers.
pub fn tool_names(request: &Value) -> Vec<&str> {
    request["tools"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|t| {
            t["function"]["name"]
                .as_str()
                .or_else(|| t["name"].as_str())
        })
        .collect()
}

/// The request is a subagent's: omp opens its conversation with the `Complete assignment
/// thoroughly:` brief (which the parent's never carries), and gives a subagent with an output
/// schema the `yield` finish tool.
pub fn is_subagent(request: &Value) -> bool {
    tool_names(request).contains(&"yield") || last_user_text(request).contains(SUBAGENT_BRIEF)
}

/// The line omp puts above a task's assignment in the subagent's first user message.
const SUBAGENT_BRIEF: &str = "Complete assignment thoroughly";

/// Scripts the parent and its subagent for the keywords [`TASK_MARK`] and [`TASK_YIELD_MARK`];
/// `None` for any other request, so a test's own script handles the rest. Call it before the
/// script's other branches.
///
/// - Parent, keyword in the newest user message, no tool result yet: one `task` call (agent
///   `task`, name `Helper`; with an output schema for the yield keyword).
/// - The subagent: answers [`TASK_RESULT`] as text, or submits it through `yield` when that tool
///   is offered; either way over about `hold`, so the agent can be seen running.
/// - Parent, after the `task` result: [`TASK_PARENT_REPLY`].
pub fn task_script(request: &Value, hold: Duration) -> Option<Reply> {
    if is_subagent(request) {
        return Some(if tool_names(request).contains(&"yield") {
            Reply::slow_tool_call("yield", &json!({ "data": { "answer": TASK_RESULT } }), hold)
        } else {
            Reply::slow_text(TASK_RESULT, hold)
        });
    }
    let user = last_user_text(request);
    let structured = user.contains(TASK_YIELD_MARK);
    if !has_tools(request) || !(structured || user.contains(TASK_MARK)) {
        return None;
    }
    let fresh = !after_last_user(request).iter().any(|m| role(m) == "tool");
    Some(if fresh {
        let mut args = json!({
            "name": "Helper",
            "agent": "task",
            "task": "Report that the job is done.",
            "solutionSpace": "one fix: reply, nothing to research",
        });
        if structured {
            args["outputSchema"] = json!({
                "type": "object",
                "properties": { "answer": { "type": "string" } },
                "required": ["answer"],
            });
        }
        Reply::tool_call("task", &args)
    } else {
        Reply::text(TASK_PARENT_REPLY)
    })
}
