//! Background thread that keeps every paired Xbox controller's Bluetooth LE link alive and
//! reads battery / firmware over GATT. Each controller gets one long-lived GattSession with
//! MaintainConnection, plus an active reconnect loop while it's missing (see reconnect.rs).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattCharacteristicUuids, GattCommunicationStatus, GattDeviceService, GattServiceUuids,
    GattSession,
};
use windows::Devices::Bluetooth::{BluetoothAdapter, BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Devices::Radios::{Radio, RadioState};
use windows::Storage::Streams::DataReader;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::System::WindowsProgramming::{QueryInterruptTime, QueryUnbiasedInterruptTime};
use windows::core::{Error, GUID, HSTRING, Result};

use crate::reconnect::Reconnector;
use crate::{events, pair};

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
    /// The controller keeps connecting and getting refused before GATT works: it has most likely
    /// forgotten this PC, and only re-pairing (its pair button) can fix that.
    pub pairing_lost: bool,
}

/// Pairing progress, driven by the pairing thread and shown by the UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Repair {
    #[default]
    Idle,
    /// The UI asked to (re-)pair this address (0 = any Xbox controller in pairing mode).
    Requested(u64),
    /// Waiting for the controller to be put in pairing mode; nothing has been changed yet.
    Waiting,
    Removing,
    Pairing,
    Done,
    Failed(String),
}

impl Repair {
    /// Something is in progress (as opposed to idle or showing a result).
    pub fn busy(&self) -> bool {
        !matches!(self, Repair::Idle | Repair::Done | Repair::Failed(_))
    }
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
    pub repair: Mutex<Repair>,
    pub repair_cancel: AtomicBool,
    /// A pairing job wants the keeper to let go of this controller (session and reconnects)...
    pub release: Mutex<Option<u64>>,
    /// ...and the keeper confirms here, once nothing of its own touches that address any more.
    pub released: AtomicU64,
    /// A pairing job just (re-)paired this address: forget any old pairing-loss evidence for it.
    pub paired: AtomicU64,
    /// The window is showing the Events tab, so input changes needn't trigger repaints.
    pub events_tab: AtomicBool,
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
        Keeper {
            shared,
            ctrls: Vec::new(),
            memory: HashMap::new(),
            reconnectors: HashMap::new(),
            next_scan: Instant::now(),
            backoff: 5,
            last_count: None,
            radio: None,
            radio_on: true,
            next_radio: Instant::now(),
        }
        .run();
    });
}

/// What survives reopening a controller, so the UI doesn't flicker and pairing-loss evidence
/// isn't thrown away by exactly the resets a bad pairing tends to cause.
#[derive(Default, Clone)]
struct Memory {
    address: u64,
    firmware: Option<String>,
    refused: u32,
    pairing_lost: bool,
}

struct Keeper {
    shared: std::sync::Arc<Shared>,
    ctrls: Vec<Ctrl>,
    memory: HashMap<String, Memory>,
    reconnectors: HashMap<u64, Reconnector>,
    next_scan: Instant,
    backoff: u64,
    last_count: Option<usize>,
    radio: Option<Radio>,
    radio_on: bool,
    next_radio: Instant,
}

/// Suspend-aware clocks: interrupt time includes time spent asleep, unbiased time doesn't.
fn clocks() -> (u64, u64) {
    let mut unbiased = 0u64;
    unsafe {
        let _ = QueryUnbiasedInterruptTime(&mut unbiased);
        (QueryInterruptTime(), unbiased)
    }
}

