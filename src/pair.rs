//! Pairing and re-pairing, on its own thread so the keep-alive loop keeps running meanwhile.
//!
//! Safety rules:
//! - An existing pairing is only removed once the controller is seen *in pairing mode*
//!   (advertising as discoverable). Cancelling or timing out before that leaves it untouched.
//! - Only "just works" pairing (ConfirmOnly) is accepted: Xbox controllers have no display or
//!   keypad, so anything asking for a PIN comparison isn't one.
//! - "Pair a controller" (no known address) refuses to choose between several candidates, and
//!   removes the new pairing again unless the device reports Microsoft's vendor id.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristicUuids, GattCommunicationStatus, GattServiceUuids, GattSession,
};
use windows::Devices::Bluetooth::{BluetoothCacheMode, BluetoothLEDevice};
use windows::Devices::Enumeration::{
    DeviceInformationCustomPairing, DevicePairingKinds, DevicePairingRequestedEventArgs, DevicePairingResultStatus,
    DeviceUnpairingResultStatus,
};
use windows::Foundation::TypedEventHandler;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::{Error, Result};

use crate::bt::{Repair, Shared, format_address, lock, read_value};
use crate::{adv, events};

const MICROSOFT_VID: u16 = 0x045E;

/// Runs a pairing job for `addr` (0 = whichever Xbox controller is put in pairing mode).
pub fn spawn(shared: Arc<Shared>, addr: u64) {
    let s = shared.clone();
    let spawned = std::thread::Builder::new().name("pairing".into()).spawn(move || {
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let who = if addr == 0 { "a new controller".to_string() } else { format_address(addr) };
        events::log(format!("Pairing {who}: waiting for pairing mode"));
        let result = run(&s, addr);
        *lock(&s.release) = None;
        s.rescan.store(true, Ordering::Relaxed);
        match result {
            Ok(()) => {
                events::log("Pairing succeeded");
                *lock(&s.repair) = Repair::Done;
            }
            Err(e) => {
                events::log(format!("Pairing ended: {e}"));
                *lock(&s.repair) = Repair::Failed(e);
            }
        }
    });
    if spawned.is_err() {
        *lock(&shared.repair) = Repair::Failed("Couldn't start pairing.".into());
    }
}

/// What has been heard from one address during this job.
#[derive(Default)]
struct Heard {
    discoverable: bool,
    xbox: bool,
    logged: bool,
}

