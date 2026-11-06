use crate::{CapabilitySet, DeliveryOutcome, PlatformError, Result};
use serde::Deserialize;
use std::{
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const STEP: Duration = Duration::from_millis(500);
const OUTPUT_LIMIT: u64 = 128 * 1024;
const COPY_LIMIT: u64 = 4 * 1024 * 1024;

/// Fixed synthetic payload for the explicit in-application delivery probe.
/// Never substitute user text or accept a configurable probe payload.
pub const DELIVERY_PROBE_TEXT: &str =
    "语音输入粘贴验证 2026\nVoice input paste probe: /tmp/example.txt";

/// No native output is embedded in errors or diagnostics.
fn run(program: &str, args: &[&str]) -> Result<Vec<u8>> {
    run_until(program, args, Instant::now() + STEP)
}

fn terminate_group(child: &mut Child) {
    // Call only while the direct child is still unreaped. Newly spawned copy
    // owners satisfy this; maintenance removes an owner immediately when it
    // successfully reaps it. Do not try_wait before killing: the zombie leader
    // may be keeping a live pipe-holding descendant's group identity reserved.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Deserialize)]
struct ActiveWindow {
    address: String,
    pid: u32,
    class: String,
    #[serde(default)]
    mapped: Option<bool>,
}

/// Class/address are needed only for live safety checks and are never logged.
#[derive(Clone)]
pub struct TargetSnapshot {
    address: String,
    pid: u32,
    class: String,
    epoch: u64,
    attempted: Arc<AtomicBool>,
    own_probe_sent: Arc<AtomicBool>,
}
impl TargetSnapshot {
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn focus_epoch(&self) -> u64 {
        self.epoch
    }
}
impl std::fmt::Debug for TargetSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TargetSnapshot")
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct FocusState {
    epoch: AtomicU64,
    connected: AtomicBool,
    stop: AtomicBool,
    unlocked: AtomicBool,
}
impl FocusState {
    fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }
    fn disconnected(&self) {
        self.unlocked.store(false, Ordering::SeqCst);
        self.connected.store(false, Ordering::SeqCst);
        self.invalidate();
    }
    fn observe_lock(&self, unlocked: bool) {
        if self.unlocked.swap(unlocked, Ordering::SeqCst) != unlocked || !unlocked {
            self.invalidate();
        }
    }
    fn event(&self, name: &[u8]) {
        if matches!(
            name,
            b"activewindow"
                | b"activewindowv2"
                | b"closewindow"
                | b"lockgroups"
                | b"monitorremoved"
        ) {
            self.invalidate();
        }
    }
}

struct FocusMonitor {
    state: Arc<FocusState>,
    thread: Option<JoinHandle<()>>,
}
impl FocusMonitor {
    fn start(path: Option<PathBuf>) -> Self {
        let state = Arc::new(FocusState::default());
        let shared = Arc::clone(&state);
        let handle = thread::spawn(move || {
            while !shared.stop.load(Ordering::SeqCst) {
                let connection = path.as_ref().and_then(|p| UnixStream::connect(p).ok());
                if let Some(mut stream) = connection {
                    if stream
                        .set_read_timeout(Some(Duration::from_millis(200)))
                        .is_err()
                    {
                        shared.disconnected();
                        continue;
                    }
                    shared.invalidate();
                    shared.connected.store(true, Ordering::SeqCst);
                    shared.observe_lock(compositor_unlocked());
                    let mut last_lock_check = Instant::now();
                    let mut carry = Vec::new();
                    let mut bytes = [0; 4096];
                    while !shared.stop.load(Ordering::SeqCst) {
                        if last_lock_check.elapsed() >= Duration::from_millis(200) {
                            shared.observe_lock(compositor_unlocked());
                            last_lock_check = Instant::now();
                        }
                        match stream.read(&mut bytes) {
                            Ok(0) => break,
                            Ok(count) => {
                                carry.extend_from_slice(&bytes[..count]);
                                if carry.len() > OUTPUT_LIMIT as usize {
                                    break;
                                }
                                while let Some(end) = carry.iter().position(|b| *b == b'\n') {
                                    let line = &carry[..end];
                                    let separator = line
                                        .windows(2)
                                        .position(|w| w == b">>")
                                        .unwrap_or(line.len());
                                    shared.event(&line[..separator]);
                                    carry.drain(..=end);
                                }
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::TimedOut
                                        | std::io::ErrorKind::Interrupted
                                ) => {}
                            Err(_) => break,
                        }
                    }
                }
                shared.disconnected();
                if !shared.stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        });
        Self {
            state,
            thread: Some(handle),
        }
    }
}
impl Drop for FocusMonitor {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn tool_present(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|p| p.join(tool).is_file()))
}

fn event_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    if signature.contains('/') || signature.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(runtime)
            .join("hypr")
            .join(signature)
            .join(".socket2.sock"),
    )
}

