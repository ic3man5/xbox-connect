//! Persistent settings: "Start with Windows" (HKCU Run key) and app prefs (HKCU\Software\XboxConnect).
//! Uses the Win32 registry API directly (no extra crate).

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows::core::w;

const RUN_KEY: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: windows::core::PCWSTR = w!("XboxConnect");
const APP_KEY: windows::core::PCWSTR = w!(r"Software\XboxConnect");
const KEEP_VALUE: windows::core::PCWSTR = w!("KeepConnected");

fn run_command() -> Option<String> {
    Some(format!("\"{}\" --minimized", std::env::current_exe().ok()?.display()))
}

fn read_string(key: windows::core::PCWSTR, value: windows::core::PCWSTR) -> Option<String> {
    unsafe {
        let mut len = 0u32;
        if RegGetValueW(HKEY_CURRENT_USER, key, value, RRF_RT_REG_SZ, None, None, Some(&mut len)) != ERROR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        if RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

pub fn autostart_enabled() -> bool {
    read_string(RUN_KEY, RUN_VALUE).is_some()
}

pub fn set_autostart(on: bool) -> Result<(), ()> {
    unsafe {
        let rc = if on {
            let cmd: Vec<u16> = run_command().ok_or(())?.encode_utf16().chain([0]).collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(cmd.as_ptr().cast()),
                (cmd.len() * 2) as u32,
            )
        } else {
            match RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) {
                ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                rc => rc,
            }
        };
        if rc == ERROR_SUCCESS { Ok(()) } else { Err(()) }
    }
}

/// If autostart is on but its exe no longer exists (moved/reinstalled), point it at this one.
/// A copy run from elsewhere (e.g. Downloads) doesn't take over a working installed entry.
pub fn heal_autostart() {
    let Some(stored) = read_string(RUN_KEY, RUN_VALUE) else { return };
    let path = stored.trim_start_matches('"').split('"').next().unwrap_or_default();
    if !path.is_empty() && !std::path::Path::new(path).exists() {
        let _ = set_autostart(true);
    }
}

pub fn keep_connected() -> bool {
    let mut v = 1u32;
    let mut len = 4u32;
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            APP_KEY,
            KEEP_VALUE,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut v as *mut u32).cast()),
            Some(&mut len),
        );
    }
    v != 0
}

pub fn set_keep_connected(on: bool) {
    let v = on as u32;
    unsafe {
        let _ =
            RegSetKeyValueW(HKEY_CURRENT_USER, APP_KEY, KEEP_VALUE, REG_DWORD.0, Some((&v as *const u32).cast()), 4);
    }
}
