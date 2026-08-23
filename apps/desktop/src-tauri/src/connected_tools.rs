//! Config-patching connectors for coding-agent CLIs and editors.
//!
//! `gateway` already speaks the two API shapes these tools understand
//! (Anthropic Messages, OpenAI chat completions) on loopback. The manual path
//! is copying a snippet from the Connect screen into a shell — this module is
//! the one-click version of that for tools with a single, patchable config
//! file: instead of a recipe to paste, connecting here edits the file
//! directly, and disconnecting removes just what we added.
//!
//! Each tool gets a surgical patch to its own on-disk format rather than a
//! full parse-and-rewrite: Codex's TOML keeps every line the user did not
//! touch, JSON files keep everything outside the one provider key we own. A
//! `.rootmode-bak` copy is kept the first time a file is touched, as a safety
//! net a person can restore from by hand — it is not used to undo a connect,
//! since the file may have changed since (Codex itself rewrites its config).
//!
//! Known gap: Windows support resolves config paths correctly (APPDATA /
//! LOCALAPPDATA) but does not probe WSL distros the way native installs are
//! probed. A tool installed only inside WSL will show as not installed.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::{AppError, Result};
use crate::gateway::GatewayStatus;
use crate::state::AppState;

const SETTING_PREFIX: &str = "connected_tool_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFormat {
    OpenCode,
    Codex,
    ClaudeCode,
    Pi,
    Crush,
    Goose,
    Zed,
}

pub struct ToolProfile {
    pub key: &'static str,
    pub display_name: &'static str,
    pub format: ToolFormat,
    /// Binary name probed on PATH when the config directory itself is not a
    /// reliable install signal (a tool run once but never configured).
    pub binary: &'static str,
    /// Shown under the tool's name so "connect" doesn't feel like a black box.
    pub method: &'static str,
    /// Product name for tools that ship a desktop app rather than (or in
    /// addition to) a CLI. When present and installed, this is what "launch"
    /// opens instead of a terminal, since a terminal running a CLI that
    /// doesn't exist is not "opening the app". Checked on all three
    /// platforms — see `app_path`. Superseded by `macos_bundle_id` /
    /// `macos_url_scheme` on macOS when those are set, since a bundle
    /// identifier or a registered URL scheme finds the app wherever
    /// LaunchServices knows it to be, rather than guessing a folder name —
    /// Codex, for one, now ships inside ChatGPT.app, not its own bundle.
    pub app_name: Option<&'static str>,
    /// macOS bundle identifier, checked via Spotlight (`mdfind`) purely as an
    /// extra *install* signal — correct even when the product's `.app` has
    /// been renamed or folded into a different bundle (Codex's is
    /// `com.openai.codex`, but the bundle on disk is ChatGPT.app). Launching
    /// still goes through `app_name` / `open -a`: AntSeed's own launcher
    /// (`tool-resume.ts`) notes none of these tools expose a reliable
    /// per-session deep link, so a URL scheme isn't worth trusting even when
    /// one happens to be registered.
    pub macos_bundle_id: Option<&'static str>,
}

pub const TOOLS: &[ToolProfile] = &[
    ToolProfile {
        key: "codex", display_name: "Codex", format: ToolFormat::Codex, binary: "codex",
        method: "Patches ~/.codex/config.toml",
        // Codex's desktop experience ships inside the ChatGPT app, not a
        // standalone bundle — same mapping AntSeed's tool-resume.ts uses.
        app_name: Some("ChatGPT"), macos_bundle_id: Some("com.openai.codex"),
    },
    ToolProfile { key: "claude-code", display_name: "Claude Code", format: ToolFormat::ClaudeCode, binary: "claude", method: "Patches ~/.claude/settings.json", app_name: None, macos_bundle_id: None },
    ToolProfile { key: "opencode", display_name: "OpenCode", format: ToolFormat::OpenCode, binary: "opencode", method: "Patches ~/.config/opencode/opencode.jsonc", app_name: Some("OpenCode"), macos_bundle_id: None },
    ToolProfile { key: "pi", display_name: "pi", format: ToolFormat::Pi, binary: "pi", method: "Patches ~/.pi/agent/models.json", app_name: None, macos_bundle_id: None },
    ToolProfile { key: "crush", display_name: "Crush", format: ToolFormat::Crush, binary: "crush", method: "Patches crush.json", app_name: None, macos_bundle_id: None },
    ToolProfile { key: "goose", display_name: "Goose", format: ToolFormat::Goose, binary: "goose", method: "Patches goose's config.yaml", app_name: Some("Goose"), macos_bundle_id: None },
    ToolProfile { key: "zed", display_name: "Zed", format: ToolFormat::Zed, binary: "zed", method: "Patches Zed's settings.json (paste the key when asked)", app_name: Some("Zed"), macos_bundle_id: None },
];

