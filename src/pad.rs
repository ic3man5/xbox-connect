//! Live controller input.
//! - Windows.Gaming.Input `Gamepad` for buttons, sticks, triggers, Elite paddles and battery.
//! - `RawGameController` for the Share button (not exposed by `Gamepad`).
//! - xinput1_4 ordinal 100 (XInputGetStateEx) for the Guide button (which nothing public reports),
//!   and as the input source while our window is in the background (WGI is focus-gated).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::Gaming::Input::{
    GameControllerButtonLabel, GameControllerSwitchPosition, Gamepad, GamepadReading, RawGameController,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::Win32::UI::Input::XboxController::XINPUT_STATE;
use windows::core::{PCSTR, w};

// Bit values match Windows.Gaming.Input.GamepadButtons; GUIDE and SHARE are ours.
pub const MENU: u32 = 1 << 0;
pub const VIEW: u32 = 1 << 1;
pub const A: u32 = 1 << 2;
pub const B: u32 = 1 << 3;
pub const X: u32 = 1 << 4;
pub const Y: u32 = 1 << 5;
pub const DPAD_UP: u32 = 1 << 6;
pub const DPAD_DOWN: u32 = 1 << 7;
pub const DPAD_LEFT: u32 = 1 << 8;
pub const DPAD_RIGHT: u32 = 1 << 9;
pub const LB: u32 = 1 << 10;
pub const RB: u32 = 1 << 11;
pub const LS: u32 = 1 << 12;
pub const RS: u32 = 1 << 13;
pub const PADDLES: [u32; 4] = [1 << 14, 1 << 15, 1 << 16, 1 << 17];
pub const GUIDE: u32 = 1 << 20;
pub const SHARE: u32 = 1 << 21;

#[derive(Clone, Default)]
pub struct Pad {
    pub connected: bool,
    pub name: String,
    pub count: usize,
    pub buttons: u32,
    pub lt: f32,
    pub rt: f32,
    pub left: (f32, f32),
    pub right: (f32, f32),
    pub battery: Option<u8>,
    pub has_share: bool,
}

impl Pad {
    pub fn down(&self, mask: u32) -> bool {
        self.buttons & mask != 0
    }
}

struct Tracked {
    pad: Gamepad,
    raw: Option<RawRead>,
    name: String,
    last: GamepadReading,
    /// Reading at the last detected activity; slow stick/trigger travel accumulates against it.
    anchor: GamepadReading,
    last_active: Instant,
}

/// RawGameController with cached counts and buffers, used only for the Share button.
struct RawRead {
    raw: RawGameController,
    share: usize,
    buttons: Vec<bool>,
    switches: Vec<GameControllerSwitchPosition>,
    axes: Vec<f64>,
}

impl RawRead {
    fn new(raw: RawGameController) -> Option<Self> {
        let share = find_share(&raw)?;
        let (nb, ns, na) = (raw.ButtonCount().ok()?, raw.SwitchCount().ok()?, raw.AxisCount().ok()?);
        Some(Self {
            raw,
            share,
            buttons: vec![false; nb.max(0) as usize],
            switches: vec![GameControllerSwitchPosition::default(); ns.max(0) as usize],
            axes: vec![0.0; na.max(0) as usize],
        })
    }

    fn share_down(&mut self) -> bool {
        self.raw.GetCurrentReading(&mut self.buttons, &mut self.switches, &mut self.axes).is_ok()
            && self.buttons.get(self.share).copied().unwrap_or(false)
    }
}

pub struct Poller {
    tracked: Vec<Tracked>,
    list_at: Option<Instant>,
    battery: Option<u8>,
    battery_at: Option<Instant>,
    battery_pad: Option<Gamepad>,
    slots: Vec<u32>,
    slots_at: Option<Instant>,
}

impl Poller {
    pub fn new() -> Self {
        Self {
            tracked: Vec::new(),
            list_at: None,
            battery: None,
            battery_at: None,
            battery_pad: None,
            slots: Vec::new(),
            slots_at: None,
        }
    }

