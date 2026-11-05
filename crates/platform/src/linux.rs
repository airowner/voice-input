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
fn run(program: &str, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    run_for(program, args, input, STEP)
}

fn run_for(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    run_for_limit(program, args, input, timeout, OUTPUT_LIMIT)
}
fn run_for_limit(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    timeout: Duration,
    output_limit: u64,
) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| PlatformError::Command)?;
    let stdout = child.stdout.take().ok_or(PlatformError::Command)?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(output_limit + 1).read_to_end(&mut bytes);
        (result, bytes)
    });
    let writer = input.map(|bytes| {
        let bytes = bytes.to_vec();
        let mut stdin = child.stdin.take().expect("piped stdin");
        thread::spawn(move || stdin.write_all(&bytes))
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                terminate_group(&mut child);
                break Err(PlatformError::Timeout);
            }
            Err(_) => {
                terminate_group(&mut child);
                break Err(PlatformError::Command);
            }
        }
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let (read_result, bytes) = reader.join().map_err(|_| PlatformError::Command)?;
    if bytes.len() as u64 > output_limit {
        return Err(PlatformError::OutputLimit);
    }
    read_result.map_err(|_| PlatformError::Command)?;
    if !status?.success() {
        return Err(PlatformError::Command);
    }
    Ok(bytes)
}

fn terminate_group(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    // The child was created in a dedicated process group above.
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
    let Ok(bytes) = run("hyprctl", &["-j", "locked"], None) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    value.get("locked").and_then(|v| v.as_bool()) == Some(false)
}

fn session_safe() -> bool {
    // logind LockedHint alone can be stale: compositor is authoritative.
    if !compositor_unlocked() {
        return false;
    }
    let session = std::env::var("XDG_SESSION_ID").unwrap_or_else(|_| "self".into());
    let Ok(status) = run(
        "loginctl",
        &["show-session", &session, "-p", "LockedHint", "-p", "Active"],
        None,
    ) else {
        return false;
    };
    let status = String::from_utf8_lossy(&status);
    if !status.lines().any(|s| s == "LockedHint=no") || !status.lines().any(|s| s == "Active=yes") {
        return false;
    }
    let Ok(monitors) = run("hyprctl", &["-j", "monitors"], None) else {
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
            virtual_keyboard_tool_present: tool_present("wtype"),
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
        serde_json::from_slice(&run("hyprctl", &["-j", "activewindow"], None)?)
            .map_err(|_| PlatformError::Target)
    }
    fn target_valid(&self, target: &TargetSnapshot) -> bool {
        let state = &self.monitor.state;
        if !session_safe() {
            return false;
        }
        let Ok(current) = self.active_window() else {
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
        let mut child = Command::new("wl-copy")
            .args(["--foreground", "--type", "text/plain;charset=utf-8"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| PlatformError::Command)?;
        let mut stdin = child.stdin.take().ok_or(PlatformError::Command)?;
        let deadline = Instant::now() + STEP;
        let fd = stdin.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            terminate_group(&mut child);
            return Err(PlatformError::Clipboard);
        }
        let mut remaining = text.as_bytes();
        while !remaining.is_empty() {
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
            let Some(budget) = deadline.checked_duration_since(Instant::now()) else {
                break false;
            };
            if let Ok(readback) =
                run_for_limit("wl-paste", &["--no-newline"], None, budget, COPY_LIMIT)
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
    /// Caller must durably mark the attempt before calling. Never retries wtype.
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
        match run("wtype", &["-M", "ctrl", "-k", "v", "-m", "ctrl"], None) {
            Ok(_) => DeliveryOutcome::SentUnconfirmed,
            Err(_) => DeliveryOutcome::Unknown,
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
