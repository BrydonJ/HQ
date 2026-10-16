//! Debug-build conveniences. None of this is compiled into a release build.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use tauri::utils::assets::{AssetKey, AssetsIter, CspHash};
use tauri::{Assets, Context, Runtime};

#[cfg(windows)]
const DEFAULT_DEBUG_PORT: &str = "9223";
/// What wry passes when a window sets no browser arguments of its own.
#[cfg(windows)]
const WRY_DEFAULT_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

/// Gives the window the yellow debug icon, serves the UI from disk and, on Windows, opens a DevTools port (`HQ_DEBUG_PORT`, default 9223)
/// so the dev copy can be inspected and driven over CDP. (The port is a WebView2 switch; on a Mac
/// use Safari's Develop menu instead.)
pub fn prepare<R: Runtime>(context: &mut Context<R>) {
    context.set_default_window_icon(Some(tauri::include_image!("icons/debug-icon.ico")));
    #[cfg(windows)]
    {
        let port = std::env::var("HQ_DEBUG_PORT").unwrap_or_else(|_| DEFAULT_DEBUG_PORT.into());
        if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
            for window in &mut context.config_mut().app.windows {
                window.additional_browser_args = Some(format!("{WRY_DEFAULT_ARGS} --remote-debugging-port={port}"));
            }
        }
    }
    let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/ui"));
    let embedded = context.set_assets(Box::new(NoAssets));
    context.set_assets(Box::new(DiskAssets { root, embedded }));
}

/// Reads each UI file from `ui/` when it is requested, so a reload (F5) shows an edit without
/// a rebuild. A file missing on disk falls back to the copy embedded at compile time.
struct DiskAssets<R: Runtime> {
    root: PathBuf,
    embedded: Box<dyn Assets<R>>,
}

impl<R: Runtime> Assets<R> for DiskAssets<R> {
    fn get(&self, key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        let rel = Path::new(key.as_ref().trim_start_matches('/'));
        if rel.components().all(|c| matches!(c, Component::Normal(_))) {
            if let Ok(bytes) = std::fs::read(self.root.join(rel)) {
                return Some(Cow::Owned(bytes));
            }
        }
        self.embedded.get(key)
    }

    fn iter(&self) -> Box<AssetsIter<'_>> {
        self.embedded.iter()
    }

    fn csp_hashes(&self, html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        self.embedded.csp_hashes(html_path)
    }
}

/// Placeholder held for the moment the embedded assets are swapped out.
struct NoAssets;

impl<R: Runtime> Assets<R> for NoAssets {
    fn get(&self, _key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        None
    }

    fn iter(&self) -> Box<AssetsIter<'_>> {
        Box::new(std::iter::empty())
    }

    fn csp_hashes(&self, _html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        Box::new(std::iter::empty())
    }
}
