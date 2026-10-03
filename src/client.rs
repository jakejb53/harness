//! Streaming chat completions over server-sent events.

use anyhow::{Context, bail};
use futures::StreamExt;
use serde_json::{Map, Number, Value, json};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::config::{ApprovalDescriptionSettings, ResolvedEndpoint};

/// REPL requests use the configured description verbatim.
fn repl_tool(tool_description: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "REPL",
            "description": tool_description,
            "parameters": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "The Python source to execute."
                    }
                },
                "required": ["code"]
            }
        }
    })
}

/// Patch text is decoded once from JSON and passed directly to the Rust editor.
fn patch_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "apply_patch",
            "description": crate::patch::DESCRIPTION,
            "parameters": {
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "The complete *** Begin Patch ... *** End Patch text."}
                },
                "required": ["patch"]
            }
        }
    })
}

/// File reads expose only a relative path and optional inclusive line bounds.
fn read_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a UTF-8 text file with its original line numbers. Paths must be repository-relative. Optional start_line and end_line are 1-based and inclusive; omit them to read from line 1 through EOF. Requires read-only or read-write permission. Relative symlinks may resolve within the repository; absolute paths and escaping resolution are rejected. Results use the existing output limit.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1}
                },
                "required": ["path"]
            }
        }
    })
}

pub enum StreamEvent {
    /// One chunk's `delta`, every field as sent. The reply is assembled from these
    /// with `accumulate`.
    Delta(Map<String, Value>),
    /// Usage from the final usage chunk: `total` is prompt + completion, `prompt` is
    /// the prompt alone, sent back to the model as the next turn's FYI count.
    Usage { total: u64, prompt: Option<u64> },
    /// The reply finished normally. `truncated` means it stopped at the token limit
    /// (`finish_reason: "length"`), so a tool call may have been cut off mid-stream.
    Done { truncated: bool },
    /// The request or stream failed. Deltas already sent remain part of the reply.
    Error(String),
}

/// Start a streaming request. Each request gets its own channel, so aborting the
/// task and dropping the receiver discards everything from that request.
pub fn start(
    http: &reqwest::Client,
    endpoint: &ResolvedEndpoint,
    system_prompt: Option<&str>,
    tool_description: &str,
    messages: Vec<Value>,
) -> (JoinHandle<()>, UnboundedReceiver<StreamEvent>) {
    // The effective prompt (config or CLI override) is added at send time, never
    // saved in the session, so this invocation's prompt applies on resume too.
    let messages: Vec<Value> = system_prompt
        .map(|prompt| json!({ "role": "system", "content": prompt }))
        .into_iter()
        .chain(messages)
        .collect();
    let mut body = json!({
        "model": endpoint.model,
        "stream": true,
        "stream_options": { "include_usage": true },
        "tools": [repl_tool(tool_description), patch_tool(), read_tool()],
        "messages": messages,
    });
    // Config validation reserves the fields above; server-specific parameters
    // are forwarded at the top level without supplying sampling defaults.
    body.as_object_mut()
        .expect("request body is an object")
        .extend(endpoint.parameters.clone());
    let request = post(http, endpoint, &body);

    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let event = match stream(request, &tx).await {
            Ok(truncated) => StreamEvent::Done { truncated },
            Err(e) => StreamEvent::Error(format!("{e:#}")),
        };
        let _ = tx.send(event);
    });
    (task, rx)
}

/// One independent request: no chat history, tool definitions, or chat prompt are sent.
pub async fn describe_approval(
    http: &reqwest::Client,
    settings: &ApprovalDescriptionSettings,
    operation: &str,
) -> anyhow::Result<String> {
    let response = post(
        http,
        &settings.endpoint,
        &description_body(settings, operation),
    )
    .send()
    .await
    .context("requesting approval description")?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("reading approval description")?;
    if !status.is_success() {
        bail!("description endpoint HTTP {status}: {text}");
    }
    let body: Value =
        serde_json::from_str(&text).context("parsing approval description response")?;
    description_text(&body)
}

