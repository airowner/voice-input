//! The host links no inference libraries. All model paths travel over private pipes.
use crossbeam_channel::{Receiver, bounded};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use voice_core::AppError;

const MAX_FRAME: usize = 4 * 1024 * 1024;

fn failure(code: &str) -> AppError {
    AppError::new(
        code,
        "worker",
        true,
        "本地推理未完成，请检查模型与资源；已有原文可在结果页重试",
    )
}

fn write_frame(writer: &mut impl Write, kind: u8, bytes: &[u8]) -> Result<(), AppError> {
    let size = bytes
        .len()
        .checked_add(1)
        .filter(|n| *n <= MAX_FRAME)
        .ok_or_else(|| failure("worker_frame_limit"))?;
    writer
        .write_all(&(size as u32).to_le_bytes())
        .and_then(|_| writer.write_all(&[kind]))
        .and_then(|_| writer.write_all(bytes))
        .and_then(|_| writer.flush())
        .map_err(|_| failure("worker_pipe"))
}

fn read_frame(reader: &mut impl Read) -> Result<Value, AppError> {
    let mut size = [0; 4];
    reader
        .read_exact(&mut size)
        .map_err(|_| failure("worker_closed"))?;
    let size = u32::from_le_bytes(size) as usize;
    if !(2..=MAX_FRAME).contains(&size) {
        return Err(failure("worker_frame_limit"));
    }
    let mut bytes = vec![0; size];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| failure("worker_closed"))?;
    if bytes[0] != 1 {
        return Err(failure("worker_frame_type"));
    }
    serde_json::from_slice(&bytes[1..]).map_err(|_| failure("worker_frame_json"))
}

struct Worker {
    child: Child,
    input: Arc<Mutex<ChildStdin>>,
    events: Receiver<Result<Value, AppError>>,
    signature: String,
    last_used: Instant,
    log_overflow: Arc<AtomicBool>,
    terminated: bool,
}

impl Worker {
    fn spawn(
        executable: &Path,
        kind: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self, AppError> {
        if !executable.is_absolute() {
            return Err(failure("worker_path"));
        }
        let mut command = Command::new(executable);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        if let Some(systemroot) = std::env::var_os("SYSTEMROOT") {
            command.env("SYSTEMROOT", systemroot);
        }
        let mut child = command.spawn().map_err(|_| failure("worker_start"))?;
        let input = child.stdin.take().ok_or_else(|| failure("worker_pipe"))?;
        let mut stdout = child.stdout.take().ok_or_else(|| failure("worker_pipe"))?;
        let mut stderr = child.stderr.take().ok_or_else(|| failure("worker_pipe"))?;
        let (sender, events) = bounded(16);
        thread::spawn(move || {
            loop {
                let event = read_frame(&mut stdout);
                let failed = event.is_err();
                if sender.send(event).is_err() || failed {
                    break;
                }
            }
        });
        let log_overflow = Arc::new(AtomicBool::new(false));
        let overflow = log_overflow.clone();
        thread::spawn(move || {
            let mut total = 0_usize;
            let mut buffer = [0; 4096];
            while let Ok(n) = stderr.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                total = total.saturating_add(n);
                if total > 65536 {
                    overflow.store(true, Ordering::Release);
                }
                // Never retain engine stderr: it can include model names or input text.
            }
        });
        let mut worker = Self {
            child,
            input: Arc::new(Mutex::new(input)),
            events,
            signature: String::new(),
            last_used: Instant::now(),
            log_overflow,
            terminated: false,
        };
        let hello = worker.receive(Duration::from_secs(5), cancelled, None)?;
        if hello.get("event").and_then(Value::as_str) != Some("hello")
            || hello.get("protocol").and_then(Value::as_u64) != Some(1)
            || hello.get("worker").and_then(Value::as_str) != Some(kind)
        {
            return Err(failure("worker_handshake"));
        }
        Ok(worker)
    }

    fn send(&mut self, message: &Value) -> Result<(), AppError> {
        self.send_with_budget(message, Duration::from_millis(500))
    }
    fn send_with_budget(&mut self, message: &Value, budget: Duration) -> Result<(), AppError> {
        let bytes = serde_json::to_vec(message).map_err(|_| failure("worker_json"))?;
        self.send_bytes_with_budget(1, bytes, budget)
    }

