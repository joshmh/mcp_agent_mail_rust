//! Invoke a golem-owned notify hook after a durable message insert.
//!
//! Agent Mail owns the *event* (this module). Golem owns the *actuator*
//! (`golem/bin/am-notify-hook.sh`): debounce, pane resolve, idle-gated ping.
//!
//! `aud-mail-aware-pings-zio` Stage 2.
//!
//! The hook path is `AM_NOTIFY_HOOK`. Unset means "not yet deployed" — send
//! still succeeds; the skuld sweep is the backstop. A *configured* hook that
//! cannot be spawned is logged at error (fail-loud). The hook is fire-and-
//! forget so `send_message` never waits on the idle gate. The child is
//! detached from the daemon's stdio and reaped (`setsid -f`, or a reaper
//! thread) so a long-lived server does not accumulate zombies.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

/// Environment variable naming the golem actuator.
pub const AM_NOTIFY_HOOK_ENV: &str = "AM_NOTIFY_HOOK";

#[cfg(test)]
thread_local! {
    /// Test-only hook path. `Some(None)` means "explicitly unset".
    /// Avoids `std::env::set_var`, which is `unsafe` under Rust 2024 and
    /// forbidden by this crate's `#![forbid(unsafe_code)]`.
    static TEST_HOOK_PATH: std::cell::RefCell<Option<Option<std::ffi::OsString>>> =
        const { std::cell::RefCell::new(None) };
}

fn notify_hook_path() -> Option<std::ffi::OsString> {
    #[cfg(test)]
    {
        if let Some(overridden) = TEST_HOOK_PATH.with(|cell| cell.borrow().clone()) {
            return overridden;
        }
    }
    std::env::var_os(AM_NOTIFY_HOOK_ENV).filter(|value| !value.is_empty())
}

/// Spawn the notify hook once per unique recipient. Never blocks on idle-gate
/// wait. Failures print and log; they do not undo the durable insert.
pub fn spawn_after_insert<'a, I>(
    project_key: &str,
    project_slug: &str,
    message_id: i64,
    recipients: I,
    importance: &str,
) where
    I: IntoIterator<Item = &'a String>,
{
    let hook = match notify_hook_path() {
        Some(value) => value,
        None => return,
    };
    let hook_path = Path::new(&hook);
    if !hook_path.is_file() {
        tracing::error!(
            hook = %hook_path.display(),
            project = %project_slug,
            message_id,
            "AM_NOTIFY_HOOK is set but is not a file — mail landed, doorbell will not ring (skuld sweep is the backstop)"
        );
        eprintln!(
            "am-notify: FAIL: AM_NOTIFY_HOOK={} is not a file (message_id={message_id})",
            hook_path.display()
        );
        return;
    }

    let mut seen = std::collections::HashSet::new();
    for name in recipients {
        if name.is_empty() || !seen.insert(name.as_str()) {
            continue;
        }
        spawn_one(
            hook_path.as_os_str(),
            project_key,
            project_slug,
            message_id,
            name,
            importance,
        );
    }
}

fn spawn_one(
    hook: &OsStr,
    project_key: &str,
    project_slug: &str,
    message_id: i64,
    recipient: &str,
    importance: &str,
) {
    match spawn_detached(hook, project_key, project_slug, message_id, recipient, importance) {
        Ok(pid) => {
            tracing::info!(
                hook = ?hook,
                recipient,
                project = %project_slug,
                message_id,
                pid,
                "spawned AM_NOTIFY_HOOK"
            );
        }
        Err(error) => {
            tracing::error!(
                hook = ?hook,
                recipient,
                project = %project_slug,
                message_id,
                error = %error,
                "failed to spawn AM_NOTIFY_HOOK — mail landed, doorbell will not ring (skuld sweep is the backstop)"
            );
            eprintln!(
                "am-notify: FAIL: spawn {hook:?} for {recipient}: {error} (message_id={message_id})"
            );
        }
    }
}

fn apply_hook_stdio_and_env(
    cmd: &mut Command,
    project_key: &str,
    project_slug: &str,
    message_id: i64,
    recipient: &str,
    importance: &str,
) {
    cmd.env("AM_NOTIFY_RECIPIENT", recipient)
        .env("AM_NOTIFY_PROJECT", project_key)
        .env("AM_NOTIFY_PROJECT_SLUG", project_slug)
        .env("AM_NOTIFY_MESSAGE_ID", message_id.to_string())
        .env("AM_NOTIFY_IMPORTANCE", importance)
        .stdin(Stdio::null())
        // aud-axg: never inherit the daemon's stdout/stderr.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
}

