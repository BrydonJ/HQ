//! "Keep busy" (Developer settings). Teams marks you Away once Windows has seen no keyboard or
//! mouse input for a few minutes. While the switch is on and a session is busy, the UI calls
//! `nudge_idle` once a minute; it sends one F15 press, a key nothing binds, which resets the idle
//! timer Teams (and the screen lock) reads. Windows only.

/// Err off Windows, or when Windows refused the input (a locked screen or a
/// higher-integrity window in front blocks it).
#[tauri::command]
pub fn nudge_idle() -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_F15};
        let key = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_F15, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        };
        let inputs = [key(0), key(KEYEVENTF_KEYUP)];
        // SAFETY: a pointer to two initialised INPUTs, with their count and size.
        let sent = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            return Err("SendInput was blocked".into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    Err("Keep busy works on Windows only".into())
}