fn terminal_or_sensitive(class: &str, extra: &[String]) -> bool {
    let lower = class.to_lowercase();
    [
        "terminal",
        "foot",
        "kitty",
        "alacritty",
        "ghostty",
        "wezterm",
        "konsole",
        "xterm",
        "terminator",
        "org.omarchy.agent",
        "hyprlock",
        "polkit",
        "pinentry",
        "1password",
        "keepass",
    ]
    .iter()
    .any(|name| lower.contains(name))
        || extra
            .iter()
            .any(|name| !name.is_empty() && lower.contains(&name.to_lowercase()))
}

fn guard(
    target: &TargetSnapshot,
    current: &ActiveWindow,
    epoch: u64,
    connected: bool,
    safe_session: bool,
    sensitive: &[String],
) -> bool {
    connected
        && safe_session
        && target.epoch == epoch
        && target.pid == current.pid
        && target.address == current.address
        && target.class == current.class
        && current.mapped != Some(false)
        && !terminal_or_sensitive(&target.class, sensitive)
}

fn compositor_unlocked() -> bool {
    let Ok(bytes) = run("hyprctl", &["-j", "locked"]) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    value.get("locked").and_then(|v| v.as_bool()) == Some(false)
}

fn run_until(program: &str, args: &[&str], deadline: Instant) -> Result<Vec<u8>> {
    run_until_limit(program, args, deadline, OUTPUT_LIMIT)
}

fn run_until_limit(
    program: &str,
    args: &[&str],
    deadline: Instant,
    output_limit: u64,
) -> Result<Vec<u8>> {
    if Instant::now() >= deadline {
        return Err(PlatformError::Timeout);
    }
    let mut child = Command::new(program)
        .args(args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| PlatformError::Command)?;
    let mut reaped = false;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or(PlatformError::Command)?;
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(PlatformError::Command);
        }
        let mut output = Vec::new();
        let mut eof = false;
        loop {
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            let mut block = [0u8; 4096];
            match stdout.read(&mut block) {
                Ok(0) => eof = true,
                Ok(count) => {
                    output.extend_from_slice(&block[..count]);
                    if output.len() as u64 > output_limit {
                        return Err(PlatformError::OutputLimit);
                    }
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return Err(PlatformError::Command),
            }
            // Keep the direct child unreaped until all pipe owners close. Its
            // retained PID also keeps the process-group identity safe to kill
            // if a descendant holds stdout open beyond the shared deadline.
            if eof && let Some(status) = child.try_wait().map_err(|_| PlatformError::Command)? {
                reaped = true;
                return if status.success() {
                    Ok(output)
                } else {
                    Err(PlatformError::Command)
                };
            }
            let budget = deadline
                .checked_duration_since(Instant::now())
                .ok_or(PlatformError::Timeout)?;
            thread::sleep(budget.min(Duration::from_millis(2)));
        }
    })();
    if result.is_err() && !reaped {
        // Do not try_wait here: the leader may already be a zombie while an
        // owned descendant still holds the pipe. Keeping it unreaped above
        // prevents this process-group ID from being reused before this kill.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn session_safe() -> bool {
    session_safe_until(Instant::now() + STEP * 3)
}

fn session_safe_until(deadline: Instant) -> bool {
    // logind LockedHint alone can be stale: compositor is authoritative.
    let Ok(bytes) = run_until("hyprctl", &["-j", "locked"], deadline) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    if value.get("locked").and_then(|v| v.as_bool()) != Some(false) {
        return false;
    }
    let session = std::env::var("XDG_SESSION_ID").unwrap_or_else(|_| "self".into());
    let Ok(status) = run_until(
        "loginctl",
        &["show-session", &session, "-p", "LockedHint", "-p", "Active"],
        deadline,
    ) else {
        return false;
    };
    let status = String::from_utf8_lossy(&status);
    if !status.lines().any(|s| s == "LockedHint=no") || !status.lines().any(|s| s == "Active=yes") {
        return false;
    }
    let Ok(monitors) = run_until("hyprctl", &["-j", "monitors"], deadline) else {
        return false;
    };
    let Ok(monitors) = serde_json::from_slice::<Vec<serde_json::Value>>(&monitors) else {
        return false;
    };
    !monitors.is_empty()
        && monitors
            .iter()
            .all(|m| m.get("dpmsStatus").and_then(|v| v.as_bool()) == Some(true))
}