/// Substitute the operation once; placeholder-like text in arguments stays literal.
fn description_body(settings: &ApprovalDescriptionSettings, operation: &str) -> Value {
    let mut body = json!({
        "model": settings.endpoint.model,
        "stream": false,
        "messages": [{"role": "user", "content": settings.prompt.replace("${command}", operation)}],
    });
    body.as_object_mut()
        .expect("request body is an object")
        .extend(settings.endpoint.parameters.clone());
    body
}

/// Empty, malformed, and endpoint-error responses are visible failures, never blank descriptions.
fn description_text(body: &Value) -> anyhow::Result<String> {
    if let Some(error) = body.get("error").filter(|error| !error.is_null()) {
        bail!("description endpoint: {error}");
    }
    let text = body
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .context("description endpoint returned no text")?;
    Ok(text.to_owned())
}

async fn stream(
    request: reqwest::RequestBuilder,
    tx: &UnboundedSender<StreamEvent>,
) -> anyhow::Result<bool> {
    let response = request.send().await.context("sending request")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("HTTP {status}: {body}");
    }

    let mut bytes = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut finish_reason: Option<String> = None;
    while let Some(chunk) = bytes.next().await {
        buf.extend_from_slice(&chunk.context("reading stream")?);
        // Split on raw `\n` bytes; this never lands inside a UTF-8 sequence, and
        // multi-byte characters split across chunks stay buffered until complete.
        while let Some(end) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            // Only `data:` lines matter; comments, `event:` lines, and the blank
            // lines separating events are ignored.
            let Some(data) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();
            if data == "[DONE]" {
                return Ok(finish_reason.as_deref() == Some("length"));
            }
            let mut chunk: Value = serde_json::from_str(data)
                .with_context(|| format!("parsing stream data: {data}"))?;
            if let Some(error) = chunk.get("error") {
                bail!("{error}");
            }
            if let Some(reason) = chunk["choices"][0]["finish_reason"].as_str() {
                finish_reason = Some(reason.to_string());
            }
            // The final usage chunk has no choices, hence `pointer_mut`.
            if let Some(Value::Object(delta)) =
                chunk.pointer_mut("/choices/0/delta").map(Value::take)
                && !delta.is_empty()
                && tx.send(StreamEvent::Delta(delta)).is_err()
            {
                return Ok(false); // Receiver dropped: the request was cancelled.
            }
            if let Some(total) = chunk["usage"]["total_tokens"].as_u64()
                && tx
                    .send(StreamEvent::Usage {
                        total,
                        prompt: chunk["usage"]["prompt_tokens"].as_u64(),
                    })
                    .is_err()
            {
                return Ok(false);
            }
        }
    }
    // Some servers close the stream after the final chunk without sending `[DONE]`.
    match finish_reason {
        Some(reason) => Ok(reason == "length"),
        None => bail!("stream ended before the reply finished"),
    }
}

/// Merge a streamed `delta` into the message assembled so far, by the OpenAI SDK's
/// rule (`accumulate_delta`): strings are appended, numbers added, objects merged
/// recursively, and lists of entries with an `index` merged entry by entry. `index`
/// and `type` are replaced instead, since fragments repeat them. The rule doesn't
/// depend on field names, so fields the app doesn't know are reassembled too.
pub fn accumulate(acc: &mut Map<String, Value>, delta: Map<String, Value>) {
    for (key, value) in delta {
        let slot = acc.entry(key.as_str()).or_insert(Value::Null);
        // Even the first indexed list is merged: one chunk can hold several
        // fragments of the same entry.
        if slot.is_null()
            && let Value::Array(entries) = &value
            && has_indexed_entries(entries)
        {
            *slot = Value::Array(Vec::new());
        }
        if slot.is_null() || key == "index" || key == "type" {
            *slot = value;
            continue;
        }
        match (slot, value) {
            (Value::String(acc), Value::String(delta)) => acc.push_str(&delta),
            (Value::Number(acc), Value::Number(delta)) => *acc = add(acc, &delta),
            (Value::Object(acc), Value::Object(delta)) => accumulate(acc, delta),
            (Value::Array(acc), Value::Array(delta)) => accumulate_entries(acc, delta),
            // Mismatched types, such as a later `null`: keep what was assembled.
            _ => {}
        }
    }
}

