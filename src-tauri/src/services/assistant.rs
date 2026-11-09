//! Built-in assistant: AI providers used to review extensions and code
//! (design: docs/rust-rewrite/MCP_DESIGN.md §7, phase 3).
//!
//! * Providers: Anthropic (Claude), OpenAI, OpenRouter and any OpenAI-compatible
//!   server (Ollama, LM Studio, llama.cpp, vLLM). Several can be configured.
//! * API keys live in the OS keychain (`keyring`): Windows Credential Manager,
//!   macOS Keychain, Secret Service on Linux. Never in settings, files or logs,
//!   and never sent to the webview: requests are made here, in Rust.
//! * Optional daily token limit per provider.
//! * No telemetry: the only network calls are the user's own requests to the
//!   provider they configured.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, Runtime};

const KEYCHAIN_SERVICE: &str = "code-rs.assistant";
const ANTHROPIC_VERSION: &str = "2026-10-21";
const DEFAULT_MAX_TOKENS: u32 = 16_000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    Openai,
    Openrouter,
    /// Any server speaking the OpenAI chat-completions API (Ollama, LM Studio, ...).
    OpenaiCompatible,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub label: String,
    pub kind: ProviderKind,
    pub model: String,
    /// Required for `openai-compatible`; optional override otherwise.
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    #[serde(flatten)]
    pub provider: Provider,
    pub has_key: bool,
    /// True when requests never leave the machine (localhost endpoint).
    pub local: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    pub text: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

fn base_url(p: &Provider) -> String {
    let default = match p.kind {
        ProviderKind::Anthropic => "https://api.anthropic.com",
        ProviderKind::Openai => "https://api.openai.com/v1",
        ProviderKind::Openrouter => "https://openrouter.ai/api/v1",
        ProviderKind::OpenaiCompatible => "http://localhost:11434/v1",
    };
    p.base_url.clone().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| default.to_string()).trim_end_matches('/').to_string()
}

fn is_local(url: &str) -> bool {
    let rest = url.split("://").nth(1).unwrap_or("");
    let host = rest.split(['/', ':']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// Plain HTTP is only allowed for local servers; everything else must be HTTPS.
fn check_url(url: &str) -> Result<(), String> {
    if url.starts_with("https://") || (url.starts_with("http://") && is_local(url)) {
        Ok(())
    } else {
        Err(format!("refusing insecure endpoint {url}: use https (http is allowed only for localhost)"))
    }
}

fn config_path<R: Runtime>(app: &AppHandle<R>, name: &str) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join(name))
}

fn load<R: Runtime>(app: &AppHandle<R>) -> Vec<Provider> {
    config_path(app, "assistant-providers.json")
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn store<R: Runtime>(app: &AppHandle<R>, providers: &[Provider]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(providers).map_err(|e| e.to_string())?;
    std::fs::write(config_path(app, "assistant-providers.json")?, text).map_err(|e| e.to_string())
}

fn key_entry(id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, id).map_err(|e| e.to_string())
}

fn get_key(id: &str) -> Option<String> {
    key_entry(id).ok()?.get_password().ok().filter(|k| !k.is_empty())
}

#[tauri::command]
pub fn assistant_providers<R: Runtime>(app: AppHandle<R>) -> Vec<ProviderInfo> {
    load(&app)
        .into_iter()
        .map(|p| ProviderInfo { has_key: get_key(&p.id).is_some(), local: is_local(&base_url(&p)), provider: p })
        .collect()
}

