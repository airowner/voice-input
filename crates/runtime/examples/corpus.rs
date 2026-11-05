//! Explicit developer corpus run. Public fixture text is written only to the
//! requested report; this is never the application's default logging path.

#[cfg(target_os = "linux")]
mod linux {
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeMap,
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        path::{Path, PathBuf},
        sync::atomic::AtomicBool,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use voice_core::{AppError, Config, DeliveryMode, DeliveryStatus, Language, RecoveryStore};
    use voice_runtime::{
        rewrite,
        service::{load_message, worker_path},
        supervisor::WorkerPool,
    };

    #[derive(Deserialize)]
    struct Corpus {
        schema_version: u32,
        dataset_revision: String,
        dataset_source: String,
        license: String,
        known_gaps: Vec<String>,
        samples: Vec<Sample>,
    }
    #[derive(Deserialize)]
    struct Sample {
        id: String,
        path: String,
        kind: String,
        languages: Vec<String>,
        reference_text: String,
        sha256: String,
        bytes: u64,
    }
    #[derive(Deserialize)]
    struct ModelLock {
        schema_version: u32,
        models: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        id: String,
        path: String,
        sha256: String,
        bytes: u64,
    }
    struct CheckedModel {
        id: String,
        path: PathBuf,
        sha256: String,
    }
    #[derive(Default, Serialize)]
    struct Times {
        fixture_check_ms: u128,
        voice_gate_ms: u128,
        asr_ms: Option<u128>,
        raw_commit_ms: Option<u128>,
        prompt_ms: Option<u128>,
        llm_ms: Option<u128>,
        validation_ms: Option<u128>,
        processed_commit_ms: Option<u128>,
        total_ms: u128,
    }
    #[derive(Serialize)]
    struct Entry {
        id: String,
        kind: String,
        languages: Vec<String>,
        fixture_sha256: String,
        reference_text: String,
        duration_seconds: Option<f64>,
        contains_voice: Option<bool>,
        status: String,
        asr_requested: bool,
        local_llm_requested: bool,
        remote_requests: u32,
        raw: Option<String>,
        processed: Option<String>,
        local_llm_response: Option<String>,
        raw_committed: bool,
        processed_committed: bool,
        raw_saved_before_llm: Option<bool>,
        json_and_protected_literal_validation: String,
        asr_cache_state: Option<String>,
        llm_cache_state: Option<String>,
        error_at: Option<String>,
        error: Option<AppError>,
        manual_review: &'static str,
        timings: Times,
    }
    impl Entry {
        fn new(sample: &Sample) -> Self {
            Self {
                id: sample.id.clone(),
                kind: sample.kind.clone(),
                languages: sample.languages.clone(),
                fixture_sha256: sample.sha256.clone(),
                reference_text: sample.reference_text.clone(),
                duration_seconds: None,
                contains_voice: None,
                status: "pending".into(),
                asr_requested: false,
                local_llm_requested: false,
                remote_requests: 0,
                raw: None,
                processed: None,
                local_llm_response: None,
                raw_committed: false,
                processed_committed: false,
                raw_saved_before_llm: None,
                json_and_protected_literal_validation: "not_run".into(),
                asr_cache_state: None,
                llm_cache_state: None,
                error_at: None,
                error: None,
                manual_review: "pending",
                timings: Times::default(),
            }
        }
    }

