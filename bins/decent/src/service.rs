//! Daemon supervision, per platform.
//!
//! `decent install` runs the node unattended. How that is arranged is the one
//! genuinely OS-specific part of the CLI — everything else, including the token
//! at `~/.config/decent/` and the whole of `supervisor-core`, is already
//! portable. Keeping it behind this interface is what stops `main.rs` growing a
//! second copy of every command with `#[cfg]` sprinkled through it.
//!
//! **macOS** — a launchd user agent (`~/Library/LaunchAgents/*.plist`).
//! **Linux** — a systemd *user* unit (`~/.config/systemd/user/decent.service`).
//!
//! The two line up more closely than they look:
//!
//! | intent | launchd | systemd --user |
//! | --- | --- | --- |
//! | start at login | `RunAtLoad` | `WantedBy=default.target` |
//! | restart on exit | `KeepAlive` | `Restart=always` |
//! | stop, stay installed | `bootout` | `stop` (unit stays enabled) |
//! | remove | delete plist | `disable` + delete unit |
//!
//! One real difference: a launchd agent starts at login, while a systemd user
//! manager is torn down when the last session ends unless the user has
//! *lingering* enabled. A headless render box has no interactive session, so
//! without linger the node simply never comes back after a reboot. `install`
//! turns it on and reports whether it succeeded, because a silent failure here
//! looks exactly like a working install right up until the machine restarts.

use std::path::{Path, PathBuf};

/// Everything needed to write a unit.
pub struct ServiceSpec {
    pub exe: PathBuf,
    pub dispatch_url: String,
    pub log_path: PathBuf,
}

/// Where the daemon stands, as `decent status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    NotInstalled,
    Running,
    /// Installed but stopped. Comes back on reboot — `uninstall` is the off
    /// switch that survives one.
    Paused,
}

pub struct InstallReport {
    /// The plist or `.service` file that was written.
    pub unit_path: PathBuf,
    /// Platform-specific things the operator needs to be told.
    pub notes: Vec<String>,
}

/// Human name for the supervision system, for messages.
pub const fn manager_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "launchd"
    }
    #[cfg(target_os = "linux")]
    {
        "systemd"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "an unsupported service manager"
    }
}

pub use imp::{install, manual_restart_hint, pause, restart, resume, state, uninstall};

