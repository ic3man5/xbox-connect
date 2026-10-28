//! Background thread that keeps every paired Xbox controller's Bluetooth LE link
//! alive (GattSession.MaintainConnection) and reads battery / firmware over GATT.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattCharacteristicUuids, GattCommunicationStatus, GattDeviceService, GattServiceUuids,
    GattSession, GattSessionStatus,
};
use windows::Devices::Bluetooth::{BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Storage::Streams::DataReader;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::{Error, GUID, HSTRING, Result};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Link {
    Connecting,
    Disconnected,
    Connected,
}

#[derive(Clone, Debug)]
pub struct CtrlInfo {
    pub id: String,
    pub name: String,
    pub address: u64,
    pub link: Link,
    pub battery: Option<u8>,
    pub firmware: Option<String>,
}

#[derive(Default)]
pub struct Shared {
    pub ctrls: Mutex<Vec<CtrlInfo>>,
    pub error: Mutex<Option<String>>,
    /// Set once the first scan has finished, so the UI can tell "none paired" from "still looking".
    pub scanned: AtomicBool,
    pub maintain: AtomicBool,
    pub autostart: AtomicBool,
    pub rescan: AtomicBool,
    /// Set when the window couldn't be created; the app keeps running tray-only.
    pub ui_failed: AtomicBool,
    /// Main window HWND (0 until the window exists, or if the UI failed to start).
    pub hwnd: AtomicIsize,
}

/// Mutex poisoning can't happen with panic=abort, but don't turn it into a second panic in debug builds.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    pub fn snapshot(&self) -> Vec<CtrlInfo> {
        lock(&self.ctrls).clone()
    }
}

pub fn format_address(addr: u64) -> String {
    let b = addr.to_be_bytes();
    format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", b[2], b[3], b[4], b[5], b[6], b[7])
}

pub fn spawn(shared: std::sync::Arc<Shared>) {
    let _ = std::thread::Builder::new().name("bluetooth".into()).spawn(move || {
        if let Err(e) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            *lock(&shared.error) = Some(format!("Windows Runtime failed to start: {e}"));
            return;
        }
        Keeper { shared, ctrls: Vec::new(), firmware: Default::default(), next_scan: Instant::now(), backoff: 5 }.run();
    });
}

struct Keeper {
    shared: std::sync::Arc<Shared>,
    ctrls: Vec<Ctrl>,
    /// Firmware strings remembered across reopen, so the UI doesn't flicker.
    firmware: std::collections::HashMap<String, String>,
    next_scan: Instant,
    backoff: u64,
}

