// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! Muse gadget service (Contract M).
//!
//! code-rs acts as a Muse "Home Link" gadget: the user's own Muse agent can
//! chat with code-rs and call tools code-rs exposes. This module reimplements,
//! in Rust, the parts of Meta's open-source `muse-gadget-sdk` (Apache-2.0)
//! Linux client needed for that: the Noise XX transport, the length-prefixed
//! JSON link protocol (`/link-control`), chat send/subscribe (`/chat/stream`,
//! `/chat/subscribe`), VM lookup and device-token refresh.
//!
//! Protocol notes, with file/line references into the SDK, are in
//! `docs/rust-rewrite/MUSE_PROTOCOL.md`.
//!
//! Secrets (the `mgst_` SDK token and the device access/refresh tokens) live in
//! the OS keychain under the service name `code-rs.muse`; non-secret device
//! state lives in `app_config_dir/muse.json`. Tokens are never logged.
//!
//! See the "Open question: pairing transport" section of the protocol doc:
//! obtaining the first device token requires the Muse phone app over BLE, which
//! is not implemented here; everything downstream of having a device token is.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod proto;
mod noise;
mod transport;
mod link;

#[cfg(test)]
mod tests;

pub use link::{DeviceDescription, MuseCommand};

/// Keychain service name for all Muse secrets (Contract M pattern
/// `code-rs.<feature>`).
const KEYCHAIN_SERVICE: &str = "code-rs.muse";
/// Keychain entry name for the user's personal SDK token.
const KEY_SDK_TOKEN: &str = "sdk-token";
/// Keychain entry name for the device access token.
const KEY_ACCESS_TOKEN: &str = "access-token";
/// Keychain entry name for the device refresh token.
const KEY_REFRESH_TOKEN: &str = "refresh-token";

/// Default Muse API root (overridable by the `api_url_v2` saved at pairing).
const API_BASE: &str = "https://api.muse.ai";
/// Default Noise host (overridable by the `noise_host` saved at pairing).
const DEFAULT_NOISE_HOST: &str = "hatch.metaaivm.com";