fn tool(key: &str) -> Result<&'static ToolProfile> {
    TOOLS
        .iter()
        .find(|t| t.key == key)
        .ok_or_else(|| AppError::NotFound(format!("unknown tool '{key}'")))
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub key: String,
    pub display_name: String,
    pub method: String,
    pub installed: bool,
    pub connected: bool,
    pub config_path: String,
}

pub fn list(state: &AppState) -> Vec<ToolStatus> {
    TOOLS
        .iter()
        .map(|t| {
            let path = config_path(t).ok();
            ToolStatus {
                key: t.key.into(),
                display_name: t.display_name.into(),
                method: t.method.into(),
                installed: installed(t),
                connected: is_connected(state, t),
                config_path: path.map(|p| p.display().to_string()).unwrap_or_default(),
            }
        })
        .collect()
}

fn is_connected(state: &AppState, t: &ToolProfile) -> bool {
    matches!(
        state.db.get_setting(&format!("{SETTING_PREFIX}{}", t.key)),
        Ok(Some(v)) if v == "true"
    )
}

fn mark_connected(state: &AppState, t: &ToolProfile, connected: bool) -> Result<()> {
    state
        .db
        .set_setting(&format!("{SETTING_PREFIX}{}", t.key), if connected { "true" } else { "false" })
        .map_err(Into::into)
}

// ------------------------------------------------------------------ paths

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| AppError::Invalid("could not determine the home directory".into()))
}

fn windows_known_folder(local: bool) -> Option<PathBuf> {
    let var = if local { "LOCALAPPDATA" } else { "APPDATA" };
    std::env::var_os(var).map(PathBuf::from)
}

fn config_path(t: &ToolProfile) -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(match t.key {
        "codex" => home.join(".codex").join("config.toml"),
        "claude-code" => home.join(".claude").join("settings.json"),
        "opencode" => home.join(".config").join("opencode").join("opencode.jsonc"),
        "pi" => home.join(".pi").join("agent").join("models.json"),
        "crush" => {
            if cfg!(windows) {
                windows_known_folder(true)
                    .unwrap_or_else(|| home.clone())
                    .join("crush")
                    .join("crush.json")
            } else {
                home.join(".config").join("crush").join("crush.json")
            }
        }
        "goose" => {
            if cfg!(windows) {
                windows_known_folder(false)
                    .unwrap_or_else(|| home.clone())
                    .join("Block")
                    .join("goose")
                    .join("config")
                    .join("config.yaml")
            } else {
                home.join(".config").join("goose").join("config.yaml")
            }
        }
        "zed" => {
            if cfg!(windows) {
                windows_known_folder(false)
                    .unwrap_or_else(|| home.clone())
                    .join("Zed")
                    .join("settings.json")
            } else {
                home.join(".config").join("zed").join("settings.json")
            }
        }
        _ => unreachable!("every ToolProfile.key must be handled above"),
    })
}

fn pi_settings_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".pi").join("agent").join("settings.json"))
}

fn installed(t: &ToolProfile) -> bool {
    if cfg!(target_os = "macos") {
        if let Some(id) = t.macos_bundle_id {
            if macos_app_by_bundle_id(id).is_some() {
                return true;
            }
        }
    }
    if let Some(app) = t.app_name {
        if app_path(app).is_some() {
            return true;
        }
    }
    if let Ok(path) = config_path(t) {
        if let Some(dir) = path.parent() {
            if dir.exists() {
                return true;
            }
        }
    }
    binary_on_path(t.binary)
}