/// One systemd ExecStart argument, double-quoted per systemd.syntax: `\`
/// and `"` are backslash-escaped inside the quotes, and a literal `%` is
/// written as `%%` (systemd specifier escape — `%h`, `%t` and friends must
/// not expand inside an operator-supplied URL or install path).
/// Compiled on Linux (the unit builder uses it) and under test on every
/// host, so the exact line shape is pinned even from a macOS checkout.
#[cfg(any(target_os = "linux", test))]
fn quote_exec_arg(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for c in arg.chars() {
        match c {
            '\\' | '"' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// The systemd `ExecStart` line for the node's user unit. String building
/// is platform-free and test-pinned on every host; only the Linux unit
/// writer consumes it in production. Every argument is quoted (D-13): an
/// unquoted space in a URL or install path would word-split into arguments
/// the daemon never sees.
#[cfg(any(target_os = "linux", test))]
fn exec_start_line(exe: &Path, dispatch_url: &str) -> String {
    format!(
        "ExecStart={} {} {} {} {}",
        quote_exec_arg(&exe.display().to_string()),
        quote_exec_arg("start"),
        quote_exec_arg("--dispatch-url"),
        quote_exec_arg(dispatch_url),
        quote_exec_arg("--allow-real-jobs"),
    )
}

// ── macOS: launchd ──────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod imp {
    use super::{DaemonState, InstallReport, ServiceSpec};
    use std::path::{Path, PathBuf};

    pub(super) const LABEL: &str = "com.decent-render.decent";
    const LEGACY_LABEL: &str = "com.decent-render.decent-node";

    fn launch_agents_dir() -> anyhow::Result<PathBuf> {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
        let dir = PathBuf::from(home).join("Library/LaunchAgents");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn unit_path() -> anyhow::Result<PathBuf> {
        Ok(launch_agents_dir()?.join(format!("{LABEL}.plist")))
    }

    fn legacy_plist_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        let path = PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(format!("{LEGACY_LABEL}.plist"));
        path.exists().then_some(path)
    }

    /// Current numeric UID, for the `gui/<uid>/<label>` service target.
    fn current_uid() -> Option<String> {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn launchctl_list() -> String {
        std::process::Command::new("launchctl")
            .arg("list")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    /// PACKET 40 (audit 7): launchctl prints TAB-separated rows
    /// (`PID\tStatus\tLabel`). Substring matching was wrong in BOTH
    /// directions: LEGACY_LABEL ("com.decent-render.decent-node") contains
    /// LABEL as a substring, so `contains(LABEL)` was true whenever the
    /// legacy agent was loaded — `legacy_daemon_is_loaded()` could never
    /// fire and `is_loaded()` reported Running for a legacy-only install,
    /// making `decent pause` contradict `decent status`. Match the label
    /// as an EXACT trailing field of a row.
    pub(super) fn label_is_loaded(out: &str, label: &str) -> bool {
        out.lines().any(|line| {
            line.rsplit('\t')
                .next()
                .is_some_and(|field| field.trim() == label)
        })
    }

    fn is_loaded() -> bool {
        let out = launchctl_list();
        label_is_loaded(&out, LABEL) || label_is_loaded(&out, LEGACY_LABEL)
    }

    /// Is ONLY the legacy decent-node daemon loaded (not the new one)?
    pub(super) fn legacy_daemon_is_loaded() -> bool {
        let out = launchctl_list();
        label_is_loaded(&out, LEGACY_LABEL) && !label_is_loaded(&out, LABEL)
    }

    /// Unload the legacy decent-node agent if present (one-time migration
    /// during the decent-node → decent rename). No-op if not found.
    fn unload_legacy_agent() {
        let _ = std::process::Command::new("launchctl")
            .args([
                "bootout",
                &format!("gui/{}", current_uid().unwrap_or_default()),
                LEGACY_LABEL,
            ])
            .output();
    }

    /// Runs `decent start --allow-real-jobs` at login, restarts on exit.
    /// PACKET 40 (audit 8): XML-escape every interpolated value. A legal
    /// `?a=1&b=2` dispatch URL (or any path with `&`/`<`) produced an
    /// INVALID plist launchd silently refuses to load. The old test passed
    /// only because its URL was benign.
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn build_plist(exe: &Path, dispatch_url: &str, log_path: &Path) -> String {
        let exe_str = exe.to_string_lossy();
        let mut args = String::new();
        for arg in [
            exe_str.as_ref(),
            "start",
            "--dispatch-url",
            dispatch_url,
            "--allow-real-jobs",
        ] {
            args.push_str("        <string>");
            args.push_str(&xml_escape(arg));
            args.push_str("</string>\n");
        }
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ExitTimeOut</key>
    <integer>30</integer>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
</dict>
</plist>
"#,
            label = LABEL,
            log = xml_escape(&log_path.display().to_string()),
        )
    }

    /// PACKET 41 (step 1): the pre-install unload exists for clean
    /// reinstalls; on a first install there is nothing to unload and
    /// launching it anyway prints launchctl's "Load failed" noise as the
    /// command's first output.
    pub(super) fn should_attempt_pre_install_unload(plist: &Path) -> bool {
        plist.exists()
    }

    pub fn install(spec: &ServiceSpec) -> anyhow::Result<InstallReport> {
        unload_legacy_agent();
        let plist = unit_path()?;
        let xml = build_plist(&spec.exe, &spec.dispatch_url, &spec.log_path);
        // PACKET 41: only attempt the pre-install unload when a plist
        // ALREADY exists (a reinstall). On a first install there is nothing
        // to unload, and launchctl prints "Load failed: 5: Input/output
        // error" noise as the command's very first output. A genuine
        // unload failure during a REAL reinstall stays reported, with the
        // case named.
        if should_attempt_pre_install_unload(&plist) {
            let unload = std::process::Command::new("launchctl")
                .args(["unload", &plist.to_string_lossy()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output();
            if let Ok(out) = unload {
                if !out.status.success() {
                    eprintln!(
                        "Warning: launchctl unload of the existing daemon failed (reinstall): {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            }
        }
        std::fs::write(&plist, xml)?;
        // PACKET 41: `launchctl load` is legacy-but-quiet; it also prints
        // "Load failed: 5: Input/output error" to STDERR while still
        // succeeding through the modern bootstrap path in some states,
        // which surfaced as the scary first line of a first install.
        // `launchctl bootstrap` is the documented modern form; capture its
        // output and only surface it when it actually fails.
        let uid = current_uid().unwrap_or_default();
        let bootstrap = std::process::Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()?;
        if !bootstrap.status.success() {
            // bootout-then-bootstrap handles the reinstall case (already
            // loaded); only then is it a real failure.
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}"), LABEL])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let retry = std::process::Command::new("launchctl")
                .args(["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output()?;
            if !retry.status.success() {
                anyhow::bail!(
                    "launchctl bootstrap failed: {}. Inspect the plist at {}",
                    String::from_utf8_lossy(&retry.stderr).trim(),
                    plist.display()
                );
            }
        }

        let mut notes = vec![format!("agent: {LABEL}")];
        // Remove the old plist so it doesn't reload at next login.
        if let Some(legacy) = legacy_plist_path() {
            let _ = std::fs::remove_file(&legacy);
            notes.push(format!("Removed legacy plist: {}", legacy.display()));
        }
        Ok(InstallReport {
            unit_path: plist,
            notes,
        })
    }

    pub fn uninstall() -> anyhow::Result<PathBuf> {
        let plist = unit_path()?;
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist.to_string_lossy()])
            .status();
        if plist.exists() {
            std::fs::remove_file(&plist)?;
        }
        Ok(plist)
    }

    pub fn state() -> DaemonState {
        let present = unit_path().map(|p| p.exists()).unwrap_or(false);
        // PACKET 40: launchctl listing is MACHINE-GLOBAL — another user's
        // (or a leftover, or this very machine's REAL daemon while testing
        // under a different HOME) loaded agent with our label must not make
        // THIS home's status say "running". The unit FILE is the per-home
        // truth: no file here ⇒ not installed here, whatever launchd lists.
        if !present {
            return DaemonState::NotInstalled;
        }
        if is_loaded() {
            DaemonState::Running
        } else {
            DaemonState::Paused
        }
    }

    pub fn pause() -> anyhow::Result<bool> {
        let plist = unit_path()?;
        if !plist.exists() {
            anyhow::bail!("No launchd agent installed — run `decent install` first.");
        }
        let uid = current_uid().ok_or_else(|| anyhow::anyhow!("Could not determine UID."))?;
        // bootout returns non-zero if the agent isn't loaded — that's "already
        // stopped", not an error.
        Ok(std::process::Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{LABEL}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false))
    }

    pub fn resume() -> anyhow::Result<()> {
        let plist = unit_path()?;
        if !plist.exists() {
            anyhow::bail!("No launchd agent installed — run `decent install` first.");
        }
        let uid = current_uid().ok_or_else(|| anyhow::anyhow!("Could not determine UID."))?;
        let bootstrapped = std::process::Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if bootstrapped {
            return Ok(());
        }
        // bootstrap fails if already loaded — kick it instead.
        let kicked = std::process::Command::new("launchctl")
            .args(["kickstart", &format!("gui/{uid}/{LABEL}")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if kicked {
            Ok(())
        } else {
            anyhow::bail!("Could not resume the daemon. Try `decent install` to reload it.")
        }
    }

    /// Restart so the new binary is picked up. `Ok(false)` means "not loaded",
    /// which is informational rather than a failure.
    pub fn restart() -> anyhow::Result<bool> {
        if !is_loaded() {
            return Ok(false);
        }
        let uid = current_uid().ok_or_else(|| anyhow::anyhow!("Could not determine UID."))?;
        let target = format!("gui/{uid}/{LABEL}");
        // KeepAlive makes `kickstart -k` sufficient: kill and relaunch.
        let kicked = std::process::Command::new("launchctl")
            .args(["kickstart", "-k", &target])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !kicked {
            anyhow::bail!("launchctl kickstart failed");
        }
        Ok(true)
    }

    pub fn manual_restart_hint() -> String {
        format!("launchctl kickstart -k gui/$(id -u)/{LABEL}")
    }

    #[cfg(test)]
    pub(super) fn render_for_test(spec: &ServiceSpec) -> String {
        build_plist(&spec.exe, &spec.dispatch_url, &spec.log_path)
    }
}

// ── Linux: systemd user unit ────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod imp {
    use super::{DaemonState, InstallReport, ServiceSpec};
    use std::path::PathBuf;

    pub(super) const UNIT: &str = "decent.service";

    fn unit_dir() -> anyhow::Result<PathBuf> {
        // XDG_CONFIG_HOME when set, else ~/.config — the same rule the token
        // file follows, so both land in the same place under a custom XDG root.
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => {
                let home =
                    std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
                PathBuf::from(home).join(".config")
            }
        };
        let dir = base.join("systemd/user");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn unit_path() -> anyhow::Result<PathBuf> {
        Ok(unit_dir()?.join(UNIT))
    }

    /// `systemctl --user <args>`, returning (success, trimmed stdout).
    fn systemctl(args: &[&str]) -> (bool, String) {
        let mut full = vec!["--user"];
        full.extend_from_slice(args);
        match std::process::Command::new("systemctl").args(&full).output() {
            Ok(out) => (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            ),
            Err(_) => (false, String::new()),
        }
    }

    /// Is there a usable per-user systemd instance?
    ///
    /// Checked before writing anything: containers, WSL without systemd, and
    /// distros using another init all fail here, and a unit file written into a
    /// directory nothing reads is worse than an explicit refusal.
    fn require_systemd() -> anyhow::Result<()> {
        if std::process::Command::new("systemctl")
            .arg("--version")
            .output()
            .is_err()
        {
            anyhow::bail!(
                "`systemctl` not found — this build installs a systemd user unit.\n\
                 Run `decent start --allow-real-jobs` under your own supervisor instead."
            );
        }
        // `is-system-running` reports degraded/starting states too; any answer
        // at all means a user manager responded.
        let (ok, _) = systemctl(&["is-system-running"]);
        let (ping, _) = systemctl(&["show", "--property=Version"]);
        if !ok && !ping {
            anyhow::bail!(
                "No systemd user instance is available (`systemctl --user` failed).\n\
                 On a headless box this usually means the user session is not set up;\n\
                 see `loginctl enable-linger`, or run `decent start` under your own supervisor."
            );
        }
        Ok(())
    }

    fn build_unit(spec: &ServiceSpec) -> String {
        // `append:` needs systemd 240 (2018 — Debian 10, Ubuntu 20.04). It keeps
        // decent.log meaningful on both platforms rather than making Linux
        // operators learn journalctl to read the same lines a Mac writes to a
        // file. The journal still gets them too.
        format!(
            "[Unit]\n\
             Description=Decent render network node supervisor\n\
             Documentation=https://github.com/decent-render/decent-render\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exec_start}\n\
             Restart=always\n\
             RestartSec=10\n\
             TimeoutStopSec=30\n\
             Environment=RUST_LOG=info\n\
             StandardOutput=append:{log}\n\
             StandardError=append:{log}\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            exec_start = exec_start_line(&spec.exe, &spec.dispatch_url),
            log = spec.log_path.display(),
        )
    }

    /// Ask logind to keep this user's manager alive without a login session.
    ///
    /// Returns a note describing what actually happened. Best-effort by design:
    /// on some distros polkit requires admin auth for this, and failing to
    /// enable linger must not fail the install — but it must not be silent
    /// either, because the symptom is "works until you reboot".
    fn enable_linger() -> String {
        let attempted = std::process::Command::new("loginctl")
            .arg("enable-linger")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        let user = std::process::Command::new("id")
            .arg("-un")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        // Verify rather than trust the exit code.
        let lingering = std::process::Command::new("loginctl")
            .args(["show-user", &user, "--property=Linger"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("Linger=yes"))
            .unwrap_or(false);

        if lingering {
            "linger: enabled — the node starts at boot without a login session".to_string()
        } else if attempted {
            "⚠ linger: could not be confirmed. Without it the node stops when your \
             session ends. Run: loginctl enable-linger"
                .to_string()
        } else {
            "⚠ linger: NOT enabled — the node will not survive logout or reboot. \
             Run: sudo loginctl enable-linger $USER"
                .to_string()
        }
    }

    pub fn install(spec: &ServiceSpec) -> anyhow::Result<InstallReport> {
        require_systemd()?;
        let path = unit_path()?;
        std::fs::write(&path, build_unit(spec))?;

        let (reloaded, _) = systemctl(&["daemon-reload"]);
        if !reloaded {
            anyhow::bail!(
                "`systemctl --user daemon-reload` failed; inspect the unit at {}",
                path.display()
            );
        }
        let (enabled, err) = systemctl(&["enable", "--now", UNIT]);
        if !enabled {
            anyhow::bail!(
                "`systemctl --user enable --now {UNIT}` failed{}; inspect the unit at {}",
                if err.is_empty() {
                    String::new()
                } else {
                    format!(" ({err})")
                },
                path.display()
            );
        }

        Ok(InstallReport {
            unit_path: path,
            notes: vec![
                format!("unit: {UNIT} (systemd --user)"),
                enable_linger(),
                format!("logs: journalctl --user -u {UNIT} -f"),
            ],
        })
    }

    pub fn uninstall() -> anyhow::Result<PathBuf> {
        let path = unit_path()?;
        let _ = systemctl(&["disable", "--now", UNIT]);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let _ = systemctl(&["daemon-reload"]);
        Ok(path)
    }

    pub fn state() -> DaemonState {
        let present = unit_path().map(|p| p.exists()).unwrap_or(false);
        if !present {
            return DaemonState::NotInstalled;
        }
        // is-active exits non-zero when inactive, so read the word, not status.
        let (_, status) = systemctl(&["is-active", UNIT]);
        if status == "active" || status == "activating" {
            DaemonState::Running
        } else {
            DaemonState::Paused
        }
    }

    pub fn pause() -> anyhow::Result<bool> {
        if !unit_path()?.exists() {
            anyhow::bail!("No systemd unit installed — run `decent install` first.");
        }
        let was_running = state() == DaemonState::Running;
        // `stop` leaves the unit ENABLED, so it returns after a reboot — the
        // same semantics as launchd reloading a booted-out agent at login.
        // `uninstall` remains the off switch that survives a restart.
        let (ok, _) = systemctl(&["stop", UNIT]);
        if !ok && was_running {
            anyhow::bail!("`systemctl --user stop {UNIT}` failed");
        }
        Ok(was_running)
    }

    pub fn resume() -> anyhow::Result<()> {
        if !unit_path()?.exists() {
            anyhow::bail!("No systemd unit installed — run `decent install` first.");
        }
        let (ok, err) = systemctl(&["start", UNIT]);
        if !ok {
            anyhow::bail!(
                "Could not resume the daemon{}. Try `decent install` to reinstall the unit.",
                if err.is_empty() {
                    String::new()
                } else {
                    format!(" ({err})")
                }
            );
        }
        Ok(())
    }

    pub fn restart() -> anyhow::Result<bool> {
        if state() == DaemonState::NotInstalled {
            return Ok(false);
        }
        let (ok, _) = systemctl(&["restart", UNIT]);
        if !ok {
            anyhow::bail!("`systemctl --user restart {UNIT}` failed");
        }
        Ok(true)
    }

    pub fn manual_restart_hint() -> String {
        format!("systemctl --user restart {UNIT}")
    }

    #[cfg(test)]
    pub(super) fn render_for_test(spec: &ServiceSpec) -> String {
        build_unit(spec)
    }
}

// ── Anywhere else ───────────────────────────────────────────────────────────

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::{DaemonState, InstallReport, ServiceSpec};
    use std::path::PathBuf;

    fn unsupported<T>() -> anyhow::Result<T> {
        anyhow::bail!(
            "Unattended install is only supported on macOS (launchd) and Linux (systemd).\n\
             Run `decent start --allow-real-jobs` under your own supervisor."
        )
    }

    pub fn unit_path() -> anyhow::Result<PathBuf> {
        unsupported()
    }
    pub fn install(_spec: &ServiceSpec) -> anyhow::Result<InstallReport> {
        unsupported()
    }
    pub fn uninstall() -> anyhow::Result<PathBuf> {
        unsupported()
    }
    pub fn state() -> DaemonState {
        DaemonState::NotInstalled
    }
    pub fn pause() -> anyhow::Result<bool> {
        unsupported()
    }
    pub fn resume() -> anyhow::Result<()> {
        unsupported()
    }
    pub fn restart() -> anyhow::Result<bool> {
        unsupported()
    }
    pub fn manual_restart_hint() -> String {
        "(no supported service manager on this platform)".to_string()
    }
}

/// macOS-only: is the pre-rename decent-node agent still loaded?
///
/// Lives here rather than in `main` so the legacy-migration knowledge stays with
/// the launchd code it is about. Always false off macOS — the legacy agent only
/// ever existed there.
pub fn legacy_daemon_is_loaded() -> bool {
    #[cfg(target_os = "macos")]
    {
        imp::legacy_daemon_is_loaded()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Log destination for a freshly installed daemon: alongside the token.
pub fn default_log_path(token_path: &Path) -> anyhow::Result<PathBuf> {
    Ok(token_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("token file has no parent"))?
        .join("decent.log"))
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::imp::should_attempt_pre_install_unload;
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            exe: PathBuf::from("/opt/decent/bin/decent"),
            dispatch_url: "wss://dispatch.example.com/ws".to_string(),
            log_path: PathBuf::from("/home/op/.config/decent/decent.log"),
        }
    }

    #[test]
    fn log_path_sits_beside_the_token() {
        let token = PathBuf::from("/home/op/.config/decent/worker-token");
        assert_eq!(
            default_log_path(&token).unwrap(),
            PathBuf::from("/home/op/.config/decent/decent.log")
        );
    }

    // ── PACKET 40 ──────────────────────────────────────────────────────────

    /// PACKET 41 (step 1): first install must not fire the pre-install
    /// unload at all (there is no plist yet); a REINSTALL with an existing
    /// plist does attempt it. Pinned by observing whether launchctl is
    /// even invoked — the helper is extracted for testability.
    #[cfg(target_os = "macos")]
    #[test]
    fn pre_install_unload_only_runs_when_a_plist_exists() {
        let dir = std::env::temp_dir().join(format!(
            "p41-unload-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let plist = dir.join("agent.plist");
        // First install: no plist -> no unload attempted.
        assert!(!should_attempt_pre_install_unload(&plist));
        // Reinstall: plist exists -> unload attempted.
        std::fs::write(&plist, "plist-bytes").unwrap();
        assert!(should_attempt_pre_install_unload(&plist));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// AUDIT 7 (label collision): `com.decent-render.decent-node` contains
    /// `com.decent-render.decent` as a substring, so substring matching made
    /// legacy_daemon_is_loaded() unreachable and is_loaded() true for a
    /// legacy-only install (pause contradicting status). These pin EXACT
    /// field matching against real-shaped launchctl output.
    #[cfg(target_os = "macos")]
    #[test]
    fn label_matching_is_exact_not_substring() {
        // launchctl list rows: PID\tStatus\tLabel
        let legacy_only = "-\t0\tcom.decent-render.decent-node\n";
        assert!(super::imp::label_is_loaded(
            legacy_only,
            "com.decent-render.decent-node"
        ));
        // THE regression: the NEW label must NOT match a legacy-only load.
        assert!(
            !super::imp::label_is_loaded(legacy_only, "com.decent-render.decent"),
            "the new label must not substring-match the legacy label"
        );
        let modern = "123\t0\tcom.decent-render.decent\n";
        assert!(super::imp::label_is_loaded(
            modern,
            "com.decent-render.decent"
        ));
        assert!(
            !super::imp::label_is_loaded(modern, "com.decent-render.decent-node"),
            "the legacy label must not match a modern-only load"
        );
    }

    /// AUDIT 8 (plist escaping): a legal dispatch URL with XML-special
    /// characters must round-trip through the plist as an ESCAPED value —
    /// raw `&` produces a plist launchd refuses to load.
    #[cfg(target_os = "macos")]
    #[test]
    fn hostile_but_legal_url_escapes_in_plist() {
        let hostile = ServiceSpec {
            exe: PathBuf::from("/opt/decent/bin/decent"),
            // Legal URL shape carrying XML-hostile characters.
            dispatch_url: "wss://dispatch.example.com/ws?team=a&env=<prod>".to_string(),
            log_path: PathBuf::from("/tmp/log&path/decent.log"),
        };
        let text = rendered_unit(&hostile);
        // No RAW special characters inside the document body...
        assert!(!text.contains("a&env"), "raw & leaked into plist:\n{text}");
        assert!(!text.contains("<prod>"), "raw < leaked into plist:\n{text}");
        // ...and the escaped forms ARE present, so the URL round-trips.
        assert!(text.contains("a&amp;env"), "escaped & missing:\n{text}");
        assert!(
            text.contains("&lt;prod&gt;"),
            "escaped < > missing:\n{text}"
        );
    }

    /// AUDIT R-8, Linux half (D-13): a legal dispatch URL with systemd-hostile
    /// characters must reach the daemon INTACT. systemd word-splits ExecStart
    /// on unquoted whitespace and expands `%` specifiers, so every argument
    /// is double-quoted per systemd.syntax with `\` and `"` backslash-escaped
    /// inside, and a literal `%` written as `%%`.
    #[test]
    fn hostile_but_legal_url_escapes_in_unit_exec_start() {
        let hostile = ServiceSpec {
            exe: PathBuf::from("/opt/decent/bin/decent"),
            // Legal URL shape carrying a space, a double quote, a backslash
            // and a percent sign.
            dispatch_url: "wss://dispatch.example.com/ws?team=a b\"q\"\\d%h".to_string(),
            log_path: PathBuf::from("/tmp/log path/decent.log"),
        };
        let line = exec_start_line(&hostile.exe, &hostile.dispatch_url);
        assert_eq!(
            line,
            "ExecStart=\"/opt/decent/bin/decent\" \"start\" \"--dispatch-url\" \
             \"wss://dispatch.example.com/ws?team=a b\\\"q\\\"\\\\d%%h\" \"--allow-real-jobs\"",
            "hostile URL must be quoted and escaped, got:\n{line}"
        );
    }

    /// The daemon exists to render, so the unit MUST pass --allow-real-jobs.
    /// Without it `start` registers and heartbeats forever while refusing every
    /// job — a node that looks perfectly healthy and never does any work.
    #[test]
    fn unit_opts_into_real_jobs() {
        let text = rendered_unit(&spec());
        assert!(
            text.contains("--allow-real-jobs"),
            "unit must opt into real jobs, got:\n{text}"
        );
        assert!(text.contains("wss://dispatch.example.com/ws"));
    }

    /// A crashing node must come back without an operator present.
    #[test]
    fn unit_restarts_on_exit() {
        let text = rendered_unit(&spec());
        #[cfg(target_os = "macos")]
        assert!(text.contains("<key>KeepAlive</key>"), "{text}");
        #[cfg(target_os = "linux")]
        assert!(text.contains("Restart=always"), "{text}");
        let _ = text;
    }

    /// Both platforms write to the same log path so `decent status` can point
    /// at one file regardless of where it is running.
    #[test]
    fn unit_logs_where_we_said_it_would() {
        let text = rendered_unit(&spec());
        assert!(
            text.contains("/home/op/.config/decent/decent.log"),
            "{text}"
        );
    }

    /// The unit must run THE BINARY THE OPERATOR INSTALLED — by absolute
    /// path, first among ProgramArguments/ExecStart. A unit that resolved
    /// `decent` through PATH would silently run whatever another package
    /// dropped on PATH, on every restart, forever (KeepAlive makes launchd
    /// the thing that keeps re-running it).
    #[test]
    fn unit_runs_the_installed_binary_by_absolute_path() {
        let text = rendered_unit(&spec());
        let expected = "/opt/decent/bin/decent";
        #[cfg(target_os = "macos")]
        {
            // First <string> after ProgramArguments is the executable.
            let args_block = text
                .split("<key>ProgramArguments</key>")
                .nth(1)
                .expect("plist has ProgramArguments");
            let first = args_block
                .split("<string>")
                .nth(1)
                .expect("at least one argument string")
                .split("</string>")
                .next()
                .unwrap();
            assert_eq!(first, expected, "plist:\n{text}");
        }
        #[cfg(target_os = "linux")]
        {
            let exec_start = text
                .lines()
                .find(|l| l.starts_with("ExecStart="))
                .expect("unit has ExecStart");
            assert!(
                exec_start.starts_with(&format!("ExecStart=\"{expected}\" ")),
                "unit:\n{text}"
            );
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = (&text, expected);
    }

    /// The exact argument vector an operator can rely on: `<exe> start
    /// --dispatch-url <url> --allow-real-jobs`. Pinned as a sequence, not a
    /// bag of substrings, so a future flag reorder or insertion is a visible
    /// diff rather than a silent behavior change on every installed node.
    #[test]
    fn unit_argument_vector_is_exactly_start_dispatch_allow() {
        let text = rendered_unit(&spec());
        #[cfg(target_os = "macos")]
        {
            let args: Vec<String> = text
                .split("<key>ProgramArguments</key>")
                .nth(1)
                .expect("plist has ProgramArguments")
                .split("<key>")
                .next()
                .unwrap()
                .split("<string>")
                .skip(1)
                .map(|s| s.split("</string>").next().unwrap().to_string())
                .collect();
            assert_eq!(
                args,
                vec![
                    "/opt/decent/bin/decent",
                    "start",
                    "--dispatch-url",
                    "wss://dispatch.example.com/ws",
                    "--allow-real-jobs",
                ],
                "plist:\n{text}"
            );
        }
        #[cfg(target_os = "linux")]
        {
            let exec_start = text
                .lines()
                .find(|l| l.starts_with("ExecStart="))
                .expect("unit has ExecStart");
            assert_eq!(
                exec_start,
                "ExecStart=\"/opt/decent/bin/decent\" \"start\" \"--dispatch-url\" \
                 \"wss://dispatch.example.com/ws\" \"--allow-real-jobs\"",
                "unit:\n{text}"
            );
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = text;
    }

    /// Start at login/boot: launchd's RunAtLoad (the systemd leg is pinned by
    /// linux_unit_starts_at_boot_not_just_on_demand). Without it an installed
    /// node stays down after every reboot until the operator notices.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unit_runs_at_load() {
        let text = rendered_unit(&spec());
        assert!(text.contains("<key>RunAtLoad</key>"), "{text}");
        assert!(
            text.contains("<key>RunAtLoad</key>\n    <true/>"),
            "RunAtLoad must be true:\n{text}"
        );
    }

    /// A graceful stop must be BOUNDED: the daemon supervises render jobs and
    /// its shutdown drains them (cancel → TERM → 10s grace → KILL → purge).
    /// ExitTimeOut/TimeoutStopSec is what stops launchd/systemd SIGKILLing
    /// the supervisor mid-drain on every `decent stop`/upgrade.
    #[test]
    fn unit_gives_the_supervisor_time_to_drain_on_stop() {
        let text = rendered_unit(&spec());
        #[cfg(target_os = "macos")]
        {
            assert!(
                text.contains("<key>ExitTimeOut</key>\n    <integer>30</integer>"),
                "ExitTimeOut must be a 30s bound:\n{text}"
            );
        }
        #[cfg(target_os = "linux")]
        {
            assert!(
                text.contains("TimeoutStopSec=30"),
                "TimeoutStopSec must be a 30s bound:\n{text}"
            );
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = text;
    }

    /// Logs must be INFO-level by default — enough for an operator to see
    /// job flow without enabling anything, not so chatty the log file drowns
    /// the disk a render farm is filling.
    #[test]
    fn unit_sets_rust_log_info() {
        let text = rendered_unit(&spec());
        #[cfg(target_os = "macos")]
        assert!(
            text.contains("<key>RUST_LOG</key>\n        <string>info</string>"),
            "{text}"
        );
        #[cfg(target_os = "linux")]
        assert!(text.contains("Environment=RUST_LOG=info"), "{text}");
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = text;
    }

    /// macOS label is the identity launchctl addresses the agent by; the
    /// manual restart hint must use the SAME label or the operator's
    /// copy-paste restarts nothing. (Systemd names the unit by filename,
    /// pinned by the unit_path tests on Linux.)
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_label_is_stable_and_used_by_the_restart_hint() {
        assert_eq!(imp::LABEL, "com.decent-render.decent");
        let hint = manual_restart_hint();
        assert!(
            hint.contains(imp::LABEL),
            "hint {hint:?} must address the installed label"
        );
    }

    /// The log FILENAME must be decent.log on both platforms — `decent
    /// status` prints one path shape regardless of OS, and operators grep
    /// for it in runbooks.
    #[test]
    fn log_file_is_named_decent_log_beside_the_token() {
        let token = PathBuf::from("/var/lib/op/.config/decent/worker-token");
        let log = default_log_path(&token).unwrap();
        assert_eq!(log.file_name().unwrap(), "decent.log");
        assert_eq!(log.parent().unwrap(), token.parent().unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_unit_starts_at_boot_not_just_on_demand() {
        // WantedBy=default.target is what `systemctl --user enable` hooks into.
        // Without it, `enable` succeeds and the node never starts by itself.
        let text = rendered_unit(&spec());
        assert!(text.contains("WantedBy=default.target"), "{text}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_unit_waits_for_the_network() {
        // The node's first action is a WebSocket dial. Starting before the
        // network is up turns a healthy boot into a reconnect-backoff cycle.
        let text = rendered_unit(&spec());
        assert!(text.contains("After=network-online.target"), "{text}");
    }

    fn rendered_unit(spec: &ServiceSpec) -> String {
        #[cfg(target_os = "macos")]
        {
            imp::render_for_test(spec)
        }
        #[cfg(target_os = "linux")]
        {
            imp::render_for_test(spec)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = spec;
            String::new()
        }
    }
}
