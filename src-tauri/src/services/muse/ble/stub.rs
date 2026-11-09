// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! BLE pairing fallback for platforms without a GATT peripheral implementation.
//! Linux (BlueZ over D-Bus) and macOS (CoreBluetooth) are possible but not yet
//! ported; until then `muse_pair` returns a clear, actionable error on them.

use super::pairing::Credentials;
use super::SetupParams;

/// Host one BLE setup window. On unsupported OSes this returns immediately with
/// an error; the signature matches the Windows implementation so the caller is
/// OS-agnostic.
pub fn run_setup_window<F>(_params: SetupParams, _commit: F) -> Result<Credentials, String>
where
    F: FnMut(&Credentials) -> Result<(), String> + Send,
{
    Err("BLE pairing is not supported on this OS yet (Windows only for now); \
         see docs/rust-rewrite/MUSE_PROTOCOL.md"
        .into())
}