fn run(shared: &Shared, addr: u64) -> std::result::Result<(), String> {
    let cancelled = || shared.repair_cancel.load(Ordering::Relaxed);
    // Scan first, so a pair-button press during the hand-off below isn't missed.
    let scanner =
        adv::Scanner::start((addr != 0).then_some(addr)).map_err(|e| format!("Couldn't scan: {}", e.message()))?;
    if addr != 0 {
        // Stop the keeper reconnecting it meanwhile; its pairing stays as it is until step 2.
        let _ = release(shared, addr); // best effort here; step 2 insists
    }

    // 1. Wait for the controller to advertise in pairing mode.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut heard: HashMap<u64, Heard> = HashMap::new();
    let mut first_candidate: Option<Instant> = None;
    let target = loop {
        if cancelled() {
            return Err("Cancelled. Nothing was changed.".into());
        }
        if Instant::now() >= deadline {
            return Err("No controller in pairing mode was found. Nothing was changed.".into());
        }
        for s in scanner.drain() {
            let h = heard.entry(s.addr).or_default();
            h.discoverable |= s.discoverable;
            h.xbox |= s.name.to_lowercase().contains("xbox");
            if addr != 0 && !h.logged {
                // Diagnostic: what the controller advertises while we wait.
                h.logged = true;
                events::log(format!(
                    "Pairing: {} advertising ({}, discoverable: {}, {} dBm)",
                    format_address(s.addr),
                    adv::kind_name(s.kind),
                    if s.discoverable { "yes" } else { "no" },
                    s.rssi
                ));
            }
        }
        let candidates: Vec<u64> = heard
            .iter()
            .filter(|(a, h)| h.discoverable && if addr != 0 { **a == addr } else { h.xbox })
            .map(|(a, _)| *a)
            .collect();
        if addr != 0 && !candidates.is_empty() {
            break addr;
        }
        if addr == 0 && !candidates.is_empty() {
            // Give a second pad in pairing mode a moment to show up, then refuse to guess.
            let since = *first_candidate.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(3) {
                if candidates.len() > 1 {
                    return Err("More than one controller is in pairing mode. Pair them one at a time.".into());
                }
                break candidates[0];
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    drop(scanner);
    events::log(format!("Pairing: {} is in pairing mode", format_address(target)));
    if cancelled() {
        return Err("Cancelled. Nothing was changed.".into());
    }

    // 2. If Windows still has a pairing for it, the controller has discarded its side; replace it.
    let dev = open(target).map_err(|e| format!("Couldn't open the controller: {}", e.message()))?;
    if dev.DeviceInformation().and_then(|i| i.Pairing()).and_then(|p| p.IsPaired()).unwrap_or(false) {
        *lock(&shared.repair) = Repair::Removing;
        if !release(shared, target) {
            return Err("Couldn't take over the controller from the reconnect loop. Nothing was changed.".into());
        }
        unpair(&dev).map_err(|e| format!("Couldn't remove the old pairing: {}", e.message()))?;
        events::log("Pairing: old pairing removed");
    }

    // 3. Pair. The pad stays in pairing mode for a while, so retry briefly if Windows isn't ready.
    *lock(&shared.repair) = Repair::Pairing;
    let deadline = Instant::now() + Duration::from_secs(30);
    let dev = loop {
        let dev = open(target).map_err(|e| format!("Couldn't open the controller: {}", e.message()))?;
        match pair(&dev) {
            Ok(()) => break dev,
            Err(e) if Instant::now() < deadline && !cancelled() => {
                events::log(format!("Pairing attempt failed: {}", e.message()));
                let _ = dev.Close();
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(e) => {
                let _ = dev.Close();
                return Err(format!("Pairing didn't complete ({}). Try again.", e.message()));
            }
        }
    };

    // Hold the link while the bond settles and the keeper takes over: dropping every handle
    // right after pairing lets Windows disconnect mid-way.
    let session = GattSession::FromDeviceIdAsync(&dev.BluetoothDeviceId().map_err(|e| e.message())?)
        .and_then(|op| op.join())
        .ok();
    if let Some(s) = &session {
        let _ = s.SetMaintainConnection(true);
    }

    // 4. A pad picked by name alone must prove it's an Xbox controller.
    if addr == 0 {
        match vendor_id(&dev) {
            Vendor::Id(MICROSOFT_VID) => {}
            // GATT can be slow right after pairing; the name and pairing mode already matched.
            Vendor::Unknown => events::log("Pairing: couldn't read the vendor id to double-check; keeping it"),
            other => {
                let what = match other {
                    Vendor::Id(vid) => format!("vendor id {vid:04X} isn't Microsoft's"),
                    _ => "it has no vendor id".into(),
                };
                events::log(format!("Pairing: {what}; removing it again"));
                let _ = unpair(&dev);
                return Err("That device isn't an Xbox controller, so it was removed again.".into());
            }
        }
    }
    shared.paired.store(target, Ordering::Release);
    *lock(&shared.release) = None;
    shared.rescan.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(10));
    if let Some(s) = session {
        let _ = s.Close();
    }
    let _ = dev.Close();
    Ok(())
}

fn open(addr: u64) -> Result<BluetoothLEDevice> {
    BluetoothLEDevice::FromBluetoothAddressAsync(addr)?.join()
}

/// Asks the keeper to let go of `addr` (session and reconnect thread) and waits until it has.
/// Returns false if it didn't confirm in time.
fn release(shared: &Shared, addr: u64) -> bool {
    *lock(&shared.release) = Some(addr);
    shared.released.store(0, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(20); // a reconnect attempt can't be cancelled
    while shared.released.load(Ordering::Acquire) != addr {
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

fn unpair(dev: &BluetoothLEDevice) -> Result<()> {
    let r = dev.DeviceInformation()?.Pairing()?.UnpairAsync()?.join()?;
    match r.Status()? {
        DeviceUnpairingResultStatus::Unpaired | DeviceUnpairingResultStatus::AlreadyUnpaired => Ok(()),
        s => Err(Error::new(E_FAIL, format!("unpairing status {}", s.0))),
    }
}

/// Pairs without Windows' own prompt. Only "just works" (ConfirmOnly) is offered and accepted.
fn pair(dev: &BluetoothLEDevice) -> Result<()> {
    let info = dev.DeviceInformation()?;
    if info.Pairing()?.IsPaired()? {
        return Ok(());
    }
    let custom = info.Pairing()?.Custom()?;
    let handler =
        TypedEventHandler::<DeviceInformationCustomPairing, DevicePairingRequestedEventArgs>::new(|_, args| {
            if let Ok(args) = args.ok()
                && args.PairingKind()? == DevicePairingKinds::ConfirmOnly
            {
                args.Accept()?;
            }
            Ok(())
        });
    let token = custom.PairingRequested(&handler)?;
    let r = custom.PairAsync(DevicePairingKinds::ConfirmOnly).and_then(|op| op.join());
    let _ = custom.RemovePairingRequested(token);
    match r?.Status()? {
        DevicePairingResultStatus::Paired | DevicePairingResultStatus::AlreadyPaired => Ok(()),
        s => Err(Error::new(E_FAIL, format!("pairing status {}", s.0))),
    }
}

enum Vendor {
    Id(u16),
    /// GATT answered, but there's no Device Information service / PnP ID: not an Xbox controller.
    Absent,
    /// Couldn't get an answer (link not ready yet, timed out).
    Unknown,
}

/// USB vendor id from the Device Information service's PnP ID (retried while GATT settles).
/// Distinguishes "the device doesn't have one" from "no answer yet", so leaving the service out
/// can't be used to skip the check.
fn vendor_id(dev: &BluetoothLEDevice) -> Vendor {
    let until = Instant::now() + Duration::from_secs(15);
    let mut absent_once = false;
    while Instant::now() < until {
        let read = (|| -> Result<Option<Vendor>> {
            let r = dev
                .GetGattServicesForUuidWithCacheModeAsync(
                    GattServiceUuids::DeviceInformation()?,
                    BluetoothCacheMode::Uncached,
                )?
                .join()?;
            if r.Status()? != GattCommunicationStatus::Success {
                return Ok(None);
            }
            let list = r.Services()?;
            if list.Size()? == 0 {
                return Ok(Some(Vendor::Absent));
            }
            let svc = list.GetAt(0)?;
            for i in 1..list.Size()? {
                let _ = list.GetAt(i)?.Close();
            }
            let found = (|| -> Result<Option<Vendor>> {
                let c = svc
                    .GetCharacteristicsForUuidWithCacheModeAsync(
                        GattCharacteristicUuids::PnpId()?,
                        BluetoothCacheMode::Uncached,
                    )?
                    .join()?;
                if c.Status()? != GattCommunicationStatus::Success {
                    return Ok(None);
                }
                if c.Characteristics()?.Size()? == 0 {
                    return Ok(Some(Vendor::Absent));
                }
                // PnP ID: source (1 byte), vendor id (u16 LE), product id, version.
                let b = read_value(&c.Characteristics()?.GetAt(0)?)?;
                Ok((b.len() >= 3).then(|| Vendor::Id(u16::from_le_bytes([b[1], b[2]]))))
            })();
            let _ = svc.Close();
            found
        })();
        match read {
            // Right after pairing the service list can briefly come back empty: only believe
            // "absent" when a second look a second later agrees.
            Ok(Some(Vendor::Absent)) if !absent_once => absent_once = true,
            Ok(Some(v)) => return v,
            _ => absent_once = false,
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Vendor::Unknown
}