/// Adds or updates a provider. `api_key`: Some(non-empty) stores it in the OS
/// keychain, Some("") removes it, None keeps the current one.
#[tauri::command]
pub fn assistant_provider_save<R: Runtime>(app: AppHandle<R>, provider: Provider, api_key: Option<String>) -> Result<(), String> {
    if provider.id.trim().is_empty() || provider.model.trim().is_empty() {
        return Err("provider id and model are required".into());
    }
    check_url(&base_url(&provider))?;
    match api_key.as_deref() {
        Some("") => {
            let _ = key_entry(&provider.id)?.delete_credential();
        }
        Some(key) => key_entry(&provider.id)?.set_password(key).map_err(|e| e.to_string())?,
        None => {}
    }
    let mut providers = load(&app);
    match providers.iter_mut().find(|p| p.id == provider.id) {
        Some(existing) => *existing = provider,
        None => providers.push(provider),
    }
    store(&app, &providers)
}

#[tauri::command]
pub fn assistant_provider_delete<R: Runtime>(app: AppHandle<R>, id: String) -> Result<(), String> {
    if let Ok(entry) = key_entry(&id) {
        let _ = entry.delete_credential();
    }
    let providers: Vec<Provider> = load(&app).into_iter().filter(|p| p.id != id).collect();
    store(&app, &providers)
}

/// Tokens used today per provider; enforces the optional daily limit.
fn usage_check_and_add<R: Runtime>(app: &AppHandle<R>, id: &str, add: u64, limit: u64) -> Result<(), String> {
    let path = config_path(app, "assistant-usage.json")?;
    let today = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) / 86_400).to_string();
    let mut usage: BTreeMap<String, BTreeMap<String, u64>> =
        std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    usage.retain(|day, _| *day == today);
    let used = usage.entry(today).or_default().entry(id.to_string()).or_insert(0);
    if add == 0 && limit > 0 && *used >= limit {
        return Err(format!("daily token limit reached for provider '{id}' ({used}/{limit} tokens)"));
    }
    *used += add;
    let _ = std::fs::write(&path, serde_json::to_string(&usage).unwrap_or_default());
    Ok(())
}

fn http_error(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(300).collect());
    format!("provider returned HTTP {status}: {message}")
}

