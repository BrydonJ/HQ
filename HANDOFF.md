# Handoff: API key in the system keychain

Branch: `claude/peaceful-clarke-hah70t` (on top of v0.17.0). Delete this file before merging.

```powershell
git fetch origin claude/peaceful-clarke-hah70t
git checkout claude/peaceful-clarke-hah70t
```

## What changed

v0.17.0 saved the Console API key in plain text, in a file called `api-key` beside `board.json`.
It now lives in the OS keychain.

| File | Change |
| --- | --- |
| `src/secret.rs` (new) | Read, write and delete in the login Keychain (macOS, `security-framework`) and Credential Manager (Windows, `CredReadW`/`CredWriteW`/`CredDeleteW`). Other OSes save nothing. |
| `src/store.rs` | `api_key()` reads the keychain lazily and caches the key for the run. `saved_api_key_hint()` reads a plain `api-key-hint` file (just `sk-ant-…abcd`), so startup never reads the keychain. The old `api-key` file is moved to the keychain on start, then deleted. |
| `src/main.rs` | `api_key_hint` and `set_api_key` are now `async`, so they run off the main thread and a Keychain prompt can't freeze the window. |
| `src/session.rs` | Shows a clear error when the keychain read fails, for example if you click Deny. |
| `Cargo.toml` | Adds `security-framework` and `security-framework-sys` for macOS (already in the tree through native-tls). Adds the `Win32_Security_Credentials` feature to `windows-sys`. |
| `README.md`, `ui/app.js` | Wording only. |

- **Where it's stored:** account `anthropic-api-key` under service `dev.brydon.hq`. On Windows the target is `dev.brydon.hq/anthropic-api-key`, saved with `CRED_PERSIST_LOCAL_MACHINE`, so it doesn't roam with your profile.
- **Dev copies:** a copy run with `HQ_STATE_DIR` uses the account `anthropic-api-key:<dir>`, so it never overwrites your real key.

## What has and hasn't been verified

- **Done (cross-compiled from Linux):** `cargo clippy --target x86_64-pc-windows-msvc` is clean for the whole app, apart from 3 warnings that were already there. `src/secret.rs` also compiles with clippy for `x86_64-apple-darwin` and `aarch64-apple-darwin`.
- **Not done:** `cargo test`, and any run of the app on Windows or macOS.

## To do on Windows

1. `cargo test`
2. Run `dev.cmd` and save a key in Settings.
3. Check it's in Credential Manager: run `cmdkey /list | findstr hq`, or open Control Panel > Credential Manager > Windows Credentials.
4. Check there is **no** `%APPDATA%\dev.brydon.hq\api-key` file, only `api-key-hint`.
5. Test the migration: put a valid key in a plain `api-key` file in that folder, then restart. It should end up in Credential Manager, and the file should be gone.
6. Remove the key in Settings. The credential and the hint file should both be gone.
7. Switch a session to API credits. Its init should report `apiKeySource` as `ANTHROPIC_API_KEY`.

## To do on macOS

`release-mac.yml` only builds commits on `main`, so CI can't build this branch. Either merge first,
or test on a Mac with `./dev-mac.sh`.

1. Save a key, then check it's there: `security find-generic-password -s dev.brydon.hq -a anthropic-api-key`. This prints the item's details, not the key.
2. `~/Library/Application Support/dev.brydon.hq/` should hold `api-key-hint` and no `api-key`.
3. Switch a session to API credits. Expect no prompt on the build that saved the key.
4. Install an update (any newer build). The first switch to API credits should show the Keychain
   prompt once; click **Always Allow**. Clicking Deny should fail only that start, with the error above.

## Next: stop macOS treating each update as a new app

**Why it happens:** the app is signed ad hoc (`"signingIdentity": "-"` in `tauri.macos.conf.json`).
An ad hoc signature's identity is the hash of that exact binary, so the Keychain's "who may read
this" list stops matching after every update. A stable signing identity fixes this. There are two
ways to get one:

**A. Apple Developer ID ($99/year). This is the proper fix.**
- **Why it works:** the Keychain then trusts your Team ID and bundle ID, which stay the same across updates.
- **Bonus:** notarisation also removes the "Open Anyway" / `xattr` step on first launch, which matters if anyone else uses HQ.
- **Steps:**
  1. Create a "Developer ID Application" certificate and export it as a `.p12`.
  2. Add the repo secrets `APPLE_CERTIFICATE` (the `.p12`, base64) and `APPLE_CERTIFICATE_PASSWORD`. For notarisation, also add `APPLE_ID`, `APPLE_PASSWORD` (an app-specific password) and `APPLE_TEAM_ID`.
  3. Pass those secrets to the Build step in `release-mac.yml`. Tauri's bundler imports the certificate and notarises by itself when they're set.
  4. Set `signingIdentity` to the certificate's name, or drop it and let the imported certificate be used.

**B. A self-signed code-signing certificate. Free, and fine for just you.**
- **Why it works:** the Keychain then trusts that certificate, which stays the same across updates.
- **Downside:** the first-launch Gatekeeper block stays as it is now.
- **Steps:**
  1. Make the certificate once, for example in Keychain Access > Certificate Assistant > Create a Certificate, with type "Code Signing". OpenSSL with the codeSigning extended key usage works too, and you can run it on Windows.
  2. Export it as a `.p12`.
  3. Wire it up the same way as A, without the notarisation secrets.
- **Not verified:** whether Tauri's certificate import in CI accepts a self-signed certificate that isn't trusted. Test one workflow run before you rely on it.

**Either way:** the first update after the switch is signed differently, so it still prompts once
(and macOS privacy permissions may ask again once). After that, updates keep their access. The
Tauri updater checks its own minisign signature, not Apple's, so updates aren't affected.