    fn send_bytes(&mut self, kind: u8, bytes: Vec<u8>) -> Result<(), AppError> {
        self.send_bytes_with_budget(kind, bytes, Duration::from_millis(500))
    }
    fn send_bytes_with_budget(
        &mut self,
        kind: u8,
        bytes: Vec<u8>,
        budget: Duration,
    ) -> Result<(), AppError> {
        let input = self.input.clone();
        let (sender, result) = bounded(1);
        let writer = thread::spawn(move || {
            let value = input
                .lock()
                .map_err(|_| failure("worker_pipe"))
                .and_then(|mut writer| write_frame(&mut *writer, kind, &bytes));
            let _ = sender.send(value);
        });
        let value = result.recv_timeout(budget);
        if value.is_err() {
            self.terminate();
        }
        let _ = writer.join();
        value.map_err(|_| failure("worker_pipe_timeout"))?
    }

    fn receive(
        &mut self,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
        job: Option<&str>,
    ) -> Result<Value, AppError> {
        let start = Instant::now();
        loop {
            if self.log_overflow.load(Ordering::Acquire) {
                return Err(failure("worker_stderr_limit"));
            }
            if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                let deadline = Instant::now() + Duration::from_millis(500);
                let _ = self.send_with_budget(
                    &json!({"op":"cancel","job_id":job}),
                    Duration::from_millis(500),
                );
                while Instant::now() < deadline {
                    if let Ok(Ok(event)) = self.events.recv_timeout(Duration::from_millis(20))
                        && event.get("event").and_then(Value::as_str) == Some("cancelled")
                        && event.get("job_id").and_then(Value::as_str) == job
                    {
                        break;
                    }
                }
                return Err(failure("cancelled"));
            }
            if start.elapsed() >= timeout {
                return Err(failure("worker_timeout"));
            }
            match self.events.recv_timeout(Duration::from_millis(25)) {
                Ok(event) => {
                    let value = event?;
                    if let Some(job) = job
                        && value.get("job_id").and_then(Value::as_str) != Some(job)
                    {
                        return Err(failure("worker_job_mismatch"));
                    }
                    if value.get("event").and_then(Value::as_str) == Some("error") {
                        let code = value
                            .get("code")
                            .and_then(Value::as_str)
                            .filter(|s| {
                                s.len() < 64
                                    && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                            })
                            .unwrap_or("worker_error");
                        return Err(failure(code));
                    }
                    return Ok(value);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => return Err(failure("worker_closed")),
            }
        }
    }
    fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub struct WorkerPool {
    executable: PathBuf,
    kind: String,
    idle: Duration,
    worker: Mutex<Option<Worker>>,
    loaded_model: Mutex<Option<Arc<crate::models::ModelRecord>>>,
}

impl WorkerPool {
    pub fn new(executable: PathBuf, kind: &str, idle_seconds: u64) -> Self {
        Self {
            executable,
            kind: kind.into(),
            idle: Duration::from_secs(idle_seconds),
            worker: Mutex::new(None),
            loaded_model: Mutex::new(None),
        }
    }
    pub fn reap_idle(&self) {
        if let Ok(mut guard) = self.worker.try_lock()
            && guard
                .as_ref()
                .is_some_and(|w| w.last_used.elapsed() >= self.idle)
        {
            guard.take();
            if let Ok(mut model) = self.loaded_model.lock() {
                model.take();
            }
        }
    }
    pub fn unload(&self) {
        if let Ok(mut guard) = self.worker.lock() {
            guard.take();
            if let Ok(mut model) = self.loaded_model.lock() {
                model.take();
            }
        }
    }
    /// Keep immutable model files referenced for the entire resident worker life,
    /// including idle time after a task has returned.
    pub fn pin_model(&self, model: Arc<crate::models::ModelRecord>) {
        if let Ok(worker) = self.worker.lock()
            && worker.is_some()
            && let Ok(mut lease) = self.loaded_model.lock()
        {
            *lease = Some(model);
        }
    }
    pub fn verify(&self, load: Value) -> Result<(), AppError> {
        self.verify_cancellable(load, &AtomicBool::new(false))
    }
    pub fn verify_cancellable(&self, load: Value, cancelled: &AtomicBool) -> Result<(), AppError> {
        validate_memory_budget(&load, &self.kind)?;
        let mut worker = Worker::spawn(&self.executable, &self.kind, Some(cancelled))?;
        worker.send(&load)?;
        let ready = worker.receive(Duration::from_secs(60), Some(cancelled), None)?;
        if ready.get("event").and_then(Value::as_str) != Some("ready") {
            return Err(failure("worker_load_protocol"));
        }
        Ok(())
    }
    fn ensure_loaded(
        &self,
        slot: &mut Option<Worker>,
        signature: &str,
        load: &Value,
        cancelled: &AtomicBool,
    ) -> Result<(), AppError> {
        if slot
            .as_ref()
            .is_some_and(|worker| worker.signature != signature)
        {
            slot.take();
            if let Ok(mut lease) = self.loaded_model.lock() {
                lease.take();
            }
        }
        if slot.is_none() {
            validate_memory_budget(load, &self.kind)?;
            let mut worker = Worker::spawn(&self.executable, &self.kind, Some(cancelled))?;
            worker.send(load)?;
            let ready = worker.receive(Duration::from_secs(60), Some(cancelled), None)?;
            if ready.get("event").and_then(Value::as_str) != Some("ready") {
                return Err(failure("worker_load_protocol"));
            }
            worker.signature = signature.into();
            *slot = Some(worker);
        }
        Ok(())
    }
    /// Loads without generating, and keeps the resident worker for later jobs.
    pub fn preload(
        &self,
        signature: &str,
        load: Value,
        cancelled: &AtomicBool,
    ) -> Result<(), AppError> {
        let mut guard = self.worker.lock().map_err(|_| failure("worker_lock"))?;
        let result = self.ensure_loaded(&mut guard, signature, &load, cancelled);
        if result.is_err() {
            guard.take();
            if let Ok(mut lease) = self.loaded_model.lock() {
                lease.take();
            }
        } else if let Some(worker) = guard.as_mut() {
            worker.last_used = Instant::now();
        }
        result
    }
    pub fn is_loaded(&self, signature: &str) -> bool {
        self.worker.try_lock().is_ok_and(|worker| {
            worker
                .as_ref()
                .is_some_and(|worker| worker.signature == signature)
        })
    }
    pub fn run(
        &self,
        signature: &str,
        load: Value,
        run: Value,
        pcm: Option<&[f32]>,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<String, AppError> {
        let mut guard = self.worker.lock().map_err(|_| failure("worker_lock"))?;
        let result = (|| {
            self.ensure_loaded(&mut guard, signature, &load, cancelled)?;
            if cancelled.load(Ordering::Acquire) {
                return Err(failure("cancelled"));
            }
            let worker = guard.as_mut().unwrap();
            if let Some(pcm) = pcm {
                if pcm.len() > 960_000 || pcm.iter().any(|v| !v.is_finite()) {
                    return Err(failure("worker_audio_limit"));
                }
                let bytes: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
                worker.send_bytes(2, bytes)?;
            }
            let job = run
                .get("job_id")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("worker_job"))?;
            worker.send(&run)?;
            let result = worker.receive(timeout, Some(cancelled), Some(job))?;
            if result.get("event").and_then(Value::as_str) != Some("result") {
                return Err(failure("worker_result_protocol"));
            }
            let text = result
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("worker_result_text"))?
                .to_owned();
            worker.last_used = Instant::now();
            Ok(text)
        })();
        if result.is_err() {
            guard.take();
            if let Ok(mut model) = self.loaded_model.lock() {
                model.take();
            }
        }
        result
    }
}