fn own_probe_confirmation_allowed(
    target: &TargetSnapshot,
    observed: &str,
    own_pid: u32,
    live_guard: bool,
) -> bool {
    live_guard
        && target.pid == own_pid
        && target.attempted.load(Ordering::SeqCst)
        && target.own_probe_sent.load(Ordering::SeqCst)
        && observed == DELIVERY_PROBE_TEXT
}

pub struct Hyprland {
    monitor: FocusMonitor,
    automatic_verified: bool,
    sensitive: Vec<String>,
    clipboard_owner: Option<Child>,
}
impl Hyprland {
    /// `automatic_verified` must be false on this host until a real controlled
    /// paste probe succeeds; binary presence is not protocol capability evidence.
    pub fn new(automatic_verified: bool, sensitive_classes: Vec<String>) -> Self {
        crate::virtual_keyboard::prepare();
        Self {
            monitor: FocusMonitor::start(event_path()),
            automatic_verified,
            sensitive: sensitive_classes,
            clipboard_owner: None,
        }
    }
    /// Forget in-session protocol evidence and invalidate every pending target.
    /// Call on a failed/new probe or any desktop capability change.
    pub fn invalidate_automatic_delivery(&mut self) {
        self.automatic_verified = false;
        self.monitor.state.invalidate();
    }
    /// Explicit typed UI test only: paste fixed synthetic text into this
    /// process's already-focused app window. Does not activate any window.
    /// The host must focus its dedicated test input before calling and keep a
    /// random token/deadline for the returned opaque target. Dispatch success
    /// alone never enables automatic delivery.
    pub fn probe_own_window(&mut self) -> Result<TargetSnapshot> {
        self.invalidate_automatic_delivery();
        let target = self.capture_target()?;
        if target.pid != std::process::id() || !self.target_valid(&target) {
            return Err(PlatformError::Target);
        }
        self.automatic_verified = true;
        let outcome = self.deliver(&target, DELIVERY_PROBE_TEXT);
        self.automatic_verified = false;
        match outcome {
            DeliveryOutcome::SentUnconfirmed => {
                target.own_probe_sent.store(true, Ordering::SeqCst);
                Ok(target)
            }
            DeliveryOutcome::Manual => Err(PlatformError::Target),
            DeliveryOutcome::Unknown => Err(PlatformError::Command),
        }
    }
    /// Accept only an actual input-event readback from the host's pending
    /// token-bound probe (host enforces a five-second deadline). Plain command
    /// success, arbitrary user text, stale focus, and other processes are not
    /// delivery evidence. Successful confirmation consumes this probe proof.
    pub fn accept_own_probe(&mut self, target: &TargetSnapshot, observed: &str) -> Result<()> {
        self.automatic_verified = false;
        let own_pid = std::process::id();
        if !own_probe_confirmation_allowed(target, observed, own_pid, true)
            || !self.target_valid(target)
            || !target.own_probe_sent.swap(false, Ordering::SeqCst)
        {
            self.invalidate_automatic_delivery();
            return Err(PlatformError::Target);
        }
        self.automatic_verified = true;
        Ok(())
    }
    pub fn set_sensitive_classes(&mut self, classes: Vec<String>) {
        self.sensitive = classes;
        self.monitor.state.invalidate();
    }
    pub fn capabilities(&self) -> CapabilitySet {
        CapabilitySet {
            hyprland: event_path().is_some(),
            focus_events_connected: self.monitor.state.connected.load(Ordering::SeqCst),
            automatic_delivery_verified: self.automatic_verified,
            clipboard_tools_present: tool_present("wl-copy") && tool_present("wl-paste"),
            virtual_keyboard_tool_present: crate::virtual_keyboard::prepared(),
        }
    }
    pub fn capture_target(&self) -> Result<TargetSnapshot> {
        if !self.monitor.state.connected.load(Ordering::SeqCst)
            || !self.monitor.state.unlocked.load(Ordering::SeqCst)
            || !session_safe()
        {
            return Err(PlatformError::Session);
        }
        let epoch = self.monitor.state.epoch.load(Ordering::SeqCst);
        let current = self.active_window()?;
        if current.pid == 0
            || !current.address.starts_with("0x")
            || current.mapped == Some(false)
            || self.monitor.state.epoch.load(Ordering::SeqCst) != epoch
        {
            return Err(PlatformError::Target);
        }
        Ok(TargetSnapshot {
            address: current.address,
            pid: current.pid,
            class: current.class,
            epoch,
            attempted: Arc::new(AtomicBool::new(false)),
            own_probe_sent: Arc::new(AtomicBool::new(false)),
        })
    }
    fn active_window(&self) -> Result<ActiveWindow> {
        serde_json::from_slice(&run("hyprctl", &["-j", "activewindow"])?)
            .map_err(|_| PlatformError::Target)
    }
    fn target_valid(&self, target: &TargetSnapshot) -> bool {
        self.target_valid_until(target, Instant::now() + STEP * 4)
    }
    fn target_valid_until(&self, target: &TargetSnapshot, deadline: Instant) -> bool {
        let state = &self.monitor.state;
        if !session_safe_until(deadline) {
            return false;
        }
        let Ok(bytes) = run_until("hyprctl", &["-j", "activewindow"], deadline) else {
            return false;
        };
        let Ok(current) = serde_json::from_slice::<ActiveWindow>(&bytes) else {
            return false;
        };
        guard(
            target,
            &current,
            state.epoch.load(Ordering::SeqCst),
            state.connected.load(Ordering::SeqCst),
            state.unlocked.load(Ordering::SeqCst),
            &self.sensitive,
        )
    }
    /// Read-only preparation check. Delivery repeats the guard immediately
    /// before injection; a positive result is never a future-focus guarantee.
    pub fn can_deliver(&self, target: &TargetSnapshot) -> bool {
        self.automatic_verified
            && !target.attempted.load(Ordering::SeqCst)
            && self.target_valid(target)
    }
    /// Copies text only. It deliberately does not inject or activate any window.
    pub fn copy(&mut self, text: &str) -> Result<()> {
        if text.len() as u64 > COPY_LIMIT {
            return Err(PlatformError::OutputLimit);
        }
        self.reap_owner();
        let deadline = Instant::now() + STEP;
        let mut child = Command::new("wl-copy")
            .args(["--foreground", "--type", "text/plain;charset=utf-8"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| PlatformError::Command)?;
        let mut stdin = child.stdin.take().ok_or(PlatformError::Command)?;
        let fd = stdin.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            terminate_group(&mut child);
            return Err(PlatformError::Clipboard);
        }
        let mut remaining = text.as_bytes();
        while !remaining.is_empty() {
            if Instant::now() >= deadline {
                terminate_group(&mut child);
                return Err(PlatformError::Timeout);
            }
            match stdin.write(remaining) {
                Ok(0) => {
                    terminate_group(&mut child);
                    return Err(PlatformError::Clipboard);
                }
                Ok(count) => remaining = &remaining[count..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        terminate_group(&mut child);
                        return Err(PlatformError::Timeout);
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => {
                    terminate_group(&mut child);
                    return Err(PlatformError::Clipboard);
                }
            }
        }
        drop(stdin);
        let verified = loop {
            let Some(_) = deadline.checked_duration_since(Instant::now()) else {
                break false;
            };
            if let Ok(readback) =
                run_until_limit("wl-paste", &["--no-newline"], deadline, COPY_LIMIT)
                && readback == text.as_bytes()
            {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(5));
        };
        if !verified {
            terminate_group(&mut child);
            return Err(PlatformError::Clipboard);
        }
        if let Some(mut old) = self.clipboard_owner.replace(child) {
            let _ = old.kill();
            let _ = old.wait();
        }
        Ok(())
    }
    /// Caller must durably mark the attempt before calling. Never retries an injection.
    pub fn deliver(&mut self, target: &TargetSnapshot, text: &str) -> DeliveryOutcome {
        if text.len() > 12 * 1024
            || !self.automatic_verified
            || !self.target_valid(target)
            || target.attempted.swap(true, Ordering::SeqCst)
        {
            return DeliveryOutcome::Manual;
        }
        if self.copy(text).is_err() || !self.target_valid(target) {
            return DeliveryOutcome::Manual;
        }
        match crate::virtual_keyboard::paste(|deadline| self.target_valid_until(target, deadline)) {
            Ok(()) => DeliveryOutcome::SentUnconfirmed,
            Err(crate::virtual_keyboard::PasteError::NotSent) => DeliveryOutcome::Manual,
            Err(crate::virtual_keyboard::PasteError::Unknown) => DeliveryOutcome::Unknown,
        }
    }
    /// Call from the application's regular timer to reap an owner which lost
    /// selection ownership while no new copy action is being performed.
    pub fn maintenance(&mut self) {
        self.reap_owner();
    }
    fn reap_owner(&mut self) {
        if self
            .clipboard_owner
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))))
        {
            self.clipboard_owner.take();
        }
    }
}
impl Drop for Hyprland {
    fn drop(&mut self) {
        if let Some(mut child) = self.clipboard_owner.take() {
            terminate_group(&mut child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_diagnostics_and_clipboard_keep_separate_output_limits() {
        let source = "import sys; sys.stdout.buffer.write(b'x' * 200000)";
        assert!(matches!(
            run_until("python3", &["-c", source], Instant::now() + STEP),
            Err(PlatformError::OutputLimit)
        ));
        let clipboard = run_until_limit(
            "python3",
            &["-c", source],
            Instant::now() + STEP,
            COPY_LIMIT,
        )
        .unwrap();
        assert_eq!(clipboard.len(), 200000);
        assert!(clipboard.iter().all(|byte| *byte == b'x'));
    }

    #[test]
    fn shared_deadline_kills_pipe_holder_after_direct_child_exit() {
        let directory = tempfile::tempdir().unwrap();
        let identity_path = directory.path().join("owned-child.json");
        // A real owned descendant inherits stdout, while its direct parent
        // exits immediately. No desktop, input, user process or model is used.
        let source = r#"
import json, os, pathlib, sys, time
pid = os.fork()
if pid == 0:
    time.sleep(30)
else:
    stat = pathlib.Path('/proc', str(pid), 'stat').read_text().rsplit(')', 1)[1].split()
    pathlib.Path(sys.argv[1]).write_text(json.dumps([pid, stat[19]]))
"#;
        let started = Instant::now();
        let result = run_until(
            "python3",
            &["-c", source, identity_path.to_str().unwrap()],
            started + Duration::from_millis(300),
        );
        let identity: (u32, String) =
            serde_json::from_slice(&std::fs::read(identity_path).unwrap()).unwrap();
        let is_running = || {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", identity.0)) else {
                return false;
            };
            let Some((_, rest)) = stat.rsplit_once(')') else {
                return false;
            };
            let fields: Vec<_> = rest.split_whitespace().collect();
            fields.get(19) == Some(&identity.1.as_str())
                && fields
                    .first()
                    .is_some_and(|state| !matches!(*state, "Z" | "X"))
        };
        let stopped_by = Instant::now() + Duration::from_secs(1);
        while is_running() && Instant::now() < stopped_by {
            thread::sleep(Duration::from_millis(5));
        }
        let leaked = is_running();
        if leaked {
            // Test-failure cleanup uses the recorded start identity above,
            // never an arbitrary or possibly reused process ID.
            unsafe { libc::kill(identity.0 as i32, libc::SIGKILL) };
        }
        assert!(matches!(result, Err(PlatformError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!leaked, "owned pipe-holding descendant survived deadline");
    }
    fn pair() -> (TargetSnapshot, ActiveWindow) {
        (
            TargetSnapshot {
                address: "0xabc".into(),
                pid: 42,
                class: "editor".into(),
                epoch: 7,
                attempted: Arc::new(AtomicBool::new(false)),
                own_probe_sent: Arc::new(AtomicBool::new(false)),
            },
            ActiveWindow {
                address: "0xabc".into(),
                pid: 42,
                class: "editor".into(),
                mapped: Some(true),
            },
        )
    }
    #[test]
    fn returning_to_same_window_after_focus_event_is_still_rejected() {
        let (target, current) = pair();
        assert!(guard(&target, &current, 7, true, true, &[]));
        assert!(!guard(&target, &current, 8, true, true, &[]));
    }
    #[test]
    fn disconnect_locked_screen_and_reused_address_fail_closed() {
        let (target, mut current) = pair();
        assert!(!guard(&target, &current, 7, false, true, &[]));
        assert!(!guard(&target, &current, 7, true, false, &[]));
        current.pid = 43;
        assert!(!guard(&target, &current, 7, true, true, &[]));
    }
    #[test]
    fn terminal_sensitive_and_closed_target_are_manual() {
        let (mut target, mut current) = pair();
        target.class = "org.omarchy.agent".into();
        current.class = target.class.clone();
        assert!(!guard(&target, &current, 7, true, true, &[]));
        let (target, mut current) = pair();
        assert!(!guard(&target, &current, 7, true, true, &["EDITOR".into()]));
        current.mapped = Some(false);
        assert!(!guard(&target, &current, 7, true, true, &[]));
    }
    #[test]
    fn all_focus_close_disconnect_and_reconnect_invalidate_epoch() {
        let state = FocusState::default();
        state.event(b"activewindowv2");
        state.event(b"closewindow");
        state.disconnected();
        assert_eq!(state.epoch.load(Ordering::SeqCst), 3);
        assert!(!state.connected.load(Ordering::SeqCst));
    }
    #[test]
    fn compositor_lock_and_unlock_invalidate_old_snapshot() {
        let state = FocusState::default();
        state.observe_lock(true);
        let before = state.epoch.load(Ordering::SeqCst);
        state.observe_lock(false);
        assert!(!state.unlocked.load(Ordering::SeqCst));
        state.observe_lock(true);
        assert!(state.epoch.load(Ordering::SeqCst) > before);
    }
    #[test]
    fn cloned_targets_share_one_attempt_budget() {
        let (target, _) = pair();
        let cloned = target.clone();
        assert!(!target.attempted.swap(true, Ordering::SeqCst));
        assert!(cloned.attempted.swap(true, Ordering::SeqCst));
    }
    #[test]
    fn own_probe_confirmation_requires_dispatch_readback_and_own_process() {
        let (target, _) = pair();
        assert!(!own_probe_confirmation_allowed(
            &target,
            DELIVERY_PROBE_TEXT,
            42,
            true
        ));
        target.attempted.store(true, Ordering::SeqCst);
        // An ordinary delivered target is not evidence of an explicit probe.
        assert!(!own_probe_confirmation_allowed(
            &target,
            DELIVERY_PROBE_TEXT,
            42,
            true
        ));
        target.own_probe_sent.store(true, Ordering::SeqCst);
        assert!(own_probe_confirmation_allowed(
            &target,
            DELIVERY_PROBE_TEXT,
            42,
            true
        ));
        assert!(!own_probe_confirmation_allowed(
            &target,
            DELIVERY_PROBE_TEXT,
            43,
            true
        ));
        assert!(!own_probe_confirmation_allowed(
            &target,
            "different input",
            42,
            true
        ));
        assert!(!own_probe_confirmation_allowed(
            &target,
            DELIVERY_PROBE_TEXT,
            42,
            false
        ));
        target.own_probe_sent.store(false, Ordering::SeqCst);
        assert!(!own_probe_confirmation_allowed(
            &target,
            DELIVERY_PROBE_TEXT,
            42,
            true
        ));
    }
    #[test]
    fn own_probe_cannot_confirm_after_focus_change_lock_disconnect_or_sensitive_rule() {
        let (target, mut current) = pair();
        target.attempted.store(true, Ordering::SeqCst);
        target.own_probe_sent.store(true, Ordering::SeqCst);
        for safe in [
            guard(&target, &current, 8, true, true, &[]),
            guard(&target, &current, 7, false, true, &[]),
            guard(&target, &current, 7, true, false, &[]),
            guard(&target, &current, 7, true, true, &["editor".into()]),
        ] {
            assert!(!own_probe_confirmation_allowed(
                &target,
                DELIVERY_PROBE_TEXT,
                42,
                safe
            ));
        }
        current.address = "0xother".into();
        assert!(!own_probe_confirmation_allowed(
            &target,
            DELIVERY_PROBE_TEXT,
            42,
            guard(&target, &current, 7, true, true, &[])
        ));
    }
    #[test]
    fn window_title_is_ignored_and_debug_is_redacted() {
        let current: ActiveWindow = serde_json::from_str(
            r#"{"address":"0xabc","pid":42,"class":"editor","title":"private title"}"#,
        )
        .unwrap();
        assert_eq!(current.pid, 42);
        let (target, _) = pair();
        let debug = format!("{target:?}");
        assert!(!debug.contains("editor"));
        assert!(!debug.contains("0xabc"));
    }
}