impl Keeper {
    fn run(&mut self) {
        loop {
            let now = Instant::now();
            let forced = self.shared.rescan.swap(false, Ordering::Relaxed);
            if forced || now >= self.next_scan {
                // Back off 5s -> 60s while nothing is paired (or enumeration fails); once
                // controllers are held, re-enumerate every 15s to pick up pairing changes.
                self.backoff = match self.scan() {
                    Ok(0) | Err(_) => (self.backoff * 2).clamp(5, 60),
                    Ok(_) => 15,
                };
                self.next_scan = Instant::now() + Duration::from_secs(self.backoff);
                self.shared.scanned.store(true, Ordering::Relaxed);
            }

            let maintain = self.shared.maintain.load(Ordering::Relaxed);
            for c in &mut self.ctrls {
                match c.tick(maintain) {
                    Ok(()) => c.errors = 0,
                    Err(_) => {
                        c.errors += 1;
                        c.drop_handles();
                    }
                }
            }
            // A controller that keeps failing, or can't get a session for 30s, may hold a stale
            // device object (re-pairing keeps the same id); reopen it on the next scan.
            let before = self.ctrls.len();
            let stale =
                |c: &Ctrl| c.errors >= 3 || c.no_session_since.is_some_and(|t| t.elapsed() >= Duration::from_secs(30));
            for c in self.ctrls.iter().filter(|c| stale(c)) {
                if let Some(fw) = &c.info.firmware {
                    self.firmware.insert(c.info.id.clone(), fw.clone());
                }
            }
            self.ctrls.retain(|c| !stale(c));
            if self.ctrls.len() != before {
                self.next_scan = Instant::now();
            }
            *lock(&self.shared.ctrls) = self.ctrls.iter().map(|c| c.info.clone()).collect();
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Re-enumerates paired controllers. A removed pairing disappears from the list; a re-paired
    /// controller keeps the same id, so `run` reopens devices that keep failing instead.
    fn scan(&mut self) -> Result<usize> {
        let ids = paired_controller_ids()?;
        let keep: HashSet<&String> = ids.iter().collect();
        self.ctrls.retain(|c| keep.contains(&c.info.id));
        for id in &ids {
            if self.ctrls.iter().any(|c| &c.info.id == id) {
                continue;
            }
            if let Ok(mut c) = Ctrl::open(id) {
                c.info.firmware = self.firmware.get(id).cloned();
                self.ctrls.push(c);
            }
        }
        Ok(self.ctrls.len())
    }
}

fn paired_controller_ids() -> Result<Vec<String>> {
    let selector = BluetoothLEDevice::GetDeviceSelectorFromPairingState(true)?;
    let list = DeviceInformation::FindAllAsyncAqsFilter(&selector)?.join()?;
    let mut ids = Vec::new();
    for i in 0..list.Size()? {
        let info = list.GetAt(i)?;
        if info.Name()?.to_string().to_lowercase().contains("xbox") {
            ids.push(info.Id()?.to_string());
        }
    }
    Ok(ids)
}

struct Ctrl {
    info: CtrlInfo,
    device: BluetoothLEDevice,
    session: Option<GattSession>,
    applied: Option<bool>,
    next_session_try: Instant,
    battery: Option<(GattDeviceService, GattCharacteristic)>,
    next_battery: Instant,
    battery_fails: u32,
    firmware_tries: u8,
    firmware_fresh: bool,
    next_firmware: Instant,
    errors: u32,
    no_session_since: Option<Instant>,
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        // Release OS handles deterministically rather than whenever the last reference goes.
        self.close_battery();
        if let Some(s) = self.session.take() {
            let _ = s.Close();
        }
        let _ = self.device.Close();
    }
}

impl Ctrl {
    fn open(id: &str) -> Result<Self> {
        let device = BluetoothLEDevice::FromIdAsync(&HSTRING::from(id))?.join()?;
        let info = CtrlInfo {
            id: id.to_string(),
            name: device.Name()?.to_string(),
            address: device.BluetoothAddress()?,
            link: Link::Connecting,
            battery: None,
            firmware: None,
        };
        let now = Instant::now();
        Ok(Self {
            info,
            device,
            session: None,
            applied: None,
            next_session_try: now,
            battery: None,
            next_battery: now,
            battery_fails: 0,
            firmware_tries: 0,
            firmware_fresh: false,
            next_firmware: now,
            errors: 0,
            no_session_since: None,
        })
    }

    /// Drop OS handles after an error; keep name/firmware so the UI doesn't flicker.
    fn drop_handles(&mut self) {
        if let Some(s) = self.session.take() {
            let _ = s.Close();
        }
        self.applied = None;
        self.close_battery();
        self.info.link = Link::Connecting;
        self.next_session_try = Instant::now() + Duration::from_secs(5);
    }

    fn close_battery(&mut self) {
        if let Some((svc, _)) = self.battery.take() {
            let _ = svc.Close();
        }
    }

    fn tick(&mut self, maintain: bool) -> Result<()> {
        let now = Instant::now();

        let connected = self.device.ConnectionStatus()? == BluetoothConnectionStatus::Connected;

        // Keep exactly one session holding MaintainConnection at all times. A session can stop
        // being Active after sleep/resume, an adapter reset, or simply while the pad is off; we
        // then create a replacement *before* closing the old one, so there's never a gap where
        // nothing asks Windows to reconnect. (Slower cadence while the pad is off.)
        let status = match &self.session {
            Some(s) => Some(s.SessionStatus()?),
            None => None,
        };
        let replace = status != Some(GattSessionStatus::Active);
        if replace && now >= self.next_session_try {
            self.next_session_try = now + Duration::from_secs(if connected || status.is_none() { 5 } else { 15 });
            // A failed replacement must not cost us the session we already have.
            let fresh = (|| -> Result<GattSession> {
                let s = GattSession::FromDeviceIdAsync(&self.device.BluetoothDeviceId()?)?.join()?;
                if let Err(e) = s.SetMaintainConnection(maintain) {
                    let _ = s.Close();
                    return Err(e);
                }
                Ok(s)
            })();
            match fresh {
                Ok(fresh) => {
                    if let Some(old) = self.session.replace(fresh) {
                        let _ = old.Close();
                    }
                    self.applied = Some(maintain);
                }
                // Only an error worth escalating if we have nothing holding the link.
                Err(e) if self.session.is_none() => return Err(e),
                Err(_) => {}
            }
        }
        if self.session.is_none() {
            self.no_session_since.get_or_insert(now);
        } else {
            self.no_session_since = None;
        }
        if let Some(s) = &self.session
            && self.applied != Some(maintain)
        {
            s.SetMaintainConnection(maintain)?;
            self.applied = Some(maintain);
        }
        if !connected {
            self.info.link = Link::Disconnected;
            self.info.battery = None; // don't show a stale reading
            self.close_battery();
            self.next_battery = now;
            self.battery_fails = 0;
            self.firmware_tries = 0;
            self.firmware_fresh = false;
            self.next_firmware = now;
            return Ok(());
        }
        self.info.link = Link::Connected;

        if now >= self.next_battery {
            match self.read_battery() {
                Ok(level) => {
                    self.info.battery = Some(level.min(100));
                    self.battery_fails = 0;
                    self.next_battery = now + Duration::from_secs(60);
                }
                Err(_) => {
                    // GATT is often not ready right after reconnecting: retry soon, then back
                    // off (5s, 10s, 20s, 40s, 60s) so a pad without the service isn't polled hard.
                    self.close_battery();
                    self.next_battery = now + Duration::from_secs((5u64 << self.battery_fails.min(4)).min(60));
                    self.battery_fails += 1;
                }
            }
        }
        // Firmware: a few attempts per connection, since GATT may not be ready right away.
        // (A remembered value is shown meanwhile; one fresh read per connection catches updates.)
        if !self.firmware_fresh && self.firmware_tries < 5 && now >= self.next_firmware {
            self.firmware_tries += 1;
            self.next_firmware = now + Duration::from_secs(5);
            if let Some(fw) = read_firmware(&self.device).ok().filter(|s| !s.is_empty()) {
                self.info.firmware = Some(fw);
                self.firmware_fresh = true;
            }
        }
        Ok(())
    }

