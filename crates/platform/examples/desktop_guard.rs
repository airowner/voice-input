//! Explicit native focus/close guard component probe; never a full V02 verdict.
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use serde_json::{Value, json};
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        path::PathBuf,
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    use voice_platform::{DELIVERY_PROBE_TEXT, DeliveryOutcome, Hyprland, TargetSnapshot};
    const RESULT: &str = "固定测试结果 42\nSynthetic retained result: /tmp/example.txt";
    fn identity(pid: u32) -> Result<(u32, u64, PathBuf), Box<dyn std::error::Error>> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .ok_or("bad stat")?
            .1
            .split_whitespace()
            .collect();
        Ok((
            fields[1].parse()?,
            fields[19].parse()?,
            fs::read_link(format!("/proc/{pid}/exe"))?,
        ))
    }
    struct Fixture {
        child: Child,
        identity: (u32, u64, PathBuf),
        replies: mpsc::Receiver<Value>,
        cleaned: bool,
    }
    impl Fixture {
        fn verify(&self) -> Result<(), Box<dyn std::error::Error>> {
            if identity(self.child.id())? != self.identity || self.identity.0 != std::process::id()
            {
                return Err("owned process identity changed".into());
            }
            Ok(())
        }
        fn reply(&self) -> Result<Value, Box<dyn std::error::Error>> {
            let value = self.replies.recv_timeout(Duration::from_secs(5))?;
            if value["event"] == "error" {
                return Err("owned target command failed".into());
            }
            Ok(value)
        }
        fn request(&mut self, value: Value) -> Result<Value, Box<dyn std::error::Error>> {
            self.verify()?;
            let input = self.child.stdin.as_mut().ok_or("stdin closed")?;
            writeln!(input, "{value}")?;
            input.flush()?;
            self.reply()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                if !self.cleaned && self.verify().is_ok() {
                    let _ = self.request(json!({"op":"cleanup"}));
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline && self.child.try_wait().ok().flatten().is_none() {
                    thread::sleep(Duration::from_millis(10));
                }
                // A direct child cannot reuse its PID before being reaped. On
                // unexpected exec, require unchanged parent and start identity
                // before terminating this still-owned process.
                if self.child.try_wait().ok().flatten().is_none()
                    && identity(self.child.id())
                        .is_ok_and(|found| found.0 == self.identity.0 && found.1 == self.identity.1)
                {
                    let _ = self.child.kill();
                }
            }
            let _ = self.child.wait();
        }
    }
    fn capture(
        platform: &Hyprland,
        pid: u32,
        minimum_epoch: u64,
    ) -> Result<TargetSnapshot, Box<dyn std::error::Error>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(target) = platform.capture_target()
                && target.pid() == pid
                && target.focus_epoch() > minimum_epoch
                && platform.can_deliver(&target)
            {
                return Ok(target);
            }
            if Instant::now() >= deadline {
                return Err("native focus/event not observed".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let mut args = std::env::args().skip(1);
    let first = args
        .next()
        .ok_or("desktop_guard [--backend wayland|x11] NEW.json")?;
    let (backend, output) = if first == "--backend" {
        let backend = args.next().ok_or("explicit backend required")?;
        if !matches!(backend.as_str(), "wayland" | "x11") {
            return Err("backend must be wayland or x11".into());
        }
        (
            backend,
            PathBuf::from(args.next().ok_or("new output path required")?),
        )
    } else {
        ("wayland".to_owned(), PathBuf::from(first))
    };
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    if output.exists() {
        return Err("output exists".into());
    }
    let mut child = Command::new("python3")
        .arg(root.join("scripts/desktop_guard_target.py"))
        .args(["--backend", &backend])
        .env("GDK_BACKEND", &backend)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("missing stdout")?;
    let (sender, receiver) = mpsc::sync_channel(8);
    thread::spawn(move || {
        for line in BufReader::new(stdout.take(16 * 1024)).lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str(&line) else {
                break;
            };
            if sender.send(value).is_err() {
                break;
            }
        }
    });
    // spawn has completed exec; install cleanup before waiting for GTK, so a
    // startup timeout cannot orphan the fixture or its windows.
    let expected = identity(child.id())?;
    let mut fixture = Fixture {
        child,
        identity: expected,
        replies: receiver,
        cleaned: false,
    };
    let ready = fixture.reply()?;
    fixture.verify()?;
    if ready["event"] != "ready" || ready["pid"].as_u64() != Some(fixture.child.id().into()) {
        return Err("owned fixture not ready".into());
    }
    if ready["backend"] != backend || ready["native_xwayland"] != (backend == "x11") {
        return Err("native window backend did not match requested backend".into());
    }
    let pid = fixture.child.id();
    let mut platform = Hyprland::new(true, vec![]);
    fixture.request(json!({"op":"focus","target":"a"}))?;
    let original = capture(&platform, pid, 0)?;
    fixture.request(json!({"op":"verify_focus","target":"a"}))?;
    let started = Instant::now();
    let b = fixture.request(json!({"op":"focus","target":"b"}))?;
    if b["address"] != ready["addresses"]["b"] {
        return Err("wrong B native focus".into());
    }
    let in_b = capture(&platform, pid, original.focus_epoch())?;
    fixture.request(json!({"op":"verify_focus","target":"b"}))?;
    let a = fixture.request(json!({"op":"focus","target":"a"}))?;
    if a["address"] != ready["addresses"]["a"] {
        return Err("wrong A native focus".into());
    }
    let returned = capture(&platform, pid, in_b.focus_epoch())?;
    fixture.request(json!({"op":"verify_focus","target":"a"}))?;
    if let Some(delay) = Duration::from_secs(2).checked_sub(started.elapsed()) {
        thread::sleep(delay);
    }
    if platform.can_deliver(&original) {
        return Err("old target remained valid after native focus return".into());
    }
    let delayed_result_ms = started.elapsed().as_millis();
    let after_switch = platform.deliver(&original, RESULT);
    if after_switch != DeliveryOutcome::Manual {
        return Err("focus-change delivery was not manual".into());
    }
    let before_close = capture(&platform, pid, in_b.focus_epoch())?;
    fixture.request(json!({"op":"verify_focus","target":"a"}))?;
    fixture.request(json!({"op":"close_a"}))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = fixture.request(json!({"op":"status"}))?;
        if status["a_present"] == false {
            break;
        }
        if Instant::now() >= deadline {
            return Err("closed target still native present".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    fixture.request(json!({"op":"focus","target":"b"}))?;
    let after_close_target = capture(&platform, pid, before_close.focus_epoch())?;
    if platform.can_deliver(&before_close) {
        return Err("closed target remained valid".into());
    }
    let after_close = platform.deliver(&before_close, RESULT);
    if after_close != DeliveryOutcome::Manual {
        return Err("closed-target delivery was not manual".into());
    }
    let status = fixture.request(json!({"op":"status"}))?;
    if status["changes"] != json!({"a":0,"b":0}) || status["bytes"] != json!({"a":0,"b":0}) {
        return Err("unexpected input reached an owned target".into());
    }
    let prepared = fixture.request(json!({"op":"prepare_positive"}))?;
    if prepared["event"] != "positive_prepared" {
        return Err("positive probe clipboard backup unavailable".into());
    }
    let positive_target = capture(&platform, pid, before_close.focus_epoch())?;
    fixture.request(json!({"op":"verify_focus","target":"b"}))?;
    let positive_started = Instant::now();
    let positive_outcome = platform.deliver(&positive_target, DELIVERY_PROBE_TEXT);
    if positive_outcome != DeliveryOutcome::SentUnconfirmed {
        return Err("valid-target paste was not dispatched".into());
    }
    let positive_deadline = Instant::now() + Duration::from_secs(3);
    let positive_status = loop {
        let result = fixture.request(json!({"op":"positive_status"}))?;
        if result["matches"] == true
            && result["changes"].as_u64().is_some_and(|count| count > 0)
            && result["bytes"].as_u64() == Some(DELIVERY_PROBE_TEXT.len() as u64)
        {
            break result;
        }
        if Instant::now() >= positive_deadline {
            return Err("native GTK paste readback did not match".into());
        }
        thread::sleep(Duration::from_millis(20));
    };
    let positive_readback_ms = positive_started.elapsed().as_millis();
    let cleanup = fixture.request(json!({"op":"cleanup"}))?;
    fixture.cleaned = true;
    if cleanup["clipboard_restored"] != true {
        return Err("clipboard MIME bytes were not restored".into());
    }
    let report = json!({"scope":"Real owned GTK focus/close guard negatives followed by one positive native paste/readback; no microphone/provider; not full V01/V02 M",
        "capability_premise":"Hyprland::new(true) is explicit component-test setup based on this host's M0 capability evidence, not product own-window verification",
        "requested_backend":backend,"native_xwayland":ready["native_xwayland"],
        "native_focus_sequence":"A -> B -> A; then close A and focus B", "focus_epochs":[original.focus_epoch(),in_b.focus_epoch(),returned.focus_epoch(),after_close_target.focus_epoch()],
        "synthetic_result_delay_floor_ms":2000,"observed_result_delay_ms":delayed_result_ms,"switch_and_return_outcome":after_switch,"closed_target_outcome":after_close,
        "negative_owned_buffers_unchanged":true,"synthetic_result_retained":RESULT=="固定测试结果 42\nSynthetic retained result: /tmp/example.txt",
        "positive_outcome":positive_outcome,"positive_actual_buffer_matches":positive_status["matches"],
        "positive_buffer_changes":positive_status["changes"],"positive_buffer_bytes":positive_status["bytes"],
        "positive_dispatch_to_readback_ms":positive_readback_ms,
        "clipboard_snapshot_mime_count":prepared["clipboard_mime_count"],"clipboard_restored":cleanup["clipboard_restored"],
        "clipboard_restore_owner_lifetime":cleanup["restore_owner_lifetime"],
        "original_focus_restored":cleanup["original_focus_restored"],"passed":true});
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    serde_json::to_writer_pretty(&mut file, &report)?;
    writeln!(file)?;
    file.sync_all()?;
    println!("{report}");
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("Linux-only explicit desktop component probe");
}