fn post_json(url: &str, headers: &[(&str, String)], body: &Value) -> Result<Value, String> {
    let mut request = ureq::post(url).config().http_status_as_error(false).timeout_global(Some(std::time::Duration::from_secs(300))).build();
    for (k, v) in headers {
        request = request.header(*k, v.as_str());
    }
    let mut response = request.header("content-type", "application/json").send(body.to_string().as_str()).map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().map_err(|e| e.to_string())?;
    if !(200..300).contains(&status) {
        return Err(http_error(status, &text));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON from provider: {e}"))
}

fn complete_anthropic(p: &Provider, key: &str, system: &str, user: &str, max_tokens: u32) -> Result<(String, u64, u64), String> {
    let body = json!({
        "model": p.model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": [{ "role": "user", "content": user }],
        "output_config": { "effort": "medium" },
        // Server-side fallback: a request declined by the model's safeguards is
        // retried on another model instead of failing.
        "fallbacks": "default",
    });
    let headers = [
        ("x-api-key", key.to_string()),
        ("anthropic-version", ANTHROPIC_VERSION.to_string()),
        ("anthropic-beta", "server-side-fallback-2026-07-01".to_string()),
    ];
    let v = post_json(&format!("{}/v1/messages", base_url(p)), &headers, &body)?;
    if v.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        let why = v.pointer("/stop_details/explanation").and_then(Value::as_str).unwrap_or("declined");
        return Err(format!("the model declined the request: {why}"));
    }
    let text = v.get("content").and_then(Value::as_array).map(|blocks| {
        blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("")
    }).unwrap_or_default();
    let usage = |k: &str| v.pointer(&format!("/usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
    Ok((text, usage("input_tokens"), usage("output_tokens")))
}

fn complete_openai_like(p: &Provider, key: Option<&str>, system: &str, user: &str, max_tokens: u32) -> Result<(String, u64, u64), String> {
    let mut body = json!({
        "model": p.model,
        "messages": [{ "role": "system", "content": system }, { "role": "user", "content": user }],
    });
    // OpenAI's current models take max_completion_tokens; other servers use max_tokens.
    let limit_field = if p.kind == ProviderKind::Openai { "max_completion_tokens" } else { "max_tokens" };
    body[limit_field] = json!(max_tokens);
    let mut headers = Vec::new();
    if let Some(key) = key {
        headers.push(("authorization", format!("Bearer {key}")));
    }
    if p.kind == ProviderKind::Openrouter {
        headers.push(("x-title", "code-rs".to_string()));
    }
    let v = post_json(&format!("{}/chat/completions", base_url(p)), &headers, &body)?;
    let text = v.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or_default().to_string();
    let usage = |k: &str| v.pointer(&format!("/usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
    Ok((text, usage("prompt_tokens"), usage("completion_tokens")))
}

/// Single-turn completion with a provider. `daily_token_limit` 0 = unlimited.
#[tauri::command]
pub async fn assistant_complete<R: Runtime>(
    app: AppHandle<R>,
    provider_id: String,
    system: String,
    user: String,
    max_tokens: Option<u32>,
    daily_token_limit: Option<u64>,
) -> Result<Completion, String> {
    let provider = load(&app).into_iter().find(|p| p.id == provider_id).ok_or(format!("unknown provider '{provider_id}'"))?;
    let url = base_url(&provider);
    check_url(&url)?;
    let key = get_key(&provider.id);
    if key.is_none() && !matches!(provider.kind, ProviderKind::OpenaiCompatible) {
        return Err(format!("no API key stored for provider '{}'", provider.label));
    }
    let limit = daily_token_limit.unwrap_or(0);
    usage_check_and_add(&app, &provider.id, 0, limit)?;
    let max_tokens = max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

    let p = provider.clone();
    let (text, input_tokens, output_tokens) = tauri::async_runtime::spawn_blocking(move || match p.kind {
        ProviderKind::Anthropic => complete_anthropic(&p, key.as_deref().unwrap_or_default(), &system, &user, max_tokens),
        _ => complete_openai_like(&p, key.as_deref(), &system, &user, max_tokens),
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = usage_check_and_add(&app, &provider.id, input_tokens + output_tokens, 0);
    Ok(Completion { text, provider: provider.label, model: provider.model, input_tokens, output_tokens })
}

// ---------------------------------------------------------------------------
// Multi-turn chat with tools (Contract A: `assistant_chat`).
//
// The chat agent speaks a small provider-neutral shape (`Block` / `ChatMessage`
// / `ToolSpec` / `ChatTurn`). We translate that to each provider's wire format
// and translate the response back. The mapping functions below are pure (no
// network, no app handle) so they can be unit-tested; `assistant_chat` wires
// them to the existing keychain, HTTP and daily-usage helpers.
// ---------------------------------------------------------------------------

/// A piece of a message. Serialised with an internally tagged `type` field and
/// camelCase field names on the wire (`toolUseId`, `isError`), matching the TS
/// contract.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub role: String, // "user" | "assistant"
    pub content: Vec<Block>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON schema for the tool's arguments.
    pub input_schema: Value,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChatTurn {
    /// Only `text` and `tool_use` blocks are ever produced here.
    pub content: Vec<Block>,
    pub stop_reason: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Normalise a provider stop/finish reason to the contract's vocabulary.
fn map_stop_reason_anthropic(reason: Option<&str>) -> String {
    match reason {
        Some("end_turn") => "end_turn",
        Some("tool_use") => "tool_use",
        Some("max_tokens") => "max_tokens",
        Some("refusal") => "refusal",
        _ => "other",
    }
    .to_string()
}

fn map_finish_reason_openai(reason: Option<&str>) -> String {
    match reason {
        Some("tool_calls") | Some("function_call") => "tool_use",
        Some("length") => "max_tokens",
        Some("stop") => "end_turn",
        Some("content_filter") => "refusal",
        _ => "other",
    }
    .to_string()
}

// --- Anthropic mapping -----------------------------------------------------

/// Build the Anthropic `/v1/messages` request body from the neutral shape.
fn anthropic_request_body(model: &str, max_tokens: u32, system: &str, messages: &[ChatMessage], tools: &[ToolSpec]) -> Value {
    let msgs: Vec<Value> = messages
        .iter()
        .map(|m| {
            let content: Vec<Value> = m
                .content
                .iter()
                .map(|b| match b {
                    Block::Text { text } => json!({ "type": "text", "text": text }),
                    Block::ToolUse { id, name, input } => {
                        json!({ "type": "tool_use", "id": id, "name": name, "input": input })
                    }
                    Block::ToolResult { tool_use_id, content, is_error } => {
                        let mut v = json!({ "type": "tool_result", "tool_use_id": tool_use_id, "content": content });
                        if let Some(err) = is_error {
                            v["is_error"] = json!(err);
                        }
                        v
                    }
                })
                .collect();
            json!({ "role": m.role, "content": content })
        })
        .collect();

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": msgs,
        "output_config": { "effort": "medium" },
        // Server-side fallback: a request declined by the model's safeguards is
        // retried on another model instead of failing.
        "fallbacks": "default",
    });

    if !tools.is_empty() {
        let tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema }))
            .collect();
        body["tools"] = json!(tool_defs);
    }
    body
}

/// Parse an Anthropic response into the neutral `ChatTurn` content + usage.
/// Keeps only `text` and `tool_use` blocks.
fn parse_anthropic_response(v: &Value) -> (Vec<Block>, String, u64, u64) {
    let blocks: Vec<Block> = v
        .get("content")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                    Some("text") => Some(Block::Text { text: b.get("text").and_then(Value::as_str).unwrap_or_default().to_string() }),
                    Some("tool_use") => Some(Block::ToolUse {
                        id: b.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                        name: b.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                        input: b.get("input").cloned().unwrap_or(Value::Null),
                    }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let stop = map_stop_reason_anthropic(v.get("stop_reason").and_then(Value::as_str));
    let usage = |k: &str| v.pointer(&format!("/usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
    (blocks, stop, usage("input_tokens"), usage("output_tokens"))
}

// --- OpenAI mapping --------------------------------------------------------

/// Build the OpenAI/OpenRouter/compatible `/chat/completions` request body.
/// `system` becomes the first message; assistant `tool_use` blocks become
/// `tool_calls`; `tool_result` blocks become `role: "tool"` messages.
fn openai_request_body(model: &str, is_openai: bool, max_tokens: u32, system: &str, messages: &[ChatMessage], tools: &[ToolSpec]) -> Value {
    let mut msgs: Vec<Value> = Vec::new();
    if !system.is_empty() {
        msgs.push(json!({ "role": "system", "content": system }));
    }

    for m in messages {
        // A single neutral message may carry text, tool_use and tool_result
        // blocks; OpenAI wants text + tool_calls on one assistant/user message
        // and each tool_result as its own `role: "tool"` message.
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_results: Vec<Value> = Vec::new();

        for b in &m.content {
            match b {
                Block::Text { text } => text_parts.push(text.clone()),
                Block::ToolUse { id, name, input } => tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": input.to_string() },
                })),
                Block::ToolResult { tool_use_id, content, .. } => tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                })),
            }
        }

        let has_text = !text_parts.is_empty();
        if has_text || !tool_calls.is_empty() {
            let mut msg = json!({ "role": m.role });
            // content is required; use null when there is only tool_calls.
            msg["content"] = if has_text { json!(text_parts.join("")) } else { Value::Null };
            if !tool_calls.is_empty() {
                msg["tool_calls"] = json!(tool_calls);
            }
            msgs.push(msg);
        }
        msgs.extend(tool_results);
    }

    let mut body = json!({ "model": model, "messages": msgs });
    // OpenAI's current models take max_completion_tokens; other servers use max_tokens.
    let limit_field = if is_openai { "max_completion_tokens" } else { "max_tokens" };
    body[limit_field] = json!(max_tokens);

    if !tools.is_empty() {
        let tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "function": { "name": t.name, "description": t.description, "parameters": t.input_schema },
            }))
            .collect();
        body["tools"] = json!(tool_defs);
    }
    body
}