fn binary_on_path(name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    let candidates: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd"), format!("{name}.bat")]
    } else {
        vec![name.to_string()]
    };
    std::env::split_paths(&path_var)
        .any(|dir| candidates.iter().any(|exe| dir.join(exe).is_file()))
}

/// Finds an installed app by macOS bundle identifier via Spotlight, which
/// knows where it actually is regardless of folder name or nesting inside
/// another bundle. Returns nothing (rather than erroring) when Spotlight
/// indexing is off — `installed` still has the path- and PATH-based checks
/// as a fallback.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_app_by_bundle_id(id: &str) -> Option<PathBuf> {
    let output = std::process::Command::new("mdfind")
        .arg(format!("kMDItemCFBundleIdentifier == '{id}'"))
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(PathBuf::from)
}

/// Where a tool's own desktop app would live, by platform — the conventional
/// install locations each packager (electron-builder, Tauri) uses:
///
/// - macOS: `/Applications/<Name>.app`, or the same under `~/Applications`.
/// - Windows: `%LOCALAPPDATA%\Programs\<Name>\<Name>.exe`, the default
///   electron-builder per-user NSIS target, then `%ProgramFiles%\<Name>\`.
/// - Linux: `/opt/<Name>/<name>`, the default electron-builder `.deb`/AppImage
///   integration target (`<name>` lowercase), then `/usr/lib/<Name>/<name>`.
///
/// Best-effort by nature — a tool installed somewhere unconventional (a
/// portable AppImage on Linux, say) won't be found this way, and `installed`
/// still falls back to the config directory and PATH checks below.
fn app_path(name: &str) -> Option<PathBuf> {
    let home = home_dir().ok();
    let lower = name.to_lowercase();

    let candidates: Vec<PathBuf> = if cfg!(target_os = "macos") {
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(h) = &home {
            roots.push(h.join("Applications"));
        }
        roots.into_iter().map(|r| r.join(format!("{name}.app"))).collect()
    } else if cfg!(windows) {
        let mut out = Vec::new();
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            out.push(PathBuf::from(local).join("Programs").join(name).join(format!("{name}.exe")));
        }
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            out.push(PathBuf::from(pf).join(name).join(format!("{name}.exe")));
        }
        out
    } else {
        vec![
            PathBuf::from("/opt").join(name).join(&lower),
            PathBuf::from("/usr/lib").join(name).join(&lower),
        ]
    };

    candidates.into_iter().find(|p| p.exists())
}

// -------------------------------------------------------------------- files

fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".rootmode-bak");
    PathBuf::from(s)
}

fn backup_once(path: &Path) -> Result<()> {
    if path.exists() {
        let backup = backup_path(path);
        if !backup.exists() {
            fs::copy(path, &backup)?;
        }
    }
    Ok(())
}

fn write_text(path: &Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    backup_once(path)?;
    fs::write(path, content)?;
    Ok(())
}

