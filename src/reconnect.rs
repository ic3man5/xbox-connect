//! Actively reconnects a controller that's switched off or out of range.
//!
//! On this class of adapter Windows' background auto-connect (MaintainConnection) never catches a
//! controller that's searching for its host: right after power-on the controller calls out only
//! to its last host, and only a pending *foreground* connect hears that. So while a controller is
//! missing we keep exactly one explicit connect pending at all times — an uncached GATT discovery,
//! which Windows serves by trying to connect (~7.7 s per try) — the way Linux keeps an LE
//! connection pending for bonded HID devices. Verified: a single power-on connects in seconds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Devices::Bluetooth::GenericAttributeProfile::GattCommunicationStatus;
use windows::Devices::Bluetooth::{BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

use crate::events;

/// Handle to one controller's reconnect thread. There is at most one per controller: the keeper
/// keeps the handle until the thread has really exited, so attempts are never stacked.
pub struct Reconnector {
    want: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    /// Probing slowly (one attempt a minute), for a controller whose pairing looks lost.
    pub slow: bool,
}

impl Reconnector {
    pub fn start(addr: u64, label: String, slow: bool) -> Option<Self> {
        let want = Arc::new(AtomicBool::new(true));
        let done = Arc::new(AtomicBool::new(false));
        let (w, d) = (want.clone(), done.clone());
        std::thread::Builder::new()
            .name("reconnect".into())
            .spawn(move || {
                run(addr, &label, &w, slow);
                d.store(true, Ordering::Release);
            })
            .ok()?;
        Some(Self { want, done, slow })
    }

    /// Asks the thread to stop after its current attempt (attempts can't be cancelled).
    pub fn stop(&self) {
        self.want.store(false, Ordering::Relaxed);
    }

    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

fn run(addr: u64, label: &str, want: &AtomicBool, slow: bool) {
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let Ok(dev) = BluetoothLEDevice::FromBluetoothAddressAsync(addr).and_then(|op| op.join()) else {
        return; // the keeper starts another on its next pass
    };
    let started = Instant::now();
    let mut tries = 0u32;
    let mut reported = false;
    if slow {
        events::log(format!("{label}: retrying once a minute (pairing looks lost)"));
    } else {
        events::log(format!("{label}: reconnecting (waiting for the controller to turn on)"));
    }
    while want.load(Ordering::Relaxed) {
        // A pad that looks unpaired is still probed now and then, so a wrong verdict can't
        // switch reconnecting off for good, but it isn't hammered with refused connects.
        if slow && tries > 0 {
            for _ in 0..60 {
                if !want.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            if !want.load(Ordering::Relaxed) {
                break;
            }
        }
        if dev.ConnectionStatus().is_ok_and(|s| s == BluetoothConnectionStatus::Connected) {
            let secs = started.elapsed().as_secs();
            events::log(format!("{label}: reconnected after {secs}s ({tries} attempts)"));
            break;
        }
        tries += 1;
        let attempt = Instant::now();
        let status = dev.GetGattServicesWithCacheModeAsync(BluetoothCacheMode::Uncached).and_then(|op| op.join());
        match status.and_then(|r| r.Status()) {
            // Unreachable is the normal "nobody answered" outcome while the pad is off.
            Ok(GattCommunicationStatus::Success | GattCommunicationStatus::Unreachable) => {}
            other => {
                if !reported {
                    reported = true;
                    let what = match other {
                        Ok(s) => format!("status {}", s.0),
                        Err(e) => e.message(),
                    };
                    events::log(format!("{label}: reconnect attempt failed: {what}"));
                }
            }
        }
        // An attempt normally takes ~7.7 s; one that comes back at once (adapter gone, Windows
        // failing fast) mustn't turn this into a busy loop.
        if let Some(rest) = Duration::from_secs(2).checked_sub(attempt.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    let _ = dev.Close();
}