impl Keeper {
    fn run(&mut self) {
        events::log(format!("Xbox Connect v{} started", env!("CARGO_PKG_VERSION")));
        let mut last = clocks();
        loop {
            // A pairing request from the UI: claim it here (so it's started exactly once) and run
            // it on its own thread; this loop keeps serving the other controllers meanwhile.
            {
                let mut r = lock(&self.shared.repair);
                if let Repair::Requested(addr) = *r {
                    *r = Repair::Waiting;
                    drop(r);
                    pair::spawn(self.shared.clone(), addr);
                }
            }

            // Sessions are held for as long as possible. Start over only when the Bluetooth stack
            // itself went away: the radio was turned back on, or the PC was suspended.
            let now = clocks();
            let asleep = (now.0.saturating_sub(last.0)).saturating_sub(now.1.saturating_sub(last.1));
            last = now;
            if asleep > 50_000_000 {
                // 100 ns units: more than 5 s spent suspended since the last pass.
                self.reset("PC resumed from sleep");
            } else if self.radio_turned_on() {
                self.reset("Bluetooth turned back on");
            }

            let paired = self.shared.paired.swap(0, Ordering::AcqRel);
            if paired != 0 {
                self.memory.retain(|_, m| m.address != paired);
                for c in self.ctrls.iter_mut().filter(|c| c.info.address == paired) {
                    c.refused = 0;
                    c.info.pairing_lost = false;
                }
            }

            let released = *lock(&self.shared.release);
            if let Some(addr) = released {
                self.forget(|c| c.info.address == addr);
            }

            let maintain = self.shared.maintain.load(Ordering::Relaxed);
            for c in &mut self.ctrls {
                match c.tick(maintain) {
                    Ok(()) => {
                        c.errors = 0;
                        c.last_error = None;
                    }
                    Err(e) => {
                        let msg = e.message();
                        if c.last_error.as_deref() != Some(msg.as_str()) {
                            events::log(format!("{}: error: {msg}", c.label()));
                            c.last_error = Some(msg);
                        }
                        c.errors += 1;
                        c.drop_handles();
                    }
                }
            }
            // A controller that keeps failing, or can't get a session for 30s, may hold a stale
            // device object (re-pairing keeps the same id); reopen it right away.
            let stale =
                |c: &Ctrl| c.errors >= 3 || c.no_session_since.is_some_and(|t| t.elapsed() >= Duration::from_secs(30));
            let forced = self.shared.rescan.swap(false, Ordering::Relaxed) | self.forget(stale);
            if forced || Instant::now() >= self.next_scan {
                // Back off 5s -> 60s while nothing is paired (or enumeration fails); once
                // controllers are held, re-enumerate every 15s to pick up pairing changes.
                self.backoff = match self.scan(released) {
                    Ok(0) | Err(_) => (self.backoff * 2).clamp(5, 60),
                    Ok(_) => 15,
                };
                self.next_scan = Instant::now() + Duration::from_secs(self.backoff);
                self.shared.scanned.store(true, Ordering::Relaxed);
            }

            self.update_reconnectors(maintain, released);
            *lock(&self.shared.ctrls) = self.ctrls.iter().map(|c| c.info.clone()).collect();
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Drops the matching controllers (closing their sessions), remembering what should survive.
    /// Returns whether any were dropped.
    fn forget(&mut self, which: impl Fn(&Ctrl) -> bool) -> bool {
        let before = self.ctrls.len();
        for c in self.ctrls.iter().filter(|c| which(c)) {
            self.memory.insert(c.info.id.clone(), c.memory());
        }
        self.ctrls.retain(|c| !which(c));
        self.ctrls.len() != before
    }

    /// Drops every controller and rescans right away.
    fn reset(&mut self, why: &str) {
        events::log(format!("{why}: reopening controllers"));
        self.forget(|_| true);
        // A fresh start for the stack is a fresh start for the pairing-loss verdict too.
        for m in self.memory.values_mut() {
            m.refused = 0;
            m.pairing_lost = false;
        }
        for r in self.reconnectors.values() {
            r.stop();
        }
        self.next_scan = Instant::now();
    }

    /// Keeps one reconnect loop running per controller that should be held but isn't connected.
    /// Only for controllers with a keeper session (that session holds the link once it's up), and
    /// only while the radio is on. One whose pairing looks lost is only probed slowly: it would
    /// mostly just be refused, but a wrong verdict mustn't switch reconnecting off for good.
    fn update_reconnectors(&mut self, maintain: bool, released: Option<u64>) {
        let wanted: HashMap<u64, (String, bool)> = self
            .ctrls
            .iter()
            .filter(|c| maintain && self.radio_on && c.session.is_some() && c.info.link != Link::Connected)
            .map(|c| (c.info.address, (c.label(), c.info.pairing_lost)))
            .collect();
        self.reconnectors.retain(|addr, r| {
            // Stop it if it's no longer wanted, or wanted at the other pace (restarted once done).
            if wanted.get(addr).is_none_or(|(_, slow)| *slow != r.slow) {
                r.stop();
            }
            !r.finished()
        });
        for (addr, (label, slow)) in wanted {
            if !self.reconnectors.contains_key(&addr)
                && let Some(r) = Reconnector::start(addr, label, slow)
            {
                self.reconnectors.insert(addr, r);
            }
        }
        // Tell a waiting pairing job once nothing of ours touches its controller any more.
        if let Some(addr) = released
            && !self.reconnectors.contains_key(&addr)
            && !self.ctrls.iter().any(|c| c.info.address == addr)
        {
            self.shared.released.store(addr, Ordering::Release);
        }
    }

    /// True on the tick where the Bluetooth radio goes from off to on.
    fn radio_turned_on(&mut self) -> bool {
        if self.radio.is_none() && Instant::now() >= self.next_radio {
            self.next_radio = Instant::now() + Duration::from_secs(30);
            self.radio = (|| BluetoothAdapter::GetDefaultAsync()?.join()?.GetRadioAsync()?.join())().ok();
        }
        let Some(radio) = &self.radio else { return false };
        match radio.State() {
            Ok(state) => {
                let on = state == RadioState::On;
                let turned_on = on && !self.radio_on;
                if on != self.radio_on && !on {
                    events::log("Bluetooth turned off");
                }
                self.radio_on = on;
                turned_on
            }
            Err(_) => {
                self.radio = None;
                false
            }
        }
    }

    /// Re-enumerates paired controllers. A removed pairing disappears from the list; a re-paired
    /// controller keeps the same id, so `run` reopens devices that keep failing instead.
    fn scan(&mut self, released: Option<u64>) -> Result<usize> {
        let ids = paired_controller_ids()?;
        if self.last_count != Some(ids.len()) {
            events::log(format!("{} paired controller(s)", ids.len()));
            self.last_count = Some(ids.len());
        }
        let keep: HashSet<&String> = ids.iter().collect();
        self.forget(|c| !keep.contains(&c.info.id));
        for id in &ids {
            if self.ctrls.iter().any(|c| &c.info.id == id) {
                continue;
            }
            if let Ok(mut c) = Ctrl::open(id) {
                if released == Some(c.info.address) {
                    continue; // a pairing job owns it right now
                }
                if let Some(m) = self.memory.get(id) {
                    c.restore(m);
                }
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
        if info.Name().is_ok_and(|n| n.to_string().to_lowercase().contains("xbox")) {
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
    last_error: Option<String>,
    no_session_since: Option<Instant>,
    /// Link tracking for the log and for spotting a lost pairing.
    was_connected: bool,
    connected_at: Option<Instant>,
    /// A GATT read worked during this connection, i.e. the link is encrypted and usable.
    gatt_ok: bool,
    /// A GATT read was tried during this connection and failed.
    gatt_failed: bool,
    /// Consecutive short connections where GATT was tried and never worked.
    refused: u32,
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
            pairing_lost: false,
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
            last_error: None,
            no_session_since: None,
            was_connected: false,
            connected_at: None,
            gatt_ok: false,
            gatt_failed: false,
            refused: 0,
        })
    }

    fn memory(&self) -> Memory {
        Memory {
            address: self.info.address,
            firmware: self.info.firmware.clone(),
            refused: self.refused,
            pairing_lost: self.info.pairing_lost,
        }
    }

    fn restore(&mut self, m: &Memory) {
        self.info.firmware = m.firmware.clone();
        self.refused = m.refused;
        self.info.pairing_lost = m.pairing_lost;
    }

    fn label(&self) -> String {
        format!("{} {}", self.info.name, format_address(self.info.address))
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

    fn gatt_worked(&mut self) {
        if !self.gatt_ok {
            self.gatt_ok = true;
            self.refused = 0;
            if self.info.pairing_lost {
                self.info.pairing_lost = false;
                events::log(format!("{}: pairing works again", self.label()));
            }
        }
    }

    fn track_link(&mut self, connected: bool, now: Instant) {
        if connected == self.was_connected {
            return;
        }
        self.was_connected = connected;
        if connected {
            self.connected_at = Some(now);
            self.gatt_ok = false;
            self.gatt_failed = false;
            events::log(format!("{}: connected", self.label()));
            return;
        }
        let held = self.connected_at.take().map_or(0, |t| now.duration_since(t).as_secs());
        if self.gatt_failed && !self.gatt_ok && held < 30 {
            // Windows reached the controller, GATT was tried and refused, and the controller hung
            // up: the classic sign of it having dropped its pairing keys for this PC. (A short
            // session where nothing was tried, e.g. switched off again quickly, doesn't count.)
            self.refused += 1;
            events::log(format!(
                "{}: disconnected after {held}s without a working link ({} in a row)",
                self.label(),
                self.refused
            ));
            if self.refused >= 2 && !self.info.pairing_lost {
                self.info.pairing_lost = true;
                events::log(format!("{}: pairing looks lost; re-pair needed", self.label()));
            }
        } else {
            events::log(format!("{}: disconnected after {held}s", self.label()));
        }
    }

    fn tick(&mut self, maintain: bool) -> Result<()> {
        let now = Instant::now();

        let connected = self.device.ConnectionStatus()? == BluetoothConnectionStatus::Connected;
        self.track_link(connected, now);

        // Hold one session for the life of this device object. Its status reads Closed whenever
        // the controller is off; it's never replaced just for that (reconnect.rs does the work).
        if self.session.is_none() && now >= self.next_session_try {
            self.next_session_try = now + Duration::from_secs(5);
            let s = GattSession::FromDeviceIdAsync(&self.device.BluetoothDeviceId()?)?.join()?;
            if let Err(e) = s.SetMaintainConnection(maintain) {
                let _ = s.Close();
                return Err(e);
            }
            self.session = Some(s);
            self.applied = Some(maintain);
            let state = if maintain { "on" } else { "off" };
            events::log(format!("{}: holding connection (auto-reconnect: {state})", self.label()));
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
                    self.gatt_worked();
                }
                Err(_) => {
                    // GATT is often not ready right after reconnecting: retry soon, then back
                    // off (5s, 10s, 20s, 40s, 60s) so a pad without the service isn't polled hard.
                    // Only a failure once the link has had time to settle counts as a refusal.
                    if self.connected_at.is_some_and(|t| now.duration_since(t) >= Duration::from_secs(5)) {
                        self.gatt_failed = true;
                    }
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
                self.gatt_worked();
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
pub fn open_service(dev: &BluetoothLEDevice, uuid: GUID) -> Result<GattDeviceService> {
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

pub fn characteristic(svc: &GattDeviceService, uuid: GUID) -> Result<GattCharacteristic> {
    let lookup = |mode| -> Result<GattCharacteristic> {
        let r = svc.GetCharacteristicsForUuidWithCacheModeAsync(uuid, mode)?.join()?;
        if r.Status()? != GattCommunicationStatus::Success || r.Characteristics()?.Size()? == 0 {
            return Err(fail());
        }
        r.Characteristics()?.GetAt(0)
    };
    lookup(BluetoothCacheMode::Cached).or_else(|_| lookup(BluetoothCacheMode::Uncached))
}

pub fn read_value(ch: &GattCharacteristic) -> Result<Vec<u8>> {
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
