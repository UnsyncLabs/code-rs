// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! Windows GATT peripheral for Muse BLE pairing, using WinRT via the `windows`
//! crate. This cannot be compiled on the (Linux) worktree where it was written;
//! it targets the real `windows` 0.58 API and is compiled by the integrator on
//! Windows. The requirements are checked at runtime (BLE adapter with the
//! peripheral role) and surfaced as errors.
//!
//! Design (mirrors `ble_server.py`, adapted to WinRT):
//!   * Create a `GattServiceProvider` for the Muse setup service UUID.
//!   * Add two characteristics:
//!       - RX: Write + WriteWithoutResponse, the phone writes setup commands.
//!         We subscribe to `WriteRequested` and feed bytes to the controller.
//!       - TX: Read + Notify, we push responses with `NotifyValueAsync`.
//!   * Start advertising with `IsConnectable = true` and `IsDiscoverable =
//!     true`; the local name is set on the adapter is not possible through the
//!     service provider, so we also run a `BluetoothLEAdvertisementPublisher`
//!     carrying the complete local name and the service UUID so the Muse app
//!     shows `MuseGadget-XXXXXX`.
//!   * A single worker owns the `SetupController`; WinRT callbacks hand it bytes
//!     through a channel so all protocol state stays on one thread (the SDK's
//!     invariant: encrypted record counters must reach the phone in order).

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::GUID;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisement, BluetoothLEAdvertisementPublisher,
};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristicProperties, GattLocalCharacteristic, GattLocalCharacteristicParameters,
    GattProtectionLevel, GattServiceProvider, GattServiceProviderAdvertisingParameters,
    GattServiceProviderResult, GattWriteRequestedEventArgs,
};
use windows::Devices::Bluetooth::{BluetoothAdapter, BluetoothError};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataReader, DataWriter};

use super::pairing::{Credentials, PairingSession};
use super::setup::{SetupController, Transport};
use super::{SetupParams, RX_UUID, SERVICE_UUID, TX_UUID};

fn guid(s: &str) -> windows::core::Result<GUID> {
    Ok(GUID::from(s))
}

/// Messages from WinRT callbacks to the single protocol worker.
enum Event {
    Write(Vec<u8>),
    Disconnect,
}

/// Transport backed by the TX `GattLocalCharacteristic`. `send_packets` paces
/// notifications like the reference (a short stagger between chunks) so the
/// phone reassembles them in order.
struct WinTransport {
    tx: GattLocalCharacteristic,
    mtu: Arc<Mutex<usize>>,
}

const CHUNK_STAGGER: Duration = Duration::from_millis(50);

impl Transport for WinTransport {
    fn send_packets(&self, packets: &[Vec<u8>]) {
        for (i, packet) in packets.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(CHUNK_STAGGER);
            }
            if let Err(e) = notify(&self.tx, packet) {
                log::warn!("muse/ble(win): notify failed: {e}");
                break;
            }
        }
    }

    fn mtu(&self) -> usize {
        *self.mtu.lock().unwrap()
    }

    fn disconnect(&self, _delay: f64) {
        // WinRT has no direct "drop this client" on the service provider; the
        // app disconnects after a fatal pairing error on its own. We rely on the
        // setup-window timeout / completion to tear advertising down.
    }
}

unsafe impl Send for WinTransport {}
unsafe impl Sync for WinTransport {}

fn notify(tx: &GattLocalCharacteristic, data: &[u8]) -> windows::core::Result<()> {
    let writer = DataWriter::new()?;
    writer.WriteBytes(data)?;
    let buffer = writer.DetachBuffer()?;
    // Fire-and-forget: NotifyValueAsync pushes to every subscribed client.
    tx.NotifyValueAsync(&buffer)?;
    Ok(())
}

