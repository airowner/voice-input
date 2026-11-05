//! Explicit developer smoke using an authorized public WAV, never microphone input.
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use voice_core::{Config, DeliveryStatus, RecoveryStore};
use voice_runtime::{
    models::ModelManager,
    rewrite,
    service::{load_message, worker_path},
    supervisor::WorkerPool,
};

fn timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let directory = tempfile::tempdir()?;
    let worker_dir = root.join("build/workers-avx2");
    let asr = WorkerPool::new(worker_path(&worker_dir, "voice-asr-worker"), "asr", 120);
    let llm = WorkerPool::new(worker_path(&worker_dir, "voice-llm-worker"), "llm", 60);
    let mut manager = ModelManager::open(directory.path().join("models"))?;
    let mut config = Config::default();
    let base = manager.import(
        &root.join("models/whisper/ggml-base.bin"),
        "whisper",
        |path| asr.verify(load_message(path, &config, true)),
    )?;
    let qwen = manager.import(
        &root.join("models/gguf/qwen2.5-1.5b-instruct-q4_k_m.gguf"),
        "gguf",
        |path| llm.verify(load_message(path, &config, false)),
    )?;
    config.asr.model_id = base.id.clone();
    config.llm.local.model_id = qwen.id.clone();
    config.validate_ready()?;
    let base = manager.resolve(&base.id, "whisper")?;
    let qwen = manager.resolve(&qwen.id, "gguf")?;
    let mut wav = hound::WavReader::open(root.join("tests/fixtures/jfk.wav"))?;
    assert_eq!(wav.spec().sample_rate, 16000);
    assert_eq!(wav.spec().channels, 1);
    assert_eq!(wav.spec().bits_per_sample, 16);
    let pcm: Vec<f32> = wav
        .samples::<i16>()
        .map(|sample| sample.map(|sample| sample as f32 / 32768.0))
        .collect::<Result<_, _>>()?;
    let path = directory.path().join("recovery.sqlite3");
    let mut store = RecoveryStore::open(&path, true)?;
    let cancel = AtomicBool::new(false);
    let mut runs = Vec::new();
    for index in 0..2 {
        let id = format!("public-fixture-{index}");
        let started = Instant::now();
        let raw = asr.run(
            "public-base",
            load_message(&base.path, &config, true),
            json!({"op":"run","job_id":id,"language":"en"}),
            Some(&pcm),
            Duration::from_secs(120),
            &cancel,
        )?;
        let asr_ms = started.elapsed().as_millis();
        assert!(raw.to_lowercase().contains("fellow americans"));
        store.commit_raw(&id, &raw, &config, timestamp())?;
        let generation = Instant::now();
        let response=llm.run("public-qwen",load_message(&qwen.path,&config,false),json!({"op":"run","job_id":id,"messages":rewrite::messages(&raw,&config)?,"max_tokens":768}),None,Duration::from_secs(60),&cancel)?;
        let processed = rewrite::validate_output_with_config(&raw, &response, &config)?;
        assert!(processed.to_lowercase().contains("country"));
        assert!(processed.to_lowercase().contains("not"));
        store.commit_processed(&id, &processed, &config, timestamp())?;
        store.mark_delivery(&id, DeliveryStatus::Attempting, timestamp())?;
        runs.push(json!({"cache_state":if index==0{"cold worker; OS cache may be warm"}else{"warm resident worker"},"asr_ms":asr_ms,"llm_ms":generation.elapsed().as_millis(),"total_ms":started.elapsed().as_millis(),"raw_sha256":digest(&raw),"processed_sha256":digest(&processed),"public_fixture_raw":raw,"public_fixture_processed":processed}));
    }
    drop(store);
    let mut restored = RecoveryStore::open(&path, true)?;
    let recovered = restored.list(timestamp())?;
    assert_eq!(recovered.len(), 2);
    assert!(
        recovered
            .iter()
            .all(|r| r.delivery == DeliveryStatus::Unknown && r.preview_required)
    );
    asr.unload();
    llm.unload();
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"scope":"public JFK fixture developer pipeline, no microphone/no paste/no network; not V01/V10/V11 acceptance","model_sha256":{"asr":base.info.sha256,"llm":qwen.info.sha256},"runs":runs,"recovery_unknown_preview_records":recovered.len(),"passed":true})
        )?
    );
    Ok(())
}
