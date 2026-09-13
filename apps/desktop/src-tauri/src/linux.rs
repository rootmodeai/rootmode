//! Linux webview and AppImage workarounds.
//!
//! The AppImage is built on Ubuntu 22.04 and runs against the host's
//! WebKitGTK. linuxdeploy then points GStreamer at a plugin directory it
//! never filled, and WebKit's media setup dies the moment a `<video>` is
//! created — `autoaudiosink not found`, a NULL GObject, and a black window
//! over the boot screen. NVIDIA's DMA-BUF path has the same end-state for
//! a different reason. These run before the webview exists so neither is
//! left for the user to export.

use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
static SKIP_MEDIA: AtomicBool = AtomicBool::new(false);

/// Apply host-specific env so the webview can draw and play media.
///
/// Safe to call more than once; later calls are cheap. Must run after
/// logging is up (so the original environment is already on disk) and
/// before Tauri builds the window.
pub fn apply() {
    #[cfg(target_os = "linux")]
    apply_linux();
}

/// Whether a `<video>` is safe to create. False means the intro must not
/// hand WebKit a file: the web process crashes instead of firing `error`.
pub fn media_playback_available() -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        !SKIP_MEDIA.load(Ordering::Relaxed) && autodetect_plugin_exists()
    }
}

#[cfg(target_os = "linux")]
fn apply_linux() {
    repair_gst_plugin_path();
    isolate_gio_modules();
    apply_nvidia_webview_workarounds();
    if SKIP_MEDIA.load(Ordering::Relaxed) {
        tracing::warn!(
            "intro and in-app video will be skipped: this AppImage has GStreamer without plugins"
        );
    } else if autodetect_plugin_exists() {
        tracing::info!("gstreamer autoaudiosink plugin is present");
    } else {
        tracing::warn!(
            "gstreamer autoaudiosink plugin was not found; the intro film will be skipped"
        );
    }
}