    /// `foreground`: whether our window has focus. Windows.Gaming.Input only delivers input to
    /// the foreground app, so when we're in the background we read XInput instead.
    pub fn poll(&mut self, foreground: bool) -> Pad {
        let now = Instant::now();
        if self.list_at.is_none_or(|t| now.duration_since(t) > Duration::from_millis(250)) {
            self.list_at = Some(now);
            self.sync_list();
        }
        for t in &mut self.tracked {
            if let Ok(r) = t.pad.GetCurrentReading() {
                if changed(&t.anchor, &r) {
                    t.anchor = r;
                    t.last_active = now;
                }
                t.last = r;
            }
        }
        let xinput = self.read_slots();
        let guide = xinput.iter().any(|s| s.state.Gamepad.wButtons.0 & 0x0400 != 0);

        // With several pads, show whichever one was used most recently.
        let selected = (0..self.tracked.len()).max_by_key(|&i| self.tracked[i].last_active);
        let Some(i) = selected else {
            self.battery = None;
            self.battery_at = None;
            self.battery_pad = None;
            // No WGI pad (yet): fall back to XInput alone.
            return xinput.first().map_or_else(Pad::default, |s| {
                let mut p = from_xinput(&s.state.Gamepad);
                p.name = "Xbox Controller".into();
                p.count = xinput.len();
                p
            });
        };

        if self.battery_pad.as_ref() != Some(&self.tracked[i].pad)
            || self.battery_at.is_none_or(|at| now.duration_since(at) > Duration::from_secs(5))
        {
            self.battery_at = Some(now);
            self.battery_pad = Some(self.tracked[i].pad.clone());
            self.battery = battery_percent(&self.tracked[i].pad);
        }

        let count = self.tracked.len();
        let t = &mut self.tracked[i];
        // In the background WGI (and RawGameController) readings freeze; XInput keeps working.
        // Only used with exactly one pad on each side, since XInput slots can't be matched to
        // WGI pads. XInput has no Share or paddle bits, so those read as released meanwhile.
        let background = !foreground && count == 1 && xinput.len() == 1;
        let share = !background && t.raw.as_mut().is_some_and(|r| r.share_down());
        let mut pad = match xinput.first() {
            Some(s) if background => from_xinput(&s.state.Gamepad),
            _ => {
                let r = &t.last;
                Pad {
                    connected: true,
                    buttons: r.Buttons.0 & 0x3FFFF,
                    lt: r.LeftTrigger as f32,
                    rt: r.RightTrigger as f32,
                    left: deadzone(r.LeftThumbstickX, r.LeftThumbstickY),
                    right: deadzone(r.RightThumbstickX, r.RightThumbstickY),
                    ..Pad::default()
                }
            }
        };
        if share {
            pad.buttons |= SHARE;
        }
        if guide {
            pad.buttons |= GUIDE;
        }
        pad.name = t.name.clone();
        pad.count = count;
        pad.battery = self.battery;
        pad.has_share = t.raw.is_some();
        pad
    }