fn read_text(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Strips `//` and `/* */` comments outside of strings, and trailing commas
/// before `}`/`]` — enough to turn the JSONC most tools ship into something
/// `serde_json` accepts. Comments in the user's original file are lost on
/// write-back; the file stays valid JSON (a strict subset of JSONC) either
/// way.
fn strip_jsonc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    for nc in chars.by_ref() {
                        if nc == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                    continue;
                }
                Some('*') => {
                    chars.next();
                    let mut prev = ' ';
                    for nc in chars.by_ref() {
                        if prev == '*' && nc == '/' {
                            break;
                        }
                        prev = nc;
                    }
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    let comma_free = regex_lite_strip_trailing_commas(&out);
    comma_free
}

/// Minimal trailing-comma remover (`,` followed only by whitespace then `}`
/// or `]`) — no regex crate in this workspace, so a small hand-rolled pass.
fn regex_lite_strip_trailing_commas(input: &str) -> String {
    let bytes: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == ',' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == '}' || bytes[j] == ']') {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn read_json_lenient(path: &Path) -> Value {
    match read_text(path) {
        Some(raw) => serde_json::from_str(&strip_jsonc(&raw)).unwrap_or(Value::Object(Map::new())),
        None => Value::Object(Map::new()),
    }
}

fn write_json_pretty(path: &Path, value: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    write_text(path, &format!("{text}\n"))
}

fn as_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().expect("just coerced to object")
}

// ----------------------------------------------------------------- connect

/// What a connect needs from the gateway: where it listens, the key a tool
/// should send, and the model name to route to.
pub struct Endpoint {
    pub base_url: String,
    pub token: String,
    pub model: String,
}

pub fn endpoint_from_status(status: &GatewayStatus, fallback_model: Option<&str>) -> Endpoint {
    let model = status
        .model
        .clone()
        .or_else(|| fallback_model.map(str::to_string))
        .unwrap_or_else(|| "auto".to_string());
    Endpoint {
        base_url: status.base_url.clone(),
        token: status.token.clone(),
        model,
    }
}

pub fn connect(state: &AppState, key: &str, endpoint: &Endpoint) -> Result<ToolStatus> {
    let t = tool(key)?;
    if !installed(t) {
        return Err(AppError::Invalid(format!(
            "{} was not found on this machine — install it (or run it once), then connect.",
            t.display_name
        )));
    }
    match t.format {
        ToolFormat::Codex => connect_codex(state, endpoint)?,
        ToolFormat::ClaudeCode => connect_claude_code(endpoint)?,
        ToolFormat::OpenCode => connect_opencode(endpoint)?,
        ToolFormat::Pi => connect_pi(endpoint)?,
        ToolFormat::Crush => connect_crush(endpoint)?,
        ToolFormat::Goose => connect_goose(endpoint)?,
        ToolFormat::Zed => connect_zed(endpoint)?,
    }
    mark_connected(state, t, true)?;
    Ok(status_of(state, t))
}

pub fn disconnect(state: &AppState, key: &str) -> Result<ToolStatus> {
    let t = tool(key)?;
    match t.format {
        ToolFormat::Codex => disconnect_codex()?,
        ToolFormat::ClaudeCode => disconnect_claude_code()?,
        ToolFormat::OpenCode => disconnect_opencode()?,
        ToolFormat::Pi => disconnect_pi()?,
        ToolFormat::Crush => disconnect_crush()?,
        ToolFormat::Goose => disconnect_goose()?,
        ToolFormat::Zed => disconnect_zed()?,
    }
    mark_connected(state, t, false)?;
    Ok(status_of(state, t))
}

fn status_of(state: &AppState, t: &ToolProfile) -> ToolStatus {
    let path = config_path(t).ok();
    ToolStatus {
        key: t.key.into(),
        display_name: t.display_name.into(),
        method: t.method.into(),
        installed: installed(t),
        connected: is_connected(state, t),
        config_path: path.map(|p| p.display().to_string()).unwrap_or_default(),
    }
}

const PROVIDER_KEY: &str = "rootmode";

// --------------------------------------------------------------- opencode

fn connect_opencode(ep: &Endpoint) -> Result<()> {
    let path = config_path(tool("opencode")?)?;
    let mut config = read_json_lenient(&path);
    let root = as_object(&mut config);
    let providers = as_object(root.entry("provider").or_insert_with(|| Value::Object(Map::new())));
    providers.insert(
        PROVIDER_KEY.into(),
        serde_json::json!({
            "name": "rootmode",
            "npm": "@ai-sdk/openai-compatible",
            "options": { "baseURL": format!("{}/v1", ep.base_url), "apiKey": ep.token },
            "models": { ep.model.clone(): { "name": ep.model } },
        }),
    );
    root.insert("model".into(), Value::String(format!("{PROVIDER_KEY}/{}", ep.model)));
    write_json_pretty(&path, &config)
}

fn disconnect_opencode() -> Result<()> {
    let path = config_path(tool("opencode")?)?;
    let mut config = read_json_lenient(&path);
    let root = as_object(&mut config);
    if let Some(Value::Object(providers)) = root.get_mut("provider") {
        providers.remove(PROVIDER_KEY);
    }
    if matches!(root.get("model"), Some(Value::String(m)) if m.starts_with(&format!("{PROVIDER_KEY}/"))) {
        root.remove("model");
    }
    write_json_pretty(&path, &config)
}

// ------------------------------------------------------------------- codex

fn codex_lines(path: &Path) -> Vec<String> {
    read_text(path)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn first_table_index(lines: &[String]) -> usize {
    lines.iter().position(|l| l.trim_start().starts_with('[')).unwrap_or(lines.len())
}

fn is_provider_header(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == format!("[model_providers.{PROVIDER_KEY}]")
}

fn remove_provider_table(lines: &[String]) -> Vec<String> {
    let Some(start) = lines.iter().position(|l| is_provider_header(l)) else {
        return lines.to_vec();
    };
    let mut end = start + 1;
    while end < lines.len() && !lines[end].trim_start().starts_with('[') {
        end += 1;
    }
    [&lines[..start], &lines[end..]].concat()
}

fn set_top_level(lines: &[String], key: &str, assignment: String) -> Vec<String> {
    let limit = first_table_index(lines);
    if let Some(i) = lines[..limit].iter().position(|l| l.trim_start().starts_with(&format!("{key} "))) {
        let mut out = lines.to_vec();
        out[i] = assignment;
        out
    } else {
        let mut out = lines[..limit].to_vec();
        out.push(assignment);
        out.extend_from_slice(&lines[limit..]);
        out
    }
}

fn remove_top_level(lines: &[String], key: &str) -> Vec<String> {
    let limit = first_table_index(lines);
    match lines[..limit].iter().position(|l| l.trim_start().starts_with(&format!("{key} "))) {
        Some(i) => [&lines[..i], &lines[i + 1..]].concat(),
        None => lines.to_vec(),
    }
}

/// The env var Codex is told to read its key from. Codex's own config format
/// has no field for a literal key — only the name of an environment variable
/// — so a connect also ensures that variable is exported for new shells.
const CODEX_ENV_VAR: &str = "ROOTMODE_GATEWAY_TOKEN";

fn connect_codex(state: &AppState, ep: &Endpoint) -> Result<()> {
    let path = config_path(tool("codex")?)?;
    backup_once(&path)?;
    let mut lines = codex_lines(&path);
    lines = remove_provider_table(&lines);
    lines = set_top_level(&lines, "model_provider", format!("model_provider = {}", toml_string(PROVIDER_KEY)));
    lines = set_top_level(&lines, "model", format!("model = {}", toml_string(&ep.model)));
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.push(String::new());
    lines.push(format!("[model_providers.{PROVIDER_KEY}]"));
    lines.push(format!("name = {}", toml_string("rootmode")));
    lines.push(format!("base_url = {}", toml_string(&format!("{}/v1", ep.base_url))));
    // Codex dropped chat.completions support — "responses" is the only wire
    // format it still speaks, which is what gateway's /v1/responses answers.
    lines.push("wire_api = \"responses\"".into());
    lines.push(format!("env_key = {}", toml_string(CODEX_ENV_VAR)));
    write_text(&path, &format!("{}\n", lines.join("\n")))?;
    export_env_var(CODEX_ENV_VAR, &ep.token)?;
    let _ = state;
    Ok(())
}

fn disconnect_codex() -> Result<()> {
    let path = config_path(tool("codex")?)?;
    if path.exists() {
        let mut lines = codex_lines(&path);
        lines = remove_provider_table(&lines);
        lines = remove_top_level(&lines, "model_provider");
        write_text(&path, &format!("{}\n", lines.join("\n")))?;
    }
    unset_env_var(CODEX_ENV_VAR)
}

// ------------------------------------------------------------- claude code

fn connect_claude_code(ep: &Endpoint) -> Result<()> {
    let path = config_path(tool("claude-code")?)?;
    let mut config = read_json_lenient(&path);
    let root = as_object(&mut config);
    let env = as_object(root.entry("env").or_insert_with(|| Value::Object(Map::new())));
    env.insert("ANTHROPIC_BASE_URL".into(), Value::String(ep.base_url.clone()));
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), Value::String(ep.token.clone()));
    env.insert("ANTHROPIC_MODEL".into(), Value::String(ep.model.clone()));
    write_json_pretty(&path, &config)
}

