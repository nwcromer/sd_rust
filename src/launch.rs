//! Launch-application action + application icon resolution.
//!
//! Launch-an-application only — we resolve a `.desktop` entry and run its
//! `Exec`. (Limited to installed apps to keep the trust surface small.)
//!
//! Launching goes through `systemd-run --user`, which asks the user's systemd
//! manager to start the app as a fresh transient unit. This matters for two
//! reasons:
//!   1. The app escapes sd_rust's own service sandbox. A direct fork/exec child
//!      would inherit `MemoryDenyWriteExecute=yes` (crashes JIT apps like
//!      browsers), `ProtectHome=read-only`, etc.
//!   2. The app's lifetime is decoupled from the daemon — restarting sd_rust
//!      doesn't kill launched apps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use log::{debug, warn};

/// Environment variables forwarded to launched apps so GUI programs find the
/// Wayland/X11 display and session bus even if the user manager's environment
/// is sparse. Only forwarded when set in our environment.
const FORWARD_ENV: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
];

/// Icon extensions we can decode: raster (via the `image` crate) plus SVG (via
/// `resvg`, see `render::load_icon`). Other formats (e.g. XPM) are skipped.
const DECODABLE_EXTS: &[&str] =
    &["png", "jpg", "jpeg", "bmp", "gif", "webp", "tiff", "tif", "ico", "svg", "svgz"];

/// Launch the app, or — if it's already running — raise its existing window
/// instead of starting a second copy (§5.1, launch-or-raise).
///
/// Decision is by a process check (reliable for native apps; Flatpak/tray-only
/// are the fuzzy cases, covered by `window_class` + falling back to launch).
/// Raising goes through KWin over D-Bus (see [`raise_window`]) — no external
/// tool, no new dependency.
pub fn launch_or_raise(app: &str, window_class: Option<&str>) -> Result<()> {
    let desktop = find_desktop_file(app);
    let entry = desktop.as_deref().and_then(|p| parse_desktop_entry(p).ok());

    // The process name to look for = the .desktop Exec's basename (falling back
    // to `app` itself if there's no .desktop).
    let exec_name = entry
        .as_ref()
        .and_then(|e| e.get("Exec"))
        .and_then(|exec| exec_argv(exec).into_iter().next())
        .and_then(|arg0| {
            Path::new(&arg0)
                .file_name()
                .and_then(|s| s.to_str())
                .map(String::from)
        })
        .unwrap_or_else(|| app.to_string());

    if is_running(&exec_name) {
        let classes = window_class_candidates(app, entry.as_ref(), &exec_name, window_class);
        match raise_window(&classes) {
            Ok(()) => {
                debug!("{app}: already running — raised existing window");
                return Ok(());
            }
            // If raising fails (KWin unreachable), fall back to launching so the
            // button still does something useful.
            Err(e) => warn!("{app}: raise failed ({e:#}); launching instead"),
        }
    }
    launch(app)
}

/// Window-class candidates to match a running window against (case-insensitive,
/// sanitized, deduped): explicit override + app id + `StartupWMClass` + exec.
fn window_class_candidates(
    app: &str,
    entry: Option<&HashMap<String, String>>,
    exec_name: &str,
    window_class: Option<&str>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let c = sanitize_class(s);
        if !c.is_empty() && !out.contains(&c) {
            out.push(c);
        }
    };
    if let Some(wc) = window_class {
        push(wc);
    }
    push(app);
    // The desktop-file stem (id) — same as `app` unless `app` was a name.
    push(app.trim_end_matches(".desktop"));
    if let Some(wmclass) = entry.and_then(|e| e.get("StartupWMClass")) {
        push(wmclass);
    }
    push(exec_name);
    out
}

/// Keep only characters that safely appear in a window class, lowercased — so a
/// class name can be embedded in the generated KWin script without escaping.
fn sanitize_class(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Is a process named `exec_name` (by argv[0] basename) currently running?
fn is_running(exec_name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        // Only numeric (pid) directories.
        if !name.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        // argv is NUL-separated; compare argv[0]'s basename to exec_name.
        let cmdline = entry.path().join("cmdline");
        if let Ok(bytes) = std::fs::read(&cmdline)
            && let Some(arg0) = bytes.split(|&b| b == 0).next()
            && let Ok(arg0) = std::str::from_utf8(arg0)
            && Path::new(arg0).file_name().and_then(|s| s.to_str()) == Some(exec_name)
        {
            return true;
        }
    }
    false
}