// ---------------------------------------------------------------------------
// Public (serialized) types — Contract M
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct MuseStatus {
    pub paired: bool,
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MuseReply {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Non-secret device state persisted to `app_config_dir/muse.json`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct DeviceState {
    /// MAC-shaped identity, e.g. `02:ab:cd:ef:12:34`. Kept across unpairing.
    mac: String,
    /// True once a device token has been provisioned (pairing completed).
    #[serde(default)]
    paired: bool,
    /// First 12 chars of the SDK token that was in use at pairing, for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sdk_token_hint: Option<String>,
    /// Muse API root override (`api_url_v2` from pairing), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_url_v2: Option<String>,
    /// Noise host override from pairing, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    noise_host: Option<String>,
    /// Unix seconds the access token was last saved (for refresh timing).
    #[serde(default)]
    access_token_saved_at: i64,
    /// Human-friendly device/VM name learned from `/fetch_vms`, for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Identity (identity.py)
// ---------------------------------------------------------------------------

mod identity {
    /// Generate a locally administered, unicast MAC-shaped id (identity.py).
    pub fn generate_mac() -> String {
        let mut octets = [0u8; 6];
        getrandom::fill(&mut octets).expect("getrandom");
        octets[0] = (octets[0] & 0xFC) | 0x02; // unicast, locally administered
        octets
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    pub fn suffix(mac: &str) -> String {
        let hex: String = mac.chars().filter(|c| *c != ':').collect();
        let n = hex.len();
        hex[n.saturating_sub(6)..].to_string()
    }

    pub fn node_id(mac: &str) -> String {
        format!("homelink-{}", suffix(mac))
    }

    pub fn device_id(mac: &str) -> String {
        format!("hatch-link:{mac}")
    }

    pub fn is_valid(mac: &str) -> bool {
        let parts: Vec<&str> = mac.split(':').collect();
        parts.len() == 6
            && parts
                .iter()
                .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
    }
}

/// Validate an SDK token against the gadgets.muse.ai format (config.py:36).
/// `mgst_` + 42 base64url chars + one of `[AEIMQUYcgkosw048]`.
fn validate_sdk_token(token: &str) -> Result<(), String> {
    let t = token.trim();
    let rest = t
        .strip_prefix("mgst_")
        .ok_or_else(|| "the SDK token is not valid; copy it again from gadgets.muse.ai".to_string())?;
    let ok = rest.len() == 43
        && rest[..42].bytes().all(|b| {
            b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
        })
        && matches!(
            rest.as_bytes()[42],
            b'A' | b'E' | b'I' | b'M' | b'Q' | b'U' | b'Y' | b'c' | b'g' | b'k' | b'o' | b's'
                | b'w' | b'0' | b'4' | b'8'
        );
    if ok {
        Ok(())
    } else {
        Err("the SDK token is not valid; copy it again from gadgets.muse.ai".into())
    }
}

/// The part of the SDK token shown to users (config.py:sdk_token_hint).
fn sdk_token_hint(token: Option<&str>) -> String {
    match token {
        Some(t) if !t.is_empty() => t.chars().take(12).collect(),
        _ => "none".into(),
    }
}

// ---------------------------------------------------------------------------
// Keychain helpers
// ---------------------------------------------------------------------------

fn key_entry(name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, name).map_err(|e| e.to_string())
}

fn get_secret(name: &str) -> Option<String> {
    key_entry(name)
        .ok()?
        .get_password()
        .ok()
        .filter(|s| !s.is_empty())
}

fn set_secret(name: &str, value: &str) -> Result<(), String> {
    key_entry(name)?.set_password(value).map_err(|e| e.to_string())
}

fn delete_secret(name: &str) {
    if let Ok(entry) = key_entry(name) {
        let _ = entry.delete_credential();
    }
}

// ---------------------------------------------------------------------------
// Muse API HTTP (muse_api.py) — ureq, blocking
// ---------------------------------------------------------------------------

/// The API root: `api_url_v2` if set, else the default Muse API.
fn api_root(api_url_v2: Option<&str>) -> String {
    api_url_v2
        .filter(|u| u.starts_with("https://"))
        .unwrap_or(API_BASE)
        .trim_end_matches('/')
        .to_string()
}

#[derive(Debug, Clone)]
struct Vm {
    vm_url: String,
    vm_auth_token: String,
    vm_name: String,
    vm_id: String,
    is_default: bool,
}

/// `GET /fetch_vms` (muse_api.py:fetch_vms_with_status). Returns the VMs and
/// the HTTP status, if one was received (None = transport failure).
fn fetch_vms(access_token: &str, root: &str) -> (Vec<Vm>, Option<u16>) {
    let url = format!("{root}/fetch_vms");
    let request = ureq::get(&url)
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("X-API-Version", "1.0.0");
    let mut response = match request.call() {
        Ok(r) => r,
        Err(e) => {
            log::warn!("muse: VM fetch failed: {e}");
            return (vec![], None);
        }
    };
    let status = response.status().as_u16();
    let text = match response.body_mut().read_to_string() {
        Ok(t) => t,
        Err(_) => return (vec![], Some(status)),
    };
    if status == 401 {
        return (vec![], Some(status));
    }
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return (vec![], Some(status));
    };
    if data.get("error_title").is_some() || data.get("backend_error_code").is_some() {
        log::warn!("muse: VM fetch returned an error");
        return (vec![], Some(status));
    }
    let Some(list) = data.get("vm_list").and_then(Value::as_array) else {
        return (vec![], Some(status));
    };
    let vms = list
        .iter()
        .filter_map(|entry| {
            let vm_url = entry
                .get("vm_ws_url")
                .or_else(|| entry.get("vm_url"))
                .and_then(Value::as_str)?;
            let vm_auth_token = entry.get("vm_auth_token").and_then(Value::as_str)?;
            Some(Vm {
                vm_url: vm_url.to_string(),
                vm_auth_token: vm_auth_token.to_string(),
                vm_name: entry.get("vm_name").and_then(Value::as_str).unwrap_or("").to_string(),
                vm_id: entry.get("vm_id").and_then(Value::as_str).unwrap_or("").to_string(),
                is_default: entry.get("default").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    (vms, Some(status))
}

/// `POST /device_token/refresh` (muse_api.py:refresh_device_token).
/// Returns `(Some((access, refresh)), status)` on success.
fn refresh_device_token(
    refresh_token: &str,
    device_id: &str,
    root: &str,
    sdk_token: Option<&str>,
) -> (Option<(String, String)>, Option<u16>) {
    // Strip any existing `…:` prefix; the server rejects a doubled hatch_refresh:.
    let raw_refresh = refresh_token.rsplit(':').next().unwrap_or(refresh_token);
    let mut body = json!({ "device_id": device_id });
    if let Some(sdk) = sdk_token {
        body["sdk_token"] = json!(sdk);
    }
    let url = format!("{root}/device_token/refresh");
    let request = ureq::post(&url)
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .header("Authorization", &format!("Bearer hatch_refresh:{raw_refresh}"))
        .header("Content-Type", "application/json");
    let mut response = match request.send(body.to_string().as_str()) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("muse: token refresh failed: {e}");
            return (None, None);
        }
    };
    let status = response.status().as_u16();
    let text = match response.body_mut().read_to_string() {
        Ok(t) => t,
        Err(_) => return (None, Some(status)),
    };
    if status == 401 {
        log::warn!("muse: token refresh rejected; device must pair again");
        return (None, Some(status));
    }
    if !(200..300).contains(&status) {
        log::warn!("muse: token refresh failed: HTTP {status}");
        return (None, Some(status));
    }
    let Ok(mut data) = serde_json::from_str::<Value>(&text) else {
        return (None, Some(status));
    };
    if let Some(payload) = data.get("payload").filter(|p| p.is_object()).cloned() {
        data = payload;
    }
    let access = data.get("access_token").and_then(Value::as_str);
    let refresh = data.get("refresh_token").and_then(Value::as_str);
    match (access, refresh) {
        (Some(a), Some(r)) => (Some((a.to_string(), r.to_string())), Some(status)),
        _ => {
            log::warn!("muse: token refresh response missing tokens");
            (None, Some(status))
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// In-memory, cross-command state for the Muse service.
#[derive(Default)]
pub struct MuseState {
    inner: Mutex<MuseInner>,
}

#[derive(Default)]
struct MuseInner {
    /// Live connection flag (set by the session loop).
    connected: bool,
    /// Last error surfaced to the UI, if any.
    error: Option<String>,
    /// The command set to register with Muse (replaced by muse_set_commands).
    commands: Vec<link::MuseCommand>,
    /// True while the background connect loop is running.
    running: bool,
}

pub type SharedMuseState = Arc<MuseState>;

use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager, Runtime};

const STATUS_EVENT: &str = "muse://status";
const COMMAND_EVENT: &str = "muse://command";

fn state_path<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("muse.json"))
}

fn load_state<R: Runtime>(app: &AppHandle<R>) -> Option<DeviceState> {
    let path = state_path(app).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_state<R: Runtime>(app: &AppHandle<R>, state: &DeviceState) -> Result<(), String> {
    let text = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(state_path(app)?, text).map_err(|e| e.to_string())
}

/// Load or create the device identity, persisting a freshly minted one.
fn load_or_create_state<R: Runtime>(app: &AppHandle<R>) -> DeviceState {
    if let Some(mut s) = load_state(app) {
        if identity::is_valid(&s.mac) {
            return s;
        }
        s.mac = identity::generate_mac();
        let _ = save_state(app, &s);
        return s;
    }
    let s = DeviceState { mac: identity::generate_mac(), ..Default::default() };
    let _ = save_state(app, &s);
    s
}

fn current_status<R: Runtime>(app: &AppHandle<R>, state: &SharedMuseState) -> MuseStatus {
    let device = load_state(app);
    let inner = state.inner.lock().unwrap();
    MuseStatus {
        paired: device.as_ref().map(|d| d.paired).unwrap_or(false),
        connected: inner.connected,
        device_name: device.and_then(|d| d.device_name),
        error: inner.error.clone(),
    }
}

fn emit_status<R: Runtime>(app: &AppHandle<R>, state: &SharedMuseState) {
    let status = current_status(app, state);
    let _ = app.emit(STATUS_EVENT, status);
}

// ---------------------------------------------------------------------------
// Tauri commands (Contract M)
// ---------------------------------------------------------------------------

/// Auto-connect at app start when the device is already paired (Contract M).
/// Reads the managed `SharedMuseState` and starts the background loop.
pub fn init_autoconnect<R: Runtime>(app: &AppHandle<R>) {
    let paired = load_state(app).map(|d| d.paired).unwrap_or(false);
    if !paired {
        return;
    }
    let Some(state) = app.try_state::<SharedMuseState>() else {
        return;
    };
    {
        let mut inner = state.inner.lock().unwrap();
        if inner.running {
            return;
        }
        inner.running = true;
        inner.error = None;
    }
    let shared: SharedMuseState = (*state).clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        connect_loop(app2, shared);
    });
}

#[tauri::command]
pub fn muse_status<R: Runtime>(app: AppHandle<R>, state: tauri::State<'_, SharedMuseState>) -> MuseStatus {
    current_status(&app, &state)
}

/// Validate and store the SDK token, then begin pairing.
///
/// See docs/rust-rewrite/MUSE_PROTOCOL.md "Open question: pairing transport":
/// obtaining the first device token requires the Muse phone app over BLE, which
/// is not implemented here. We store the token and identity and record intent,
/// then return a clear, actionable error rather than silently appearing paired.
#[tauri::command]
pub fn muse_pair<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, SharedMuseState>,
    sdk_token: String,
) -> Result<(), String> {
    validate_sdk_token(&sdk_token)?;
    set_secret(KEY_SDK_TOKEN, sdk_token.trim())?;

    let mut device = load_or_create_state(&app);
    device.sdk_token_hint = Some(sdk_token_hint(Some(sdk_token.trim())));
    save_state(&app, &device)?;
    emit_status(&app, &state);

    // TODO(muse-pair): run the BLE `provision_v2` flow (or a non-BLE
    // gadgets.muse.ai token exchange, if Muse offers one) to obtain the device
    // access/refresh tokens, store them in the keychain, set device.paired =
    // true and device.{api_url_v2,noise_host,access_token_saved_at,device_name}.
    Err("muse_pair: the SDK token is saved, but obtaining the device token \
         requires the Muse phone app's BLE pairing flow, which is not yet \
         implemented on the desktop. See docs/rust-rewrite/MUSE_PROTOCOL.md."
        .into())
}

/// Forget the device: delete the pairing state and all keychain secrets.
#[tauri::command]
pub fn muse_unpair<R: Runtime>(app: AppHandle<R>, state: tauri::State<'_, SharedMuseState>) -> Result<(), String> {
    delete_secret(KEY_SDK_TOKEN);
    delete_secret(KEY_ACCESS_TOKEN);
    delete_secret(KEY_REFRESH_TOKEN);
    // Keep the identity (as the SDK does across unpairing) but clear pairing.
    if let Some(mut device) = load_state(&app) {
        device.paired = false;
        device.sdk_token_hint = None;
        device.access_token_saved_at = 0;
        device.device_name = None;
        let _ = save_state(&app, &device);
    }
    {
        let mut inner = state.inner.lock().unwrap();
        inner.connected = false;
        inner.running = false;
        inner.error = None;
    }
    emit_status(&app, &state);
    Ok(())
}

/// Start (or keep) the background connect loop; idempotent. Also called at app
/// start when the device is already paired.
#[tauri::command]
pub fn muse_connect<R: Runtime>(app: AppHandle<R>, state: tauri::State<'_, SharedMuseState>) -> Result<(), String> {
    let device = load_state(&app);
    if device.as_ref().map(|d| !d.paired).unwrap_or(true) {
        return Err("not paired; run muse_pair first".into());
    }
    {
        let mut inner = state.inner.lock().unwrap();
        if inner.running {
            return Ok(()); // idempotent
        }
        inner.running = true;
        inner.error = None;
    }
    let shared: SharedMuseState = (*state).clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        connect_loop(app2, shared);
    });
    Ok(())
}