fn disconnect_claude_code() -> Result<()> {
    let path = config_path(tool("claude-code")?)?;
    let mut config = read_json_lenient(&path);
    let root = as_object(&mut config);
    if let Some(Value::Object(env)) = root.get_mut("env") {
        for k in ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_MODEL"] {
            env.remove(k);
        }
        if env.is_empty() {
            root.remove("env");
        }
    }
    write_json_pretty(&path, &config)
}

// ---------------------------------------------------------------------- pi

fn connect_pi(ep: &Endpoint) -> Result<()> {
    let models_path = config_path(tool("pi")?)?;
    let settings_path = pi_settings_path()?;

    let mut models = read_json_lenient(&models_path);
    let models_root = as_object(&mut models);
    models_root.insert(
        PROVIDER_KEY.into(),
        serde_json::json!({
            "baseURL": format!("{}/v1", ep.base_url),
            "apiKey": ep.token,
            "api": "openai-completions",
            "models": [ep.model.clone()],
        }),
    );
    write_json_pretty(&models_path, &models)?;

    let mut settings = read_json_lenient(&settings_path);
    let settings_root = as_object(&mut settings);
    settings_root.insert("provider".into(), Value::String(PROVIDER_KEY.into()));
    settings_root.insert("model".into(), Value::String(ep.model.clone()));
    write_json_pretty(&settings_path, &settings)
}

