//! Owned browser fixture driver, not a product command or microphone test.
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use voice_platform::{DELIVERY_PROBE_TEXT, Hyprland};

fn identity(pid: u32) -> io::Result<(u32, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("stat"))?
        .1
        .split_whitespace()
        .collect();
    Ok((
        fields[1].parse().map_err(io::Error::other)?,
        fields[19].into(),
    ))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("browser_paste PID START_TICKS EXPECTED_EXE".into());
    }
    let pid: u32 = args[1].parse()?;
    let expected_exe = fs::canonicalize(PathBuf::from(&args[3]))?;
    let parent = unsafe { libc::getppid() } as u32;
    let valid = || {
        if identity(pid)
            .map(|(_, start)| start != args[2])
            .unwrap_or(true)
            || fs::read_link(format!("/proc/{pid}/exe")).ok().as_ref() != Some(&expected_exe)
        {
            return false;
        }
        let mut ancestor = pid;
        for _ in 0..32 {
            if ancestor == parent {
                return true;
            }
            ancestor = match identity(ancestor) {
                Ok((next, _)) if next > 1 && next != ancestor => next,
                _ => return false,
            };
        }
        false
    };
    if !valid() {
        return Err("browser is not a current owned fixture descendant".into());
    }
    let mut desktop = Hyprland::new(true, vec![]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !desktop.capabilities().focus_events_connected {
        if Instant::now() >= deadline {
            return Err("focus stream unavailable".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(300));
    let target = desktop.capture_target()?;
    if target.pid() != pid || !valid() {
        return Err("owned browser is not focused".into());
    }
    let started = Instant::now();
    let result = desktop.deliver(&target, DELIVERY_PROBE_TEXT);
    println!(
        "{}",
        serde_json::json!({"outcome":result,"delivery_call_ms":started.elapsed().as_secs_f64()*1000.0,"scope":"fixed synthetic fixture through production Hyprland adapter; capability precondition from separately verified native probes; timing excludes controller persistence, recording and model inference"})
    );
    io::stdout().flush()?;
    // Keep the selection owner alive until the actual browser consumes it.
    let mut input = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut input, 1, 3000) };
    Ok(())
}