    /// Track the current Gamepad list, keeping per-pad state for pads we already know.
    fn sync_list(&mut self) {
        let Ok(list) = Gamepad::Gamepads() else { return };
        let n = list.Size().unwrap_or(0);
        let pads: Vec<Gamepad> = (0..n).filter_map(|i| list.GetAt(i).ok()).collect();
        self.tracked.retain(|t| pads.contains(&t.pad));
        for pad in pads {
            if self.tracked.iter().any(|t| t.pad == pad) {
                continue;
            }
            let raw = RawGameController::FromGameController(&pad).ok();
            let name = raw
                .as_ref()
                .and_then(|r| r.DisplayName().ok())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty() && !s.to_lowercase().starts_with("hid-"))
                .unwrap_or_else(|| "Xbox Controller".into());
            let last = pad.GetCurrentReading().unwrap_or_default();
            let raw = raw.and_then(RawRead::new);
            self.tracked.push(Tracked { pad, raw, name, last, anchor: last, last_active: Instant::now() });
        }
    }

    /// Reads every connected XInput slot. Empty slots are slow to query, so the set of
    /// connected slots is only rescanned every 2s.
    fn read_slots(&mut self) -> Vec<StateEx> {
        let Some(get) = get_state_ex() else { return Vec::new() };
        if self.slots_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.slots_at = Some(Instant::now());
            self.slots = (0..4).filter(|&i| read_ex(get, i).is_some()).collect();
        }
        let mut out = Vec::with_capacity(self.slots.len());
        self.slots.retain(|&i| match read_ex(get, i) {
            Some(st) => {
                out.push(st);
                true
            }
            None => false,
        });
        out
    }
}

/// Maps an XInput gamepad state onto our bit layout (which follows WGI's GamepadButtons).
fn from_xinput(g: &windows::Win32::UI::Input::XboxController::XINPUT_GAMEPAD) -> Pad {
    let w = g.wButtons.0;
    let map = [
        (0x0001, DPAD_UP),
        (0x0002, DPAD_DOWN),
        (0x0004, DPAD_LEFT),
        (0x0008, DPAD_RIGHT),
        (0x0010, MENU),
        (0x0020, VIEW),
        (0x0040, LS),
        (0x0080, RS),
        (0x0100, LB),
        (0x0200, RB),
        (0x0400, GUIDE),
        (0x1000, A),
        (0x2000, B),
        (0x4000, X),
        (0x8000, Y),
    ];
    let axis = |v: i16| (v as f64 / 32767.0).clamp(-1.0, 1.0);
    Pad {
        connected: true,
        buttons: map.iter().filter(|(x, _)| w & x != 0).fold(0, |acc, (_, b)| acc | b),
        lt: g.bLeftTrigger as f32 / 255.0,
        rt: g.bRightTrigger as f32 / 255.0,
        left: deadzone(axis(g.sThumbLX), axis(g.sThumbLY)),
        right: deadzone(axis(g.sThumbRX), axis(g.sThumbRY)),
        count: 1,
        ..Pad::default()
    }
}

fn changed(a: &GamepadReading, b: &GamepadReading) -> bool {
    const T: f64 = 0.2;
    a.Buttons != b.Buttons
        || (a.LeftTrigger - b.LeftTrigger).abs() > T
        || (a.RightTrigger - b.RightTrigger).abs() > T
        || (a.LeftThumbstickX - b.LeftThumbstickX).abs() > T
        || (a.LeftThumbstickY - b.LeftThumbstickY).abs() > T
        || (a.RightThumbstickX - b.RightThumbstickX).abs() > T
        || (a.RightThumbstickY - b.RightThumbstickY).abs() > T
}

fn same(a: &Pad, b: &Pad) -> bool {
    const E: f32 = 0.01;
    let close = |x: f32, y: f32| (x - y).abs() < E;
    a.connected == b.connected
        && a.buttons == b.buttons
        && a.battery == b.battery
        && a.count == b.count
        && a.name == b.name
        && close(a.lt, b.lt)
        && close(a.rt, b.rt)
        && close(a.left.0, b.left.0)
        && close(a.left.1, b.left.1)
        && close(a.right.0, b.right.0)
        && close(a.right.1, b.right.1)
}

