#![cfg(target_os = "linux")]
//! V05 A test: kill a real child with SIGKILL while its SQLite connection is
//! still open. A temporary counter file models the native delivery adapter's
//! side effect; no keyboard/clipboard is used. This is not a desktop M test.

use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::OpenOptionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};
use voice_core::{
    Action, Config, Controller, DeliveryStatus, DispatchResult, Phase, RecoveryStore,
};

const DIRECTORY: &str = "VOICE_V1_RECOVERY_CRASH_CHILD_DIR";
const STAGE: &str = "VOICE_V1_RECOVERY_CRASH_CHILD_STAGE";
const MARKER: &str = "VOICE_V1_RECOVERY_DURABLE_MARKER";
const RAW: &str = "固定合成原文：不要删除 /tmp/voice-input；OpenAI 返回 42 ms，数字 2026。";
const JOB: &str = "synthetic-crash-job";
const NOW: i64 = 1_000_000;

/// This helper is selected explicitly in the new process. A normal test run
/// does nothing here; all assertions are in the bounded parent scenario below.
#[test]
fn helper() {
    let Some(directory) = std::env::var_os(DIRECTORY) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let stage = std::env::var(STAGE).expect("child phase is required");
    assert!(["after_raw", "before_native", "after_native"].contains(&stage.as_str()));
    let mut store = RecoveryStore::open(directory.join("recovery.sqlite3"), true).unwrap();
    assert!(store.secure_delete_enabled().unwrap());
    store.commit_raw(JOB, RAW, &Config::default(), NOW).unwrap();
    if stage != "after_raw" {
        // Production ordering: durable attempt marker precedes native dispatch.
        store
            .mark_delivery(JOB, DeliveryStatus::Attempting, NOW + 1)
            .unwrap();
    }
    if stage == "after_native" {
        // A deterministic test adapter, never the real desktop. create_new()
        // makes a duplicate dispatch fail rather than silently overwrite count.
        let mut count = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join("native-side-effect-count.txt"))
            .unwrap();
        count.write_all(b"1\n").unwrap();
        count.sync_all().unwrap();
        fs::File::open(&directory).unwrap().sync_all().unwrap();
    }
    // Leading newline makes this a distinct line even with libtest's progress
    // prefix. Parent kills only after this explicit durable-stage handshake.
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "\n{MARKER}").unwrap();
    stdout.flush().unwrap();
    drop(stdout);
    loop {
        std::hint::black_box(&store); // Keep the live SQLite handle until SIGKILL.
        thread::park();
    }
}

struct KillAndReap(Child);
impl Drop for KillAndReap {
    fn drop(&mut self) {
        // Covers early panic, channel timeout, assertion failure and I/O errors.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn side_effect(path: &Path) -> Option<String> {
    match fs::read_to_string(path.join("native-side-effect-count.txt")) {
        Ok(value) => Some(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("cannot inspect simulated delivery counter: {error}"),
    }
}

#[test]
fn sigkill_at_raw_before_and_after_native_dispatch_never_replays_delivery() {
    for phase in ["after_raw", "before_native", "after_native"] {
        let directory = tempfile::tempdir().unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "helper", "--nocapture"])
            .env_clear()
            .env(DIRECTORY, directory.path())
            .env(STAGE, phase)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = KillAndReap(child);
        let stdout = child.0.stdout.take().unwrap();
        let stderr = child.0.stderr.take().unwrap();
        let (marker_sender, marker_receiver) = mpsc::sync_channel(1);
        let output_reader = thread::spawn(move || {
            for line in BufReader::new(stdout.take(16 * 1024)).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(_) => break,
                };
                if line == MARKER {
                    let _ = marker_sender.send(true);
                    return;
                }
            }
            let _ = marker_sender.send(false);
        });
        let error_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.take(16 * 1024).read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let marker = marker_receiver.recv_timeout(Duration::from_secs(3));
        // Always terminate and reap BEFORE joining any pipe reader or asserting
        // the handshake. Readers then observe EOF even when the helper failed.
        let kill = child.0.kill();
        let status = child.0.wait().unwrap();
        output_reader.join().unwrap();
        let diagnostics = error_reader.join().unwrap();
        assert_eq!(
            marker,
            Ok(true),
            "{phase}: child did not reach durable marker; {diagnostics}"
        );
        assert!(kill.is_ok(), "{phase}: parked child must still be alive");
        assert_eq!(status.signal(), Some(9), "{phase}: expected real SIGKILL");
        let expected_count = if phase == "after_native" {
            Some("1\n".to_owned())
        } else {
            None
        };
        assert_eq!(side_effect(directory.path()), expected_count);

        let mut reopened =
            RecoveryStore::open(directory.path().join("recovery.sqlite3"), true).unwrap();
        let recovered = reopened.list(NOW + 2).unwrap();
        assert_eq!(
            recovered.len(),
            1,
            "{phase}: durable raw must survive SIGKILL"
        );
        assert_eq!(
            recovered[0].raw, RAW,
            "{phase}: raw must be byte-for-byte identical"
        );
        assert!(recovered[0].processed.is_none());
        assert!(recovered[0].preview_required);
        assert_eq!(
            recovered[0].delivery,
            if phase == "after_raw" {
                DeliveryStatus::NotAttempted
            } else {
                DeliveryStatus::Unknown
            }
        );

        let mut controller = Controller::new(Config::default());
        let (result, effects) = controller.dispatch(Action::Restore { records: recovered });
        assert_eq!(result, DispatchResult::Accepted);
        assert!(
            effects.is_empty(),
            "{phase}: recovery may not start capture, rewrite or native delivery"
        );
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.phase, Phase::Ready);
        assert!(snapshot.current.is_none());
        assert_eq!(snapshot.history[0].raw, RAW);
        assert!(snapshot.history[0].preview_required);
        assert!(
            controller
                .dispatch(Action::Tick { now_ms: NOW + 3 })
                .1
                .is_empty()
        );
        assert_eq!(
            side_effect(directory.path()),
            expected_count,
            "{phase}: simulated native side effect may not replay"
        );
    }
}
