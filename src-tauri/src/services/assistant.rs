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
}