/// Fire-and-forget spawn that does not leave zombies in the long-lived
/// daemon (aud-mz1) and does not share the daemon's stdio (aud-axg).
///
/// On Unix, `setsid -f` double-forks: we wait on the short-lived setsid
/// parent (milliseconds) and the hook runs in a new session reparented
/// to init. Fallback is a new process group plus a reaper thread.
fn spawn_detached(
    hook: &OsStr,
    project_key: &str,
    project_slug: &str,
    message_id: i64,
    recipient: &str,
    importance: &str,
) -> std::io::Result<u32> {
    #[cfg(unix)]
    {
        if let Ok(pid) = spawn_via_setsid(
            hook,
            project_key,
            project_slug,
            message_id,
            recipient,
            importance,
        ) {
            return Ok(pid);
        }
    }

    let mut cmd = Command::new(hook);
    cmd.arg(recipient);
    apply_hook_stdio_and_env(
        &mut cmd,
        project_key,
        project_slug,
        message_id,
        recipient,
        importance,
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();
    // Reap so an exited hook does not sit as a zombie under the daemon.
    let _ = std::thread::Builder::new()
        .name(format!("am-notify-reap-{pid}"))
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(pid)
}

#[cfg(unix)]
fn spawn_via_setsid(
    hook: &OsStr,
    project_key: &str,
    project_slug: &str,
    message_id: i64,
    recipient: &str,
    importance: &str,
) -> std::io::Result<u32> {
    let mut cmd = Command::new("/usr/bin/setsid");
    cmd.arg("-f").arg(hook).arg(recipient);
    apply_hook_stdio_and_env(
        &mut cmd,
        project_key,
        project_slug,
        message_id,
        recipient,
        importance,
    );
    let mut child = cmd.spawn()?;
    let pid = child.id();
    // setsid -f: this child is the fork parent and exits immediately
    // after creating the new session. wait() is milliseconds; the hook
    // is already detached and will be reaped by init.
    let _ = child.wait();
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    fn with_hook_path<R>(path: Option<std::ffi::OsString>, f: impl FnOnce() -> R) -> R {
        TEST_HOOK_PATH.with(|cell| {
            *cell.borrow_mut() = Some(path);
        });
        let result = f();
        TEST_HOOK_PATH.with(|cell| {
            *cell.borrow_mut() = None;
        });
        result
    }

    #[test]
    fn unset_hook_is_a_no_op() {
        with_hook_path(None, || {
            spawn_after_insert(
                "/data/projects/golem",
                "golem",
                1,
                [&"OliveBluff".to_string()],
                "normal",
            );
        });
    }

    #[test]
    fn configured_hook_is_spawned_once_per_unique_recipient() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hook = dir.path().join("hook.sh");
        let log = dir.path().join("hook.log");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf '%s %s %s\\n' \"$1\" \"$AM_NOTIFY_MESSAGE_ID\" \"$AM_NOTIFY_PROJECT\" >>'{}'\n",
                log.display()
            ),
        )
        .expect("write hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&hook).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook, perms).unwrap();
        }
        let alice = "AliceSeat".to_string();
        let bob = "BobSeat".to_string();
        with_hook_path(Some(hook.as_os_str().to_os_string()), || {
            spawn_after_insert(
                "/data/projects/golem",
                "golem",
                42,
                [&alice, &alice, &bob],
                "high",
            );
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let body = loop {
            if let Ok(text) = fs::read_to_string(&log) {
                if text.lines().count() >= 2 {
                    break text;
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("hook did not write two lines; log={log:?}");
            }
            thread::sleep(Duration::from_millis(20));
        };
        let mut lines: Vec<_> = body.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            [
                "AliceSeat 42 /data/projects/golem",
                "BobSeat 42 /data/projects/golem"
            ]
        );
    }

    #[test]
    fn missing_hook_path_does_not_panic() {
        with_hook_path(Some(std::ffi::OsString::from("/no/such/am-notify-hook")), || {
            spawn_after_insert(
                "/data/projects/golem",
                "golem",
                7,
                [&"GhostSeat".to_string()],
                "normal",
            );
        });
    }
}
