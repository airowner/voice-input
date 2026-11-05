//! Serial controller actor; capture, model loading and inference never run on UI.
use crate::{
    credentials::CredentialStore,
    models::{HardwareFacts, ModelInfo, ModelManager, ModelRecommendation, ModelRecord},
    rewrite::{self, RemoteProvider},
    supervisor::WorkerPool,
};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use voice_core::*;
use voice_platform::{DeliveryOutcome, Hyprland, TargetSnapshot, ipc::*};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn error(code: &str, message: &str) -> AppError {
    AppError::new(code, "runtime", true, message)
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiCommand {
    Start,
    Stop,
    Cancel,
    Retry {
        job_id: String,
        #[serde(default)]
        use_current_config: bool,
    },
    ClearHistory,
    DeleteResult {
        job_id: String,
    },
    CopyResult {
        job_id: String,
        processed: bool,
    },
    SaveConfig {
        config: Box<Config>,
        remote_confirmed: bool,
        key: Option<String>,
        session_only: bool,
    },
    ImportModel {
        engine: String,
    },
    DownloadModel {
        model_id: String,
    },
    DeleteModel {
        model_id: String,
    },
    DeleteCredential,
    TestRemote,
    TestDelivery,
    CompleteDeliveryTest {
        token: String,
        observed_text: String,
    },
    CancelDeliveryTest,
}
#[derive(Clone, Serialize)]
pub struct Snapshot {
    pub state: UiState,
    pub config: Config,
    pub recent: Vec<Record>,
    pub models: Vec<ModelInfo>,
    pub hardware: HardwareFacts,
    pub model_recommendations: Vec<ModelRecommendation>,
    pub devices: Vec<voice_audio::DeviceInfo>,
    pub ready: bool,
    pub microphone_ready: bool,
    pub asr_ready: bool,
    pub llm_ready: bool,
    pub remote_confirmed: bool,
    pub credential_present: bool,
    pub detail: String,
    pub operation_busy: bool,
    pub capabilities: voice_platform::CapabilitySet,
    pub warnings: Vec<AppError>,
}
enum Message {
    Command(UiCommand, Sender<Result<Value, AppError>>),
    Callback(Action),
    AutoStop(WorkToken),
    AudioReady(WorkToken, Vec<f32>),
    ModelFinished(Result<(ModelInfo, Vec<ModelInfo>), AppError>),
    RemoteTestFinished(Result<String, AppError>),
    Shutdown,
}
struct CaptureHandle {
    control: Sender<bool>,
    token: WorkToken,
}
pub struct Runtime {
    sender: Sender<Message>,
    cached: Arc<RwLock<Option<Snapshot>>>,
    join: Mutex<Option<JoinHandle<()>>>,
}
impl Runtime {
    pub fn start(
        data: PathBuf,
        workers: PathBuf,
        socket: SessionSocket,
        notify: impl Fn(&UiState) + Send + 'static,
    ) -> Result<Self, AppError> {
        std::fs::create_dir_all(&data)
            .map_err(|_| error("data_directory", "无法创建应用数据目录"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| error("data_permissions", "无法保护应用数据目录"))?;
        }
        let config_path = data.join("config.toml");
        let (config, config_error) = if config_path.exists() {
            match Config::load(&config_path) {
                Ok(c) => (c, None),
                Err(e) => (Config::default(), Some(e)),
            }
        } else {
            (Config::default(), None)
        };
        let (mut store, recovery_error) =
            match RecoveryStore::open(data.join("recovery.sqlite3"), config.recovery.persist) {
                Ok(store) => (store, None),
                Err(error) => (
                    RecoveryStore::in_memory(data.join("recovery.sqlite3")),
                    Some(error),
                ),
            };
        store.set_limits(config.recovery.max_entries, config.recovery.ttl_hours)?;
        let history = store.list(now())?;
        let (models, model_error) = ModelManager::open_for_runtime(data.join("models"))?;
        let hardware = crate::models::probe_hardware(&data.join("models"));
        let models = Arc::new(Mutex::new(models));
        let inventory = models
            .lock()
            .map_err(|_| error("model_lock", "模型目录暂不可用"))?
            .inventory();
        let (credentials, credential_error) =
            match CredentialStore::new(data.join("confirmed-endpoints.json")) {
                Ok(store) => (store, None),
                Err(error) => (
                    CredentialStore::in_memory(data.join("confirmed-endpoints.json")),
                    Some(error),
                ),
            };
        if config.llm.mode == LlmMode::Remote
            && !config.llm.remote.profile_id.is_empty()
            && !config.llm.remote.base_url.is_empty()
        {
            let _ = credentials
                .refresh_presence(&config.llm.remote.profile_id, &config.llm.remote.base_url);
        }
        let provider = Arc::new(RemoteProvider::new()?);
        let asr = Arc::new(WorkerPool::new(
            worker_path(&workers, "voice-asr-worker"),
            "asr",
            120,
        ));
        let llm = Arc::new(WorkerPool::new(
            worker_path(&workers, "voice-llm-worker"),
            "llm",
            60,
        ));
        let (sender, receiver) = unbounded();
        let cached = Arc::new(RwLock::new(None));
        let output = cached.clone();
        let callback = sender.clone();
        let initial_detail = if config.recovery.persist && !store.is_persistent() {
            "恢复存储不可用，原文仅保留在内存并强制预览".into()
        } else {
            String::new()
        };
        let handle = thread::spawn(move || {
            let mut controller = Controller::new(config.clone());
            let _ = controller.dispatch(Action::Restore { records: history });
            let mut actor = Actor {
                controller,
                store,
                models,
                inventory,
                hardware,
                credentials,
                provider,
                asr,
                llm,
                platform: Hyprland::new(false, config.delivery.sensitive_apps.clone()),
                targets: HashMap::new(),
                capture: None,
                work_cancel: Arc::new(AtomicBool::new(false)),
                operation_cancel: Arc::new(AtomicBool::new(false)),
                operation_busy: false,
                pending_delivery_probe: None,
                detail: initial_detail,
                config_path,
                config_error,
                recovery_error,
                credential_error,
                model_error,
                sender: callback,
                cached: output,
                devices: voice_audio::devices().unwrap_or_default(),
                notify: Box::new(notify),
                last_phase: None,
            };
            actor.publish();
            actor.run(receiver, socket);
        });
        Ok(Self {
            sender,
            cached,
            join: Mutex::new(Some(handle)),
        })
    }
    pub fn snapshot(&self) -> Result<Snapshot, AppError> {
        self.cached
            .read()
            .map_err(|_| error("snapshot", "状态暂不可用"))?
            .clone()
            .ok_or_else(|| error("starting", "应用正在初始化"))
    }
    pub fn execute(&self, command: UiCommand) -> Result<Value, AppError> {
        let (tx, rx) = bounded(1);
        self.sender
            .send(Message::Command(command, tx))
            .map_err(|_| error("closed", "应用已退出"))?;
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| error("busy", "操作尚在处理中，请查看当前状态"))?
    }
    pub fn shutdown(&self) {
        let _ = self.sender.send(Message::Shutdown);
        if let Ok(mut join) = self.join.lock()
            && let Some(handle) = join.take()
        {
            let _ = handle.join();
        }
    }
}
struct Actor {
    controller: Controller,
    store: RecoveryStore,
    models: Arc<Mutex<ModelManager>>,
    inventory: Vec<ModelInfo>,
    hardware: HardwareFacts,
    credentials: CredentialStore,
    provider: Arc<RemoteProvider>,
    asr: Arc<WorkerPool>,
    llm: Arc<WorkerPool>,
    platform: Hyprland,
    targets: HashMap<String, TargetSnapshot>,
    capture: Option<CaptureHandle>,
    work_cancel: Arc<AtomicBool>,
    operation_cancel: Arc<AtomicBool>,
    operation_busy: bool,
    pending_delivery_probe: Option<(String, TargetSnapshot, Instant)>,
    detail: String,
    config_path: PathBuf,
    config_error: Option<AppError>,
    recovery_error: Option<AppError>,
    credential_error: Option<AppError>,
    model_error: Option<AppError>,
    sender: Sender<Message>,
    cached: Arc<RwLock<Option<Snapshot>>>,
    devices: Vec<voice_audio::DeviceInfo>,
    notify: Box<dyn Fn(&UiState) + Send>,
    last_phase: Option<Phase>,
}
impl Actor {
    fn run(&mut self, receiver: Receiver<Message>, socket: SessionSocket) {
        let mut last_tick = Instant::now();
        let mut last_toggle = Instant::now() - Duration::from_secs(1);
        loop {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(Message::Command(command, reply)) => {
                    let result = self.command(command);
                    self.publish();
                    let _ = reply.send(result);
                }
                Ok(Message::Callback(action)) => {
                    self.apply(action);
                    self.publish();
                }
                Ok(Message::AudioReady(token, pcm)) => {
                    self.start_asr(token, pcm);
                    self.publish();
                }
                Ok(Message::AutoStop(token)) => {
                    if self
                        .controller
                        .snapshot()
                        .current
                        .as_ref()
                        .is_some_and(|j| j.token == token)
                        && self.controller.snapshot().phase == Phase::Recording
                    {
                        let target = self.freeze_target();
                        self.apply(Action::Stop { target });
                        self.publish();
                    }
                }
                Ok(Message::ModelFinished(result)) => {
                    self.operation_busy = false;
                    match result {
                        Ok((info, inventory)) => {
                            self.inventory = inventory;
                            self.detail = format!("模型已验证：{}，请在设置中选择", info.name);
                        }
                        Err(e) => self.detail = e.message,
                    }
                    if let Ok(manager) = self.models.try_lock() {
                        self.inventory = manager.inventory();
                    }
                    self.publish();
                }
                Ok(Message::RemoteTestFinished(result)) => {
                    self.operation_busy = false;
                    self.detail = match result {
                        Ok(_) => "合成文本连接测试通过".into(),
                        Err(e) => e.message,
                    };
                    self.publish();
                }
                Ok(Message::Shutdown) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    break;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
            if let Ok(Some(mut connection)) = socket.accept() {
                match connection.read_request() {
                    Ok(request) => {
                        let state = self.controller.snapshot();
                        let mut code = ReplyCode::Accepted;
                        if request.action != ControlAction::Status {
                            if request.action == ControlAction::Toggle
                                && last_toggle.elapsed() < Duration::from_millis(250)
                            {
                                code = ReplyCode::Busy;
                            } else {
                                if request.action == ControlAction::Toggle {
                                    last_toggle = Instant::now();
                                }
                                if request.action == ControlAction::Cancel {
                                    self.cancel_delivery_probe();
                                    self.operation_cancel.store(true, Ordering::Release);
                                }
                                let action = match request.action {
                                    ControlAction::Start => Action::Start {
                                        source: StartSource::Shortcut,
                                    },
                                    ControlAction::Stop => Action::Stop {
                                        target: if state.phase == Phase::Recording {
                                            self.freeze_target()
                                        } else {
                                            None
                                        },
                                    },
                                    ControlAction::Toggle => Action::Toggle {
                                        source: StartSource::Shortcut,
                                        target: if state.phase == Phase::Recording {
                                            self.freeze_target()
                                        } else {
                                            None
                                        },
                                    },
                                    ControlAction::Cancel => Action::Cancel,
                                    ControlAction::Status => unreachable!(),
                                };
                                if !self.ready()
                                    && matches!(
                                        action,
                                        Action::Start { .. } | Action::Toggle { .. }
                                    )
                                    && state.phase != Phase::Recording
                                {
                                    code = ReplyCode::Invalid;
                                } else {
                                    let (result, effects) = self.controller.dispatch(action);
                                    code = match result {
                                        DispatchResult::Accepted => ReplyCode::Accepted,
                                        DispatchResult::Busy => ReplyCode::Busy,
                                        DispatchResult::Invalid => ReplyCode::Invalid,
                                    };
                                    self.effects(effects);
                                }
                            }
                        }
                        let current = self.controller.snapshot();
                        let state = match current.phase {
                            Phase::Ready => ControlState::Ready,
                            Phase::Recording => ControlState::Recording,
                            Phase::Transcribing => ControlState::Transcribing,
                            Phase::Rewriting => ControlState::Rewriting,
                            Phase::Delivering => ControlState::Delivering,
                            Phase::Result => ControlState::Result,
                        };
                        let _ = connection.reply(ControlReply {
                            code,
                            status: if request.action == ControlAction::Status {
                                Some(SanitizedStatus {
                                    state,
                                    busy: !matches!(current.phase, Phase::Ready | Phase::Result),
                                })
                            } else {
                                None
                            },
                        });
                        self.publish();
                    }
                    Err(_) => {
                        let _ = connection.reply(ControlReply {
                            code: ReplyCode::Invalid,
                            status: None,
                        });
                    }
                }
            }
            if last_tick.elapsed() >= Duration::from_millis(500) {
                if self
                    .pending_delivery_probe
                    .as_ref()
                    .is_some_and(|(_, _, started)| started.elapsed() > Duration::from_secs(5))
                {
                    self.cancel_delivery_probe();
                    self.detail = "粘贴验证未在时限内完成，自动交付仍未启用".into();
                }
                self.apply(Action::Tick { now_ms: now() });
                let _ = self.store.list(now());
                self.asr.reap_idle();
                self.llm.reap_idle();
                self.platform.maintenance();
                self.publish();
                last_tick = Instant::now();
            }
        }
        self.work_cancel.store(true, Ordering::Release);
        self.operation_cancel.store(true, Ordering::Release);
        if let Some(capture) = self.capture.take() {
            let _ = capture.control.try_send(true);
        }
        self.asr.unload();
        self.llm.unload();
        self.credentials.clear_session();
    }
    fn freeze_target(&mut self) -> Option<Target> {
        let native = self.platform.capture_target().ok()?;
        let id = uuid::Uuid::new_v4().to_string();
        let target = Target {
            id: id.clone(),
            pid: native.pid(),
            focus_epoch: native.focus_epoch(),
        };
        self.targets.clear();
        self.targets.insert(id, native);
        Some(target)
    }
    fn ready(&self) -> bool {
        let config = self.controller.snapshot().config;
        self.config_error.is_none()
            && matches!(config.asr.backend, AsrBackend::Auto | AsrBackend::Cpu)
            && (config.llm.mode != LlmMode::Local || config.llm.local.gpu_layers == 0)
            && !self.operation_busy
            && !self.devices.is_empty()
            && config.validate_ready().is_ok()
            && self
                .inventory
                .iter()
                .any(|m| m.id == config.asr.model_id && m.engine == "whisper")
            && match config.llm.mode {
                LlmMode::Local => self
                    .inventory
                    .iter()
                    .any(|m| m.id == config.llm.local.model_id && m.engine == "gguf"),
                LlmMode::Remote => {
                    self.credentials
                        .is_confirmed(&config.llm.remote.profile_id, &config.llm.remote.base_url)
                        .unwrap_or(false)
                        && self
                            .credentials
                            .contains(&config.llm.remote.profile_id, &config.llm.remote.base_url)
                            .unwrap_or(false)
                }
            }
    }
    fn publish(&mut self) {
        let mut state = self.controller.snapshot();
        if let Some(e) = &self.config_error {
            state.error = Some(e.clone());
        }
        if self.last_phase.as_ref() != Some(&state.phase) {
            (self.notify)(&state);
            self.last_phase = Some(state.phase);
        }
        let config = state.config.clone();
        let remote_confirmed = self
            .credentials
            .is_confirmed(&config.llm.remote.profile_id, &config.llm.remote.base_url)
            .unwrap_or(false);
        let credential_present = remote_confirmed
            && self
                .credentials
                .contains(&config.llm.remote.profile_id, &config.llm.remote.base_url)
                .unwrap_or(false);
        let asr_ready = self
            .inventory
            .iter()
            .any(|m| m.id == config.asr.model_id && m.engine == "whisper");
        let llm_ready = match config.llm.mode {
            LlmMode::Local => self
                .inventory
                .iter()
                .any(|m| m.id == config.llm.local.model_id && m.engine == "gguf"),
            LlmMode::Remote => remote_confirmed && credential_present,
        };
        let warnings: Vec<_> = [
            self.recovery_error.clone(),
            self.credential_error.clone(),
            self.model_error.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let notice = warnings
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("；");
        let snapshot = Snapshot {
            recent: state.history.clone(),
            state,
            config,
            models: self.inventory.clone(),
            hardware: self.hardware.clone(),
            model_recommendations: crate::models::recommend(&self.hardware, &self.inventory),
            devices: self.devices.clone(),
            ready: self.ready(),
            microphone_ready: !self.devices.is_empty(),
            asr_ready,
            llm_ready,
            remote_confirmed,
            credential_present,
            detail: if notice.is_empty() {
                self.detail.clone()
            } else {
                format!("{notice} {}", self.detail)
            },
            operation_busy: self.operation_busy,
            capabilities: self.platform.capabilities(),
            warnings,
        };
        if let Ok(mut output) = self.cached.write() {
            *output = Some(snapshot);
        }
    }
    fn apply(&mut self, action: Action) {
        let (_, effects) = self.controller.dispatch(action);
        self.effects(effects);
    }
    fn callback(&self, action: Action) {
        let _ = self.sender.send(Message::Callback(action));
    }
    fn effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::StartCapture { token, config } => {
                    self.work_cancel = Arc::new(AtomicBool::new(false));
                    let (tx, rx) = bounded(1);
                    let output = self.sender.clone();
                    let current = token.clone();
                    let flag = self.work_cancel.clone();
                    thread::spawn(move || {
                        let result = (|| {
                            let capture = voice_audio::Capture::start(
                                &config.audio.device_id,
                                config.audio.max_seconds,
                            )?;
                            let started = Instant::now();
                            let mut auto_sent = false;
                            loop {
                                if flag.load(Ordering::Acquire) {
                                    drop(capture);
                                    return Err(error("cancelled", "录音已取消"));
                                }
                                match rx.recv_timeout(Duration::from_millis(25)) {
                                    Ok(true)
                                    | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                                        drop(capture);
                                        return Err(error("cancelled", "录音已取消"));
                                    }
                                    Ok(false) => return capture.stop(),
                                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                                }
                                if !auto_sent
                                    && started.elapsed()
                                        >= Duration::from_secs(config.audio.max_seconds.into())
                                {
                                    let _ = output.send(Message::AutoStop(current.clone()));
                                    auto_sent = true;
                                }
                            }
                        })();
                        let message = match result {
                            Ok(pcm) => {
                                if !voice_audio::contains_voice(&pcm) {
                                    Action::Failed {
                                        token: current,
                                        error: AppError::new(
                                            "no_speech",
                                            "capture",
                                            true,
                                            "未检测到语音",
                                        ),
                                    }
                                } else {
                                    // PCM is transferred through a private background message below.
                                    let _ = output.send(Message::AudioReady(current, pcm));
                                    return;
                                }
                            }
                            Err(error) => Action::Failed {
                                token: current,
                                error,
                            },
                        };
                        let _ = output.send(Message::Callback(message));
                    });
                    self.capture = Some(CaptureHandle { control: tx, token });
                }
                Effect::StopCaptureAndTranscribe { .. } => {
                    if let Some(capture) = &self.capture {
                        let _ = capture.control.try_send(false);
                    }
                }
                Effect::CancelWork { .. } => {
                    self.work_cancel.store(true, Ordering::Release);
                    if let Some(capture) = self.capture.take() {
                        let _ = capture.control.try_send(true);
                    }
                }
                Effect::CommitRaw { token, record } => {
                    let result = self.store.commit_raw(
                        &token.job_id,
                        &record.raw,
                        &record.config,
                        record.created_at_ms,
                    );
                    self.callback(Action::RawCommitted {
                        token,
                        persisted: result.is_ok()
                            && self.recovery_error.is_none()
                            && (!record.config.recovery.persist || self.store.is_persistent()),
                    });
                }
                Effect::CommitProcessed { token, record } => {
                    let result = self.store.commit_processed(
                        &token.job_id,
                        record.processed.as_deref().unwrap_or(""),
                        &record.config,
                        now(),
                    );
                    self.callback(Action::ProcessedCommitted {
                        token,
                        persisted: result.is_ok(),
                    });
                }
                Effect::Rewrite { token, raw, config } => {
                    self.start_rewrite(token, raw, config);
                }
                Effect::PrepareDelivery { token, target, .. } => {
                    let safe = self
                        .targets
                        .get(&target.id)
                        .is_some_and(|t| self.platform.can_deliver(t));
                    self.callback(Action::DeliveryPrepared { token, safe });
                }
                Effect::MarkDeliveryAttempt { token } => {
                    let result =
                        self.store
                            .mark_delivery(&token.job_id, DeliveryStatus::Attempting, now());
                    self.callback(Action::DeliveryAttemptSaved {
                        token,
                        persisted: result.is_ok(),
                    });
                }
                Effect::Deliver {
                    token,
                    target,
                    text,
                } => {
                    let outcome = self
                        .targets
                        .get(&target.id)
                        .map_or(DeliveryOutcome::Manual, |t| self.platform.deliver(t, &text));
                    let status = match outcome {
                        DeliveryOutcome::SentUnconfirmed => DeliveryStatus::SentUnconfirmed,
                        DeliveryOutcome::Manual => DeliveryStatus::Manual,
                        DeliveryOutcome::Unknown => DeliveryStatus::Unknown,
                    };
                    self.callback(Action::Delivered { token, status });
                }
                Effect::PersistDelivery { token, status } => {
                    let _ = self.store.mark_delivery(&token.job_id, status, now());
                }
            }
        }
    }

    fn command(&mut self, command: UiCommand) -> Result<Value, AppError> {
        match command {
            UiCommand::Start => {
                if !self.ready() {
                    return Err(error("setup_required", "请先完成麦克风和模型配置"));
                }
                let (result, effects) = self.controller.dispatch(Action::Start {
                    source: StartSource::Ui,
                });
                self.effects(effects);
                dispatch_reply(result)
            }
            UiCommand::Stop => {
                let target = if self.controller.snapshot().phase == Phase::Recording {
                    self.freeze_target()
                } else {
                    None
                };
                let (result, effects) = self.controller.dispatch(Action::Stop { target });
                self.effects(effects);
                dispatch_reply(result)
            }
            UiCommand::Cancel => {
                self.cancel_delivery_probe();
                self.operation_cancel.store(true, Ordering::Release);
                self.apply(Action::Cancel);
                Ok(json!({"accepted":true}))
            }
            UiCommand::Retry {
                job_id,
                use_current_config,
            } => {
                if self.operation_busy {
                    return Err(error("busy", "模型操作尚未完成"));
                }
                let (result, effects) = self.controller.dispatch(Action::Retry {
                    job_id,
                    use_current_config,
                });
                self.effects(effects);
                dispatch_reply(result)
            }
            UiCommand::ClearHistory => {
                self.apply(Action::Cancel);
                self.store.purge()?;
                self.apply(Action::Forget { job_id: None });
                self.targets.clear();
                Ok(json!({"accepted":true}))
            }
            UiCommand::DeleteResult { job_id } => {
                if !matches!(
                    self.controller.snapshot().phase,
                    Phase::Ready | Phase::Result
                ) {
                    return Err(error("busy", "请先取消当前任务再删除结果"));
                }
                self.store.delete(&job_id)?;
                self.apply(Action::Forget {
                    job_id: Some(job_id),
                });
                Ok(json!({"accepted":true}))
            }
            UiCommand::CopyResult { job_id, processed } => {
                let state = self.controller.snapshot();
                let record = state
                    .history
                    .iter()
                    .find(|r| r.job_id == job_id)
                    .ok_or_else(|| error("record_missing", "该结果已过期或删除"))?;
                let text = if processed {
                    record
                        .processed
                        .as_deref()
                        .ok_or_else(|| error("result_missing", "尚无处理结果"))?
                } else {
                    &record.raw
                };
                self.platform
                    .copy(text)
                    .map_err(|_| error("clipboard", "复制失败，可直接选择预览中的文字"))?;
                Ok(json!({"copied":true}))
            }
            UiCommand::SaveConfig {
                config,
                remote_confirmed,
                key,
                session_only,
            } => {
                let mut config = *config;
                config.validate()?;
                if self.config_error.is_some() {
                    return Err(self.config_error.clone().unwrap());
                }
                if !matches!(config.asr.backend, AsrBackend::Auto | AsrBackend::Cpu)
                    || (config.llm.mode == LlmMode::Local && config.llm.local.gpu_layers != 0)
                {
                    return Err(error(
                        "backend_unavailable",
                        "此构建仅验证了 CPU 后端，请使用 CPU",
                    ));
                }
                if !config.asr.model_id.is_empty() {
                    let _ = self
                        .inventory
                        .iter()
                        .find(|m| m.id == config.asr.model_id && m.engine == "whisper")
                        .ok_or_else(|| error("model_missing", "所选转写模型不可用"))?;
                }
                if !config.llm.local.model_id.is_empty() {
                    let _ = self
                        .inventory
                        .iter()
                        .find(|m| m.id == config.llm.local.model_id && m.engine == "gguf")
                        .ok_or_else(|| error("model_missing", "所选本地模型不可用"))?;
                }
                let old = self.controller.snapshot().config;
                if config.recovery != old.recovery
                    && config.recovery.persist
                    && !matches!(
                        self.controller.snapshot().phase,
                        Phase::Ready | Phase::Result
                    )
                {
                    return Err(error(
                        "busy",
                        "请等待当前任务结束后修改恢复设置，或先取消任务",
                    ));
                }
                if !config.llm.remote.base_url.is_empty() {
                    config.llm.remote.base_url =
                        rewrite::canonical_endpoint(&config.llm.remote.base_url)?;
                }
                let old_endpoint =
                    rewrite::canonical_endpoint(&old.llm.remote.base_url).unwrap_or_default();
                let endpoint_changed = config.llm.remote.base_url != old_endpoint;
                let old_confirmation = self
                    .credentials
                    .is_confirmed(&old.llm.remote.profile_id, &old.llm.remote.base_url)
                    .unwrap_or(false);
                let key = key.map(zeroize::Zeroizing::new);
                if !config.llm.remote.base_url.is_empty() {
                    config.llm.remote.base_url =
                        rewrite::canonical_endpoint(&config.llm.remote.base_url)?;
                    if endpoint_changed
                        || key.is_some()
                        || config.llm.remote.profile_id.is_empty()
                        || remote_confirmed != old_confirmation
                        || config.llm.remote.profile_id != old.llm.remote.profile_id
                    {
                        config.llm.remote.profile_id = uuid::Uuid::new_v4().to_string();
                    }
                    if remote_confirmed {
                        self.credentials.confirm_endpoint(
                            &config.llm.remote.profile_id,
                            &config.llm.remote.base_url,
                        )?;
                    } else {
                        let _ = self.credentials.forget_confirmation(
                            &config.llm.remote.profile_id,
                            &config.llm.remote.base_url,
                        );
                    }
                    if let Some(key) = &key {
                        if !remote_confirmed {
                            return Err(error(
                                "remote_confirmation_required",
                                "请先确认远程目的地和发送内容",
                            ));
                        }
                        self.credentials.put(
                            &config.llm.remote.profile_id,
                            &config.llm.remote.base_url,
                            key,
                            session_only,
                        )?;
                    }
                } else if key.is_some() {
                    return Err(error("remote_endpoint", "请先填写有效的 HTTPS 基地址"));
                }
                // New credential reference is prepared first. A failed config save
                // cannot replace the old profile's credential.
                self.store
                    .set_limits(config.recovery.max_entries, config.recovery.ttl_hours)?;
                if old.recovery.persist != config.recovery.persist {
                    if let Err(error) = self.store.set_persist(config.recovery.persist) {
                        let _ = self
                            .store
                            .set_limits(old.recovery.max_entries, old.recovery.ttl_hours);
                        self.discard_prepared_credential(&config, &old, key.is_some());
                        return Err(error);
                    }
                    if !config.recovery.persist {
                        self.apply(Action::Cancel);
                        self.apply(Action::Forget { job_id: None });
                    }
                }
                let save_result = config.save(&self.config_path);
                if let Err(error) = &save_result
                    && error.code != "config_sync_failed"
                {
                    let _ = self.store.set_persist(old.recovery.persist);
                    let _ = self
                        .store
                        .set_limits(old.recovery.max_entries, old.recovery.ttl_hours);
                    self.discard_prepared_credential(&config, &old, key.is_some());
                    return Err(error.clone());
                }
                self.apply(Action::Configure {
                    config: config.clone(),
                });
                self.platform
                    .set_sensitive_classes(config.delivery.sensitive_apps);
                self.devices = voice_audio::devices().unwrap_or_default();
                self.detail = "设置已保存".into();
                save_result?;
                Ok(json!({"saved":true}))
            }
            UiCommand::ImportModel { engine } => {
                self.model_operation(None, Some(engine))?;
                Ok(json!({"accepted":true}))
            }
            UiCommand::DownloadModel { model_id } => {
                self.model_operation(Some(model_id), None)?;
                Ok(json!({"accepted":true}))
            }
            UiCommand::DeleteModel { model_id } => {
                if self.operation_busy
                    || !matches!(
                        self.controller.snapshot().phase,
                        Phase::Ready | Phase::Result
                    )
                {
                    return Err(error("busy", "请等待当前任务结束"));
                }
                let mut models = self
                    .models
                    .lock()
                    .map_err(|_| error("model_lock", "模型目录不可用"))?;
                let result = models.delete(&model_id, &self.controller.snapshot().config);
                self.inventory = models.inventory();
                result?;
                Ok(json!({"deleted":true}))
            }
            UiCommand::DeleteCredential => {
                let config = self.controller.snapshot().config;
                self.credentials
                    .delete(&config.llm.remote.profile_id, &config.llm.remote.base_url)?;
                Ok(json!({"deleted":true}))
            }
            UiCommand::TestRemote => {
                if self.operation_busy
                    || !matches!(
                        self.controller.snapshot().phase,
                        Phase::Ready | Phase::Result
                    )
                {
                    return Err(error("busy", "已有模型操作或连接测试正在运行"));
                }
                let config = self.controller.snapshot().config;
                let key = self
                    .credentials
                    .get(&config.llm.remote.profile_id, &config.llm.remote.base_url)?
                    .ok_or_else(|| error("credential_missing", "请先保存此端点的配置和凭据"))?;
                self.operation_busy = true;
                self.operation_cancel = Arc::new(AtomicBool::new(false));
                self.detail = "正在用合成文本测试连接".into();
                let provider = self.provider.clone();
                let flag = self.operation_cancel.clone();
                let sender = self.sender.clone();
                thread::spawn(move || {
                    let result = provider.test_fixed(&config, &key, &flag);
                    let _ = sender.send(Message::RemoteTestFinished(result));
                });
                Ok(json!({"accepted":true}))
            }
            UiCommand::TestDelivery => {
                if self.operation_busy
                    || !matches!(
                        self.controller.snapshot().phase,
                        Phase::Ready | Phase::Result
                    )
                {
                    return Err(error("busy", "请等待当前任务结束后验证粘贴"));
                }
                self.platform.invalidate_automatic_delivery();
                let target = self.platform.probe_own_window().map_err(|_| {
                    error(
                        "delivery_probe_failed",
                        "无法向本应用验证框粘贴。请确认桌面已解锁、窗口有焦点且 wtype/剪贴板可用",
                    )
                })?;
                let token = uuid::Uuid::new_v4().to_string();
                self.pending_delivery_probe = Some((token.clone(), target, Instant::now()));
                self.operation_busy = true;
                self.detail = "正在等待验证框回读，自动交付尚未启用".into();
                Ok(json!({"token":token,"expected":voice_platform::DELIVERY_PROBE_TEXT}))
            }
            UiCommand::CompleteDeliveryTest {
                token,
                observed_text,
            } => {
                let pending = self.pending_delivery_probe.take();
                let Some((expected, target, started)) = pending else {
                    return Err(error("delivery_probe_missing", "没有待完成的粘贴验证"));
                };
                self.operation_busy = false;
                if token != expected || started.elapsed() > Duration::from_secs(5) {
                    self.platform.invalidate_automatic_delivery();
                    return Err(error("delivery_probe_expired", "验证已过期，请重新开始"));
                }
                self.platform
                    .accept_own_probe(&target, &observed_text)
                    .map_err(|_| {
                        error(
                            "delivery_probe_mismatch",
                            "验证文字或目标已改变，自动交付仍未启用",
                        )
                    })?;
                self.detail = "本次会话粘贴能力已验证；实际任务仍逐次检查目标与焦点".into();
                Ok(json!({"verified":true}))
            }
            UiCommand::CancelDeliveryTest => {
                self.cancel_delivery_probe();
                Ok(json!({"accepted":true}))
            }
        }
    }
    fn cancel_delivery_probe(&mut self) {
        if self.pending_delivery_probe.take().is_some() {
            self.operation_busy = false;
            self.platform.invalidate_automatic_delivery();
        }
    }
    fn model_operation(
        &mut self,
        download: Option<String>,
        import: Option<String>,
    ) -> Result<(), AppError> {
        if self.operation_busy
            || !matches!(
                self.controller.snapshot().phase,
                Phase::Ready | Phase::Result
            )
        {
            return Err(error("busy", "请等待当前任务结束后操作模型"));
        }
        self.operation_busy = true;
        self.operation_cancel = Arc::new(AtomicBool::new(false));
        // No task is active here. Release idle weights before validating a
        // replacement so the resource budget does not count both models.
        self.asr.unload();
        self.llm.unload();
        self.detail = if download.is_some() {
            "正在下载、校验并验证模型加载".into()
        } else {
            "请选择本地模型文件".into()
        };
        let manager = self.models.clone();
        let verification_config = self.controller.snapshot().config;
        let asr = self.asr.clone();
        let llm = self.llm.clone();
        let flag = self.operation_cancel.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let result = (|| {
                let mut manager = manager
                    .lock()
                    .map_err(|_| error("model_lock", "模型目录暂不可用"))?;
                let engine = import.clone().unwrap_or_else(|| {
                    if download.as_deref().unwrap_or("").starts_with("whisper") {
                        "whisper".into()
                    } else {
                        "gguf".into()
                    }
                });
                let verify = |path: &Path| {
                    let pool = if engine == "whisper" { &asr } else { &llm };
                    pool.verify_cancellable(
                        load_message(path, &verification_config, engine == "whisper"),
                        &flag,
                    )
                };
                let info = if let Some(id) = download {
                    manager.download(&id, &flag, verify)?
                } else {
                    let path = rfd::FileDialog::new()
                        .set_title("选择模型单文件")
                        .pick_file()
                        .ok_or_else(|| error("cancelled", "已取消模型导入"))?;
                    if flag.load(Ordering::Acquire) {
                        return Err(error("cancelled", "模型操作已取消"));
                    }
                    manager.import_cancellable(&path, &engine, &flag, verify)?
                };
                Ok((info, manager.inventory()))
            })();
            let _ = sender.send(Message::ModelFinished(result));
        });
        Ok(())
    }
    fn discard_prepared_credential(&mut self, config: &Config, old: &Config, has_key: bool) {
        if config.llm.remote.profile_id != old.llm.remote.profile_id {
            if has_key {
                let _ = self
                    .credentials
                    .delete(&config.llm.remote.profile_id, &config.llm.remote.base_url);
            }
            let _ = self
                .credentials
                .forget_confirmation(&config.llm.remote.profile_id, &config.llm.remote.base_url);
        }
    }
    fn start_asr(&mut self, token: WorkToken, pcm: Vec<f32>) {
        let state = self.controller.snapshot();
        let Some(job) = state
            .current
            .filter(|j| j.token == token && state.phase == Phase::Transcribing)
        else {
            return;
        };
        if self.capture.as_ref().is_some_and(|c| c.token == token) {
            self.capture.take();
        }
        let model = self
            .models
            .lock()
            .map_err(|_| error("model_lock", "模型目录暂不可用"))
            .and_then(|models| models.resolve(&job.config.asr.model_id, "whisper"));
        let model = match model {
            Ok(m) => m,
            Err(error) => {
                self.callback(Action::Failed { token, error });
                return;
            }
        };
        let pool = self.asr.clone();
        let sender = self.sender.clone();
        let flag = self.work_cancel.clone();
        let config = job.config;
        thread::spawn(move || {
            let result = pool.run(
                &model_signature(&model, &config, true),
                load_message(&model.path, &config, true),
                json!({"op":"run","job_id":token.job_id,"language":config.asr.language}),
                Some(&pcm),
                Duration::from_secs(config.asr.timeout_seconds.into()),
                &flag,
            );
            if result.is_ok() {
                pool.pin_model(model);
            }
            let action = match result {
                Ok(raw) => Action::RawReady {
                    token,
                    raw,
                    now_ms: now(),
                },
                Err(error) => Action::Failed { token, error },
            };
            let _ = sender.send(Message::Callback(action));
        });
    }
    fn start_rewrite(&mut self, token: WorkToken, raw: String, config: Config) {
        if config.llm.mode == LlmMode::Local && config.llm.local.gpu_layers != 0 {
            self.callback(Action::Failed {
                token,
                error: error(
                    "backend_unavailable",
                    "此构建仅验证 CPU，请选择当前 CPU 配置重新整理；原文已保留",
                ),
            });
            return;
        }
        self.work_cancel = Arc::new(AtomicBool::new(false));
        let flag = self.work_cancel.clone();
        let sender = self.sender.clone();
        match config.llm.mode {
            LlmMode::Local => {
                let model = self
                    .models
                    .lock()
                    .map_err(|_| error("model_lock", "模型目录暂不可用"))
                    .and_then(|m| m.resolve(&config.llm.local.model_id, "gguf"));
                let model = match model {
                    Ok(m) => m,
                    Err(error) => {
                        self.callback(Action::Failed { token, error });
                        return;
                    }
                };
                let pool = self.llm.clone();
                let asr = self.asr.clone();
                thread::spawn(move || {
                    let result = (|| {
                        if available_memory() < 3 * 1024 * 1024 * 1024 {
                            asr.unload();
                        }
                        let messages = rewrite::messages(&raw, &config)?;
                        let response=pool.run(&model_signature(&model,&config,false),load_message(&model.path,&config,false),json!({"op":"run","job_id":token.job_id,"messages":messages,"max_tokens":768}),None,Duration::from_secs(config.llm.local.timeout_seconds.into()),&flag)?;
                        pool.pin_model(model);
                        rewrite::validate_output_with_config(&raw, &response, &config)
                    })();
                    let action = match result {
                        Ok(text) => Action::Rewritten { token, text },
                        Err(error) => Action::Failed { token, error },
                    };
                    let _ = sender.send(Message::Callback(action));
                });
            }
            LlmMode::Remote => {
                let key = match self
                    .credentials
                    .get(&config.llm.remote.profile_id, &config.llm.remote.base_url)
                {
                    Ok(Some(k)) => k,
                    Ok(None) => {
                        self.callback(Action::Failed {
                            token,
                            error: error("credential_missing", "此端点没有可用凭据，原文已保留"),
                        });
                        return;
                    }
                    Err(error) => {
                        self.callback(Action::Failed { token, error });
                        return;
                    }
                };
                let provider = self.provider.clone();
                thread::spawn(move || {
                    let result = provider.process(&raw, &config, &key, &flag);
                    let action = match result {
                        Ok(text) => Action::Rewritten { token, text },
                        Err(error) => Action::Failed { token, error },
                    };
                    let _ = sender.send(Message::Callback(action));
                });
            }
        }
    }
}