/// Parse an OpenAI-family response into neutral `ChatTurn` content + usage.
/// Tool-call arguments are parsed as JSON; on failure the raw string is kept as
/// `{"_raw": "..."}`.
fn parse_openai_response(v: &Value) -> (Vec<Block>, String, u64, u64) {
    let mut blocks: Vec<Block> = Vec::new();
    let message = v.pointer("/choices/0/message");

    if let Some(text) = message.and_then(|m| m.get("content")).and_then(Value::as_str) {
        if !text.is_empty() {
            blocks.push(Block::Text { text: text.to_string() });
        }
    }

    if let Some(calls) = message.and_then(|m| m.get("tool_calls")).and_then(Value::as_array) {
        for call in calls {
            let id = call.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_string();
            let raw_args = call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("");
            let input = if raw_args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(raw_args).unwrap_or_else(|_| json!({ "_raw": raw_args }))
            };
            blocks.push(Block::ToolUse { id, name, input });
        }
    }

    let finish = map_finish_reason_openai(v.pointer("/choices/0/finish_reason").and_then(Value::as_str));
    let usage = |k: &str| v.pointer(&format!("/usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
    (blocks, finish, usage("prompt_tokens"), usage("completion_tokens"))
}

// --- Blocking provider calls ------------------------------------------------

/// Applies the effort chosen in the chat's effort dropdown, in each provider's own field.
/// Unknown values are ignored, and OpenAI-compatible (local) servers get nothing.
fn apply_effort(body: &mut Value, kind: &ProviderKind, effort: Option<&str>) {
    let Some(effort) = effort else { return };
    match kind {
        ProviderKind::Anthropic if matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") => {
            body["output_config"]["effort"] = json!(effort);
        }
        ProviderKind::Openai if matches!(effort, "low" | "medium" | "high") => {
            body["reasoning_effort"] = json!(effort);
        }
        ProviderKind::Openrouter if matches!(effort, "low" | "medium" | "high") => {
            body["reasoning"] = json!({ "effort": effort });
        }
        _ => {}
    }
}

