//! "Open in HQ" on Explorer's folder menus (Windows). A right-click on a folder, or on the empty
//! space inside one, runs `hq.exe --new-session "<folder>"`, which the single-instance plugin hands
//! to the running HQ (main.rs `parse_launch`). Written under HKCU, so it needs no admin and no
//! installer change, and only by the installed app: a dev build would point Explorer at `target\`.
//! A session opened this way is `launched`, so AFK and the approval reviewer leave it alone.

/// The two menus: a folder's own, and the background of an open folder.
#[cfg(windows)]
const KEYS: [&str; 2] = [r"Software\Classes\Directory\shell\HQ", r"Software\Classes\Directory\Background\shell\HQ"];

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn set(key: &str, name: Option<&str>, value: &str) -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
    let (k, v) = (wide(key), wide(value));
    let n = name.map(wide);
    let code = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
            REG_SZ,
            v.as_ptr().cast(),
            (v.len() * 2) as u32,
        )
    };
    if code == 0 { Ok(()) } else { Err(format!("Could not write the Explorer menu (error {code}).")) }
}

#[cfg(windows)]
fn get(key: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let k = wide(key);
    let mut buf = vec![0u16; 2048];
    let mut len = (buf.len() * 2) as u32;
    let code = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), std::ptr::null(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len)
    };
    if code != 0 {
        return None;
    }
    let chars = (len as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..chars.min(buf.len())]))
}

/// The command Explorer runs: this exe, the folder quoted. `%V` is the folder Explorer passes.
#[cfg(windows)]
fn command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(format!("\"{}\" --new-session \"%V\"", exe.display()))
}

/// None where the menu cannot be offered (macOS, or a build that is not the installed app);
/// otherwise whether both menus point at this exe.
#[tauri::command]
pub fn explorer_menu_status() -> Option<bool> {
    #[cfg(windows)]
    {
        if !crate::update::is_installed() {
            return None;
        }
        let want = command().ok()?;
        Some(KEYS.iter().all(|k| get(&format!(r"{k}\command")).is_some_and(|c| c.eq_ignore_ascii_case(&want))))
    }
    #[cfg(not(windows))]
    None
}

#[tauri::command]
pub fn set_explorer_menu(on: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        if !crate::update::is_installed() {
            return Err("Only the installed HQ can add itself to Explorer.".into());
        }
        if on {
            let cmd = command()?;
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            for k in KEYS {
                set(k, None, "Open in HQ")?;
                set(k, Some("Icon"), &format!("\"{}\",0", exe.display()))?;
                set(&format!(r"{k}\command"), None, &cmd)?;
            }
        } else {
            use windows_sys::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};
            for k in KEYS {
                let w = wide(k);
                let code = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, w.as_ptr()) };
                if code != 0 && code != 2 {
                    return Err(format!("Could not remove the Explorer menu (error {code})."));
                }
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = on;
        Err("The Explorer menu is Windows only.".into())
    }
}

/// An installed HQ that moved (a reinstall elsewhere) points the menu at its new path.
pub fn refresh_explorer_menu() {
    #[cfg(windows)]
    if crate::update::is_installed() && explorer_menu_status() == Some(false) && KEYS.iter().any(|k| get(&format!(r"{k}\command")).is_some()) {
        let _ = set_explorer_menu(true);
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn command_quotes_exe_and_folder() {
        let c = super::command().unwrap();
        assert!(c.starts_with('"') && c.ends_with(r#"--new-session "%V""#), "{c}");
    }
}