fn dispatch_reply(result: DispatchResult) -> Result<Value, AppError> {
    match result {
        DispatchResult::Accepted => Ok(json!({"accepted":true})),
        DispatchResult::Busy => Err(error("busy", "正在处理当前任务，可取消后再试")),
        DispatchResult::Invalid => Err(error("invalid_action", "当前状态或配置不允许此操作")),
    }
}
pub fn load_message(path: &Path, config: &Config, asr: bool) -> Value {
    let threads = if asr {
        config.asr.threads
    } else {
        config.llm.local.threads
    };
    let threads = if threads == 0 {
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(4)
    } else {
        threads
    };
    let mut message = json!({"op":"load","model_path":path,"threads":threads,"gpu_layers":0});
    if !asr {
        message["context_tokens"] = json!(config.llm.local.context_tokens);
    }
    message
}
fn model_signature(model: &ModelRecord, config: &Config, asr: bool) -> String {
    format!(
        "{}:{}",
        model.info.sha256,
        load_message(&model.path, config, asr)
    )
}
fn available_memory() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("MemAvailable:")
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|v| v.parse::<u64>().ok())
            })
        })
        .unwrap_or(0)
        * 1024
}

pub fn worker_path(directory: &Path, name: &str) -> PathBuf {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2")
        && std::is_x86_feature_detected!("fma")
        && std::is_x86_feature_detected!("f16c")
        && std::is_x86_feature_detected!("bmi2")
    {
        let optimized = directory.join(format!("{name}-avx2"));
        if optimized.is_file() {
            return optimized;
        }
    }
    directory.join(name)
}