/// Ask KWin (over the session bus) to raise the first window whose class
/// matches one of `candidates`. We hand KWin a tiny generated script via its
/// `/Scripting` interface — the only Wayland-safe way to activate another app's
/// window, and it uses the `zbus` dependency we already have.
fn raise_window(candidates: &[String]) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    let script_path = write_raise_script(candidates)?;
    let path_str = script_path.to_str().context("script path is not UTF-8")?;

    let conn = zbus::blocking::Connection::session().context("connecting to the session bus")?;
    let load = |method: &str| {
        conn.call_method(
            Some("org.kde.KWin"),
            "/Scripting",
            Some("org.kde.kwin.Scripting"),
            method,
            &(path_str,),
        )
    };

    // loadScript(path) -> script id; start() runs it; unloadScript(path) cleans
    // up so repeated presses don't accumulate scripts in KWin.
    load("loadScript").context("KWin loadScript failed")?;
    conn.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "start",
        &(),
    )
    .context("KWin start failed")?;
    let _ = load("unloadScript");
    Ok(())
}

/// Write the KWin activation script to the runtime dir (readable by KWin — the
/// daemon's `RuntimeDirectory=sd_rust` makes this writable under the sandbox).
fn write_raise_script(candidates: &[String]) -> Result<PathBuf> {
    let dir = runtime_dir().join("sd_rust");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("raise.js");

    // candidates are sanitized to [a-z0-9._+-], so quoting them as JS strings is
    // safe. Plasma 6 uses windowList()/activeWindow; Plasma 5 clientList()/activeClient.
    let targets = candidates
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!(
        "const targets = [{targets}];\n\
         const wins = workspace.windowList ? workspace.windowList() : workspace.clientList();\n\
         for (const w of wins) {{\n\
         \x20   const cls = (w.resourceClass || \"\").toString().toLowerCase();\n\
         \x20   if (targets.indexOf(cls) !== -1) {{\n\
         \x20       w.minimized = false;\n\
         \x20       if (\"activeWindow\" in workspace) {{ workspace.activeWindow = w; }}\n\
         \x20       else {{ workspace.activeClient = w; }}\n\
         \x20       break;\n\
         \x20   }}\n\
         }}\n"
    );
    std::fs::write(&path, script).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    // SAFETY: geteuid() has no preconditions and no side effects.
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/run/user/{uid}"))
}

/// Launch the application identified by `app` (a `.desktop` id or name).
pub fn launch(app: &str) -> Result<()> {
    let desktop = find_desktop_file(app)
        .with_context(|| format!("no .desktop entry found for {app:?}"))?;
    let entry = parse_desktop_entry(&desktop)
        .with_context(|| format!("failed to read {}", desktop.display()))?;
    let exec = entry
        .get("Exec")
        .with_context(|| format!("{} has no Exec= line", desktop.display()))?;

    let argv = exec_argv(exec);
    if argv.is_empty() {
        bail!("Exec line in {} is empty after removing field codes", desktop.display());
    }

    let mut cmd = Command::new("systemd-run");
    cmd.args(["--user", "--collect", "--quiet"]);
    cmd.arg(format!("--description=sd_rust launch: {app}"));
    for var in FORWARD_ENV {
        if let Ok(val) = std::env::var(var) {
            cmd.arg(format!("--setenv={var}={val}"));
        }
    }
    cmd.arg("--");
    cmd.args(&argv);

    debug!("launching {app}: systemd-run --user -- {}", argv.join(" "));
    let status = cmd
        .status()
        .context("failed to spawn systemd-run (is systemd user instance running?)")?;
    if !status.success() {
        bail!("systemd-run exited with {status} while launching {app}");
    }
    Ok(())
}

/// Resolve a decodable raster icon **file path** for an app, for the renderer
/// to load. Returns `None` if no `.desktop`/themed icon resolves to a format we
/// can decode (caller falls back to blank).
pub fn resolve_icon_path(app: &str) -> Option<PathBuf> {
    let desktop = find_desktop_file(app)?;
    let entry = parse_desktop_entry(&desktop).ok()?;
    let icon = entry.get("Icon")?;
    debug!("icon resolve {app:?}: desktop={}, Icon={icon:?}", desktop.display());

    // An absolute path in Icon= is used directly; otherwise it's a themed icon
    // name to resolve via the icon theme (falling back through hicolor).
    let candidate = if Path::new(icon).is_absolute() {
        PathBuf::from(icon)
    } else {
        // Request a generously large size so the 72×72 downscale stays crisp.
        freedesktop_icons::lookup(icon).with_size(256).find()?
    };
    debug!("icon resolve {app:?}: candidate={candidate:?}");

    if is_decodable(&candidate) {
        Some(candidate)
    } else {
        log::warn!(
            "icon for {app:?} resolved to {} which can't be decoded; \
             set an explicit `icon = \"…\"` (PNG/JPG/SVG) for this key",
            candidate.display()
        );
        None
    }
}

