//! "Keep busy" (Developer settings). Teams marks you Away once Windows has seen no keyboard or
//! mouse input for a few minutes. While the switch is on, a background thread presses F15, a key
//! nothing binds, every 30 seconds, which resets the idle timer Teams (and the screen lock) reads.
//! It runs whether or not a session exists or HQ is focused, and stops when the switch goes off or
//! HQ quits. Windows only. The state is never saved: the UI switches it off whenever it loads.

use std::sync::atomic::{AtomicBool, Ordering};

static ON: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
const EVERY_SECS: u32 = 30;

/// One F15 press. Fails when Windows refuses the input (a locked screen blocks it).
#[cfg(windows)]
fn press_f15() -> Result<(), String> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_F15};
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_F15, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let inputs = [key(0), key(KEYEVENTF_KEYUP)];
    // SAFETY: a pointer to two initialised INPUTs, with their count and size.
    let sent = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() { Ok(()) } else { Err("SendInput was blocked".into()) }
}

/// Starts or stops the F15 presses. Err off Windows.
#[tauri::command]
pub fn set_keep_busy(on: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        ON.store(on, Ordering::SeqCst);
        if on && !RUNNING.swap(true, Ordering::SeqCst) {
            std::thread::spawn(|| loop {
                while ON.load(Ordering::SeqCst) {
                    let _ = press_f15(); // a blocked press (locked screen) is retried at the next beat
                    for _ in 0..EVERY_SECS {
                        if !ON.load(Ordering::SeqCst) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
                RUNNING.store(false, Ordering::SeqCst);
                // Switched on again between the loop ending and RUNNING clearing: carry on here.
                if !(ON.load(Ordering::SeqCst) && !RUNNING.swap(true, Ordering::SeqCst)) {
                    break;
                }
            });
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = on;
        ON.store(false, Ordering::SeqCst);
        Err("Keep busy works on Windows only".into())
    }
}