fn disconnect_pi() -> Result<()> {
    let models_path = config_path(tool("pi")?)?;
    let mut models = read_json_lenient(&models_path);
    let models_root = as_object(&mut models);
    models_root.remove(PROVIDER_KEY);
    write_json_pretty(&models_path, &models)?;

    let settings_path = pi_settings_path()?;
    let mut settings = read_json_lenient(&settings_path);
    let settings_root = as_object(&mut settings);
    if matches!(settings_root.get("provider"), Some(Value::String(p)) if p == PROVIDER_KEY) {
        settings_root.remove("provider");
        settings_root.remove("model");
    }
    write_json_pretty(&settings_path, &settings)
}

// ------------------------------------------------------------------- crush

fn connect_crush(ep: &Endpoint) -> Result<()> {
    let path = config_path(tool("crush")?)?;
    let mut config = read_json_lenient(&path);
    let root = as_object(&mut config);
    let providers = as_object(root.entry("providers").or_insert_with(|| Value::Object(Map::new())));
    providers.insert(
        PROVIDER_KEY.into(),
        serde_json::json!({
            "type": "openai",
            "base_url": format!("{}/v1", ep.base_url),
            "api_key": ep.token,
            "models": [{ "id": ep.model.clone(), "name": ep.model.clone() }],
        }),
    );
    write_json_pretty(&path, &config)
}

fn disconnect_crush() -> Result<()> {
    let path = config_path(tool("crush")?)?;
    let mut config = read_json_lenient(&path);
    let root = as_object(&mut config);
    if let Some(Value::Object(providers)) = root.get_mut("providers") {
        providers.remove(PROVIDER_KEY);
    }
    write_json_pretty(&path, &config)
}

// ------------------------------------------------------------------- goose

/// Goose's config.yaml is flat `KEY: value` pairs, no nesting — edited the
/// same way Codex's TOML is, line by line, rather than pulling in a YAML
/// parser for four keys.
fn goose_set(lines: &[String], key: &str, value: &str) -> Vec<String> {
    let assignment = format!("{key}: {value}");
    match lines.iter().position(|l| l.trim_start().starts_with(&format!("{key}:"))) {
        Some(i) => {
            let mut out = lines.to_vec();
            out[i] = assignment;
            out
        }
        None => {
            let mut out = lines.to_vec();
            out.push(assignment);
            out
        }
    }
}

fn goose_remove(lines: &[String], key: &str) -> Vec<String> {
    lines
        .iter()
        .filter(|l| !l.trim_start().starts_with(&format!("{key}:")))
        .cloned()
        .collect()
}