/// Polls input on its own thread and asks egui to repaint only when something changed.
/// While the window is hidden or minimised it idles, so the app costs nothing in the tray.
pub fn spawn_poller(
    ctx: egui::Context,
    shared: std::sync::Arc<crate::bt::Shared>,
) -> std::sync::Arc<std::sync::Mutex<Pad>> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};

    let out = std::sync::Arc::new(std::sync::Mutex::new(Pad::default()));
    let slot = out.clone();
    let _ = std::thread::Builder::new().name("input".into()).spawn(move || {
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let mut poller = Poller::new();
        let mut last = Pad::default();
        loop {
            let raw = shared.hwnd.load(std::sync::atomic::Ordering::Relaxed);
            let h = HWND(raw as *mut _);
            let shown = raw != 0 && unsafe { IsWindowVisible(h).as_bool() && !IsIconic(h).as_bool() };
            if !shown {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            let foreground = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() } == h;
            let p = poller.poll(foreground);
            if !same(&last, &p) {
                *crate::bt::lock(&slot) = p.clone();
                last = p;
                // The Events tab doesn't show the controller; its 1 s refresh is enough there.
                if !shared.events_tab.load(std::sync::atomic::Ordering::Relaxed) {
                    ctx.request_repaint();
                }
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    });
    out
}

/// Synthetic input for `--demo` (screenshots and UI review without holding a controller).
pub fn demo(t: f32) -> Pad {
    let cycle = [A, B, X, Y, LB, RB, DPAD_UP, DPAD_RIGHT, VIEW, MENU, SHARE, GUIDE, LS, RS];
    let step = (t * 1.5) as usize;
    let buttons = cycle[step % cycle.len()] | if step.is_multiple_of(3) { cycle[(step + 5) % cycle.len()] } else { 0 };
    let wave = |k: f32| (t * k).sin() * 0.5 + 0.5;
    Pad {
        connected: true,
        name: "Demo controller".into(),
        count: 1,
        buttons,
        lt: wave(1.3),
        rt: wave(0.9),
        left: ((t * 1.1).cos() * 0.9, (t * 1.1).sin() * 0.9),
        right: ((t * 0.7).sin() * 0.6, (t * 1.7).cos() * 0.4),
        battery: Some(82),
        has_share: true,
    }
}

/// Radial deadzone so the drawn sticks don't jitter at rest.
fn deadzone(x: f64, y: f64) -> (f32, f32) {
    const DZ: f64 = 0.08;
    let m = (x * x + y * y).sqrt();
    if m < DZ {
        return (0.0, 0.0);
    }
    let k = ((m - DZ) / (1.0 - DZ)).min(1.0) / m;
    ((x * k) as f32, (y * k) as f32)
}

fn find_share(raw: &RawGameController) -> Option<usize> {
    let n = raw.ButtonCount().ok()?;
    (0..n).find(|&i| raw.GetButtonLabel(i).ok() == Some(GameControllerButtonLabel::Share)).map(|i| i as usize)
}

fn battery_percent(pad: &Gamepad) -> Option<u8> {
    let report = pad.TryGetBatteryReport().ok()?;
    let remaining = report.RemainingCapacityInMilliwattHours().ok()?.Value().ok()?;
    let full = report.FullChargeCapacityInMilliwattHours().ok()?.Value().ok()?;
    (full > 0).then(|| ((remaining as f64 / full as f64) * 100.0).round().clamp(0.0, 100.0) as u8)
}

/// XInputGetStateEx writes a trailing reserved DWORD past XINPUT_STATE, so give it room.
#[repr(C)]
#[derive(Default)]
struct StateEx {
    state: XINPUT_STATE,
    _reserved: u32,
}

type GetStateExFn = unsafe extern "system" fn(u32, *mut StateEx) -> u32;

fn get_state_ex() -> Option<GetStateExFn> {
    static F: OnceLock<Option<GetStateExFn>> = OnceLock::new();
    *F.get_or_init(|| unsafe {
        // System32 only: never pick up a planted xinput1_4.dll next to the exe.
        let lib = LoadLibraryExW(w!("xinput1_4.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
        let f = GetProcAddress(lib, PCSTR(100usize as *const u8))?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, GetStateExFn>(f))
    })
}

fn read_ex(get: GetStateExFn, slot: u32) -> Option<StateEx> {
    let mut st = StateEx::default();
    (unsafe { get(slot, &mut st) } == 0).then_some(st)
}