/// Replace the set of commands exposed to the Muse agent.
#[tauri::command]
pub fn muse_set_commands(
    state: tauri::State<'_, SharedMuseState>,
    commands: Vec<link::MuseCommand>,
) -> Result<(), String> {
    let mut inner = state.inner.lock().unwrap();
    inner.commands = commands;
    // TODO(muse-net): if a session is live, re-send link.register with the new
    // commands_v2 so Muse picks up the change without a reconnect.
    Ok(())
}

/// Send a user message to the Muse and wait for its reply.
#[tauri::command]
pub fn muse_send<R: Runtime>(
    _app: AppHandle<R>,
    _state: tauri::State<'_, SharedMuseState>,
    _text: String,
    _session_id: Option<String>,
    _timeout_secs: Option<u64>,
) -> Result<MuseReply, String> {
    // TODO(muse-net): with a live session, open POST /chat/subscribe, send the
    // message via POST /chat/stream, drive a session::ReplyTracker to the answer
    // within the timeout, and return { text, sessionId }. Blocked on the pairing
    // transport providing a device token (see muse_pair).
    Err("muse_send: not connected (device-token pairing is not yet implemented; \
         see docs/rust-rewrite/MUSE_PROTOCOL.md)."
        .into())
}

/// Answer a relayed `link.invoke` with a `link.result` message.
#[tauri::command]
pub fn muse_command_result<R: Runtime>(
    _app: AppHandle<R>,
    _state: tauri::State<'_, SharedMuseState>,
    _call_id: String,
    _ok: bool,
    _result: Value,
) -> Result<(), String> {
    // TODO(muse-net): route the result to the live session's control stream as
    // link.result (see link::result_message). Blocked on the session runner.
    Err("muse_command_result: no live session".into())
}