fn connect_goose(ep: &Endpoint) -> Result<()> {
    let path = config_path(tool("goose")?)?;
    backup_once(&path)?;
    let mut lines = codex_lines(&path);
    lines = goose_set(&lines, "GOOSE_PROVIDER", "openai");
    lines = goose_set(&lines, "GOOSE_MODEL", &ep.model);
    lines = goose_set(&lines, "OPENAI_HOST", &ep.base_url);
    lines = goose_set(&lines, "OPENAI_API_KEY", &ep.token);
    write_text(&path, &format!("{}\n", lines.join("\n")))
}

fn disconnect_goose() -> Result<()> {
    let path = config_path(tool("goose")?)?;
    if !path.exists() {
        return Ok(());
    }
    let mut lines = codex_lines(&path);
    for key in ["GOOSE_PROVIDER", "GOOSE_MODEL", "OPENAI_HOST", "OPENAI_API_KEY"] {
        lines = goose_remove(&lines, key);
    }
    write_text(&path, &format!("{}\n", lines.join("\n")))
}

// --------------------------------------------------------------------- zed

/// Zed keeps API keys in its own keychain, not in settings.json — a connect
/// here only points it at the endpoint; Zed still prompts for the key once,
/// which is the gateway token.
fn connect_zed(ep: &Endpoint) -> Result<()> {
    let path = config_path(tool("zed")?)?;
    let mut config = read_json_lenient(&path);
    let root = as_object(&mut config);
    let lm = as_object(root.entry("language_models").or_insert_with(|| Value::Object(Map::new())));
    let openai_compat = as_object(lm.entry("openai_compatible").or_insert_with(|| Value::Object(Map::new())));
    openai_compat.insert(
        PROVIDER_KEY.into(),
        serde_json::json!({
            "api_url": format!("{}/v1", ep.base_url),
            "available_models": [{ "name": ep.model.clone(), "max_tokens": 32000 }],
        }),
    );
    write_json_pretty(&path, &config)
}

fn disconnect_zed() -> Result<()> {
    let path = config_path(tool("zed")?)?;
    let mut config = read_json_lenient(&path);
    let root = as_object(&mut config);
    if let Some(Value::Object(lm)) = root.get_mut("language_models") {
        if let Some(Value::Object(openai_compat)) = lm.get_mut("openai_compatible") {
            openai_compat.remove(PROVIDER_KEY);
        }
    }
    write_json_pretty(&path, &config)
}

// --------------------------------------------------------- shell rc export

const MARK_BEGIN: &str = "# >>> rootmode connected apps >>>";
const MARK_END: &str = "# <<< rootmode connected apps <<<";

fn shell_rc_path() -> Result<PathBuf> {
    let home = home_dir()?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.ends_with("zsh") {
        Ok(home.join(".zshrc"))
    } else if shell.ends_with("bash") {
        Ok(home.join(".bashrc"))
    } else if home.join(".zshrc").exists() {
        Ok(home.join(".zshrc"))
    } else {
        Ok(home.join(".profile"))
    }
}

/// Idempotently sets `export NAME=value` in a guarded block in the user's
/// shell rc file, the same pattern rustup/nvm/homebrew use, so a plain
/// `codex` run in a fresh terminal picks it up without this app being open.
///
/// On macOS this also does `launchctl setenv`: a GUI app opened via `open -a`
/// (Codex.app, say) is a launchd child, not a shell child, so it never reads
/// the rc file — this is what makes *that* process see the token too. It is
/// session-scoped and clears itself at logout, so nothing to clean up beyond
/// the explicit unset below.
fn export_env_var(name: &str, value: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("launchctl").args(["setenv", name, value]).status();
    }
    if cfg!(windows) {
        // Persists in the user's environment (registry-backed) for any
        // process started after this — this app's own launch button instead
        // sets it directly on that one child, since a `setx` here would not
        // reach a process spawned from this already-running one.
        let _ = std::process::Command::new("setx").args([name, value]).status();
        return Ok(());
    }
    let path = shell_rc_path()?;
    let existing = read_text(&path).unwrap_or_default();
    let block = format!("{MARK_BEGIN}\nexport {name}={}\n{MARK_END}", shell_quote(value));
    let next = replace_guarded_block(&existing, &block);
    write_text(&path, &next)
}

