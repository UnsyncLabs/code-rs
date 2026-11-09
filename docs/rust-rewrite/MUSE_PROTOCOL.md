# Muse gadget protocol (as used by code-rs)

code-rs acts as a **Muse "Home Link" gadget**: the user's own Muse agent can chat with
code-rs and call tools that code-rs exposes. This document records what the protocol looks
like on the wire, reverse-engineered from the official
[muse-gadget-sdk](https://github.com/facebookincubator/muse-gadget-sdk) (Apache-2.0) Linux
reference client (`linux/src/musegadget/*`). File/line references below point into that SDK
as cloned to `/tmp/muse-sdk` (do not vendor it; it is a reference only).

The Rust implementation lives in `src-tauri/src/services/muse.rs` and follows Contract M.

> **Important finding about pairing.** In the reference SDK the *device token* (the
> `access_token` / `refresh_token` pair the gadget authenticates with) is **not** obtained
> from the SDK token over HTTP. It is delivered to the gadget by the **Muse phone app over
> BLE** during setup (`provision_v2`, see `ble_setup.py:237-301`). The SDK token
> (`mgst_…`) is only a *maker identity* that the gadget stores before pairing and later
> presents on token refresh; Muse records it at pairing time for attribution
> (`pair.py:55-96`, `service.py:_warn_if_sdk_token_changed`). There is no
> SDK-token→device-token exchange endpoint in the SDK. See
> [Open question: pairing transport](#open-question-pairing-transport) — this is the one
> part of Contract M that cannot be fully implemented from the SDK source alone.

---

## 1. Identity (`identity.py`)

A gadget has a stable, locally generated, MAC-shaped identity, created once and kept across
unpairing. It is **not** read from a NIC.

- `mac`: 6 random bytes; first octet forced to `(b & 0xFC) | 0x02` (unicast, locally
  administered). Rendered lowercase colon-separated, e.g. `02:ab:cd:ef:12:34`
  (`identity.py:57-60`).
- `suffix` = last 6 hex digits of the MAC with colons removed.
- `node_id` = `"homelink-" + suffix` — the ID Muse knows the device by.
- `device_id` = `"hatch-link:" + mac` (the `hatch-link:` prefix must not be renamed —
  server/app depend on it, `AGENTS.md` "Say Muse, never Hatch").
- `ble_name` = `"MuseGadget" + suffix.upper()` (no separator).

Persisted as `{"mac": "..."}` in `identity.json` in the state dir. We store the non-secret
identity in `app_config_dir/muse.json` instead (Contract M), keeping secrets in the keychain.

## 2. SDK token (`config.py:36-72`)

Format regex: `mgst_` + 42 base64url chars + one char from `[AEIMQUYcgkosw048]` (43 chars
after the prefix; the last encodes only 4 bits — 32 bytes of base64url). Reject anything
else. The "hint" shown to users is the first 12 characters (`mgst_` + 7).

The token is personal and is stored in the OS keychain (service `code-rs.muse`), never in a
file or a log.

## 3. Device token lookup and refresh (`muse_api.py`)

HTTP against `https://api.muse.ai` (overridable by `api_url_v2` from pairing; the old
`api_url` field is ignored).

### `GET /fetch_vms` (`muse_api.py:60-123`)
- Header `Authorization: Bearer <access_token>`, `X-API-Version: 1.0.0`, a `User-Agent`.
- Response JSON: `{ "vm_list": [ { vm_ws_url | vm_url, vm_auth_token, vm_name, vm_id,
  default }, … ] }`. Each entry with both a URL and `vm_auth_token` is a usable VM. Pick the
  one with `default: true`, else the first.
- **401** → the device token is rejected; re-pair/refresh, do not retry the same bearer.
- `error_title` / `backend_error_code` present → error.

### `POST /device_token/refresh` (`muse_api.py:107-162`)
- Header `Authorization: Bearer hatch_refresh:<raw_refresh>` where `<raw_refresh>` is the
  refresh token with any existing `…:` prefix stripped (`refresh_token.rsplit(":",1)[-1]`;
  the server rejects a doubled `hatch_refresh:` prefix). The `hatch_refresh:` prefix must
  not be renamed.
- Body JSON: `{ "device_id": <node_id>, "sdk_token": <mgst_…>? }`.
- Response JSON (optionally wrapped in `{"payload": …}`): `{ access_token, refresh_token }`.
  Both must be present or it is a failure.
- **Never** present the access token to refresh — only the refresh token — because the
  server can answer 200 with tokens every endpoint then rejects.
- **401** → pairing is gone; the device must pair again.
- Access tokens live ~4h; rotate at ~3h (`service.py:TOKEN_REFRESH_AGE_S = 3*3600`), and
  immediately if `/fetch_vms` returns 401.

## 4. Noise transport

### 4a. Handshake — Noise XX (`noise/noise_xx.py`)
- Protocol name: `Noise_XX_25519_AESGCM_SHA256` (`noise_xx.py:38`). i.e. X25519 DH,
  AES-256-GCM AEAD, SHA-256 hash, pattern **XX**, initiator = gadget.
- HKDF-SHA256 (2 or 3 outputs), standard Noise `MixKey`/`MixHash`/`Split`.
- AEAD nonce IV = 4 zero bytes ++ big-endian u64 counter (`build_nonce_iv`,
  `noise_xx.py:233-236`). **Note: big-endian**, which is non-standard for Noise (the spec
  uses little-endian); `snow` uses little-endian, so a hand-rolled or patched AEAD is
  required — see §4d.
- Low-order X25519 public keys and all-zero DH outputs are rejected (`noise_xx.py:…`).
- Handshake payloads are all empty: msg1 = `e`, msg2 = `e, ee, s, es` (empty payload),
  msg3 = `s, se` (empty payload). The upgrade bearer already authenticated us, so msg3's
  payload is empty (`link_client.py:_handshake`, 3 WebSocket binary frames exchanged in
  order: send msg1, recv msg2, send msg3, then `split()`).
- `split()` yields `(c1=send, c2=recv)` for the initiator.

### 4b. Record framing (`noise/framing.py`)
After `split`, each Noise *record* is a protobuf `NoiseTransportFrame`:

| field | # | type | notes |
|---|---|---|---|
| chunk_id | 1 | int64 (varint) | random per logical message; omitted when 0 |
| chunk_index | 2 | uint32 (varint) | omitted when 0 |
| total_chunks | 3 | uint32 (varint) | omitted when 0 (decoder defaults to 1) |
| payload | 4 | bytes | the (to-be-encrypted) application bytes |

- A logical message is split into chunks of at most `MAX_CHUNK_PAYLOAD = 65489` bytes;
  `total_chunks ≤ 256`; assembly bounded to 16 MiB, TTL 60 s (`framing.py:38-52`).
- **Order of operations when sending:** build the inner application bytes → wrap in
  `ServiceRequest` → `encode_noise_frames` (split into `NoiseTransportFrame`s, serialized) →
  **encrypt each serialized frame** with the send CipherState (AD = empty) → send each as one
  WebSocket binary frame (`transport.py:_encrypt_request` + `NoiseTransport._send_*`).
- **When receiving:** decrypt the WebSocket binary frame → that plaintext *is* a serialized
  `NoiseTransportFrame` → feed to the reassembler → once complete, decode as
  `ServiceResponse` → its payload is a `ServiceFrame` (`transport.py:decrypt_frame`).

### 4c. Service envelope (`noise/envelope.py`) — protobuf, hand-rolled (`noise/_proto.py`)
Standard protobuf wire format (varint / length-delimited), field numbers below.

`ServiceRequest` (device→VM): `service` #1 varint (enum, omitted when 0=DAEMON),
`payload` #2 bytes.
`ServiceResponse` (VM→device): `payload` #1 bytes.

`ServiceType` enum: DAEMON=0, SENTINEL=1, VAULT=2, AUTHD=3. All link/chat traffic uses
DAEMON (so `service` is omitted).

`ServiceFrame`: `stream_id` #1 int64; then exactly one of
`request`#2 / `response`#3 / `body_chunk`#4 / `reset`#5 (all length-delimited sub-messages).

`ApplicationRequest`: `verb`#1 string, `path`#2 string, `headers`#3 repeated Header,
`body`#4 bytes, `end_body`#5 bool.
`ApplicationResponse`: `status`#1 int32, `headers`#2 repeated Header, `body`#3 bytes,
`end_body`#4 bool.
`Header`: `key`#1 string, `value`#2 string.
`BodyChunk`: `data`#1 bytes, `end_body`#2 bool.
`Reset`: `code`#1 int32 (enum), `reason`#2 string. `ResetCode`: UNSPECIFIED=0, CANCELLED=1,
TIMEOUT=2, PROTOCOL_ERROR=3, REFUSED_STREAM=4, INTERNAL_ERROR=5, SERVICE_UNAVAILABLE=6.

So a device-originated HTTP request is: `ServiceFrame{stream_id, request=ApplicationRequest}`
→ bytes → `ServiceRequest{payload}` → bytes → chunked `NoiseTransportFrame`s → each encrypted
→ WebSocket binary frames. Streams are multiplexed by `stream_id` (client picks odd/increasing
ids starting at 1; `transport.py:_next_stream_id`).

### 4d. Crypto primitive choices for Rust
- Noise XX state machine: `snow` (`Noise_XX_25519_AESGCM_SHA256`) **in principle**, but the
  big-endian AEAD nonce (§4a) differs from the Noise spec's little-endian, so `snow` is not
  wire-compatible out of the box. The Rust implementation therefore uses a **hand-rolled XX
  state machine** built on individual RustCrypto primitives (`x25519-dalek`,
  `aes-gcm`, `sha2`, `hkdf`) with the big-endian nonce, mirroring `noise_xx.py` exactly. This
  keeps the wire bytes identical to the SDK and its published test vectors.
- WebSocket: `tungstenite` (sync) with `rustls` for TLS.
- Protobuf: hand-rolled varint/length-delimited codec in `muse.rs` (no `prost`), matching
  `_proto.py`, so no new proto toolchain is needed.

## 5. The link session (`link_client.py`)

### 5a. WebSocket upgrade
- URL: `wss://<noise_host>/v1/noise?vm_id=<url-encoded vm_id>` (optionally
  `&app_id=<validated app id>`; app id is only for realtime voice, not used here).
  `noise_host` defaults to `hatch.metaaivm.com` (`service.py:DEFAULT_NOISE_HOST`); may be
  overridden by the `noise_host` field saved at pairing.
- Header `Authorization: Bearer <vm_auth_token>` (the per-VM bearer from `/fetch_vms`).
- Upgrade **401** → `AUTH_REJECTED` (re-fetch VMs); **403** → `FORBIDDEN`.

### 5b. Control stream (`/link-control`)
- Open a long-lived `POST /link-control` application stream (`start_stream_request`, body
  left open). Both directions carry **length-prefixed JSON messages**: each message is
  `u32 little-endian length` ++ `compact-JSON bytes`, sent as `BodyChunk`s on that stream
  (`encode_message`, `MessageDecoder`, `link_client.py:106-152`). Empty payload = keepalive.
- Max inbound message 4 MiB; VM accepts ≤ 256 KiB per control message
  (`MAX_CONTROL_MESSAGE_BYTES`).

#### `link.register` (device→VM, first message)
```json
{ "type": "req", "id": "<uuid4>", "method": "link.register", "params": {
    "node_id": "homelink-xxxxxx",
    "display_name": "<hostname>",
    "platform": "linux",
    "version": "<ver>",
    "device_family": "homehub",
    "model_id": "linux",
    "is_wakeup_supported": false,
    "commands_v2": { "<name>": { "description": …, "required": {…}, "optional": {…},
                                 "timeout_ms"? } }
} }
```
Register as `platform: "linux"`, `device_family: "homehub"`. **Never** use family `link` or
advertise an OTA command — the server pushes ESP32 firmware to every `link` device
(`AGENTS.md`, `register_params`, `link_client.py:88-105`).

The VM replies with a message whose `id` equals the register id and no `method`
(`_handle`, `link_client.py:…`): `error`/`ok:false` → `FORBIDDEN`; otherwise registered.

#### `link.invoke` (VM→device)
```json
{ "method": "link.invoke", "id": "<call id>", "command": "<name>",
  "params": { … }, "timeout_ms": <int?> }
```
The device runs the command and replies on the same control stream:

#### `link.result` (device→VM)
```json
{ "method": "link.result", "id": "<call id>", "ok": true, "payload": <any> }
// or
{ "method": "link.result", "id": "<call id>", "ok": false, "error": "<message>" }
```
(`_invoke`, `link_client.py:…`). Up to `MAX_CONCURRENT_INVOKES = 4` at once.

#### `link.unpaired` / `node.unpaired` (VM→device)
`{ "event": "link.unpaired" }` → `UNPAIRED`; the device forgets the pairing.

### 5c. Sending a user message — `POST /chat/stream` (`link_client.py:send_chat`)
Request headers include `Content-Type: application/json`, `x-request-id: <uuid4>`,
`x-app-id: musegadget`. JSON body:
```json
{ "message": "<text>", "output_modality": "text", "device_id": "<node_id>",
  "session_id": "<side chat id>"? }
```
The HTTP response is only an **acknowledgement**: `{ "ok", "status", "response" }` where
`response` is `{ "result"?: { "message_id", "session_id", "reply_to_message_id"?,
"is_thread"? } }` or those fields at top level (`acknowledgement`, `_acknowledged_message_ids`
in `muse_turn.py`). The actual assistant reply arrives on `/chat/subscribe`, **not** here.

### 5d. Receiving the reply — `POST /chat/subscribe` + `MuseTurn`
To implement `muse_send(text, sessionId?, timeoutSecs?) → {text, sessionId}` we reproduce
`send_and_wait` (`service.py:send_and_wait`):

1. Open `POST /chat/subscribe` with body `{ "session_id": <id> }` (or `{}` for the main
   chat), `Accept: application/x-ndjson`. It streams **NDJSON events**; we read line by line,
   keeping only objects with `type == "event"` (`subscribe_chat`, `_chat_event`).
2. Send the user message with `/chat/stream`; take its acknowledgement to learn the user
   `message_id` and `session_id`.
3. Follow the events through `MuseTurn` (`muse_turn.py`), which correlates the assistant
   reply to our request. Event shape: `{ "type":"event", "event":"<name>", "seq":<int>?,
   "payload": { "session_id", "message_id", "reply_to_message_id"|"parent_message_id",
   "role", "text", "display_text", "transcript", "content", "activity_code",
   "activity_text", "status", "task_id" } }`.
   - Status events: `agent.status`, `task.status`. Terminal task statuses: completed, failed,
     errored, cancelled, canceled.
   - Message events: `delta.message_start`, `delta.text_append` (append `payload.text`),
     `delta.message_done` / `message.assistant` (final text from `display_text`, else
     `transcript`, else `content`; marks the reply done).
   - A reply is **bound** to our request when it (or its parent chain) reaches our user
     message id, or it arrives on a chat we own.
   - Muse's replies **do not name the message they answer** on a shared chat; so at most one
     wait per chat at a time, and the answer is "what follows our own `message.user` event"
     (`_ReplyReader`).
4. The answer text is the bound replies joined by blank lines, returned once the turn is
   `settled()`/`complete()` (quiet ≥ 3 s, or task terminal), bounded by `timeoutSecs`
   (`service.py` constants `REPLY_QUIET_S=3`, `DETAIL_REPLY_QUIET_S=30`, `EMPTY_REPLY_GRACE_S=1`,
   `MAX_WAIT_S=600`).

Control-character stripping (C0/C1 except tab/newline) is applied to all reply text so a reply
cannot drive the terminal (`muse_turn._strip_controls`).

### 5e. Outcomes (`link_client.Outcome`)
`CLOSED` (reconnect), `AUTH_REJECTED` (401 upgrade → re-fetch VMs), `FORBIDDEN` (403),
`UNPAIRED` (device removed), `STOPPED`.

## 6. Service loop (`service.py`)
- Load pairing; if none, idle and poll.
- Refresh the device token if older than 3 h (or forced after a 401), via
  `/device_token/refresh`; on 401 delete the pairing.
- `/fetch_vms`; pick default VM; open a `LinkSession`; serve until it ends.
- Exponential backoff (base 2 s, max 60 s), reset after a ≥ 30 s healthy session; auth
  failures floor the backoff at 15 s.
- Commands run WITH the gadget's own command set. In code-rs the commands come from the
  workbench via `muse_set_commands` and the invoke is relayed to the workbench
  (`muse://command`), which runs the MCP tool host **with approvals** and answers with
  `muse_command_result` (Contract M / Contract T).

## 7. code-rs mapping (Contract M)
| Contract M command | Behaviour |
|---|---|
| `muse_status` | `{ paired, connected, deviceName?, error? }` from in-memory state. |
| `muse_pair({sdkToken})` | Validate + store SDK token in keychain; run pairing. **See open question.** |
| `muse_unpair` | Delete pairing JSON + all keychain secrets; stop the link. |
| `muse_connect` | Start/keep the background connect loop (idempotent); auto-run at startup when paired. |
| `muse_set_commands({commands})` | Replace the command set registered with Muse (`commands_v2`). |
| `muse_send({text, sessionId?, timeoutSecs?})` | `/chat/stream` + `/chat/subscribe` wait → `{ text, sessionId }`. |
| `muse_command_result({callId, ok, result})` | Answer a relayed `link.invoke` with `link.result`. |
| event `muse://command` | Emitted on `link.invoke`: `{ callId, name, args }`. |
| event `muse://status` | Emitted on any status change: `MuseStatus`. |

The SDK's built-in `system.run` / `file.read` / `file.write` are **never** exposed; only
commands registered via `muse_set_commands` are offered to Muse.

## Open question: pairing transport
Contract M's `muse_pair({sdkToken})` is specified to "store the token in the keychain, run
the SDK pairing flow, and resolve when paired". In the reference SDK the pairing flow that
actually yields a device token is **BLE provisioning driven by the Muse phone app**
(`ble_setup.py`, `ble_server.py`, `pairing.py` — community pairing v5: P-256 ECDH,
HKDF-SHA256, AES-256-GCM, `confirm_app`), delivering `access_token`/`refresh_token`/
`noise_host` via `provision_v2`. There is **no SDK-token→device-token HTTP endpoint** in the
SDK that a desktop app without a BLE GATT peripheral could call.

Consequences for code-rs (a desktop Tauri app):
1. **If code-rs can run a BLE GATT peripheral** (Linux: BlueZ over D-Bus, as the reference
   does; Windows/macOS: platform BLE peripheral APIs), the full `provision_v2` flow can be
   ported. This needs a BLE stack not currently in `Cargo.toml` and is platform-specific; it
   is **not implemented here** and is left as a `TODO` in `muse.rs::muse_pair`.
2. **Alternative**: if Muse offers a non-BLE pairing path for SDK gadgets (e.g. a
   gadgets.muse.ai device-code / token-exchange endpoint keyed by the `mgst_` token), that
   endpoint is not present in this SDK and must be confirmed against Muse's gadget docs before
   it can be implemented.

Everything else in Contract M (identity, Noise XX, transport/envelope codec, link register /
invoke / result, chat send + subscribe wait, VM fetch, token refresh, keychain storage,
persistence, status/command events, and the connect/reconnect loop) **is** fully implemented
from the SDK source and does not depend on the pairing transport, except that the connect loop
needs a stored device token to run. `muse_pair` currently accepts and validates+stores the
SDK token and records intent, but returns a clear error explaining that the device-token
provisioning transport is pending the decision above.