// ---------------------------------------------------------------------------
// Background connect/reconnect loop (service.py:run)
// ---------------------------------------------------------------------------

/// The reconnect loop: refresh the device token when due, fetch VMs, open a
/// session and serve until it ends, with exponential backoff.
///
/// The actual WebSocket session (session::run_session) is not yet wired in
/// (blocked on the pairing transport); this function implements the token /
/// VM-fetch scaffolding and keeps the published status accurate.
fn connect_loop<R: Runtime>(app: AppHandle<R>, state: SharedMuseState) {
    let mut backoff_secs = 2.0f64;
    loop {
        if !state.inner.lock().unwrap().running {
            break;
        }
        let Some(mut device) = load_state(&app) else {
            set_error(&app, &state, Some("not paired".into()));
            break;
        };
        if !device.paired {
            set_error(&app, &state, Some("not paired".into()));
            break;
        }

        // Rotate the device token if it is older than ~3h.
        let age = now_secs() - device.access_token_saved_at;
        if age >= 3 * 3600 {
            if let Some(refresh) = get_secret(KEY_REFRESH_TOKEN) {
                let root = api_root(device.api_url_v2.as_deref());
                let sdk = get_secret(KEY_SDK_TOKEN);
                let (tokens, status) = refresh_device_token(
                    &refresh,
                    &identity::node_id(&device.mac),
                    &root,
                    sdk.as_deref(),
                );
                if let Some((access, new_refresh)) = tokens {
                    let _ = set_secret(KEY_ACCESS_TOKEN, &access);
                    let _ = set_secret(KEY_REFRESH_TOKEN, &new_refresh);
                    device.access_token_saved_at = now_secs();
                    let _ = save_state(&app, &device);
                } else if status == Some(401) {
                    // Pairing revoked.
                    device.paired = false;
                    let _ = save_state(&app, &device);
                    set_error(&app, &state, Some("pairing revoked; pair again".into()));
                    break;
                }
            }
        }

        let Some(access) = get_secret(KEY_ACCESS_TOKEN) else {
            set_error(&app, &state, Some("missing device token".into()));
            break;
        };
        let root = api_root(device.api_url_v2.as_deref());
        let (vms, status) = fetch_vms(&access, &root);
        if status == Some(401) {
            // Force a refresh next loop; back off a little.
            device.access_token_saved_at = 0;
            let _ = save_state(&app, &device);
            sleep_backoff(&mut backoff_secs, 15.0);
            continue;
        }
        let Some(vm) = vms.iter().find(|v| v.is_default).or_else(|| vms.first()) else {
            sleep_backoff(&mut backoff_secs, 2.0);
            continue;
        };

        // Surface the VM name and mark paired/known for the UI.
        if device.device_name.as_deref() != Some(&vm.vm_name) && !vm.vm_name.is_empty() {
            device.device_name = Some(vm.vm_name.clone());
            let _ = save_state(&app, &device);
        }

        let _cfg = link::session::SessionConfig {
            noise_host: device.noise_host.clone().unwrap_or_else(|| DEFAULT_NOISE_HOST.into()),
            vm_id: if vm.vm_id.is_empty() { vm.vm_name.clone() } else { vm.vm_id.clone() },
            vm_auth_token: vm.vm_auth_token.clone(),
        };

        // TODO(muse-net): let outcome = session::run_session(&app, &state, _cfg, ...);
        // For now, without the WebSocket runner we cannot hold a live session.
        set_error(
            &app,
            &state,
            Some("session runner not yet implemented (see MUSE_PROTOCOL.md)".into()),
        );
        break;
    }
    {
        let mut inner = state.inner.lock().unwrap();
        inner.connected = false;
        inner.running = false;
    }
    emit_status(&app, &state);
}

fn set_error<R: Runtime>(app: &AppHandle<R>, state: &SharedMuseState, err: Option<String>) {
    {
        let mut inner = state.inner.lock().unwrap();
        inner.error = err;
        inner.connected = false;
    }
    emit_status(app, state);
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sleep_backoff(backoff_secs: &mut f64, floor: f64) {
    let delay = backoff_secs.max(floor).min(60.0);
    *backoff_secs = (*backoff_secs * 2.0).min(60.0);
    std::thread::sleep(std::time::Duration::from_secs_f64(delay));
}
