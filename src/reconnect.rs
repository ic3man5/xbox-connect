//! Actively reconnects a controller that's switched off or out of range.
//!
//! On this class of adapter Windows' background auto-connect (MaintainConnection) never catches a
//! controller that's searching for its host: right after power-on the controller calls out only
//! to its last host, and only a pending *foreground* connect hears that. So while a controller is
//! missing we keep exactly one explicit connect pending at all times — an uncached GATT discovery,
//! which Windows serves by trying to connect (~7.7 s per try) — the way Linux keeps an LE
//! connection pending for bonded HID devices. Verified: a single power-on connects in seconds.
//!
//! That keeps the radio busy, and on combo Wi-Fi/Bluetooth chips it can cost ping, so the user can
//! have it slow down or pause while a fullscreen game runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Devices::Bluetooth::GenericAttributeProfile::GattCommunicationStatus;
use windows::Devices::Bluetooth::{BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::Shell::{QUNS_BUSY, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState};

use crate::bt::{Fullscreen, Shared};
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
    pub fn start(addr: u64, label: String, slow: bool, shared: Arc<Shared>) -> Option<Self> {
        let want = Arc::new(AtomicBool::new(true));
        let done = Arc::new(AtomicBool::new(false));
        let (w, d) = (want.clone(), done.clone());
        std::thread::Builder::new()
            .name("reconnect".into())
            .spawn(move || {
                run(addr, &label, &w, slow, &shared);
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

/// A game (or other app) is running fullscreen: exclusive (D3D) or borderless/presentation mode.
fn fullscreen_app() -> bool {
    fullscreen_kind().is_some()
}

/// Which kind of fullscreen app is running, if any (for the log).
pub fn fullscreen_kind() -> Option<&'static str> {
    match unsafe { SHQueryUserNotificationState() } {
        Ok(QUNS_RUNNING_D3D_FULL_SCREEN) => Some("exclusive fullscreen"),
        Ok(QUNS_BUSY) => Some("borderless fullscreen or presentation"),
        _ => None,
    }
}

fn run(addr: u64, label: &str, want: &AtomicBool, slow: bool, shared: &Shared) {
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let Ok(dev) = BluetoothLEDevice::FromBluetoothAddressAsync(addr).and_then(|op| op.join()) else {
        return; // the keeper starts another on its next pass
    };
    let started = Instant::now();
    let mut tries = 0u32;
    let mut reported = false;
    let mut next = Instant::now(); // when the next attempt may start
    let mut gaming = Fullscreen::Normal; // what the fullscreen setting is doing right now
    if slow {
        events::log(format!("{label}: retrying once a minute (pairing looks lost)"));
    } else {
        events::log(format!("{label}: reconnecting (waiting for the controller to turn on)"));
    }
    while want.load(Ordering::Relaxed) {
        if dev.ConnectionStatus().is_ok_and(|s| s == BluetoothConnectionStatus::Connected) {
            let secs = started.elapsed().as_secs();
            // With no attempt of ours made (e.g. paused while gaming), Windows' own background
            // reconnect did it through the keeper's MaintainConnection session; say so.
            events::log(if tries == 0 {
                format!("{label}: reconnected by Windows on its own after {secs}s")
            } else {
                format!("{label}: reconnected after {secs}s ({tries} attempts)")
            });
            break;
        }

        // The fullscreen setting, applied only while a fullscreen app is actually running.
        let setting = Fullscreen::from_u8(shared.fullscreen.load(Ordering::Relaxed));
        let now_gaming = if setting != Fullscreen::Normal && fullscreen_app() { setting } else { Fullscreen::Normal };
        if now_gaming != gaming {
            events::log(match now_gaming {
                Fullscreen::Pause => format!("{label}: fullscreen game running; reconnecting paused"),
                Fullscreen::Slow => format!("{label}: fullscreen game running; reconnecting every 20s"),
                Fullscreen::Normal => {
                    next = Instant::now(); // back to normal: try straight away
                    if setting == Fullscreen::Normal {
                        format!("{label}: fullscreen setting changed; reconnecting normally")
                    } else {
                        format!("{label}: fullscreen ended; reconnecting normally")
                    }
                }
            });
            gaming = now_gaming;
        }

        let now = Instant::now();
        if gaming == Fullscreen::Pause || now < next {
            // Wait in short steps, so stopping, a setting change or the game ending is seen quickly.
            let step = if gaming == Fullscreen::Pause {
                Duration::from_secs(1) // stay quiet while the game runs
            } else {
                next.saturating_duration_since(now).clamp(Duration::from_millis(100), Duration::from_secs(1))
            };
            std::thread::sleep(step);
            continue;
        }

        tries += 1;
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
        // Normally the next attempt follows at once (each takes ~7.7 s), but never sooner than 2 s
        // after the last one started (one that fails instantly mustn't become a busy loop). A pad
        // whose pairing looks lost is probed once a minute; "slow down" while gaming, every 20 s.
        let gap = if slow {
            60
        } else if gaming == Fullscreen::Slow {
            20
        } else {
            2
        };
        next = now + Duration::from_secs(gap);
    }
    let _ = dev.Close();
}