fn unset_env_var(name: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("launchctl").args(["unsetenv", name]).status();
    }
    if cfg!(windows) {
        // `setx` has no unset; clearing the value is the closest Windows
        // gets without touching the registry directly.
        let _ = std::process::Command::new("setx").args([name, ""]).status();
        return Ok(());
    }
    let path = shell_rc_path()?;
    let Some(existing) = read_text(&path) else {
        return Ok(());
    };
    let next = remove_guarded_block(&existing);
    write_text(&path, &next)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn replace_guarded_block(existing: &str, block: &str) -> String {
    let stripped = remove_guarded_block(existing);
    let trimmed = stripped.trim_end();
    if trimmed.is_empty() {
        format!("{block}\n")
    } else {
        format!("{trimmed}\n\n{block}\n")
    }
}

fn remove_guarded_block(existing: &str) -> String {
    let Some(start) = existing.find(MARK_BEGIN) else {
        return existing.to_string();
    };
    let Some(end_rel) = existing[start..].find(MARK_END) else {
        return existing.to_string();
    };
    let end = start + end_rel + MARK_END.len();
    format!("{}{}", &existing[..start], &existing[end..])
}

// -------------------------------------------------------------------- launch

/// Opens the tool itself, already pointed at the network: the desktop app if
/// `app_path` finds one, on any of the three platforms, otherwise a terminal
/// running its CLI with the gateway token set for that one process — the
/// fastest way to see a connect actually work, without waiting for a new
/// shell to pick up the rc file (POSIX) or requiring one at all (Windows,
/// which never reads it).
pub fn launch(key: &str, ep: &Endpoint) -> Result<()> {
    let t = tool(key)?;
    let cmd = t.binary;

    if let Some(app) = t.app_name {
        if let Some(path) = app_path(app) {
            launch_app(&path, app, ep)?;
            return Ok(());
        }
    }

    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "tell application \"Terminal\" to do script \"export ROOTMODE_GATEWAY_TOKEN={}; {}\"",
            shell_quote(&ep.token),
            cmd
        );
        std::process::Command::new("osascript").arg("-e").arg(script).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        let inner = format!("export ROOTMODE_GATEWAY_TOKEN={}; {}; exec $SHELL", shell_quote(&ep.token), cmd);
        let terminals: [(&str, &[&str]); 3] = [
            ("x-terminal-emulator", &["-e", "sh", "-c"]),
            ("gnome-terminal", &["--", "sh", "-c"]),
            ("xterm", &["-e", "sh", "-c"]),
        ];
        let mut launched = false;
        for (bin, args) in terminals {
            let mut command = std::process::Command::new(bin);
            command.args(args).arg(&inner);
            if command.spawn().is_ok() {
                launched = true;
                break;
            }
        }
        if !launched {
            return Err(AppError::Invalid("no terminal emulator found to launch into".into()));
        }
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "cmd", "/K"])
            .arg(format!("set ROOTMODE_GATEWAY_TOKEN={}&&{cmd}", ep.token))
            .spawn()?;
    }

    Ok(())
}

/// Opens a desktop app found by `app_path`, with the gateway token set
/// directly on that one process — not relying on `launchctl setenv` /
/// shell-rc timing (those, set in `export_env_var`, are for a terminal the
/// user opens later on their own; this is for the instant this app opens
/// one itself, which should not depend on session state propagating first).
/// A tool that reads its key from its config file instead (everything but
/// Codex) simply ignores the variable.
fn launch_app(path: &Path, app: &str, ep: &Endpoint) -> Result<()> {
    if cfg!(target_os = "macos") {
        // `open` cannot pass environment to what it launches; macOS apps
        // read `launchctl setenv` state instead, set in `export_env_var`.
        let _ = path;
        std::process::Command::new("open").args(["-a", app]).spawn()?;
    } else {
        // Windows and Linux: the resolved path is the executable itself
        // (`app_path`), not a bundle to hand to a launcher — run it
        // directly, detached, with the token in its own environment.
        std::process::Command::new(path)
            .env(CODEX_ENV_VAR, &ep.token)
            .spawn()?;
    }
    Ok(())
}