/// AppRun sets `GST_PLUGIN_SYSTEM_PATH_1_0` to `$APPDIR/usr/lib/gstreamer-1.0`
/// even when `bundleMediaFramework` left that directory empty. GStreamer then
/// searches nowhere, `autoaudiosink` is missing, and WebKitWebProcess dies.
#[cfg(target_os = "linux")]
fn repair_gst_plugin_path() {
    const VARS: &[&str] = &["GST_PLUGIN_SYSTEM_PATH_1_0", "GST_PLUGIN_SYSTEM_PATH"];
    let mut unset_empty = false;
    for var in VARS {
        let Ok(value) = std::env::var(var) else {
            continue;
        };
        if gst_path_is_usable(&value) {
            tracing::info!(var, value = %value, "gstreamer plugin path is populated");
            continue;
        }
        tracing::warn!(
            var,
            value = %value,
            "gstreamer plugin path is empty or missing; unsetting so the system's plugins can be found"
        );
        remove_var(var);
        unset_empty = true;
    }

    // Bundled libgstreamer (Ubuntu 22.04, 1.20) will not load the host's
    // plugins (Ubuntu 24.04 / Mint 22, 1.24). If the AppImage shipped the
    // library without the plugins, playing media crashes the web process.
    if unset_empty && bundled_gstreamer_on_library_path() {
        SKIP_MEDIA.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
fn isolate_gio_modules() {
    if std::env::var_os("APPIMAGE").is_none() {
        return;
    }
    let Some(appdir) = std::env::var_os("APPDIR") else {
        return;
    };
    let appdir = PathBuf::from(appdir);
    // Host gvfs (Mint 22 / Ubuntu 24) was built against a newer GLib than
    // the one linuxdeploy copied in, and fails with `g_task_set_static_name`.
    // Prefer the bundle's modules; if it has none, point at a path that
    // does not exist so GIO does not load the host's.
    let bundled = [
        appdir.join("usr/lib/x86_64-linux-gnu/gio/modules"),
        appdir.join("usr/lib/aarch64-linux-gnu/gio/modules"),
        appdir.join("usr/lib/gio/modules"),
    ];
    let dir = bundled
        .into_iter()
        .find(|p| p.is_dir())
        .unwrap_or_else(|| appdir.join("usr/lib/gio/modules-none"));
    tracing::info!(dir = %dir.display(), "GIO modules restricted to the AppImage");
    set_var("GIO_MODULE_DIR", dir);
}

#[cfg(target_os = "linux")]
fn apply_nvidia_webview_workarounds() {
    if !nvidia_driver_present() {
        return;
    }
    // First-line Tauri workaround; cheap on X11, needed on Wayland.
    if std::env::var_os("__NV_DISABLE_EXPLICIT_SYNC").is_none() {
        set_var("__NV_DISABLE_EXPLICIT_SYNC", "1");
        tracing::info!("NVIDIA: set __NV_DISABLE_EXPLICIT_SYNC=1");
    }
    // WebKitGTK 2.42+ DMA-BUF is broken on the proprietary/open NVIDIA
    // modules. Leave an operator's value alone, including `=0`.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        tracing::info!("NVIDIA: set WEBKIT_DISABLE_DMABUF_RENDERER=1");
    }
}

#[cfg(target_os = "linux")]
fn nvidia_driver_present() -> bool {
    Path::new("/proc/driver/nvidia/version").exists() || Path::new("/sys/module/nvidia").exists()
}

#[cfg(target_os = "linux")]
fn bundled_gstreamer_on_library_path() -> bool {
    let Some(appdir) = std::env::var_os("APPDIR") else {
        return false;
    };
    let Some(ld) = std::env::var_os("LD_LIBRARY_PATH") else {
        return false;
    };
    let appdir = PathBuf::from(appdir);
    std::env::split_paths(&ld)
        .any(|dir| dir.starts_with(&appdir) && dir.join("libgstreamer-1.0.so.0").exists())
}

#[cfg(target_os = "linux")]
fn autodetect_plugin_exists() -> bool {
    gst_plugin_dirs().iter().any(|dir| {
        dir.join("libgstautodetect.so").exists() || dir.join("libgstautodetect.so.0").exists()
    })
}

#[cfg(target_os = "linux")]
fn gst_plugin_dirs() -> Vec<PathBuf> {
    // GST_PLUGIN_SYSTEM_PATH_1_0 replaces the default system search.
    if let Ok(v) = std::env::var("GST_PLUGIN_SYSTEM_PATH_1_0") {
        return split_gst_paths(&v);
    }
    if let Ok(v) = std::env::var("GST_PLUGIN_SYSTEM_PATH") {
        return split_gst_paths(&v);
    }
    let mut dirs = Vec::new();
    if let Ok(v) = std::env::var("GST_PLUGIN_PATH_1_0") {
        dirs.extend(split_gst_paths(&v));
    }
    if let Ok(v) = std::env::var("GST_PLUGIN_PATH") {
        dirs.extend(split_gst_paths(&v));
    }
    dirs.extend(default_system_gst_dirs());
    dirs
}

#[cfg(target_os = "linux")]
fn default_system_gst_dirs() -> Vec<PathBuf> {
    [
        "/usr/lib/x86_64-linux-gnu/gstreamer-1.0",
        "/usr/lib/aarch64-linux-gnu/gstreamer-1.0",
        "/usr/lib64/gstreamer-1.0",
        "/usr/lib/gstreamer-1.0",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

fn split_gst_paths(value: &str) -> Vec<PathBuf> {
    std::env::split_paths(value)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// A colon-separated GStreamer search path is usable only if at least one
/// directory in it actually contains plugins.
fn gst_path_is_usable(value: &str) -> bool {
    split_gst_paths(value)
        .iter()
        .any(|dir| dir_has_gst_plugins(dir))
}

fn dir_has_gst_plugins(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name.starts_with("libgst") && (name.ends_with(".so") || name.contains(".so."))
    })
}

#[cfg(target_os = "linux")]
fn set_var(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    // Called from `run` before the tokio runtime or the webview exist.
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var(key, value);
    }
}

#[cfg(target_os = "linux")]
fn remove_var(key: &str) {
    #[allow(unused_unsafe)]
    unsafe {
        std::env::remove_var(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rootmode-linux-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_gstreamer_path_is_not_usable() {
        assert!(!gst_path_is_usable(""));
        assert!(!gst_path_is_usable("/no/such/gstreamer-plugins"));
    }

    #[test]
    fn a_directory_without_plugins_is_not_usable() {
        let dir = temp_dir();
        assert!(!gst_path_is_usable(dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_with_a_gstreamer_plugin_is_usable() {
        let dir = temp_dir();
        std::fs::write(dir.join("libgstautodetect.so"), b"").unwrap();
        assert!(gst_path_is_usable(dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_second_entry_in_a_search_path_counts() {
        let empty = temp_dir();
        let filled = temp_dir();
        std::fs::write(filled.join("libgstcoreelements.so"), b"").unwrap();
        let joined = format!("{}:{}", empty.display(), filled.display());
        assert!(gst_path_is_usable(&joined));
        let _ = std::fs::remove_dir_all(&empty);
        let _ = std::fs::remove_dir_all(&filled);
    }

    #[test]
    fn media_playback_is_available_off_linux() {
        #[cfg(not(target_os = "linux"))]
        assert!(media_playback_available());
    }
}
