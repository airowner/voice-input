//! Linux-only, explicitly owned real WorkerPool lifecycle evidence, never UI V07.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, atomic::AtomicBool, mpsc},
    thread,
    time::{Duration, Instant},
};
use voice_core::Config;
use voice_runtime::{rewrite, service::load_message, supervisor::WorkerPool};
type AnyError = Box<dyn std::error::Error>;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Identity {
    pid: u32,
    parent: u32,
    start_ticks: u64,
    exe: PathBuf,
}
fn stat(pid: u32) -> Result<(char, u32, u64, u64), AnyError> {
    let data = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields: Vec<_> = data
        .rsplit_once(')')
        .ok_or("invalid stat")?
        .1
        .split_whitespace()
        .collect();
    Ok((
        fields[0].chars().next().ok_or("state")?,
        fields[1].parse()?,
        fields[19].parse()?,
        fields[11].parse::<u64>()? + fields[12].parse::<u64>()?,
    ))
}
fn identity(pid: u32) -> Result<Identity, AnyError> {
    let (_, parent, start_ticks, _) = stat(pid)?;
    Ok(Identity {
        pid,
        parent,
        start_ticks,
        exe: fs::read_link(format!("/proc/{pid}/exe"))?,
    })
}
fn owned_worker(exe: &Path) -> Result<Identity, AnyError> {
    let mut ids = Vec::new();
    for task in fs::read_dir("/proc/self/task")? {
        let children = fs::read_to_string(task?.path().join("children"))?;
        for value in children.split_whitespace() {
            let id = identity(value.parse()?)?;
            if id.parent == std::process::id() && id.exe == exe {
                ids.push(id);
            }
        }
    }
    ids.sort_by_key(|id| id.pid);
    ids.dedup_by_key(|id| id.pid);
    if ids.len() != 1 {
        return Err(format!("expected one directly owned worker, got {}", ids.len()).into());
    }
    Ok(ids.remove(0))
}
fn same_live(id: &Identity) -> bool {
    identity(id.pid).is_ok_and(|now| now.start_ticks == id.start_ticks && now.exe == id.exe)
        && stat(id.pid).is_ok_and(|(state, _, _, _)| state != 'Z' && state != 'X')
}
fn kill_owned(id: &Identity, expected_parent: u32) -> Result<(), AnyError> {
    let now = identity(id.pid)?;
    if now.parent != expected_parent || now.start_ticks != id.start_ticks || now.exe != id.exe {
        return Err("refusing changed process identity".into());
    }
    // Only a PID whose executable, direct parent and Linux starttime were checked.
    if unsafe { libc::kill(id.pid as i32, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn hash(path: &Path) -> Result<String, AnyError> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
struct Inputs {
    root: PathBuf,
    config: Config,
    paths: [PathBuf; 2],
    executables: [PathBuf; 2],
    pcm: Vec<f32>,
    hashes: Value,
}
impl Inputs {
    fn load(root: PathBuf) -> Result<Self, AnyError> {
        let lock: Value = serde_json::from_slice(&fs::read(root.join("models.lock.json"))?)?;
        let mut paths = Vec::new();
        let mut hashes = json!({});
        for model in lock["models"].as_array().ok_or("models")? {
            let path = root
                .join(model["path"].as_str().ok_or("path")?)
                .canonicalize()?;
            assert_eq!(
                fs::metadata(&path)?.len(),
                model["bytes"].as_u64().ok_or("bytes")?
            );
            let digest = hash(&path)?;
            assert_eq!(Some(digest.as_str()), model["sha256"].as_str());
            hashes[model["id"].as_str().ok_or("id")?] = json!(digest);
            paths.push(path);
        }
        let executables = ["voice-asr-worker-avx2", "voice-llm-worker-avx2"]
            .map(|name| root.join("build/workers-avx2").join(name).canonicalize())
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        let mut wav = hound::WavReader::open(root.join("tests/fixtures/jfk.wav"))?;
        assert_eq!(
            (
                wav.spec().sample_rate,
                wav.spec().channels,
                wav.spec().bits_per_sample
            ),
            (16000, 1, 16)
        );
        let pcm = wav
            .samples::<i16>()
            .map(|v| v.map(|v| f32::from(v) / 32768.0))
            .collect::<Result<_, _>>()?;
        let mut config = Config::default();
        config.asr.threads = 4;
        config.llm.local.threads = 4;
        Ok(Self {
            root,
            config,
            paths: paths.try_into().map_err(|_| "two models required")?,
            executables: executables.try_into().map_err(|_| "two workers")?,
            pcm,
            hashes,
        })
    }
    fn pool(&self, index: usize) -> Arc<WorkerPool> {
        Arc::new(WorkerPool::new(
            self.executables[index].clone(),
            if index == 0 { "asr" } else { "llm" },
            if index == 0 { 120 } else { 60 },
        ))
    }
    fn load_message(&self, index: usize) -> Value {
        load_message(&self.paths[index], &self.config, index == 0)
    }
    fn run_message(&self, index: usize, job: &str) -> Result<Value, AnyError> {
        Ok(if index == 0 {
            json!({"op":"run","job_id":job,"language":"en"})
        } else {
            json!({"op":"run","job_id":job,"messages":rewrite::messages("Keep number 42.",&self.config)?,"max_tokens":128})
        })
    }
}
fn kill_and_rebuild(input: &Inputs, index: usize) -> Result<Value, AnyError> {
    let pool = input.pool(index);
    let signature = "lifecycle";
    pool.preload(
        signature,
        input.load_message(index),
        &AtomicBool::new(false),
    )?;
    let id = owned_worker(&input.executables[index])?;
    let initial_cpu = stat(id.pid)?.3;
    let p = Arc::clone(&pool);
    let load = input.load_message(index);
    let run = input.run_message(index, "owned-kill")?;
    let pcm = (index == 0).then(|| input.pcm.clone());
    let (tx, rx) = mpsc::channel();
    let thread = thread::spawn(move || {
        let result = p.run(
            signature,
            load,
            run,
            pcm.as_deref(),
            Duration::from_secs(120),
            &AtomicBool::new(false),
        );
        let _ = tx.send(result);
    });
    let waiting = Instant::now();
    let mut heartbeats = 0;
    while stat(id.pid)?.3 < initial_cpu + 2 {
        if waiting.elapsed() > Duration::from_secs(10) {
            return Err("no compute CPU progress observed".into());
        }
        thread::sleep(Duration::from_millis(25));
        heartbeats += 1;
    }
    let cpu_delta = stat(id.pid)?.3 - initial_cpu;
    let killed = Instant::now();
    kill_owned(&id, std::process::id())?;
    let error = rx
        .recv_timeout(Duration::from_secs(5))?
        .expect_err("killed computation must fail");
    thread.join().map_err(|_| "host run thread panicked")?;
    assert!(!same_live(&id));
    assert!(!pool.is_loaded(signature));
    let failure_return_ms = killed.elapsed().as_millis();
    let recovery_started = Instant::now();
    let response = pool.run(
        signature,
        input.load_message(index),
        input.run_message(index, "owned-rebuild")?,
        if index == 0 { Some(&input.pcm) } else { None },
        Duration::from_secs(120),
        &AtomicBool::new(false),
    )?;
    if index == 0 {
        assert!(response.to_lowercase().contains("fellow americans"));
    } else {
        rewrite::validate_output_with_config("Keep number 42.", &response, &input.config)?;
    }
    let rebuilt = owned_worker(&input.executables[index])?;
    assert!(rebuilt.pid != id.pid || rebuilt.start_ticks != id.start_ticks);
    let report = json!({"kind":if index==0{"asr"}else{"llm"},"killed_identity":id,"compute_cpu_tick_delta":cpu_delta,"host_thread_heartbeats":heartbeats,"failure_code":error.code,"failure_return_after_kill_ms":failure_return_ms,"rebuilt_identity":rebuilt,"next_run_success":true,"rebuild_and_run_ms":recovery_started.elapsed().as_millis(),"scope":"Host thread responsiveness only; not UI responsiveness"});
    pool.unload();
    Ok(report)
}
fn idle(input: &Inputs) -> Result<Value, AnyError> {
    let pools = [input.pool(0), input.pool(1)];
    let mut loaded = Vec::new();
    let mut ids = Vec::new();
    for (i, pool) in pools.iter().enumerate() {
        pool.preload("idle", input.load_message(i), &AtomicBool::new(false))?;
        loaded.push(Instant::now());
        ids.push(owned_worker(&input.executables[i])?);
    }
    let mut released = [None, None];
    while released.iter().any(Option::is_none) {
        for (i, pool) in pools.iter().enumerate() {
            if released[i].is_some() {
                continue;
            }
            pool.reap_idle();
            if !pool.is_loaded("idle") {
                let seconds = loaded[i].elapsed().as_secs_f64();
                let threshold = if i == 0 { 120.0 } else { 60.0 };
                assert!((threshold..threshold + 5.0).contains(&seconds));
                assert!(!same_live(&ids[i]));
                released[i] = Some(seconds);
                eprintln!("idle released index={i} after {seconds:.3}s");
            }
            if loaded[i].elapsed() > Duration::from_secs(125) {
                return Err("idle deadline exceeded".into());
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    Ok(
        json!({"configured_idle_seconds":{"asr":120,"llm":60},"observed_release_seconds":{"asr":released[0],"llm":released[1]},"poll_ms":250,"workers_exited":true,"identities":ids,"scope":"Actual production reap_idle with default durations; example supplies polling, not GUI service scheduler"}),
    )
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn parent_death(input: &Inputs) -> Result<Value, AnyError> {
    let mut child = ChildGuard(
        Command::new(std::env::current_exe()?)
            .arg("--holder")
            .arg(&input.root)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let holder = identity(child.0.id())?;
    assert_eq!(holder.parent, std::process::id());
    let stdout = child.0.stdout.take().ok_or("child stdout")?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout)
            .take(8192)
            .read_line(&mut line)
            .map(|_| line);
        let _ = tx.send(result);
    });
    let line = rx.recv_timeout(Duration::from_secs(60))??;
    let workers: Vec<Identity> = serde_json::from_str(&line)?;
    assert_eq!(workers.len(), 2);
    for (i, id) in workers.iter().enumerate() {
        let now = identity(id.pid)?;
        assert_eq!(now.parent, holder.pid);
        assert_eq!(now.start_ticks, id.start_ticks);
        assert_eq!(now.exe, input.executables[i]);
    }
    let started = Instant::now();
    kill_owned(&holder, std::process::id())?;
    child.0.wait()?;
    while workers.iter().any(same_live) {
        if started.elapsed() > Duration::from_secs(5) {
            return Err("owned workers survived parent EOF deadline".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(
        json!({"holder":holder,"workers":workers,"parent_signal":"SIGKILL","worker_exit_after_parent_kill_ms":started.elapsed().as_millis(),"deadline_seconds":5,"scope":"Workers resident at parent death; EOF exit, not a GUI crash or during-compute parent death"}),
    )
}
fn main() -> Result<(), AnyError> {
    let args: Vec<_> = std::env::args().collect();
    let holder = args.get(1).is_some_and(|v| v == "--holder");
    let root = if holder {
        PathBuf::from(args.get(2).ok_or("holder root")?)
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?
    };
    let input = Inputs::load(root)?;
    if holder {
        let pools = [input.pool(0), input.pool(1)];
        let mut identities = Vec::new();
        for (i, pool) in pools.iter().enumerate() {
            pool.preload("parent-eof", input.load_message(i), &AtomicBool::new(false))?;
            identities.push(owned_worker(&input.executables[i])?);
        }
        println!("{}", serde_json::to_string(&identities)?);
        loop {
            std::hint::black_box(&pools);
            thread::park();
        }
    }
    eprintln!("checking ASR kill and rebuild");
    let asr = kill_and_rebuild(&input, 0)?;
    eprintln!("checking LLM kill and rebuild");
    let llm = kill_and_rebuild(&input, 1)?;
    eprintln!("checking owned holder SIGKILL and worker EOF");
    let death = parent_death(&input)?;
    eprintln!("checking default idle durations (up to 125 seconds)");
    let idle = idle(&input)?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"passed":true,"scope":"Real Linux production WorkerPool development integration; no microphone, desktop injection, remote provider or UI V07 acceptance","model_sha256":input.hashes,"worker_sha256":{"asr":hash(&input.executables[0])?,"llm":hash(&input.executables[1])?},"threads":4,"compute_kill_and_rebuild":[asr,llm],"parent_sigkill":death,"idle":idle,"ui_v07_acceptance":"pending"})
        )?
    );
    Ok(())
}
