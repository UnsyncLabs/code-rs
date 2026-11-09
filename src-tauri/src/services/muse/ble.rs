// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! BLE provisioning (`provision_v2`) for Muse pairing.
//!
//! code-rs advertises as a Muse gadget and lets the Muse phone app provision it
//! over BLE, exactly as an ESP32 gadget or the Linux reference does. The wire
//! protocol is documented in `docs/rust-rewrite/MUSE_PROTOCOL.md` and split into
//! OS-independent pieces (unit-tested here) plus a platform GATT peripheral:
//!
//!   * [`framing`] — the `0xFE` chunk framing for GATT packets (`ble_framing.py`).
//!   * [`pairing`] — the P-256/HKDF/AES-GCM v5 community handshake (`pairing.py`).
//!   * [`setup`]   — the setup command dispatcher driving a [`pairing::PairingSession`]
//!                   over a `Transport` (`ble_setup.py`).
//!   * platform GATT server: Windows uses WinRT (`win.rs`); other OSes return a
//!     clear "not supported yet" error (`stub.rs`).
//!
//! BLE identity: Muse apps look for a peripheral advertising the setup service
//! UUID and named `MuseGadget-XXXXXX`, where `XXXXXX` is the uppercase last six
//! hex digits of the gadget MAC — matching the ESP32 firmware (`identity.c`:
//! `BLE_NAME_PREFIX "-%02X%02X%02X"`). The node id the gadget reports to the
//! cloud is the lowercase `homelink-xxxxxx` form.

pub mod framing;
pub mod pairing;
pub mod setup;

#[cfg(test)]
mod tests;

/// BLE service UUID the Muse apps scan for (`ble_server.py`/`ble_server.c`).
pub const SERVICE_UUID: &str = "7fdd3d1c-38ea-46cf-8b46-314ecf5f240c";
/// RX characteristic: the phone writes setup commands here (write / write-nr).
pub const RX_UUID: &str = "4d593029-28a2-4a6e-a1f0-3c2d5e8f9b01";
/// TX characteristic: the device notifies setup responses here (read / notify).
pub const TX_UUID: &str = "d75dc4ca-7b2b-4e9c-8f0a-1d2e3f4a5b6c";

/// How long `muse_pair` keeps the setup window open waiting for the app.
pub const SETUP_WINDOW_SECS: u64 = 300;

/// The BLE advertised name for a gadget MAC, e.g. `02:ab:cd:ef:12:34` ->
/// `MuseGadget-EF1234`. Matches the ESP32 firmware the Muse apps are tested
/// against (the Linux reference drops the hyphen; code-rs follows the firmware
/// so the shipping apps recognise it).
pub fn ble_name(mac: &str) -> String {
    let hex: String = mac.chars().filter(|c| *c != ':').collect();
    let suffix: String = hex.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
    format!("MuseGadget-{}", suffix.to_uppercase())
}

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::run_setup_window;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::run_setup_window;

/// Parameters a platform GATT server needs to host one setup window.
pub struct SetupParams {
    pub local_name: String,
    pub node_id: String,
    pub device_id: String,
    pub mac: String,
    pub firmware_version: String,
    pub sdk_token: Option<String>,
    pub timeout_secs: u64,
}