/// Host one BLE setup window on Windows, blocking until the app provisions the
/// device, the timeout elapses, or a fatal error occurs. On success `commit`
/// has already persisted the credentials and the validated [`Credentials`] are
/// returned.
pub fn run_setup_window<F>(params: SetupParams, commit: F) -> Result<Credentials, String>
where
    F: FnMut(&Credentials) -> Result<(), String> + Send,
{
    check_peripheral_role()?;

    let service_uuid = guid(SERVICE_UUID).map_err(|e| e.to_string())?;
    let rx_uuid = guid(RX_UUID).map_err(|e| e.to_string())?;
    let tx_uuid = guid(TX_UUID).map_err(|e| e.to_string())?;

    // Create the GATT service provider.
    let provider_result: GattServiceProviderResult =
        GattServiceProvider::CreateAsync(service_uuid)
            .map_err(|e| e.to_string())?
            .get()
            .map_err(|e| e.to_string())?;
    if provider_result.Error().map_err(|e| e.to_string())? != BluetoothError::Success {
        return Err("Windows could not create the GATT service (is Bluetooth on?)".into());
    }
    let provider = provider_result.ServiceProvider().map_err(|e| e.to_string())?;
    let service = provider.Service().map_err(|e| e.to_string())?;

    // RX characteristic (phone writes).
    let rx_params = GattLocalCharacteristicParameters::new().map_err(|e| e.to_string())?;
    rx_params
        .SetCharacteristicProperties(
            GattCharacteristicProperties::Write | GattCharacteristicProperties::WriteWithoutResponse,
        )
        .map_err(|e| e.to_string())?;
    rx_params
        .SetWriteProtectionLevel(GattProtectionLevel::Plain)
        .map_err(|e| e.to_string())?;
    let rx_result = service
        .CreateCharacteristicAsync(rx_uuid, &rx_params)
        .map_err(|e| e.to_string())?
        .get()
        .map_err(|e| e.to_string())?;
    if rx_result.Error().map_err(|e| e.to_string())? != BluetoothError::Success {
        return Err("Windows could not create the RX characteristic".into());
    }
    let rx = rx_result.Characteristic().map_err(|e| e.to_string())?;

    // TX characteristic (device notifies).
    let tx_params = GattLocalCharacteristicParameters::new().map_err(|e| e.to_string())?;
    tx_params
        .SetCharacteristicProperties(
            GattCharacteristicProperties::Read | GattCharacteristicProperties::Notify,
        )
        .map_err(|e| e.to_string())?;
    tx_params
        .SetReadProtectionLevel(GattProtectionLevel::Plain)
        .map_err(|e| e.to_string())?;
    let tx_result = service
        .CreateCharacteristicAsync(tx_uuid, &tx_params)
        .map_err(|e| e.to_string())?
        .get()
        .map_err(|e| e.to_string())?;
    if tx_result.Error().map_err(|e| e.to_string())? != BluetoothError::Success {
        return Err("Windows could not create the TX characteristic".into());
    }
    let tx = tx_result.Characteristic().map_err(|e| e.to_string())?;

    let (events_tx, events_rx): (Sender<Event>, Receiver<Event>) = mpsc::channel();
    // We keep a fixed outbound packet cap (160-byte payloads, as the SDK does);
    // WinRT does not surface the negotiated ATT MTU per write, and over-small
    // chunks are always safe because the phone reassembles by the 0xFE header.
    let mtu = Arc::new(Mutex::new(super::framing::MAX_PACKET_BYTES + 3));

    // RX WriteRequested -> reassemble -> protocol worker.
    {
        let sender = events_tx.clone();
        let handler = TypedEventHandler::<GattLocalCharacteristic, GattWriteRequestedEventArgs>::new(
            move |_chr, args| {
                if let Some(args) = args.as_ref() {
                    // Hold a deferral so the async request read is valid.
                    let deferral = args.GetDeferral()?;
                    if let Ok(request) = args.GetRequestAsync()?.get() {
                        let value = request.Value()?;
                        let len = value.Length()? as usize;
                        let reader = DataReader::FromBuffer(&value)?;
                        let mut bytes = vec![0u8; len];
                        reader.ReadBytes(&mut bytes)?;
                        let _ = sender.send(Event::Write(bytes));
                        // A WriteWithResponse must be acknowledged; a
                        // WriteWithoutResponse must not be.
                        if let Ok(opt) = request.Option() {
                            use windows::Devices::Bluetooth::GenericAttributeProfile::GattWriteOption;
                            if opt == GattWriteOption::WriteWithResponse {
                                let _ = request.Respond();
                            }
                        }
                    }
                    deferral.Complete()?;
                }
                Ok(())
            },
        );
        rx.WriteRequested(&handler).map_err(|e| e.to_string())?;
    }

    // Subscriber changes signal (dis)connection of the phone.
    {
        let sender = events_tx.clone();
        let handler = TypedEventHandler::<GattLocalCharacteristic, windows::core::IInspectable>::new(
            move |chr, _args| {
                if let Some(chr) = chr.as_ref() {
                    let clients = chr.SubscribedClients()?;
                    if clients.Size()? == 0 {
                        let _ = sender.send(Event::Disconnect);
                    }
                }
                Ok(())
            },
        );
        tx.SubscribedClientsChanged(&handler).map_err(|e| e.to_string())?;
    }

    // Start advertising the service (connectable + discoverable).
    let adv_params = GattServiceProviderAdvertisingParameters::new().map_err(|e| e.to_string())?;
    adv_params.SetIsConnectable(true).map_err(|e| e.to_string())?;
    adv_params.SetIsDiscoverable(true).map_err(|e| e.to_string())?;
    provider
        .StartAdvertisingWithParameters(&adv_params)
        .map_err(|e| e.to_string())?;

    // A second publisher carries the complete local name so the app shows
    // "MuseGadget-XXXXXX". The GattServiceProvider advertisement cannot include
    // a local name; the LE advertisement publisher can.
    let name_publisher = start_name_advertisement(&params.local_name, service_uuid).ok();

    // Build the protocol worker on this thread's channel.
    let transport = WinTransport { tx: tx.clone(), mtu: mtu.clone() };
    let pairing = PairingSession::new(
        &params.node_id,
        &params.device_id,
        &params.mac,
        &params.firmware_version,
        params.sdk_token.clone(),
    );
    // We need the committed credentials out of the FnMut; capture them.
    let committed: Arc<Mutex<Option<Credentials>>> = Arc::new(Mutex::new(None));
    let commit_slot = committed.clone();
    let mut user_commit = commit;
    let commit_closure = Box::new(move |creds: &Credentials| -> Result<(), String> {
        user_commit(creds)?;
        *commit_slot.lock().unwrap() = Some(creds.clone());
        Ok(())
    });
    let mut controller =
        SetupController::new(pairing, &params.node_id, &params.firmware_version, true, &transport, commit_closure);

    // Pump events until provisioning completes or the window times out.
    let deadline = Instant::now() + Duration::from_secs(params.timeout_secs);
    let outcome = loop {
        if controller.completed() {
            break Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            break Err("pairing window closed before the Muse app provisioned code-rs".to_string());
        }
        match events_rx.recv_timeout(deadline - now) {
            Ok(Event::Write(bytes)) => controller.on_write(&bytes),
            Ok(Event::Disconnect) => controller.on_disconnect(),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                break Err("pairing window closed before the Muse app provisioned code-rs".into())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err("BLE setup channel closed".into()),
        }
    };

    // Tear down advertising regardless of outcome.
    let _ = provider.StopAdvertising();
    if let Some(publisher) = name_publisher {
        let _ = publisher.Stop();
    }

    match outcome {
        Ok(()) => committed
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "pairing reported success but no credentials were captured".into()),
        Err(e) => Err(e),
    }
}

/// Fail fast if the adapter cannot act as a BLE peripheral.
fn check_peripheral_role() -> Result<(), String> {
    let adapter = BluetoothAdapter::GetDefaultAsync()
        .map_err(|e| e.to_string())?
        .get()
        .map_err(|_| "no Bluetooth adapter found".to_string())?;
    let supported = adapter
        .IsPeripheralRoleSupported()
        .map_err(|e| e.to_string())?;
    if !supported {
        return Err("this Bluetooth adapter does not support the peripheral role".into());
    }
    Ok(())
}

/// Advertise the complete local name + service UUID so the Muse app lists the
/// device by name. Returned publisher must be kept alive and stopped later.
fn start_name_advertisement(
    local_name: &str,
    service_uuid: GUID,
) -> windows::core::Result<BluetoothLEAdvertisementPublisher> {
    let advertisement = BluetoothLEAdvertisement::new()?;
    advertisement.SetLocalName(&windows::core::HSTRING::from(local_name))?;
    advertisement.ServiceUuids()?.Append(service_uuid)?;
    let publisher = BluetoothLEAdvertisementPublisher::Create(&advertisement)?;
    publisher.Start()?;
    Ok(publisher)
}