/// Lists of plain values only gain entries. Otherwise each fragment merges into the
/// entry with the same `index`. Unlike the SDK, entries are matched by their `index`
/// value rather than list position (so out-of-order indices can't merge into the
/// wrong entry), and a fragment without one is appended instead of failing the reply.
fn accumulate_entries(acc: &mut Vec<Value>, delta: Vec<Value>) {
    let plain = acc.iter().all(|v| v.is_string() || v.is_number());
    if plain && (!acc.is_empty() || !has_indexed_entries(&delta)) {
        acc.extend(delta);
        return;
    }
    for fragment in delta {
        let target = fragment
            .get("index")
            .filter(|index| index.is_u64())
            .and_then(|index| {
                acc.iter()
                    .position(|entry| entry.get("index") == Some(index))
            });
        match (target, fragment) {
            (Some(position), Value::Object(fragment)) => {
                if let Value::Object(entry) = &mut acc[position] {
                    accumulate(entry, fragment);
                }
            }
            (_, fragment) => acc.push(fragment),
        }
    }
}

fn has_indexed_entries(entries: &[Value]) -> bool {
    entries
        .iter()
        .any(|entry| entry.get("index").is_some_and(Value::is_u64))
}

fn add(acc: &Number, delta: &Number) -> Number {
    match (acc.as_i64(), delta.as_i64()) {
        (Some(acc), Some(delta)) => acc.saturating_add(delta).into(),
        _ => Number::from_f64(acc.as_f64().unwrap_or(0.0) + delta.as_f64().unwrap_or(0.0))
            .unwrap_or_else(|| acc.clone()),
    }
}

fn post(
    http: &reqwest::Client,
    endpoint: &ResolvedEndpoint,
    body: &Value,
) -> reqwest::RequestBuilder {
    let request = http.post(&endpoint.url).json(body);
    match &endpoint.api_key {
        Some(key) => request.bearer_auth(key),
        None => request,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The editor uses the portable function-call shape, with no interpreter argument.
    #[test]
    fn patch_definition_is_a_literal_string_function() {
        let tool = patch_tool();
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "apply_patch");
        let parameters = &tool["function"]["parameters"];
        assert_eq!(parameters["required"], json!(["patch"]));
        assert_eq!(parameters["properties"]["patch"]["type"], "string");
    }

    /// The request carries exactly the pending operation, independent endpoint parameters, and no tools.
    #[test]
    fn approval_description_request_is_independent() {
        let settings = ApprovalDescriptionSettings {
            endpoint: ResolvedEndpoint {
                url: "http://description.test/v1/chat/completions".into(),
                model: "describer".into(),
                api_key: None,
                parameters: json!({"temperature": 0.1}).as_object().unwrap().clone(),
            },
            prompt: "Describe: ${command}".into(),
        };
        let operation =
            r#"{"executable":"/bin/echo","arguments":["${command}"],"working_directory":"/repo"}"#;
        assert_eq!(
            description_body(&settings, operation),
            json!({
                "model":"describer", "stream":false, "temperature":0.1,
                "messages":[{"role":"user","content":format!("Describe: {operation}")}]
            })
        );
    }

    /// Endpoint errors and missing text must not look like successfully generated empty descriptions.
    #[test]
    fn approval_description_response_validation() {
        assert_eq!(
            description_text(
                &json!({"choices":[{"message":{"content":" Explains the command. "}}]})
            )
            .unwrap(),
            "Explains the command."
        );
        for value in [
            json!({}),
            json!({"choices":[{"message":{"content":" "}}]}),
            json!({"error":{"message":"failed"}}),
        ] {
            assert!(description_text(&value).is_err());
        }
    }
}