    fn read_battery(&mut self) -> Result<u8> {
        if self.battery.is_none() {
            let svc = open_service(&self.device, GattServiceUuids::Battery()?)?;
            match characteristic(&svc, GattCharacteristicUuids::BatteryLevel()?) {
                Ok(ch) => self.battery = Some((svc, ch)),
                Err(e) => {
                    let _ = svc.Close();
                    return Err(e);
                }
            }
        }
        let (_, ch) = self.battery.as_ref().unwrap();
        read_value(ch)?.first().copied().ok_or_else(fail)
    }
}

fn fail() -> Error {
    Error::from(E_FAIL)
}

/// Service and characteristic discovery try the system cache first (no radio traffic) and fall
/// back to an uncached lookup if the cache is empty or stale, e.g. right after re-pairing.
fn open_service(dev: &BluetoothLEDevice, uuid: GUID) -> Result<GattDeviceService> {
    let lookup = |mode| -> Result<GattDeviceService> {
        let r = dev.GetGattServicesForUuidWithCacheModeAsync(uuid, mode)?.join()?;
        if r.Status()? != GattCommunicationStatus::Success {
            return Err(fail());
        }
        let list = r.Services()?;
        let svc = if list.Size()? > 0 { Some(list.GetAt(0)?) } else { None };
        for i in 1..list.Size()? {
            let _ = list.GetAt(i)?.Close();
        }
        svc.ok_or_else(fail)
    };
    lookup(BluetoothCacheMode::Cached).or_else(|_| lookup(BluetoothCacheMode::Uncached))
}

fn characteristic(svc: &GattDeviceService, uuid: GUID) -> Result<GattCharacteristic> {
    let lookup = |mode| -> Result<GattCharacteristic> {
        let r = svc.GetCharacteristicsForUuidWithCacheModeAsync(uuid, mode)?.join()?;
        if r.Status()? != GattCommunicationStatus::Success || r.Characteristics()?.Size()? == 0 {
            return Err(fail());
        }
        r.Characteristics()?.GetAt(0)
    };
    lookup(BluetoothCacheMode::Cached).or_else(|_| lookup(BluetoothCacheMode::Uncached))
}

fn read_value(ch: &GattCharacteristic) -> Result<Vec<u8>> {
    let r = ch.ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached)?.join()?;
    if r.Status()? != GattCommunicationStatus::Success {
        return Err(fail());
    }
    let reader = DataReader::FromBuffer(&r.Value()?)?;
    let mut bytes = vec![0u8; reader.UnconsumedBufferLength()? as usize];
    reader.ReadBytes(&mut bytes)?;
    Ok(bytes)
}

fn read_firmware(dev: &BluetoothLEDevice) -> Result<String> {
    let svc = open_service(dev, GattServiceUuids::DeviceInformation()?)?;
    let result = (|| -> Result<String> {
        let bytes = read_value(&characteristic(&svc, GattCharacteristicUuids::FirmwareRevisionString()?)?)?;
        Ok(String::from_utf8_lossy(&bytes).trim_end_matches('\0').trim().to_string())
    })();
    let _ = svc.Close();
    result
}