    struct Options {
        output: PathBuf,
        workers: Option<PathBuf>,
        limit: Option<usize>,
        dry_run: bool,
    }
    struct Pipeline<'a> {
        asr: &'a WorkerPool,
        llm: &'a WorkerPool,
        store: &'a mut RecoveryStore,
        config: &'a Config,
        base: &'a CheckedModel,
        qwen: &'a CheckedModel,
        asr_signature: String,
        llm_signature: String,
        asr_preload_error: Option<AppError>,
        llm_preload_error: Option<AppError>,
        run_id: String,
        cancel: AtomicBool,
    }

    fn failure(code: &str, message: &str) -> AppError {
        AppError::new(code, "corpus", false, message)
    }
    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64
    }
    fn hash_file(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
        let mut file = File::open(path)?;
        let mut sha = Sha256::new();
        let mut bytes = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut bytes)?;
            if count == 0 {
                break;
            }
            sha.update(&bytes[..count]);
        }
        Ok(format!("{:x}", sha.finalize()))
    }
    fn options() -> Result<Options, Box<dyn std::error::Error>> {
        let mut arguments = std::env::args_os().skip(1);
        let mut output = None;
        let mut workers = None;
        let mut limit = None;
        let mut dry_run = false;
        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("--output") => {
                    output = Some(PathBuf::from(
                        arguments.next().ok_or("--output requires a path")?,
                    ))
                }
                Some("--workers") => {
                    workers = Some(PathBuf::from(
                        arguments.next().ok_or("--workers requires a path")?,
                    ))
                }
                Some("--limit") => {
                    let value = arguments.next().ok_or("--limit requires a count")?;
                    limit = Some(value.to_str().ok_or("invalid count")?.parse::<usize>()?);
                }
                Some("--dry-run") => dry_run = true,
                Some("--help") => {
                    println!(
                        "corpus --output NEW.json [--workers DIRECTORY] [--limit 1..15] [--dry-run]"
                    );
                    std::process::exit(0);
                }
                _ => return Err("unknown argument; use --help".into()),
            }
        }
        if limit.is_some_and(|n| n == 0 || n > 15) {
            return Err("--limit must be 1..15".into());
        }
        Ok(Options { output: output.ok_or("explicit --output NEW.json is required; public fixture text is developer evidence, not application logging")?, workers, limit, dry_run })
    }
    fn checked_model(
        root: &Path,
        lock: &ModelLock,
        id: &str,
    ) -> Result<CheckedModel, Box<dyn std::error::Error>> {
        let model = lock
            .models
            .iter()
            .find(|model| model.id == id)
            .ok_or("model missing from lock")?;
        let path = root.join(&model.path).canonicalize()?;
        if !path.starts_with(root)
            || fs::metadata(&path)?.len() != model.bytes
            || hash_file(&path)? != model.sha256
        {
            return Err("pinned model file identity mismatch".into());
        }
        Ok(CheckedModel {
            id: model.id.clone(),
            path,
            sha256: model.sha256.clone(),
        })
    }
    fn fixture_pcm(directory: &Path, sample: &Sample) -> Result<Vec<f32>, AppError> {
        let path = directory
            .join(&sample.path)
            .canonicalize()
            .map_err(|_| failure("fixture_missing", "公开语音文件不可读取"))?;
        if !path.starts_with(directory)
            || sample.bytes > 2 * 1024 * 1024
            || fs::metadata(&path)
                .map_err(|_| failure("fixture_metadata", "无法读取语音文件信息"))?
                .len()
                != sample.bytes
            || hash_file(&path).map_err(|_| failure("fixture_hash", "语音文件校验失败"))?
                != sample.sha256
        {
            return Err(failure(
                "fixture_identity",
                "语音文件大小或 SHA-256 与清单不符",
            ));
        }
        let mut reader = hound::WavReader::open(path)
            .map_err(|_| failure("fixture_wav", "公开语音不是可读取的 WAV"))?;
        let specification = reader.spec();
        if specification.sample_rate != 16000
            || specification.channels != 1
            || specification.bits_per_sample != 16
            || specification.sample_format != hound::SampleFormat::Int
            || reader.duration() > 960000
        {
            return Err(failure(
                "fixture_format",
                "语音必须是最长 60 秒的 16 kHz 单声道 16 位 PCM",
            ));
        }
        reader
            .samples::<i16>()
            .map(|sample| {
                sample
                    .map(|value| f32::from(value) / 32768.0)
                    .map_err(|_| failure("fixture_decode", "语音样本解码失败"))
            })
            .collect()
    }

    fn process(sample: &Sample, directory: &Path, pipeline: Option<&mut Pipeline<'_>>) -> Entry {
        let started = Instant::now();
        let mut entry = Entry::new(sample);
        let outcome = process_inner(sample, directory, pipeline, &mut entry);
        if let Err((stage, error)) = outcome {
            entry.status = format!("{stage}_failed");
            entry.error_at = Some(stage.into());
            entry.error = Some(error);
        }
        entry.timings.total_ms = started.elapsed().as_millis();
        entry
    }
    type StageResult = Result<(), (&'static str, AppError)>;
    fn process_inner(
        sample: &Sample,
        directory: &Path,
        pipeline: Option<&mut Pipeline<'_>>,
        entry: &mut Entry,
    ) -> StageResult {
        let checking = Instant::now();
        let decoded = fixture_pcm(directory, sample);
        entry.timings.fixture_check_ms = checking.elapsed().as_millis();
        let pcm = decoded.map_err(|error| ("fixture", error))?;
        entry.duration_seconds = Some(pcm.len() as f64 / 16000.0);
        let gating = Instant::now();
        let voice = voice_audio::contains_voice(&pcm);
        entry.timings.voice_gate_ms = gating.elapsed().as_millis();
        entry.contains_voice = Some(voice);
        if !voice {
            entry.status = "skipped_no_speech".into();
            return Ok(());
        }
        if sample.kind == "generated_digital_silence" {
            return Err((
                "voice_gate",
                failure("silence_gate_failed", "数字静音未被语音门控拒绝"),
            ));
        }
        let Some(pipeline) = pipeline else {
            entry.status = "dry_run_fixture_verified".into();
            return Ok(());
        };
        if let Some(error) = &pipeline.asr_preload_error {
            return Err(("asr_preload", error.clone()));
        }
        let job_id = format!("corpus-{}-{}", pipeline.run_id, sample.id);
        let mut config = pipeline.config.clone();
        config.asr.language = match sample.languages.as_slice() {
            [language] if language == "zh" => Language::Zh,
            [language] if language == "en" => Language::En,
            _ => Language::Auto,
        };
        entry.asr_cache_state = Some(
            if pipeline.asr.is_loaded(&pipeline.asr_signature) {
                "resident expected"
            } else {
                "cold/recreated"
            }
            .into(),
        );
        let inference = Instant::now();
        entry.asr_requested = true;
        let raw = pipeline.asr.run(
            &pipeline.asr_signature,
            load_message(&pipeline.base.path, &config, true),
            json!({"op":"run","job_id":job_id,"language":config.asr.language}),
            Some(&pcm),
            Duration::from_secs(config.asr.timeout_seconds.into()),
            &pipeline.cancel,
        );
        entry.timings.asr_ms = Some(inference.elapsed().as_millis());
        let raw = match raw {
            Ok(raw) => raw,
            Err(error) => return Err(("asr", error)),
        };
        entry.raw = Some(raw.clone());
        if raw.trim().is_empty() {
            return Err(("asr", failure("empty_transcript", "本地转写未产生文字")));
        }
        let committing = Instant::now();
        let saved = pipeline.store.commit_raw(&job_id, &raw, &config, now());
        entry.timings.raw_commit_ms = Some(committing.elapsed().as_millis());
        saved.map_err(|error| ("raw_commit", error))?;
        entry.raw_committed = true;
        if let Some(error) = &pipeline.llm_preload_error {
            return Err(("llm_preload", error.clone()));
        }
        let building = Instant::now();
        let prompt = rewrite::messages(&raw, &config);
        entry.timings.prompt_ms = Some(building.elapsed().as_millis());
        let prompt = prompt.map_err(|error| ("prompt", error))?;
        entry.raw_saved_before_llm = Some(entry.raw_committed);
        entry.llm_cache_state = Some(
            if pipeline.llm.is_loaded(&pipeline.llm_signature) {
                "resident expected"
            } else {
                "cold/recreated"
            }
            .into(),
        );
        let generation = Instant::now();
        entry.local_llm_requested = true;
        let response = pipeline.llm.run(
            &pipeline.llm_signature,
            load_message(&pipeline.qwen.path, &config, false),
            json!({"op":"run","job_id":job_id,"messages":prompt,"max_tokens":768}),
            None,
            Duration::from_secs(config.llm.local.timeout_seconds.into()),
            &pipeline.cancel,
        );
        entry.timings.llm_ms = Some(generation.elapsed().as_millis());
        let response = match response {
            Ok(response) => response,
            Err(error) => return Err(("llm", error)),
        };
        entry.local_llm_response = Some(response.clone());
        let validation = Instant::now();
        let processed = rewrite::validate_output_with_config(&raw, &response, &config);
        entry.timings.validation_ms = Some(validation.elapsed().as_millis());
        entry.json_and_protected_literal_validation = if processed.is_ok() {
            "passed"
        } else {
            "rejected"
        }
        .into();
        let processed = processed.map_err(|error| ("validation", error))?;
        entry.processed = Some(processed.clone());
        let committing = Instant::now();
        let saved = pipeline
            .store
            .commit_processed(&job_id, &processed, &config, now());
        entry.timings.processed_commit_ms = Some(committing.elapsed().as_millis());
        saved.map_err(|error| ("processed_commit", error))?;
        entry.processed_committed = true;
        pipeline
            .store
            .mark_delivery(&job_id, DeliveryStatus::Preview, now())
            .map_err(|error| ("preview_record", error))?;
        entry.status = "validated_and_saved_preview".into();
        Ok(())
    }

    fn write_report(path: &Path, report: &Value) -> Result<(), Box<dyn std::error::Error>> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut file, report)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }
    fn summary(entries: &[Entry], dry_run: bool) -> Value {
        let mut errors = BTreeMap::new();
        for entry in entries {
            if let Some(error) = &entry.error {
                *errors.entry(error.code.clone()).or_insert(0_usize) += 1;
            }
        }
        let llm_requests = entries
            .iter()
            .filter(|entry| entry.local_llm_requested)
            .count();
        let raw_ordering = (llm_requests > 0).then(|| {
            entries
                .iter()
                .all(|entry| !entry.local_llm_requested || entry.raw_saved_before_llm == Some(true))
        });
        json!({
            "samples_recorded": entries.len(),
            "asr_requests": entries.iter().filter(|entry| entry.asr_requested).count(),
            "local_llm_requests": llm_requests,
            "remote_requests": 0,
            "raw_committed": entries.iter().filter(|entry| entry.raw_committed).count(),
            "raw_present_in_developer_report": entries.iter().filter(|entry| entry.raw.is_some()).count(),
            "processed_committed": entries.iter().filter(|entry| entry.processed_committed).count(),
            "json_and_literal_validation_passed": entries.iter().filter(|entry| entry.json_and_protected_literal_validation == "passed").count(),
            "silence_skips": entries.iter().filter(|entry| entry.kind == "generated_digital_silence" && entry.status == "skipped_no_speech" && !entry.asr_requested && !entry.local_llm_requested).count(),
            "every_llm_request_followed_raw_commit": raw_ordering,
            "errors_by_code": errors,
            "manual_review": "pending",
            "semantic_fidelity": "not_automatically_scored",
            "dry_run": dry_run,
            "remote_mode_run": false,
            "v10_acceptance": "pending"
        })
    }
    fn run_rows(
        corpus: &Corpus,
        directory: &Path,
        output: &Path,
        report: &mut Value,
        limit: usize,
        mut pipeline: Option<&mut Pipeline<'_>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut entries = Vec::new();
        for sample in corpus.samples.iter().take(limit) {
            let entry = process(sample, directory, pipeline.as_deref_mut());
            println!(
                "{}",
                json!({"sample":entry.id,"status":entry.status,"asr_ms":entry.timings.asr_ms,"llm_ms":entry.timings.llm_ms,"error_code":entry.error.as_ref().map(|e| &e.code)})
            );
            entries.push(entry);
            report["entries"] = serde_json::to_value(&entries)?;
            report["summary"] = summary(&entries, pipeline.is_none());
            write_report(output, report)?;
        }
        report["run_status"] = json!("finished_with_per_sample_results");
        if let Some(pipeline) = pipeline {
            report["recent_recovery_records_at_finish"] = match pipeline.store.list(now()) {
                Ok(records) => json!(records.len()),
                Err(error) => json!({"error":error}),
            };
        }
        write_report(output, report)?;
        Ok(())
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let options = options()?;
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        let directory = root.join("tests/fixtures/v10").canonicalize()?;
        let manifest_path = directory.join("manifest.json");
        let corpus: Corpus = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        let lock: ModelLock = serde_json::from_slice(&fs::read(root.join("models.lock.json"))?)?;
        if corpus.schema_version != 1 || lock.schema_version != 1 || corpus.samples.len() != 15 {
            return Err("unsupported corpus/model manifest".into());
        }
        let base = checked_model(&root, &lock, "whisper-base-multilingual")?;
        let qwen = checked_model(&root, &lock, "qwen2.5-1.5b-instruct-q4_k_m")?;
        let optimized = root.join("build/workers-avx2");
        let default_workers = if worker_path(&optimized, "voice-asr-worker").is_file()
            && worker_path(&optimized, "voice-llm-worker").is_file()
        {
            optimized
        } else {
            root.join("build/workers")
        };
        let workers = options.workers.unwrap_or(default_workers).canonicalize()?;
        let asr_path = worker_path(&workers, "voice-asr-worker").canonicalize()?;
        let llm_path = worker_path(&workers, "voice-llm-worker").canonicalize()?;
        let mut config = Config::default();
        config.asr.model_id = base.id.clone();
        config.llm.local.model_id = qwen.id.clone();
        config.asr.threads = 4;
        config.llm.local.threads = 4;
        config.delivery.mode = DeliveryMode::Preview;
        config.validate_ready()?;
        let limit = options.limit.unwrap_or(corpus.samples.len());
        let parent = options
            .output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        drop(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&options.output)?,
        );
        let mut report = json!({"scope":"Explicit public-fixture developer evidence; never default application logging; no microphone, paste, remote provider, keyring or download","run_status":"in_progress","started_at_unix_ms":now(),"dry_run":options.dry_run,"system":std::env::consts::OS,"architecture":std::env::consts::ARCH,"available_logical_cpus":std::thread::available_parallelism().map_or(1,usize::from),"backend":"cpu","threads":4,"models":{"asr_sha256":base.sha256,"llm_sha256":qwen.sha256,"verified_against":"models.lock.json"},"workers":{"asr":asr_path,"llm":llm_path,"asr_sha256":hash_file(&asr_path)?,"llm_sha256":hash_file(&llm_path)?},"corpus_manifest_sha256":hash_file(&manifest_path)?,"dataset_revision":corpus.dataset_revision,"dataset_source":corpus.dataset_source,"license":corpus.license,"known_gaps":corpus.known_gaps,"corpus_samples":corpus.samples.len(),"selected_samples":limit,"partial_run":limit<corpus.samples.len(),"prompt_version":rewrite::PROMPT_VERSION,"llm_modes_run":if options.dry_run{Vec::<&str>::new()}else{vec!["local"]},"remote_mode_run":false,"network_audit":"not_performed; runner has no remote-provider request path","recovery":"private temporary SQLite; product retention 5 entries/24h; removed on process exit; this explicit public-text report retains per-sample evidence","entries":[],"summary":{"manual_review":"pending","v10_acceptance":"pending"}});
        report["schema_version"] = json!(1);
        write_report(&options.output, &report)?;
        if options.dry_run {
            return run_rows(
                &corpus,
                &directory,
                &options.output,
                &mut report,
                limit,
                None,
            );
        }
        let asr = WorkerPool::new(asr_path, "asr", 120);
        let llm = WorkerPool::new(llm_path, "llm", 60);
        let asr_signature = format!(
            "{}:{}",
            base.sha256,
            load_message(&base.path, &config, true)
        );
        let llm_signature = format!(
            "{}:{}",
            qwen.sha256,
            load_message(&qwen.path, &config, false)
        );
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();
        let asr_preload = asr.preload(
            &asr_signature,
            load_message(&base.path, &config, true),
            &cancelled,
        );
        let asr_ms = started.elapsed().as_millis();
        let started = Instant::now();
        let llm_preload = llm.preload(
            &llm_signature,
            load_message(&qwen.path, &config, false),
            &cancelled,
        );
        let llm_ms = started.elapsed().as_millis();
        report["preload"] = json!({"asr":{"elapsed_ms":asr_ms,"error":asr_preload.as_ref().err()},"llm":{"elapsed_ms":llm_ms,"error":llm_preload.as_ref().err()},"note":"load only; no warmup generation or automatic retry; per-sample worker reload is labeled after errors"});
        write_report(&options.output, &report)?;
        let temporary = tempfile::tempdir()?;
        let mut store = RecoveryStore::open(temporary.path().join("recovery.sqlite3"), true)?;
        let mut pipeline = Pipeline {
            asr: &asr,
            llm: &llm,
            store: &mut store,
            config: &config,
            base: &base,
            qwen: &qwen,
            asr_signature,
            llm_signature,
            asr_preload_error: asr_preload.err(),
            llm_preload_error: llm_preload.err(),
            run_id: uuid::Uuid::new_v4().to_string(),
            cancel: cancelled,
        };
        run_rows(
            &corpus,
            &directory,
            &options.output,
            &mut report,
            limit,
            Some(&mut pipeline),
        )?;
        asr.unload();
        llm.unload();
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("corpus runner currently requires the Linux production runtime");
    std::process::exit(2);
}