fn is_decodable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| DECODABLE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// XDG application directories, in precedence order ($XDG_DATA_HOME first).
fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(data_home) = dirs::data_dir() {
        dirs.push(data_home.join("applications"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
        dirs.push(Path::new(d).join("applications"));
    }
    dirs
}

/// Find the `.desktop` file for `app`. Tries an exact `<app>.desktop` (the id)
/// in each applications dir first, then a case-insensitive filename match.
fn find_desktop_file(app: &str) -> Option<PathBuf> {
    let wanted_file = if app.ends_with(".desktop") {
        app.to_string()
    } else {
        format!("{app}.desktop")
    };

    let app_dirs = application_dirs();
    for dir in &app_dirs {
        let candidate = dir.join(&wanted_file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // Fallback: scan for a file whose stem matches case-insensitively (handles
    // e.g. `firefox` → `Firefox.desktop` or vendor-prefixed ids).
    let stem = wanted_file.trim_end_matches(".desktop").to_ascii_lowercase();
    for dir in &app_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|s| s.to_str())
                && (name.to_ascii_lowercase() == stem
                    || name.to_ascii_lowercase().ends_with(&format!(".{stem}")))
            {
                return Some(path);
            }
        }
    }
    None
}

/// Parse the `[Desktop Entry]` group of a `.desktop` file into key→value pairs.
/// Stops at the next group header (we only care about the main entry).
fn parse_desktop_entry(path: &Path) -> Result<HashMap<String, String>> {
    let content = std::fs::read_to_string(path)?;
    let mut map = HashMap::new();
    let mut in_entry = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            // Only keep the first occurrence; ignore locale-suffixed keys like
            // `Name[de]` by storing them too — harmless, we look up bare keys.
            map.entry(key.trim().to_string())
                .or_insert_with(|| value.trim().to_string());
        }
    }
    Ok(map)
}

/// Turn a `.desktop` `Exec` value into an argv vector: strip the field codes
/// (`%u`, `%F`, …) the spec defines, unescape `%%`, and split on whitespace
/// honoring simple double-quoting.
fn exec_argv(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = exec.chars().peekable();
    let mut started = false;

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                started = true;
            }
            '%' => {
                // Field code: consume the following code char. `%%` → literal %.
                match chars.next() {
                    Some('%') => {
                        current.push('%');
                        started = true;
                    }
                    Some(_) => {} // drop %u/%f/%F/%U/%i/%c/%k/etc.
                    None => {}
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_field_codes_and_splits() {
        assert_eq!(exec_argv("/usr/bin/firefox %u"), vec!["/usr/bin/firefox"]);
        assert_eq!(
            exec_argv("flatpak run --branch=stable org.mozilla.firefox @@u %U @@"),
            vec!["flatpak", "run", "--branch=stable", "org.mozilla.firefox", "@@u", "@@"]
        );
        assert_eq!(exec_argv("foo %% bar"), vec!["foo", "%", "bar"]);
    }

    #[test]
    fn honors_double_quotes() {
        assert_eq!(
            exec_argv(r#"/opt/My App/run --flag "two words" %f"#),
            vec!["/opt/My", "App/run", "--flag", "two words"]
        );
    }

    #[test]
    fn sanitizes_window_classes() {
        assert_eq!(sanitize_class("com.obsproject.Studio"), "com.obsproject.studio");
        assert_eq!(sanitize_class("Emacs"), "emacs");
        // Strips anything that could break out of a JS string literal.
        assert_eq!(sanitize_class(r#"a"; evil()//"#), "aevil");
        assert_eq!(sanitize_class("dev.zed.Zed"), "dev.zed.zed");
    }

    #[test]
    fn builds_deduped_class_candidates() {
        let mut entry = HashMap::new();
        entry.insert("StartupWMClass".to_string(), "obs".to_string());
        let c = window_class_candidates("com.obsproject.Studio", Some(&entry), "obs", None);
        assert!(c.contains(&"com.obsproject.studio".to_string()));
        assert!(c.contains(&"obs".to_string()));
        // `obs` from both StartupWMClass and exec name appears once.
        assert_eq!(c.iter().filter(|x| *x == "obs").count(), 1);

        // Explicit override is included and comes first.
        let c2 = window_class_candidates("firefox", None, "firefox", Some("MyClass"));
        assert_eq!(c2.first().unwrap(), "myclass");
    }

    #[test]
    fn is_running_false_for_bogus_name() {
        assert!(!is_running("sd_rust_no_such_process_xyzzy"));
    }

    #[test]
    fn decodable_detection() {
        assert!(is_decodable(Path::new("/x/icon.png")));
        assert!(is_decodable(Path::new("/x/icon.JPEG")));
        assert!(is_decodable(Path::new("/x/icon.svg"))); // SVG now supported
        assert!(!is_decodable(Path::new("/x/icon.xpm")));
        assert!(!is_decodable(Path::new("/x/icon")));
    }
}