fn chat_anthropic(p: &Provider, key: &str, system: &str, messages: &[ChatMessage], tools: &[ToolSpec], max_tokens: u32, effort: Option<&str>) -> Result<(Vec<Block>, String, u64, u64), String> {
    let mut body = anthropic_request_body(&p.model, max_tokens, system, messages, tools);
    apply_effort(&mut body, &p.kind, effort);
    let headers = [
        ("x-api-key", key.to_string()),
        ("anthropic-version", ANTHROPIC_VERSION.to_string()),
        ("anthropic-beta", "server-side-fallback-2026-07-01".to_string()),
    ];
    let v = post_json(&format!("{}/v1/messages", base_url(p)), &headers, &body)?;
    Ok(parse_anthropic_response(&v))
}

fn chat_openai_like(p: &Provider, key: Option<&str>, system: &str, messages: &[ChatMessage], tools: &[ToolSpec], max_tokens: u32, effort: Option<&str>) -> Result<(Vec<Block>, String, u64, u64), String> {
    let mut body = openai_request_body(&p.model, p.kind == ProviderKind::Openai, max_tokens, system, messages, tools);
    apply_effort(&mut body, &p.kind, effort);
    let mut headers = Vec::new();
    if let Some(key) = key {
        headers.push(("authorization", format!("Bearer {key}")));
    }
    if p.kind == ProviderKind::Openrouter {
        headers.push(("x-title", "code-rs".to_string()));
    }
    let v = post_json(&format!("{}/chat/completions", base_url(p)), &headers, &body)?;
    Ok(parse_openai_response(&v))
}

