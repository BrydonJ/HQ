//! The operating system's own secret store, for the API key: the login Keychain on macOS and
//! Credential Manager on Windows. Both encrypt what they hold for the signed-in user, so the key
//! is never on disk in the clear.

/// The Keychain item's service, and on Windows the start of the credential's target name. Each
/// secret is filed under an account name within it.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
const SERVICE: &str = "dev.brydon.hq";

#[cfg(target_os = "macos")]
mod os {
    use super::SERVICE;
    use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};
    use security_framework_sys::base::errSecItemNotFound;

    fn describe(e: security_framework::base::Error) -> String {
        e.message().unwrap_or_else(|| format!("Keychain error {}", e.code()))
    }

    pub fn read(account: &str) -> Result<Option<String>, String> {
        match get_generic_password(SERVICE, account) {
            Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| "the Keychain item is not text".into()),
            Err(e) if e.code() == errSecItemNotFound => Ok(None),
            Err(e) => Err(describe(e)),
        }
    }

    pub fn write(account: &str, secret: &str) -> Result<(), String> {
        set_generic_password(SERVICE, account, secret.as_bytes()).map_err(describe)
    }

    pub fn delete(account: &str) -> Result<(), String> {
        match delete_generic_password(SERVICE, account) {
            Err(e) if e.code() != errSecItemNotFound => Err(describe(e)),
            _ => Ok(()),
        }
    }
}

#[cfg(windows)]
mod os {
    use super::SERVICE;
    use std::io;
    use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn target(account: &str) -> Vec<u16> {
        wide(&format!("{SERVICE}/{account}"))
    }

    fn not_found(e: &io::Error) -> bool {
        e.raw_os_error() == Some(ERROR_NOT_FOUND as i32)
    }

    pub fn read(account: &str) -> Result<Option<String>, String> {
        let target = target(account);
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: on success CredReadW hands back a buffer that stays valid until CredFree.
        unsafe {
            if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) == 0 {
                let e = io::Error::last_os_error();
                return if not_found(&e) { Ok(None) } else { Err(e.to_string()) };
            }
            let c = &*cred;
            let text = if c.CredentialBlob.is_null() {
                Ok(String::new())
            } else {
                String::from_utf8(std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec())
            };
            CredFree(cred as *const _);
            text.map(Some).map_err(|_| "the saved credential is not text".into())
        }
    }

    pub fn write(account: &str, secret: &str) -> Result<(), String> {
        let mut target = target(account);
        let mut user = wide(account);
        let mut blob = secret.as_bytes().to_vec();
        // SAFETY: every pointer is to a buffer above that outlives the call; zeroed is a valid CREDENTIALW.
        let ok = unsafe {
            let mut cred: CREDENTIALW = std::mem::zeroed();
            cred.Type = CRED_TYPE_GENERIC;
            cred.TargetName = target.as_mut_ptr();
            cred.UserName = user.as_mut_ptr();
            cred.CredentialBlobSize = blob.len() as u32;
            cred.CredentialBlob = blob.as_mut_ptr();
            // This computer only: a roaming profile does not carry it to others.
            cred.Persist = CRED_PERSIST_LOCAL_MACHINE;
            CredWriteW(&cred, 0)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    pub fn delete(account: &str) -> Result<(), String> {
        let target = target(account);
        // SAFETY: target is a NUL-terminated UTF-16 string.
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            let e = io::Error::last_os_error();
            if !not_found(&e) {
                return Err(e.to_string());
            }
        }
        Ok(())
    }
}

/// HQ ships for Windows and macOS only; elsewhere there is no store, and nothing is saved.
#[cfg(not(any(target_os = "macos", windows)))]
mod os {
    const UNSUPPORTED: &str = "there is no system keychain HQ can use on this platform";

    pub fn read(_: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    pub fn write(_: &str, _: &str) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }

    pub fn delete(_: &str) -> Result<(), String> {
        Ok(())
    }
}

/// The secret saved under `account`, or None when there is none. On macOS this can show the
/// Keychain's "allow" prompt (the ad hoc signature changes with every build), so callers read it
/// only when needed.
pub fn read(account: &str) -> Result<Option<String>, String> {
    os::read(account)
}

/// Saves the secret under `account`, replacing any saved before.
pub fn write(account: &str, secret: &str) -> Result<(), String> {
    os::write(account, secret)
}

/// Removes the secret under `account`; nothing saved is not an error.
pub fn delete(account: &str) -> Result<(), String> {
    os::delete(account)
}
