//! Service registration for the honey daemon: a systemd **user** unit on
//! Linux, a launchd **agent** on macOS.
//!
//! Two entry points:
//!
//! * `honey service [--install|--uninstall|--status]` — explicit control.
//! * `ensure_first_run()` — called at the top of `main()` for every normal
//!   (non-`--internal-*`) invocation. On the first-ever run it installs the
//!   unit/agent **and enables it with `--now`**, so from that moment the
//!   daemon is always alive at login and never cold. The whole thing is
//!   silent, best-effort, and once-only (a marker file in the user's cache
//!   dir records that it ran, and for which binary path); any failure falls
//!   back to the historic on-demand `spawn_daemon()` path without a word.
//!
//! Env knobs: `HONEY_NO_DAEMON` or `HONEY_NO_SERVICE` (set non-empty) skip
//! everything — this is what the test harnesses set.

use std::process::Command;

// One reverse-DNS id on every platform; the systemd unit appends its
// `.service` suffix at use sites.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SERVICE_ID_BASE: &str = "io.github.axiomwtf.honey.daemon";
#[cfg(target_os = "macos")]
const SERVICE_ID: &str = SERVICE_ID_BASE;
#[cfg(not(target_os = "macos"))]
const SERVICE_ID: &str = "io.github.axiomwtf.honey.daemon.service";

/// Pre-rename names: cleaned up on install so an update from an older
/// honeybun never leaves the old unit enabled alongside (or instead of)
/// the current one.
#[cfg(target_os = "macos")]
const LEGACY_SERVICE_IDS: [&str; 1] = ["com.honey.daemon"];
#[cfg(not(target_os = "macos"))]
const LEGACY_SERVICE_IDS: [&str; 1] = ["honey-daemon.service"];

fn env_disabled() -> bool {
    for k in ["HONEY_NO_DAEMON", "HONEY_NO_SERVICE"] {
        if std::env::var(k).map(|v| !v.is_empty() && v != "0").unwrap_or(false) {
            return true;
        }
    }
    false
}

/// Best-effort user cache dir: XDG_CACHE_HOME or ~/.cache (Linux),
/// ~/Library/Caches (macOS).
fn cache_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    if home.is_empty() {
        return None;
    }
    #[cfg(target_os = "macos")]
    let base = std::path::PathBuf::from(home).join("Library/Caches");
    #[cfg(not(target_os = "macos"))]
    let base = match std::env::var("XDG_CACHE_HOME") {
        Ok(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => std::path::PathBuf::from(home).join(".cache"),
    };
    Some(base.join("honey"))
}

/// Directory that holds the unit / plist.
fn service_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    if home.is_empty() {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        Some(std::path::PathBuf::from(home).join("Library/LaunchAgents"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        match std::env::var("XDG_CONFIG_HOME") {
            Ok(d) if !d.is_empty() => Some(std::path::PathBuf::from(d).join("systemd/user")),
            _ => Some(std::path::PathBuf::from(home).join(".config/systemd/user")),
        }
    }
}

fn service_file() -> Option<std::path::PathBuf> {
    service_dir().map(|d| d.join(SERVICE_ID))
}

#[cfg(target_os = "macos")]
fn node_uid() -> String {
    std::env::var("UID").unwrap_or_else(|_| {
        Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "501".into())
    })
}

/// systemd user manager present? (A unit file alone is pointless without it,
/// e.g. containers, WSL1, non-systemd distros.)
#[cfg(not(target_os = "macos"))]
fn systemd_available() -> bool {
    std::path::Path::new("/run/systemd/system").exists()
}

/// Render the definition file contents for the given honey executable path.
#[cfg(target_os = "macos")]
fn plist_contents(exe: &std::path::Path) -> String {
    let exe = exe.display();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{SERVICE_ID}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Background</string>
</dict>
</plist>
"#
    )
}

#[cfg(not(target_os = "macos"))]
fn unit_contents(exe: &std::path::Path) -> String {
    let exe = exe.display();
    format!(
        r#"[Unit]
Description=honey daemon (warm JS/TS engine, always hot)
Documentation=honey stop|honey service --uninstall
After=default.target

[Service]
Type=simple
ExecStart={exe} serve
Restart=on-failure
RestartSec=1
# The daemon stops cleanly via `honey stop` and must stay stopped then.
SuccessExitStatus=0

[Install]
WantedBy=default.target
"#
    )
}

fn current_exe() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe()
}