/// Multi-turn chat with tools. The frontend passes the whole conversation each
/// call; `daily_token_limit` 0 = unlimited.
#[tauri::command]
pub async fn assistant_chat<R: Runtime>(
    app: AppHandle<R>,
    provider_id: String,
    system: String,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolSpec>,
    max_tokens: Option<u32>,
    effort: Option<String>,
    daily_token_limit: Option<u64>,
) -> Result<ChatTurn, String> {
    let provider = load(&app).into_iter().find(|p| p.id == provider_id).ok_or(format!("unknown provider '{provider_id}'"))?;
    let url = base_url(&provider);
    check_url(&url)?;
    let key = get_key(&provider.id);
    if key.is_none() && !matches!(provider.kind, ProviderKind::OpenaiCompatible) {
        return Err(format!("no API key stored for provider '{}'", provider.label));
    }
    let limit = daily_token_limit.unwrap_or(0);
    usage_check_and_add(&app, &provider.id, 0, limit)?;
    let max_tokens = max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

    let p = provider.clone();
    let (content, stop_reason, input_tokens, output_tokens) = tauri::async_runtime::spawn_blocking(move || match p.kind {
        ProviderKind::Anthropic => chat_anthropic(&p, key.as_deref().unwrap_or_default(), &system, &messages, &tools, max_tokens, effort.as_deref()),
        _ => chat_openai_like(&p, key.as_deref(), &system, &messages, &tools, max_tokens, effort.as_deref()),
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = usage_check_and_add(&app, &provider.id, input_tokens + output_tokens, 0);
    Ok(ChatTurn {
        content,
        stop_reason,
        provider: provider.label,
        model: provider.model,
        input_tokens,
        output_tokens,
    })
}

#[cfg(test)]
mod effort_tests {
    use super::*;

    #[test]
    fn effort_goes_to_each_providers_field() {
        let mut a = json!({ "output_config": { "effort": "medium" } });
        apply_effort(&mut a, &ProviderKind::Anthropic, Some("max"));
        assert_eq!(a["output_config"]["effort"], "max");
        let mut o = json!({});
        apply_effort(&mut o, &ProviderKind::Openai, Some("high"));
        assert_eq!(o["reasoning_effort"], "high");
        let mut r = json!({});
        apply_effort(&mut r, &ProviderKind::Openrouter, Some("low"));
        assert_eq!(r["reasoning"]["effort"], "low");
        let mut l = json!({});
        apply_effort(&mut l, &ProviderKind::OpenaiCompatible, Some("high"));
        assert_eq!(l, json!({}));
        let mut bad = json!({});
        apply_effort(&mut bad, &ProviderKind::Openai, Some("max"));
        assert_eq!(bad, json!({}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_endpoints_may_use_http() {
        assert!(check_url("https://api.anthropic.com").is_ok());
        assert!(check_url("http://localhost:11434/v1").is_ok());
        assert!(check_url("http://127.0.0.1:1234/v1").is_ok());
        assert!(check_url("http://evil.example.com/v1").is_err());
        assert!(is_local("http://localhost:11434/v1"));
        assert!(!is_local("https://openrouter.ai/api/v1"));
    }

    #[test]
    fn http_errors_surface_the_api_message() {
        let msg = http_error(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#);
        assert_eq!(msg, "provider returned HTTP 401: invalid x-api-key");
    }

    // --- chat mapping: shared fixtures -------------------------------------

    fn sample_messages() -> Vec<ChatMessage> {
        vec![
            ChatMessage { role: "user".into(), content: vec![Block::Text { text: "read foo.rs".into() }] },
            ChatMessage {
                role: "assistant".into(),
                content: vec![
                    Block::Text { text: "I'll read it.".into() },
                    Block::ToolUse { id: "call_1".into(), name: "file_read".into(), input: json!({ "path": "foo.rs" }) },
                ],
            },
            ChatMessage {
                role: "user".into(),
                content: vec![Block::ToolResult { tool_use_id: "call_1".into(), content: "fn main() {}".into(), is_error: Some(false) }],
            },
        ]
    }

    fn sample_tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "file_read".into(),
            description: "Read a file".into(),
            input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
        }]
    }

    // --- Block serde ------------------------------------------------------

    #[test]
    fn block_serialises_with_camelcase_tagged_fields() {
        let b = Block::ToolResult { tool_use_id: "t1".into(), content: "ok".into(), is_error: Some(true) };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v, json!({ "type": "tool_result", "toolUseId": "t1", "content": "ok", "isError": true }));

        let b2 = Block::ToolUse { id: "x".into(), name: "n".into(), input: json!({ "a": 1 }) };
        let v2 = serde_json::to_value(&b2).unwrap();
        assert_eq!(v2, json!({ "type": "tool_use", "id": "x", "name": "n", "input": { "a": 1 } }));

        // is_error omitted when None.
        let b3 = Block::ToolResult { tool_use_id: "t2".into(), content: "c".into(), is_error: None };
        let v3 = serde_json::to_value(&b3).unwrap();
        assert_eq!(v3, json!({ "type": "tool_result", "toolUseId": "t2", "content": "c" }));
    }

    // --- Anthropic request body -------------------------------------------

    #[test]
    fn anthropic_request_maps_blocks_and_tools() {
        let body = anthropic_request_body("claude-x", 8000, "be nice", &sample_messages(), &sample_tools());
        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["max_tokens"], 8000);
        assert_eq!(body["system"], "be nice");
        assert_eq!(body["output_config"]["effort"], "medium");
        assert_eq!(body["fallbacks"], "default");

        // tool_use preserved as an Anthropic content block.
        assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(body["messages"][1]["content"][1]["id"], "call_1");
        assert_eq!(body["messages"][1]["content"][1]["name"], "file_read");
        assert_eq!(body["messages"][1]["content"][1]["input"]["path"], "foo.rs");

        // tool_result uses snake_case tool_use_id + is_error.
        let tr = &body["messages"][2]["content"][0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["tool_use_id"], "call_1");
        assert_eq!(tr["content"], "fn main() {}");
        assert_eq!(tr["is_error"], false);

        // tool spec mapped to {name, description, input_schema}.
        assert_eq!(body["tools"][0]["name"], "file_read");
        assert_eq!(body["tools"][0]["description"], "Read a file");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn anthropic_request_omits_tools_when_empty() {
        let body = anthropic_request_body("m", 100, "", &[], &[]);
        assert!(body.get("tools").is_none());
    }

    // --- Anthropic response parsing ---------------------------------------

    #[test]
    fn anthropic_response_keeps_text_and_tool_use_only() {
        let v = json!({
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "thinking", "thinking": "ignored" },
                { "type": "tool_use", "id": "u1", "name": "file_read", "input": { "path": "a" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 12, "output_tokens": 34 }
        });
        let (blocks, stop, inp, out) = parse_anthropic_response(&v);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], Block::Text { text: "hello".into() });
        assert_eq!(blocks[1], Block::ToolUse { id: "u1".into(), name: "file_read".into(), input: json!({ "path": "a" }) });
        assert_eq!(stop, "tool_use");
        assert_eq!(inp, 12);
        assert_eq!(out, 34);
    }

    #[test]
    fn anthropic_stop_reasons_map_to_contract() {
        assert_eq!(map_stop_reason_anthropic(Some("end_turn")), "end_turn");
        assert_eq!(map_stop_reason_anthropic(Some("tool_use")), "tool_use");
        assert_eq!(map_stop_reason_anthropic(Some("max_tokens")), "max_tokens");
        assert_eq!(map_stop_reason_anthropic(Some("refusal")), "refusal");
        assert_eq!(map_stop_reason_anthropic(Some("pause_turn")), "other");
        assert_eq!(map_stop_reason_anthropic(None), "other");
    }

    // --- OpenAI request body ----------------------------------------------

    #[test]
    fn openai_request_maps_system_tool_calls_and_results() {
        let body = openai_request_body("gpt-x", true, 500, "be nice", &sample_messages(), &sample_tools());

        // system is the first message.
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be nice");

        // user text.
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "read foo.rs");

        // assistant text + tool_calls with arguments as a JSON string.
        assert_eq!(body["messages"][2]["role"], "assistant");
        assert_eq!(body["messages"][2]["content"], "I'll read it.");
        let call = &body["messages"][2]["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "file_read");
        let args = call["function"]["arguments"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(args).unwrap(), json!({ "path": "foo.rs" }));

        // tool_result becomes a role:tool message with tool_call_id.
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
        assert_eq!(body["messages"][3]["content"], "fn main() {}");

        // OpenAI uses max_completion_tokens.
        assert_eq!(body["max_completion_tokens"], 500);
        assert!(body.get("max_tokens").is_none());

        // tools shaped as {type:function, function:{name,description,parameters}}.
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "file_read");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn openai_request_uses_max_tokens_for_non_openai() {
        let body = openai_request_body("llama", false, 42, "s", &sample_messages(), &[]);
        assert_eq!(body["max_tokens"], 42);
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn openai_request_tool_only_assistant_has_null_content() {
        let messages = vec![ChatMessage {
            role: "assistant".into(),
            content: vec![Block::ToolUse { id: "c".into(), name: "n".into(), input: json!({}) }],
        }];
        let body = openai_request_body("m", false, 10, "", &messages, &[]);
        // No system message (empty), assistant message is first.
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert!(body["messages"][0]["content"].is_null());
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "c");
    }

    // --- OpenAI response parsing ------------------------------------------

    #[test]
    fn openai_response_parses_text_and_tool_calls() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "working on it",
                    "tool_calls": [{
                        "id": "tc1",
                        "type": "function",
                        "function": { "name": "file_read", "arguments": "{\"path\":\"a.rs\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 7, "completion_tokens": 9 }
        });
        let (blocks, stop, inp, out) = parse_openai_response(&v);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], Block::Text { text: "working on it".into() });
        assert_eq!(blocks[1], Block::ToolUse { id: "tc1".into(), name: "file_read".into(), input: json!({ "path": "a.rs" }) });
        assert_eq!(stop, "tool_use");
        assert_eq!(inp, 7);
        assert_eq!(out, 9);
    }

    #[test]
    fn openai_response_malformed_arguments_pass_raw() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "tc2",
                        "type": "function",
                        "function": { "name": "n", "arguments": "not json" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let (blocks, _stop, _i, _o) = parse_openai_response(&v);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], Block::ToolUse { id: "tc2".into(), name: "n".into(), input: json!({ "_raw": "not json" }) });
    }

    #[test]
    fn openai_response_empty_content_is_dropped() {
        let v = json!({
            "choices": [{ "message": { "content": "" }, "finish_reason": "stop" }]
        });
        let (blocks, stop, _i, _o) = parse_openai_response(&v);
        assert!(blocks.is_empty());
        assert_eq!(stop, "end_turn");
    }

    #[test]
    fn openai_finish_reasons_map_to_contract() {
        assert_eq!(map_finish_reason_openai(Some("tool_calls")), "tool_use");
        assert_eq!(map_finish_reason_openai(Some("function_call")), "tool_use");
        assert_eq!(map_finish_reason_openai(Some("length")), "max_tokens");
        assert_eq!(map_finish_reason_openai(Some("stop")), "end_turn");
        assert_eq!(map_finish_reason_openai(Some("content_filter")), "refusal");
        assert_eq!(map_finish_reason_openai(None), "other");
    }
}
