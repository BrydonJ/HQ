fn main() {
    // A debug build carries the yellow icon, so a local copy is told apart from the installed one at a glance.
    let mut windows = tauri_build::WindowsAttributes::new();
    if std::env::var("PROFILE").as_deref() == Ok("debug") {
        windows = windows.window_icon_path("icons/debug-icon.ico");
    }
    println!("cargo:rerun-if-changed=icons/debug-icon.ico");
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows)).expect("tauri build failed");
}