fn run_quiet(cmd: &mut Command) -> bool {
    match cmd.output() {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

/// Best-effort removal of a pre-rename service definition (old id, same
/// dir): disable/stop it, then delete the file. Silence is the contract.
fn uninstall_legacy() {
    for id in LEGACY_SERVICE_IDS {
        let Some(file) = service_file().map(|f| f.with_file_name(id)) else { return };
        if !file.exists() {
            continue;
        }
        #[cfg(target_os = "macos")]
        {
            let _ = run_quiet(Command::new("launchctl").args(["bootout", &format!("gui/{}/{}", node_uid(), id)]));
            let _ = run_quiet(Command::new("launchctl").args(["unload", "-w"]).arg(&file));
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = &file; // unit file removal below; disable ends management
            if systemd_available() {
                let _ = run_quiet(Command::new("systemctl").args(["--user", "disable", "--now", id]));
                let _ = run_quiet(Command::new("systemctl").args(["--user", "daemon-reload"]));
            }
        }
        let _ = std::fs::remove_file(&file);
    }
}

/// Write the definition file and enable + start the service.
/// Returns true if it didn't blow an error when asked to be loud.
fn install_and_enable() -> bool {
    uninstall_legacy();
    let Some(dir) = service_dir() else { return false };
    let Ok(exe) = current_exe() else { return false };
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let file = dir.join(SERVICE_ID);
    #[cfg(target_os = "macos")]
    let contents = plist_contents(&exe);
    #[cfg(not(target_os = "macos"))]
    let contents = unit_contents(&exe);
    if std::fs::write(&file, contents).is_err() {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        // Try the modern bootstrap first, fall back to the legacy verb.
        if !run_quiet(Command::new("launchctl").args(["bootstrap", &format!("gui/{}", node_uid())])) {
            return run_quiet(Command::new("launchctl").args(["load", "-w"]).arg(&file));
        }
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        if !systemd_available() {
            return false;
        }
        if !run_quiet(Command::new("systemctl").args(["--user", "daemon-reload"])) {
            return false;
        }
        // enable --now: create the unit's start link and start it in one shot.
        run_quiet(Command::new("systemctl").args(["--user", "enable", "--now", SERVICE_ID]))
    }
}

#[cfg_attr(target_os = "macos", allow(unused_variables))]
fn uninstall_service() -> bool {
    let Some(file) = service_file() else { return false };
    #[cfg(target_os = "macos")]
    {
        let _ = run_quiet(Command::new("launchctl").args(["bootout", &format!("gui/{}/{}", node_uid(), SERVICE_ID)]));
        let _ = run_quiet(Command::new("launchctl").args(["unload", "-w"]).arg(&file));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = &file; // unit file removal below; disable ends management
        if systemd_available() {
            let _ = run_quiet(Command::new("systemctl").args(["--user", "disable", "--now", SERVICE_ID]));
            let _ = run_quiet(Command::new("systemctl").args(["--user", "daemon-reload"]));
        }
    }
    std::fs::remove_file(&file).is_ok()
}

/// Is the definition file present on disk?
pub fn registered() -> bool {
    service_file().map(|f| f.exists()).unwrap_or(false)
}

/// Marker: records the exe path the service was ensured for. Different path
/// (rebuild / cargo install elsewhere) -> re-ensure on the next run.
fn marker_path() -> Option<std::path::PathBuf> {
    cache_dir().map(|d| d.join("service-marker"))
}

fn marker_matches(exe: &std::path::Path) -> bool {
    marker_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| {
            let mut lines = s.lines().map(str::trim);
            // Second line records the daemon syntax the unit was rendered
            // with; `--internal-daemon` markers must re-install for `serve`.
            // Third line records the service id, so a renamed unit
            // (honey-daemon.service -> honeyd.service) re-installs too.
            lines.next() == Some(exe.to_string_lossy().trim())
                && lines.next() == Some("serve")
                && lines.next() == Some(SERVICE_ID)
        })
        .unwrap_or(false)
}

fn write_marker(exe: &std::path::Path) {
    if let Some(dir) = cache_dir() {
        if std::fs::create_dir_all(&dir).is_ok() {
            let body = format!("{}\nserve\n{}\n", exe.to_string_lossy(), SERVICE_ID);
            let _ = std::fs::write(dir.join("service-marker"), body.as_bytes());
        }
    }
}

/// First-run hook: install + `enable --now` the daemon service, silently,
/// exactly once per binary path. Returns true when a service-manager start
/// was initiated (the caller should then wait briefly for the socket before
/// forking its own daemon, to avoid a bind race). Never fatal; self-heals on
/// the next run.
pub fn ensure_first_run() -> bool {
    if env_disabled() {
        return false;
    }
    // Fast, silent exit for the common case: marker matches this binary.
    let Ok(exe) = current_exe() else { return false };
    if marker_matches(&exe) {
        return false;
    }
    if install_and_enable() {
        write_marker(&exe);
        return true;
    }
    // No service manager (container, WSL1, non-systemd): don't nag every
    // run either — write the marker so we stop trying.
    write_marker(&exe);
    false
}

/// `honey service`: explicit install / uninstall / status. Returns the
/// process exit code for the command.
pub fn cli(uninstall: bool, status_only: bool) -> i32 {
    if status_only {
        let reg = registered();
        let marker = marker_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| s.lines().next().map(str::trim).map(str::to_string))
            .unwrap_or_default();
        #[cfg(target_os = "macos")]
        let kind = "launchd agent";
        #[cfg(not(target_os = "macos"))]
        let kind = "systemd user unit";
        println!("honey service");
        println!("  kind: {kind} ({SERVICE_ID})");
        println!("  installed: {reg}");
        if reg {
            if let Some(f) = service_file() {
                println!("  file: {}", f.display());
            }
        }
        println!("  last ensured for: {}", if marker.is_empty() { "<never>" } else { marker.trim() });
        return 0;
    }
    if uninstall {
        if uninstall_service() {
            // Clear the marker so the next run re-ensures (only if the user
            // asks to uninstall should we not fight them — drop the marker).
            if let Some(p) = marker_path() {
                let _ = std::fs::remove_file(p);
            }
            println!("honey daemon service uninstalled");
            0
        } else {
            eprintln!("error: no honey daemon service installed");
            1
        }
    } else if install_and_enable() {
        let exe = current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        write_marker(std::path::Path::new(&exe));
        println!("honey daemon service enabled (now)");
        0
    } else {
        eprintln!("error: could not install honey daemon service");
        1
    }
}