fn validate_memory_budget(load: &Value, kind: &str) -> Result<(), AppError> {
    let path = load
        .get("model_path")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("worker_model_path"))?;
    let metadata = std::fs::metadata(path).map_err(|_| failure("worker_model_missing"))?;
    if !metadata.is_file() {
        return Err(failure("worker_model_path"));
    }
    let context = load
        .get("context_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(4096);
    if !(2048..=16384).contains(&context) {
        return Err(failure("worker_context_limit"));
    }
    let bytes = metadata.len();
    let estimate = if kind == "asr" {
        bytes.saturating_mul(3).saturating_add(128 * 1024 * 1024)
    } else {
        bytes
            .saturating_add(context.saturating_mul(128 * 1024))
            .saturating_add(256 * 1024 * 1024)
    };
    let available = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("MemAvailable:")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse::<u64>().ok())
            })
        })
        .map(|kb| kb.saturating_mul(1024));
    if available.is_none_or(|available| estimate.saturating_add(512 * 1024 * 1024) > available) {
        return Err(AppError::new(
            "model_memory_budget",
            "models",
            true,
            "当前可用内存不足或无法确认预算，请关闭其他应用、选择更小模型或明确配置远程处理",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hostile_frames_fail_before_allocation_or_json_use() {
        for size in [0_u32, 1, (MAX_FRAME + 1) as u32, u32::MAX] {
            assert!(read_frame(&mut std::io::Cursor::new(size.to_le_bytes())).is_err());
        }
        let mut wrong = Vec::new();
        write_frame(&mut wrong, 2, b"{}").unwrap();
        assert!(read_frame(&mut std::io::Cursor::new(wrong)).is_err());
        let mut valid = Vec::new();
        write_frame(&mut valid, 1, br#"{"event":"hello"}"#).unwrap();
        assert_eq!(
            read_frame(&mut std::io::Cursor::new(valid)).unwrap()["event"],
            "hello"
        );
    }

    /// These Linux subprocess fixtures verify supervision/protocol only. Their
    /// synthetic model bytes and replies are never real inference evidence.
    #[cfg(target_os = "linux")]
    mod subprocess {
        use super::*;
        use std::{
            fs,
            os::{fd::AsRawFd, unix::fs::PermissionsExt},
            sync::mpsc,
        };

        struct FakeWorker {
            directory: tempfile::TempDir,
            executable: PathBuf,
        }
        impl FakeWorker {
            fn new(kind: &str, body: &str) -> Self {
                let python = std::env::var_os("PATH")
                    .and_then(|path| {
                        std::env::split_paths(&path)
                            .map(|p| p.join("python3"))
                            .find(|p| {
                                fs::metadata(p).is_ok_and(|m| {
                                    m.is_file() && m.permissions().mode() & 0o111 != 0
                                })
                            })
                    })
                    .expect("Linux subprocess protocol tests require python3 on PATH");
                let python = fs::canonicalize(python).unwrap();
                let directory = tempfile::tempdir().unwrap();
                let executable = directory.path().join("fake-worker.py");
                let pid_file = directory.path().join("worker.pid");
                let script = format!(
                    r#"#!{} -u
import json, os, struct, sys, time
with open({}, 'w') as pid_file:
    pid_file.write(str(os.getpid()))
def send(value):
    payload = json.dumps(value).encode('utf-8')
    sys.stdout.buffer.write(struct.pack('<I', len(payload) + 1) + b'\x01' + payload)
    sys.stdout.buffer.flush()
def exact(length):
    data = b''
    while len(data) < length:
        block = sys.stdin.buffer.read(length - len(data))
        if not block:
            raise EOFError()
        data += block
    return data
def receive():
    size = struct.unpack('<I', exact(4))[0]
    frame = exact(size)
    return frame[0], json.loads(frame[1:]) if frame[0] == 1 else frame[1:]
send({{"event":"hello", "protocol":1, "worker":{}}})
{}
"#,
                    python.display(),
                    serde_json::to_string(&pid_file).unwrap(),
                    serde_json::to_string(kind).unwrap(),
                    body
                );
                fs::write(&executable, script).unwrap();
                fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
                Self {
                    directory,
                    executable,
                }
            }
            fn watchdog(&self, timeout: Duration) -> Watchdog {
                let pid_file = self.directory.path().join("worker.pid");
                let (done, receiver) = mpsc::channel();
                let handle = thread::spawn(move || {
                    if receiver.recv_timeout(timeout).is_err()
                        && let Ok(pid) = fs::read_to_string(pid_file)
                        && let Ok(pid) = pid.trim().parse::<i32>()
                        && pid > 1
                    {
                        // Only the fixture's own recorded subprocess is killed.
                        // Process-group fallback bounds a regression in host join.
                        unsafe {
                            libc::kill(-pid, libc::SIGKILL);
                            libc::kill(pid, libc::SIGKILL);
                        }
                    }
                });
                Watchdog {
                    done: Some(done),
                    handle: Some(handle),
                }
            }
        }
        struct Watchdog {
            done: Option<mpsc::Sender<()>>,
            handle: Option<thread::JoinHandle<()>>,
        }
        impl Drop for Watchdog {
            fn drop(&mut self) {
                if let Some(done) = self.done.take() {
                    let _ = done.send(());
                }
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
            }
        }
        fn assert_reaped(worker: &mut Worker) {
            assert!(
                worker.child.try_wait().unwrap().is_some(),
                "host must reap its terminated worker"
            );
        }
        fn backpressure_stdin(worker: &Worker) {
            let mut input = worker.input.lock().unwrap();
            let fd = input.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
            let mut blocked = false;
            for _ in 0..2048 {
                match input.write(&[0; 4096]) {
                    Ok(0) => panic!("fixture stdin unexpectedly closed"),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        blocked = true;
                        break;
                    }
                    Err(error) => panic!("cannot prepare fixture backpressure: {error}"),
                }
            }
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) }, 0);
            assert!(blocked, "fixture must stop reading stdin");
        }

        #[test]
        fn stalled_pcm_write_is_bounded_and_worker_is_reaped() {
            for inherited_pipe in [false, true] {
                let body = if inherited_pipe {
                    // A finite-lived grandchild retains stdin/stdout. Killing
                    // only the direct child would delay writer.join > 1.2 s.
                    // Its own 1.5 s limit also bounds a broken group-kill test.
                    "import subprocess\nsubprocess.Popen([sys.executable, '-c', 'import time;time.sleep(1.5)'], stdin=sys.stdin, stdout=sys.stdout, stderr=sys.stderr)\nwhile True:\n    time.sleep(1)"
                } else {
                    "while True:\n    time.sleep(1)"
                };
                let fixture = FakeWorker::new("asr", body);
                let _watchdog = fixture.watchdog(Duration::from_secs(2));
                let mut worker = Worker::spawn(&fixture.executable, "asr", None).unwrap();
                let start = Instant::now();
                let failure = worker.send_bytes(2, vec![0; 3 * 1024 * 1024]).unwrap_err();
                assert_eq!(failure.code, "worker_pipe_timeout");
                assert!(
                    start.elapsed() < Duration::from_millis(1200),
                    "stalled PCM write exceeded bounded shutdown (inherited_pipe={inherited_pipe})"
                );
                assert_reaped(&mut worker);
            }
        }
        #[test]
        fn outside_job_error_is_rejected_before_worker_error_code() {
            let fixture = FakeWorker::new(
                "asr",
                r#"while True:
    kind, message = receive()
    if kind == 1 and message.get('op') == 'run':
        send({"event":"error", "job_id":"outside-job", "code":"foreign_error", "message":"synthetic-private-input"})
"#,
            );
            let _watchdog = fixture.watchdog(Duration::from_secs(3));
            let mut worker = Worker::spawn(&fixture.executable, "asr", None).unwrap();
            worker
                .send(&json!({"op":"run","job_id":"expected-job"}))
                .unwrap();
            let error = worker
                .receive(Duration::from_secs(1), None, Some("expected-job"))
                .unwrap_err();
            assert_eq!(error.code, "worker_job_mismatch");
            assert!(
                !serde_json::to_string(&error)
                    .unwrap()
                    .contains("synthetic-private-input")
            );
        }
        #[test]
        fn cancel_with_backpressured_pipe_and_no_ack_uses_one_budget() {
            let fixture = FakeWorker::new("asr", "while True:\n    time.sleep(1)");
            let _watchdog = fixture.watchdog(Duration::from_secs(2));
            let mut worker = Worker::spawn(&fixture.executable, "asr", None).unwrap();
            backpressure_stdin(&worker);
            let cancelled = AtomicBool::new(true);
            let start = Instant::now();
            let error = worker
                .receive(
                    Duration::from_secs(30),
                    Some(&cancelled),
                    Some("cancel-job"),
                )
                .unwrap_err();
            assert_eq!(error.code, "cancelled");
            assert!(
                start.elapsed() < Duration::from_millis(850),
                "cancel send and acknowledgement must share the 500 ms budget"
            );
            assert_reaped(&mut worker);
        }
        #[test]
        fn excessive_stderr_is_bounded_and_synthetic_text_is_not_returned() {
            let fixture = FakeWorker::new(
                "asr",
                r#"receive()
sys.stderr.buffer.write(b'SYNTHETIC_PRIVATE_CANARY\n' * 4096)
sys.stderr.buffer.flush()
while True:
    time.sleep(1)
"#,
            );
            let _watchdog = fixture.watchdog(Duration::from_secs(3));
            let mut worker = Worker::spawn(&fixture.executable, "asr", None).unwrap();
            worker.send(&json!({"op":"load"})).unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            while !worker.log_overflow.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(
                worker.log_overflow.load(Ordering::Acquire),
                "stderr limit must be detected"
            );
            let error = worker
                .receive(Duration::from_secs(1), None, None)
                .unwrap_err();
            assert_eq!(error.code, "worker_stderr_limit");
            assert!(
                !serde_json::to_string(&error)
                    .unwrap()
                    .contains("SYNTHETIC_PRIVATE_CANARY")
            );
            assert!(!format!("{error:?}").contains("SYNTHETIC_PRIVATE_CANARY"));
        }
        #[test]
        fn resident_worker_model_lease_blocks_delete_until_unload() {
            let fixture = FakeWorker::new(
                "llm",
                r#"while True:
    kind, message = receive()
    if kind != 1:
        continue
    if message.get('op') == 'load':
        send({"event":"ready"})
    elif message.get('op') == 'run':
        send({"event":"result", "job_id":message['job_id'], "text":"synthetic protocol result"})
    elif message.get('op') == 'cancel':
        send({"event":"cancelled", "job_id":message.get('job_id')})
"#,
            );
            let _watchdog = fixture.watchdog(Duration::from_secs(3));
            let source = fixture.directory.path().join("synthetic-model.gguf");
            let mut bytes = vec![7; 2048];
            bytes[..4].copy_from_slice(b"GGUF");
            fs::write(&source, bytes).unwrap();
            let mut models =
                crate::models::ModelManager::open(fixture.directory.path().join("models")).unwrap();
            // This closure stands in for loading a real model; it is only a
            // deterministic lease/protocol test and not an engine-load claim.
            let info = models
                .import(&source, "gguf", |path| {
                    assert!(path.is_file());
                    Ok(())
                })
                .unwrap();
            let model = models.resolve(&info.id, "gguf").unwrap();
            let published = model.path.clone();
            let pool = WorkerPool::new(fixture.executable.clone(), "llm", 60);
            let reply = pool
                .run(
                    "fixture-model",
                    json!({"op":"load","model_path":published}),
                    json!({"op":"run","job_id":"lease-job"}),
                    None,
                    Duration::from_secs(1),
                    &AtomicBool::new(false),
                )
                .unwrap();
            assert_eq!(reply, "synthetic protocol result");
            pool.pin_model(model.clone());
            drop(model);
            assert_eq!(
                models
                    .delete(&info.id, &voice_core::Config::default())
                    .unwrap_err()
                    .code,
                "model_in_use"
            );
            assert!(published.exists());
            pool.unload();
            assert!(pool.worker.lock().unwrap().is_none());
            assert!(pool.loaded_model.lock().unwrap().is_none());
            models
                .delete(&info.id, &voice_core::Config::default())
                .unwrap();
            assert!(!published.exists());
        }
    }
}
