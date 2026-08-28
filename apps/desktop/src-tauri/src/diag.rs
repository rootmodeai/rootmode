//! Logging for a launch nobody can watch.
//!
//! A desktop app that opens blank on someone else's machine gives you
//! nothing to go on unless it wrote down what it was doing. So every run
//! writes a log file next to its data, keeps the previous run's file beside
//! it, records the machine it is on, and catches panics on the way out. The
//! file is the first thing to ask for when "it just shows nothing".
//!
//! The same lines still go to stderr, so `RUST_LOG=... npm run app` reads as
//! it always did.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub const LOG_FILE: &str = "rootmode.log";
pub const PREVIOUS_LOG_FILE: &str = "rootmode.log.1";

/// The identifier Tauri files this app's data under — kept in step with
/// `tauri.conf.json` so the log lands beside the database.
const IDENTIFIER: &str = "ai.rootmode.desktop";

/// Everything our own crates say, plus what the window layers say about
/// themselves — that is where a blank window is decided. libp2p is left at
/// info: at debug it drowns everything else within a second.
const DEFAULT_FILTER: &str = "rootmode_desktop_lib=debug,rootmode_p2p=info,\
tauri=debug,tauri_runtime_wry=debug,wry=debug,tao=debug,frontend=trace,warn";

static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static STARTED: OnceLock<Instant> = OnceLock::new();

/// Where this run is writing its log, if it managed to open one.
pub fn log_path() -> Option<PathBuf> {
    LOG_PATH.get().cloned().flatten()
}

/// Milliseconds since logging started — a launch is a race against the user
/// closing a window that shows nothing, so every milestone is stamped.
pub fn uptime_ms() -> u128 {
    STARTED.get().map(|t| t.elapsed().as_millis()).unwrap_or(0)
}

/// Where Tauri will put the app data directory, worked out the same way it
/// does, so the log can open before Tauri exists.
pub fn app_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(IDENTIFIER))
}

/// Set up stderr + file logging and the panic hook. Never fails: if the file
/// cannot be opened the app still runs, and says so on stderr.
pub fn init() {
    STARTED.get_or_init(Instant::now);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| DEFAULT_FILTER.into());

    let file = app_data_dir().and_then(|dir| match open_log(&dir) {
        Ok(f) => Some((dir.join(LOG_FILE), f)),
        Err(e) => {
            eprintln!("rootmode: cannot open {}: {e}", dir.join(LOG_FILE).display());
            None
        }
    });

    let stderr_layer = fmt::layer().with_target(true).with_writer(std::io::stderr);
    let registry = tracing_subscriber::registry().with(filter).with(stderr_layer);

    let path = match file {
        Some((path, f)) => {
            let file_layer = fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true)
                .with_writer(Arc::new(f));
            let _ = registry.with(file_layer).try_init();
            Some(path)
        }
        None => {
            let _ = registry.try_init();
            None
        }
    };
    let _ = LOG_PATH.set(path);

    install_panic_hook();
    describe_machine();
}

/// Keep exactly one previous run: the file from last time becomes `.1`, so
/// "it worked yesterday and not today" has both days in it.
fn open_log(dir: &Path) -> std::io::Result<File> {
    std::fs::create_dir_all(dir)?;
    let current = dir.join(LOG_FILE);
    if current.exists() {
        let _ = std::fs::rename(&current, dir.join(PREVIOUS_LOG_FILE));
    }
    File::create(current)
}

/// A panic on the main thread ends the app; one on any other thread ends a
/// feature. Both are written down with a backtrace before the default hook
/// prints them, because the default hook only talks to stderr and stderr is
/// nowhere when the app was double-clicked.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            thread = thread.name().unwrap_or("?"),
            "panic: {info}\n{backtrace}"
        );
        previous(info);
    }));
}

/// The facts a blank-window report always ends up needing: what machine,
/// what web engine, which graphics knobs were set, and where the app is.
fn describe_machine() {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        family = std::env::consts::FAMILY,
        "rootmode starting"
    );
    tracing::info!(
        exe = %std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
        cwd = %std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        args = ?std::env::args().skip(1).collect::<Vec<_>>(),
        log = %log_path().map(|p| p.display().to_string()).unwrap_or_else(|| "(none)".into()),
        "process"
    );
    tracing::info!(release = %os_release(), "operating system");
    match tauri::webview_version() {
        Ok(v) => tracing::info!(version = %v, "webview engine"),
        Err(e) => tracing::warn!("webview engine version unknown: {e}"),
    }

    // Anything that changes how the window is drawn. Unset is worth knowing
    // too, so the whole list is written every time.
    const KNOBS: &[&str] = &[
        "RUST_LOG",
        "WEBKIT_DISABLE_DMABUF_RENDERER",
        "WEBKIT_DISABLE_COMPOSITING_MODE",
        "WEBKIT_FORCE_SANDBOX",
        "__NV_DISABLE_EXPLICIT_SYNC",
        "LIBGL_ALWAYS_SOFTWARE",
        "GDK_BACKEND",
        "GDK_SCALE",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "APPIMAGE",
        "APPDIR",
        "LANG",
    ];
    let knobs: Vec<String> = KNOBS
        .iter()
        .map(|k| match std::env::var(k) {
            Ok(v) => format!("{k}={v}"),
            Err(_) => format!("{k}=<unset>"),
        })
        .collect();
    tracing::info!(env = ?knobs, "environment");

    #[cfg(target_os = "linux")]
    {
        if let Ok(v) = std::fs::read_to_string("/proc/driver/nvidia/version") {
            tracing::info!(nvidia = %v.lines().next().unwrap_or("").trim(), "graphics driver");
        }
        // The libraries the AppImage did not bring along and must find here.
        for lib in ["libwebkit2gtk-4.1.so.0", "libjavascriptcoregtk-4.1.so.0"] {
            tracing::info!(lib, found = ?find_shared_library(lib), "system library");
        }
    }
}

/// A human name for the OS build, since `std::env::consts::OS` says only
/// "macos" or "linux" and the version is the whole question.
fn os_release() -> String {
    #[cfg(target_os = "macos")]
    {
        let run = |args: &[&str]| {
            std::process::Command::new("sw_vers")
                .args(args)
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        let version = run(&["-productVersion"]);
        let build = run(&["-buildVersion"]);
        if version.is_empty() {
            "macOS (version unknown)".into()
        } else {
            format!("macOS {version} ({build})")
        }
    }
    #[cfg(target_os = "linux")]
    {
        let pretty = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
            })
            .unwrap_or_else(|| "Linux (no /etc/os-release)".into());
        let kernel = std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        format!("{pretty}, kernel {kernel}")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        std::env::consts::OS.to_string()
    }
}

#[cfg(target_os = "linux")]
fn find_shared_library(name: &str) -> Option<PathBuf> {
    const DIRS: &[&str] = &[
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/local/lib",
    ];
    DIRS.iter().map(|d| Path::new(d).join(name)).find(|p| p.exists())
}
