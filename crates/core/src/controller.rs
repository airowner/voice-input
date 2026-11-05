use crate::{AppError, Config, DeliveryMode, DeliveryStatus, Record};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkToken {
    pub job_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub pid: u32,
    pub focus_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartSource {
    Ui,
    Shortcut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Ready,
    Recording,
    Transcribing,
    Rewriting,
    Delivering,
    Result,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchResult {
    Accepted,
    Busy,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentJob {
    pub token: WorkToken,
    pub source: StartSource,
    pub config: Config,
    pub raw: Option<String>,
    pub processed: Option<String>,
    pub created_at_ms: Option<i64>,
    pub delivery: DeliveryStatus,
    pub preview_required: bool,
    pub target: Option<Target>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiState {
    pub sequence: u64,
    pub phase: Phase,
    pub config: Config,
    pub current: Option<CurrentJob>,
    pub history: Vec<Record>,
    pub error: Option<AppError>,
}

/// Host-only callbacks must not be exposed as arbitrary UI commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Start {
        source: StartSource,
    },
    Stop {
        target: Option<Target>,
    },
    Toggle {
        source: StartSource,
        target: Option<Target>,
    },
    Cancel,
    Configure {
        config: Config,
    },
    Restore {
        records: Vec<Record>,
    },
    Forget {
        job_id: Option<String>,
    },
    Tick {
        now_ms: i64,
    },
    Retry {
        job_id: String,
        use_current_config: bool,
    },
    RawReady {
        token: WorkToken,
        raw: String,
        now_ms: i64,
    },
    RawCommitted {
        token: WorkToken,
        persisted: bool,
    },
    Rewritten {
        token: WorkToken,
        text: String,
    },
    ProcessedCommitted {
        token: WorkToken,
        persisted: bool,
    },
    DeliveryPrepared {
        token: WorkToken,
        safe: bool,
    },
    DeliveryAttemptSaved {
        token: WorkToken,
        persisted: bool,
    },
    Delivered {
        token: WorkToken,
        status: DeliveryStatus,
    },
    Failed {
        token: WorkToken,
        error: AppError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Effect {
    StartCapture {
        token: WorkToken,
        config: Config,
    },
    StopCaptureAndTranscribe {
        token: WorkToken,
        config: Config,
    },
    CancelWork {
        token: WorkToken,
    },
    CommitRaw {
        token: WorkToken,
        record: Record,
    },
    Rewrite {
        token: WorkToken,
        raw: String,
        config: Config,
    },
    CommitProcessed {
        token: WorkToken,
        record: Record,
    },
    PrepareDelivery {
        token: WorkToken,
        target: Target,
        text: String,
    },
    MarkDeliveryAttempt {
        token: WorkToken,
    },
    Deliver {
        token: WorkToken,
        target: Target,
        text: String,
    },
    PersistDelivery {
        token: WorkToken,
        status: DeliveryStatus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    Capture,
    Asr,
    RawCommit,
    Rewrite,
    ProcessedCommit,
    Prepare,
    AttemptCommit,
    Delivery,
}

pub struct Controller {
    state: UiState,
    generation: u64,
    pending: Pending,
}

impl Controller {
    pub fn new(config: Config) -> Self {
        Self {
            state: UiState {
                sequence: 0,
                phase: Phase::Ready,
                config,
                current: None,
                history: Vec::new(),
                error: None,
            },
            generation: 0,
            pending: Pending::None,
        }
    }

    pub fn snapshot(&self) -> UiState {
        self.state.clone()
    }

    pub fn dispatch(&mut self, action: Action) -> (DispatchResult, Vec<Effect>) {
        let previous_error = self.state.error.clone();
        let result = self.reduce(action);
        if result.0 == DispatchResult::Accepted || self.state.error != previous_error {
            self.state.sequence = self.state.sequence.saturating_add(1);
        }
        result
    }

    fn reduce(&mut self, action: Action) -> (DispatchResult, Vec<Effect>) {
        match action {
            Action::Toggle { source, target } => match self.state.phase {
                Phase::Ready | Phase::Result => self.start(source),
                Phase::Recording => self.stop(target),
                _ => busy(),
            },
            Action::Start { source } => self.start(source),
            Action::Stop { target } => self.stop(target),
            Action::Cancel => self.cancel(),
            Action::Configure { config } => {
                if let Err(error) = config.validate() {
                    self.state.error = Some(error);
                    return invalid();
                }
                if self.state.config.recovery.persist && !config.recovery.persist {
                    self.state.history.clear();
                }
                self.state.history.truncate(config.recovery.max_entries);
                self.state.config = config;
                accepted(Vec::new())
            }
            Action::Restore { mut records } => {
                if self.state.phase != Phase::Ready || self.state.current.is_some() {
                    return invalid();
                }
                for record in &mut records {
                    record.preview_required = true;
                    if record.delivery == DeliveryStatus::Attempting {
                        record.delivery = DeliveryStatus::Unknown;
                    }
                }
                records.sort_by_key(|record| std::cmp::Reverse(record.created_at_ms));
                records.truncate(self.state.config.recovery.max_entries.min(5));
                self.state.history = records;
                accepted(Vec::new())
            }
            Action::Forget { job_id } => {
                if !matches!(self.state.phase, Phase::Ready | Phase::Result) {
                    return busy();
                }
                self.state
                    .history
                    .retain(|record| job_id.as_ref().is_some_and(|id| &record.job_id != id));
                if self
                    .state
                    .current
                    .as_ref()
                    .is_some_and(|job| job_id.as_ref().is_none_or(|id| &job.token.job_id == id))
                {
                    self.state.current = None;
                    self.state.phase = Phase::Ready;
                    self.pending = Pending::None;
                }
                self.state.error = None;
                accepted(Vec::new())
            }
            Action::Tick { now_ms } => {
                let cutoff = now_ms.saturating_sub(
                    i64::from(self.state.config.recovery.ttl_hours.min(24)) * 3_600_000,
                );
                self.state
                    .history
                    .retain(|record| record.created_at_ms > cutoff);
                if self
                    .state
                    .current
                    .as_ref()
                    .and_then(|job| job.created_at_ms)
                    .is_some_and(|t| t <= cutoff)
                {
                    let effects = self.cancel().1;
                    self.state.current = None;
                    self.state.phase = Phase::Ready;
                    self.pending = Pending::None;
                    return accepted(effects);
                }
                accepted(Vec::new())
            }
            Action::Retry {
                job_id,
                use_current_config,
            } => self.retry(&job_id, use_current_config),
            Action::RawReady { token, raw, now_ms } => {
                if !self.expected(&token, Pending::Asr) {
                    return invalid();
                }
                if raw.trim().is_empty() {
                    self.finish_error(AppError::new("no_speech", "asr", true, "未检测到语音"));
                    return accepted(Vec::new());
                }
                let job = self.state.current.as_mut().unwrap();
                job.raw = Some(raw);
                job.created_at_ms = Some(now_ms);
                let record = self.current_record().unwrap();
                self.upsert_history(record.clone());
                self.pending = Pending::RawCommit;
                accepted(vec![Effect::CommitRaw { token, record }])
            }
            Action::RawCommitted { token, persisted } => {
                if !self.expected(&token, Pending::RawCommit) {
                    return invalid();
                }
                if !persisted {
                    self.force_preview(storage_error());
                    return accepted(Vec::new());
                }
                if self
                    .state
                    .current
                    .as_ref()
                    .unwrap()
                    .raw
                    .as_ref()
                    .unwrap()
                    .len()
                    > 8 * 1024
                {
                    self.force_preview(AppError::new(
                        "input_too_long",
                        "rewrite",
                        false,
                        "原文超过处理输入上限，完整原文已保留",
                    ));
                    return accepted(Vec::new());
                }
                self.state.phase = Phase::Rewriting;
                self.pending = Pending::Rewrite;
                let job = self.state.current.as_ref().unwrap();
                accepted(vec![Effect::Rewrite {
                    token,
                    raw: job.raw.clone().unwrap(),
                    config: job.config.clone(),
                }])
            }
            Action::Rewritten { token, text } => {
                if !self.expected(&token, Pending::Rewrite) {
                    return invalid();
                }
                if text.trim().is_empty()
                    || text.len() > 12 * 1024
                    || text
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t')
                {
                    self.force_preview(AppError::new(
                        "invalid_output",
                        "rewrite",
                        true,
                        "处理结果不符合输出要求，原文已保留",
                    ));
                    return accepted(Vec::new());
                }
                self.state.current.as_mut().unwrap().processed = Some(text);
                self.pending = Pending::ProcessedCommit;
                let record = self.current_record().unwrap();
                self.upsert_history(record.clone());
                accepted(vec![Effect::CommitProcessed { token, record }])
            }
            Action::ProcessedCommitted { token, persisted } => {
                if !self.expected(&token, Pending::ProcessedCommit) {
                    return invalid();
                }
                if !persisted {
                    self.force_preview(storage_error());
                    return accepted(Vec::new());
                }
                let job = self.state.current.as_ref().unwrap();
                if job.preview_required {
                    self.finish(DeliveryStatus::Preview);
                    return accepted(vec![Effect::PersistDelivery {
                        token,
                        status: DeliveryStatus::Preview,
                    }]);
                }
                let Some(target) = job.target.clone() else {
                    self.finish(DeliveryStatus::Manual);
                    return accepted(vec![Effect::PersistDelivery {
                        token,
                        status: DeliveryStatus::Manual,
                    }]);
                };
                let text = job.processed.clone().unwrap();
                self.state.phase = Phase::Delivering;
                self.pending = Pending::Prepare;
                accepted(vec![Effect::PrepareDelivery {
                    token,
                    target,
                    text,
                }])
            }
            Action::DeliveryPrepared { token, safe } => {
                if !self.expected(&token, Pending::Prepare) {
                    return invalid();
                }
                if !safe {
                    self.finish(DeliveryStatus::Manual);
                    return accepted(vec![Effect::PersistDelivery {
                        token,
                        status: DeliveryStatus::Manual,
                    }]);
                }
                self.pending = Pending::AttemptCommit;
                accepted(vec![Effect::MarkDeliveryAttempt { token }])
            }
            Action::DeliveryAttemptSaved { token, persisted } => {
                if !self.expected(&token, Pending::AttemptCommit) {
                    return invalid();
                }
                if !persisted {
                    self.force_preview(storage_error());
                    return accepted(Vec::new());
                }
                let job = self.state.current.as_mut().unwrap();
                job.delivery = DeliveryStatus::Attempting;
                let target = job.target.clone().unwrap();
                let text = job.processed.clone().unwrap();
                self.pending = Pending::Delivery;
                accepted(vec![Effect::Deliver {
                    token,
                    target,
                    text,
                }])
            }
            Action::Delivered { token, status } => {
                if !self.expected(&token, Pending::Delivery)
                    || !matches!(
                        status,
                        DeliveryStatus::SentUnconfirmed
                            | DeliveryStatus::Manual
                            | DeliveryStatus::Unknown
                    )
                {
                    return invalid();
                }
                self.finish(status);
                accepted(vec![Effect::PersistDelivery { token, status }])
            }
            Action::Failed { token, error } => {
                if self
                    .state
                    .current
                    .as_ref()
                    .is_none_or(|job| job.token != token)
                    || matches!(self.state.phase, Phase::Ready | Phase::Result)
                {
                    return invalid();
                }
                let status = if self.pending == Pending::Delivery {
                    DeliveryStatus::Unknown
                } else {
                    DeliveryStatus::Preview
                };
                let has_raw = self.state.current.as_ref().unwrap().raw.is_some();
                self.finish_error(error);
                self.finish(status);
                let mut effects = vec![Effect::CancelWork {
                    token: token.clone(),
                }];
                if has_raw {
                    effects.push(Effect::PersistDelivery { token, status });
                }
                accepted(effects)
            }
        }
    }

    fn start(&mut self, source: StartSource) -> (DispatchResult, Vec<Effect>) {
        if self.state.phase == Phase::Recording {
            return accepted(Vec::new());
        }
        if !matches!(self.state.phase, Phase::Ready | Phase::Result) {
            return busy();
        }
        if let Err(error) = self.state.config.validate_ready() {
            self.state.error = Some(error);
            return invalid();
        }
        self.generation = self.generation.saturating_add(1);
        let token = WorkToken {
            job_id: uuid::Uuid::new_v4().to_string(),
            generation: self.generation,
        };
        let config = self.state.config.clone();
        self.state.current = Some(CurrentJob {
            token: token.clone(),
            source,
            config: config.clone(),
            raw: None,
            processed: None,
            created_at_ms: None,
            delivery: DeliveryStatus::NotAttempted,
            preview_required: source == StartSource::Ui
                || config.delivery.mode == DeliveryMode::Preview,
            target: None,
        });
        self.state.error = None;
        self.state.phase = Phase::Recording;
        self.pending = Pending::Capture;
        accepted(vec![Effect::StartCapture { token, config }])
    }

    fn stop(&mut self, target: Option<Target>) -> (DispatchResult, Vec<Effect>) {
        if matches!(
            self.state.phase,
            Phase::Ready | Phase::Result | Phase::Transcribing
        ) {
            return accepted(Vec::new());
        }
        if self.state.phase != Phase::Recording {
            return busy();
        }
        let job = self.state.current.as_mut().unwrap();
        job.target = target;
        let token = job.token.clone();
        let config = job.config.clone();
        self.pending = Pending::Asr;
        self.state.phase = Phase::Transcribing;
        accepted(vec![Effect::StopCaptureAndTranscribe { token, config }])
    }

    fn retry(&mut self, job_id: &str, use_current_config: bool) -> (DispatchResult, Vec<Effect>) {
        if !matches!(self.state.phase, Phase::Ready | Phase::Result) {
            return busy();
        }
        let Some(record) = self
            .state
            .history
            .iter()
            .find(|record| record.job_id == job_id)
            .cloned()
        else {
            return invalid();
        };
        let config = if use_current_config {
            self.state.config.clone()
        } else {
            record.config.clone()
        };
        if record.raw.len() > 8 * 1024 {
            self.state.error = Some(AppError::new(
                "input_too_long",
                "rewrite",
                false,
                "原文超过处理输入上限，完整原文已保留",
            ));
            return invalid();
        }
        if let Err(error) = config.validate_ready() {
            self.state.error = Some(error);
            return invalid();
        }
        self.generation = self.generation.saturating_add(1);
        let token = WorkToken {
            job_id: job_id.into(),
            generation: self.generation,
        };
        self.state.current = Some(CurrentJob {
            token: token.clone(),
            source: StartSource::Ui,
            config: config.clone(),
            raw: Some(record.raw.clone()),
            processed: record.processed.clone(),
            created_at_ms: Some(record.created_at_ms),
            delivery: DeliveryStatus::Preview,
            preview_required: true,
            target: None,
        });
        self.state.error = None;
        self.state.phase = Phase::Rewriting;
        self.pending = Pending::Rewrite;
        accepted(vec![Effect::Rewrite {
            token,
            raw: record.raw,
            config,
        }])
    }

    fn cancel(&mut self) -> (DispatchResult, Vec<Effect>) {
        if matches!(self.state.phase, Phase::Ready | Phase::Result) {
            return accepted(Vec::new());
        }
        let job = self.state.current.as_ref().unwrap();
        let token = job.token.clone();
        let has_raw = job.raw.is_some();
        let status = if self.pending == Pending::Delivery {
            DeliveryStatus::Unknown
        } else {
            DeliveryStatus::Preview
        };
        let mut effects = vec![Effect::CancelWork {
            token: token.clone(),
        }];
        if has_raw {
            self.finish(status);
            effects.push(Effect::PersistDelivery {
                token: token.clone(),
                status,
            });
            self.generation = self.generation.saturating_add(1);
            self.state.current.as_mut().unwrap().token.generation = self.generation;
        } else {
            self.state.current = None;
            self.state.phase = Phase::Ready;
            self.pending = Pending::None;
        }
        accepted(effects)
    }

    fn expected(&self, token: &WorkToken, pending: Pending) -> bool {
        self.pending == pending
            && self
                .state
                .current
                .as_ref()
                .is_some_and(|job| &job.token == token)
    }

    fn current_record(&self) -> Option<Record> {
        let job = self.state.current.as_ref()?;
        Some(Record {
            job_id: job.token.job_id.clone(),
            raw: job.raw.clone()?,
            processed: job.processed.clone(),
            config: job.config.clone(),
            created_at_ms: job.created_at_ms?,
            delivery: job.delivery,
            preview_required: true,
        })
    }

    fn upsert_history(&mut self, record: Record) {
        self.state.history.retain(|old| old.job_id != record.job_id);
        self.state.history.push(record);
        self.state
            .history
            .sort_by_key(|record| std::cmp::Reverse(record.created_at_ms));
        self.state
            .history
            .truncate(self.state.config.recovery.max_entries.min(5));
    }

    fn finish(&mut self, status: DeliveryStatus) {
        if let Some(job) = self.state.current.as_mut() {
            job.delivery = status;
            job.preview_required = true;
        }
        if let Some(record) = self.current_record() {
            self.upsert_history(record);
        }
        self.state.phase = Phase::Result;
        self.pending = Pending::None;
    }

    fn finish_error(&mut self, error: AppError) {
        self.state.error = Some(error);
        self.finish(DeliveryStatus::Preview);
    }
    fn force_preview(&mut self, error: AppError) {
        self.finish_error(error);
    }
}

fn storage_error() -> AppError {
    AppError::new(
        "recovery_unavailable",
        "recovery",
        true,
        "恢复提交失败，内容保留在内存预览中，请主动复制或重试",
    )
}
fn accepted(effects: Vec<Effect>) -> (DispatchResult, Vec<Effect>) {
    (DispatchResult::Accepted, effects)
}
fn invalid() -> (DispatchResult, Vec<Effect>) {
    (DispatchResult::Invalid, Vec::new())
}
fn busy() -> (DispatchResult, Vec<Effect>) {
    (DispatchResult::Busy, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ready() -> Config {
        let mut c = Config::default();
        c.asr.model_id = "base".into();
        c.llm.local.model_id = "qwen".into();
        c
    }
    fn begin(c: &mut Controller, source: StartSource) -> WorkToken {
        let (status, effects) = c.dispatch(Action::Start { source });
        assert_eq!(status, DispatchResult::Accepted);
        assert!(matches!(effects[0], Effect::StartCapture { .. }));
        c.snapshot().current.unwrap().token
    }
    fn raw(c: &mut Controller, token: &WorkToken) {
        c.dispatch(Action::Stop {
            target: Some(Target {
                id: "window".into(),
                pid: 1,
                focus_epoch: 1,
            }),
        });
        c.dispatch(Action::RawReady {
            token: token.clone(),
            raw: "原文 42 /tmp/x".into(),
            now_ms: 100,
        });
    }
    // Each asynchronous boundary is separately reachable; tests below must not
    // infer its failure/cancellation behavior from another phase's callbacks.
    fn at_pending(pending: Pending) -> (Controller, WorkToken) {
        let mut c = Controller::new(ready());
        let token = begin(&mut c, StartSource::Shortcut);
        let steps = [
            (
                Pending::Capture,
                Action::Stop {
                    target: Some(Target {
                        id: "window".into(),
                        pid: 1,
                        focus_epoch: 1,
                    }),
                },
            ),
            (
                Pending::Asr,
                Action::RawReady {
                    token: token.clone(),
                    raw: "原文 42 /tmp/x".into(),
                    now_ms: 100,
                },
            ),
            (
                Pending::RawCommit,
                Action::RawCommitted {
                    token: token.clone(),
                    persisted: true,
                },
            ),
            (
                Pending::Rewrite,
                Action::Rewritten {
                    token: token.clone(),
                    text: "结果 42 /tmp/x".into(),
                },
            ),
            (
                Pending::ProcessedCommit,
                Action::ProcessedCommitted {
                    token: token.clone(),
                    persisted: true,
                },
            ),
            (
                Pending::Prepare,
                Action::DeliveryPrepared {
                    token: token.clone(),
                    safe: true,
                },
            ),
            (
                Pending::AttemptCommit,
                Action::DeliveryAttemptSaved {
                    token: token.clone(),
                    persisted: true,
                },
            ),
        ];
        for (before, action) in steps {
            assert_eq!(c.pending, before);
            if before == pending {
                return (c, token);
            }
            let (status, effects) = c.dispatch(action);
            assert_eq!(status, DispatchResult::Accepted);
            assert_eq!(effects.len(), 1);
        }
        assert_eq!(pending, Pending::Delivery);
        assert_eq!(c.pending, pending);
        (c, token)
    }

    const ACTIVE_BOUNDARIES: [Pending; 8] = [
        Pending::Capture,
        Pending::Asr,
        Pending::RawCommit,
        Pending::Rewrite,
        Pending::ProcessedCommit,
        Pending::Prepare,
        Pending::AttemptCommit,
        Pending::Delivery,
    ];

    fn assert_ignored(c: &mut Controller, action: Action, expected: DispatchResult) {
        let before = c.snapshot();
        let (status, effects) = c.dispatch(action);
        assert_eq!(status, expected);
        assert!(effects.is_empty());
        assert_eq!(c.snapshot(), before);
    }

    #[test]
    fn failed_at_every_async_boundary_preserves_raw_and_rejects_late_callbacks() {
        for pending in ACTIVE_BOUNDARIES {
            let (mut c, token) = at_pending(pending);
            let before = c.snapshot().current.unwrap();
            let error = AppError::new("injected_failure", "test", true, "合成故障");
            let mut stale = token.clone();
            stale.generation += 1;
            assert_ignored(
                &mut c,
                Action::Failed {
                    token: stale,
                    error: error.clone(),
                },
                DispatchResult::Invalid,
            );
            let mut other_job = token.clone();
            other_job.job_id = "another-job".into();
            assert_ignored(
                &mut c,
                Action::Failed {
                    token: other_job,
                    error: error.clone(),
                },
                DispatchResult::Invalid,
            );
            let (status, effects) = c.dispatch(Action::Failed {
                token: token.clone(),
                error: error.clone(),
            });
            assert_eq!(status, DispatchResult::Accepted);
            assert!(matches!(&effects[0], Effect::CancelWork { token: t } if t == &token));
            let delivery = if pending == Pending::Delivery {
                DeliveryStatus::Unknown
            } else {
                DeliveryStatus::Preview
            };
            assert_eq!(effects.len(), if before.raw.is_some() { 2 } else { 1 });
            if before.raw.is_some() {
                assert!(
                    matches!(&effects[1], Effect::PersistDelivery { token: t, status: s } if t == &token && *s == delivery)
                );
            }
            // The complete effect set above contains no capture/rewrite/native dispatch.
            let state = c.snapshot();
            assert_eq!(state.phase, Phase::Result);
            assert_eq!(state.error, Some(error.clone()));
            let current = state.current.unwrap();
            assert_eq!(current.raw, before.raw);
            assert_eq!(current.processed, before.processed);
            assert_eq!(current.delivery, delivery);
            assert!(current.preview_required);
            if let Some(raw) = before.raw {
                assert_eq!(state.history[0].raw, raw);
            } else {
                assert!(state.history.is_empty());
            }
            for action in [
                Action::Failed {
                    token: token.clone(),
                    error,
                },
                Action::RawReady {
                    token: token.clone(),
                    raw: "late".into(),
                    now_ms: 200,
                },
                Action::Rewritten {
                    token: token.clone(),
                    text: "late".into(),
                },
                Action::Delivered {
                    token,
                    status: DeliveryStatus::SentUnconfirmed,
                },
            ] {
                assert_ignored(&mut c, action, DispatchResult::Invalid);
            }
        }
    }

    #[test]
    fn cancellation_and_busy_toggle_at_every_async_boundary_have_no_replay() {
        for pending in ACTIVE_BOUNDARIES {
            let (mut c, token) = at_pending(pending);
            if pending != Pending::Capture {
                assert_ignored(
                    &mut c,
                    Action::Toggle {
                        source: StartSource::Shortcut,
                        target: None,
                    },
                    DispatchResult::Busy,
                );
                assert_ignored(
                    &mut c,
                    Action::Start {
                        source: StartSource::Ui,
                    },
                    DispatchResult::Busy,
                );
            }
            let before = c.snapshot().current.unwrap();
            let (status, effects) = c.dispatch(Action::Cancel);
            assert_eq!(status, DispatchResult::Accepted);
            assert!(matches!(&effects[0], Effect::CancelWork { token: t } if t == &token));
            assert_eq!(effects.len(), if before.raw.is_some() { 2 } else { 1 });
            let state = c.snapshot();
            if before.raw.is_some() {
                let delivery = if pending == Pending::Delivery {
                    DeliveryStatus::Unknown
                } else {
                    DeliveryStatus::Preview
                };
                assert!(
                    matches!(&effects[1], Effect::PersistDelivery { token: t, status: s } if t == &token && *s == delivery)
                );
                assert_eq!(state.phase, Phase::Result);
                let current = state.current.unwrap();
                assert_eq!(current.raw, before.raw);
                assert_eq!(current.processed, before.processed);
                assert!(current.preview_required);
                assert_ne!(current.token.generation, token.generation);
                assert_eq!(state.history[0].delivery, delivery);
            } else {
                assert_eq!(state.phase, Phase::Ready);
                assert!(state.current.is_none());
                assert!(state.history.is_empty());
            }
            let (status, effects) = c.dispatch(Action::Cancel);
            assert_eq!(status, DispatchResult::Accepted);
            assert!(effects.is_empty());
            for action in [
                Action::RawReady {
                    token: token.clone(),
                    raw: "late".into(),
                    now_ms: 200,
                },
                Action::RawCommitted {
                    token: token.clone(),
                    persisted: true,
                },
                Action::Rewritten {
                    token: token.clone(),
                    text: "late".into(),
                },
                Action::ProcessedCommitted {
                    token: token.clone(),
                    persisted: true,
                },
                Action::DeliveryPrepared {
                    token: token.clone(),
                    safe: true,
                },
                Action::DeliveryAttemptSaved {
                    token: token.clone(),
                    persisted: true,
                },
                Action::Delivered {
                    token,
                    status: DeliveryStatus::SentUnconfirmed,
                },
            ] {
                assert_ignored(&mut c, action, DispatchResult::Invalid);
            }
        }
    }

    #[test]
    fn recording_toggle_freezes_target_and_repeated_stops_never_restart_asr() {
        let mut c = Controller::new(ready());
        assert!(c.dispatch(Action::Stop { target: None }).1.is_empty());
        let (status, effects) = c.dispatch(Action::Toggle {
            source: StartSource::Shortcut,
            target: None,
        });
        assert_eq!(status, DispatchResult::Accepted);
        assert!(matches!(&effects[..], [Effect::StartCapture { .. }]));
        let token = c.snapshot().current.unwrap().token;
        let (status, effects) = c.dispatch(Action::Start {
            source: StartSource::Ui,
        });
        assert_eq!(status, DispatchResult::Accepted);
        assert!(effects.is_empty());
        assert_eq!(c.snapshot().current.unwrap().token, token);
        let target = Target {
            id: "frozen".into(),
            pid: 42,
            focus_epoch: 7,
        };
        let (status, effects) = c.dispatch(Action::Toggle {
            source: StartSource::Shortcut,
            target: Some(target.clone()),
        });
        assert_eq!(status, DispatchResult::Accepted);
        assert!(
            matches!(&effects[..], [Effect::StopCaptureAndTranscribe { token: t, .. }] if t == &token)
        );
        assert_eq!(c.snapshot().phase, Phase::Transcribing);
        for _ in 0..3 {
            let (status, effects) = c.dispatch(Action::Stop { target: None });
            assert_eq!(status, DispatchResult::Accepted);
            assert!(effects.is_empty());
            assert_eq!(
                c.snapshot().current.as_ref().unwrap().target,
                Some(target.clone())
            );
        }
        c.dispatch(Action::Failed {
            token: token.clone(),
            error: AppError::new("asr_failure", "asr", true, "合成故障"),
        });
        assert!(c.dispatch(Action::Stop { target: None }).1.is_empty());
        let (status, effects) = c.dispatch(Action::Toggle {
            source: StartSource::Ui,
            target: None,
        });
        assert_eq!(status, DispatchResult::Accepted);
        assert!(matches!(&effects[..], [Effect::StartCapture { .. }]));
        let next = c.snapshot().current.unwrap();
        assert_ne!(next.token.job_id, token.job_id);
        assert!(next.token.generation > token.generation);
        assert!(next.preview_required);
    }

    #[test]
    fn raw_commit_gate_busy_snapshot_and_cancel_late_results() {
        let mut c = Controller::new(ready());
        let token = begin(&mut c, StartSource::Shortcut);
        assert!(
            c.dispatch(Action::Start {
                source: StartSource::Shortcut
            })
            .1
            .is_empty()
        );
        let mut changed = ready();
        changed.llm.local.model_id = "new-model".into();
        c.dispatch(Action::Configure { config: changed });
        raw(&mut c, &token);
        assert_eq!(c.snapshot().phase, Phase::Transcribing);
        assert_eq!(
            c.snapshot().current.unwrap().config.llm.local.model_id,
            "qwen"
        );
        assert_eq!(
            c.dispatch(Action::Toggle {
                source: StartSource::Shortcut,
                target: None
            })
            .0,
            DispatchResult::Busy
        );
        assert!(c.dispatch(Action::Stop { target: None }).1.is_empty());
        assert_eq!(
            c.dispatch(Action::Rewritten {
                token: token.clone(),
                text: "too early".into()
            })
            .0,
            DispatchResult::Invalid
        );
        let effects = c
            .dispatch(Action::RawCommitted {
                token: token.clone(),
                persisted: true,
            })
            .1;
        assert!(matches!(effects[0], Effect::Rewrite { .. }));
        c.dispatch(Action::Cancel);
        assert_eq!(c.snapshot().phase, Phase::Result);
        assert_eq!(c.snapshot().current.unwrap().raw.unwrap(), "原文 42 /tmp/x");
        assert_eq!(
            c.dispatch(Action::Rewritten {
                token,
                text: "late".into()
            })
            .0,
            DispatchResult::Invalid
        );
    }
    #[test]
    fn persistence_failure_preserves_raw_and_cannot_trigger_llm() {
        let mut c = Controller::new(ready());
        let token = begin(&mut c, StartSource::Ui);
        raw(&mut c, &token);
        assert!(
            c.dispatch(Action::RawCommitted {
                token,
                persisted: false
            })
            .1
            .is_empty()
        );
        assert_eq!(c.snapshot().phase, Phase::Result);
        assert!(c.snapshot().current.unwrap().raw.is_some());
    }
    #[test]
    fn ui_and_retry_always_preview_and_old_generation_cannot_complete_retry() {
        let mut c = Controller::new(ready());
        let token = begin(&mut c, StartSource::Ui);
        raw(&mut c, &token);
        c.dispatch(Action::RawCommitted {
            token: token.clone(),
            persisted: true,
        });
        c.dispatch(Action::Rewritten {
            token: token.clone(),
            text: "整理 42 /tmp/x".into(),
        });
        let effects = c
            .dispatch(Action::ProcessedCommitted {
                token: token.clone(),
                persisted: true,
            })
            .1;
        assert!(
            effects
                .iter()
                .all(|e| !matches!(e, Effect::PrepareDelivery { .. } | Effect::Deliver { .. }))
        );
        c.dispatch(Action::Retry {
            job_id: token.job_id.clone(),
            use_current_config: false,
        });
        let retry = c.snapshot().current.unwrap();
        assert!(retry.preview_required);
        assert!(retry.target.is_none());
        assert_ne!(retry.token.generation, token.generation);
        assert_eq!(
            c.dispatch(Action::Rewritten {
                token,
                text: "late".into()
            })
            .0,
            DispatchResult::Invalid
        );
    }
    #[test]
    fn auto_delivery_requires_attempt_commit_and_is_dispatched_once() {
        let mut c = Controller::new(ready());
        let token = begin(&mut c, StartSource::Shortcut);
        raw(&mut c, &token);
        c.dispatch(Action::RawCommitted {
            token: token.clone(),
            persisted: true,
        });
        c.dispatch(Action::Rewritten {
            token: token.clone(),
            text: "处理结果".into(),
        });
        assert!(matches!(
            c.dispatch(Action::ProcessedCommitted {
                token: token.clone(),
                persisted: true
            })
            .1[0],
            Effect::PrepareDelivery { .. }
        ));
        assert!(matches!(
            c.dispatch(Action::DeliveryPrepared {
                token: token.clone(),
                safe: true
            })
            .1[0],
            Effect::MarkDeliveryAttempt { .. }
        ));
        assert!(matches!(
            c.dispatch(Action::DeliveryAttemptSaved {
                token: token.clone(),
                persisted: true
            })
            .1[0],
            Effect::Deliver { .. }
        ));
        assert_eq!(
            c.dispatch(Action::DeliveryAttemptSaved {
                token: token.clone(),
                persisted: true
            })
            .0,
            DispatchResult::Invalid
        );
        c.dispatch(Action::Delivered {
            token: token.clone(),
            status: DeliveryStatus::SentUnconfirmed,
        });
        assert_eq!(c.snapshot().phase, Phase::Result);
        assert_eq!(
            c.dispatch(Action::Delivered {
                token,
                status: DeliveryStatus::SentUnconfirmed
            })
            .0,
            DispatchResult::Invalid
        );
    }
    #[test]
    fn cancelling_before_raw_discards_late_asr_and_recovered_attempts_only_preview() {
        let mut c = Controller::new(ready());
        let token = begin(&mut c, StartSource::Shortcut);
        c.dispatch(Action::Stop { target: None });
        c.dispatch(Action::Cancel);
        assert_eq!(c.snapshot().phase, Phase::Ready);
        assert_eq!(
            c.dispatch(Action::RawReady {
                token,
                raw: "late".into(),
                now_ms: 100
            })
            .0,
            DispatchResult::Invalid
        );
        let record = Record {
            job_id: "recovered".into(),
            raw: "original".into(),
            processed: Some("result".into()),
            config: ready(),
            created_at_ms: 1,
            delivery: DeliveryStatus::Attempting,
            preview_required: false,
        };
        let effects = c
            .dispatch(Action::Restore {
                records: vec![record],
            })
            .1;
        assert!(effects.is_empty());
        assert_eq!(c.snapshot().history[0].delivery, DeliveryStatus::Unknown);
        assert!(c.snapshot().history[0].preview_required);
    }

    #[test]
    fn oversized_raw_is_retained_but_never_submitted_to_provider() {
        let mut c = Controller::new(ready());
        let token = begin(&mut c, StartSource::Shortcut);
        c.dispatch(Action::Stop { target: None });
        let long = "中文".repeat(2000);
        c.dispatch(Action::RawReady {
            token: token.clone(),
            raw: long.clone(),
            now_ms: 100,
        });
        let effects = c
            .dispatch(Action::RawCommitted {
                token: token.clone(),
                persisted: true,
            })
            .1;
        assert!(effects.is_empty());
        assert_eq!(c.snapshot().error.unwrap().code, "input_too_long");
        assert_eq!(c.snapshot().history[0].raw, long);
        assert_eq!(
            c.dispatch(Action::Retry {
                job_id: token.job_id,
                use_current_config: false
            })
            .0,
            DispatchResult::Invalid
        );
    }

    #[test]
    fn deleting_or_clearing_results_leaves_no_current_text_cache() {
        for all in [false, true] {
            let mut c = Controller::new(ready());
            let token = begin(&mut c, StartSource::Ui);
            raw(&mut c, &token);
            assert_eq!(
                c.dispatch(Action::Forget { job_id: None }).0,
                DispatchResult::Busy
            );
            c.dispatch(Action::RawCommitted {
                token: token.clone(),
                persisted: false,
            });
            let job_id = if all { None } else { Some(token.job_id) };
            c.dispatch(Action::Forget { job_id });
            assert!(c.snapshot().history.is_empty());
            assert!(c.snapshot().current.is_none());
            assert!(c.snapshot().error.is_none());
            assert_eq!(c.snapshot().phase, Phase::Ready);
        }
    }

    #[test]
    fn processed_commit_guard_and_attempt_failures_cannot_paste() {
        for failure in ["processed", "guard", "attempt"] {
            let mut c = Controller::new(ready());
            let token = begin(&mut c, StartSource::Shortcut);
            raw(&mut c, &token);
            c.dispatch(Action::RawCommitted {
                token: token.clone(),
                persisted: true,
            });
            c.dispatch(Action::Rewritten {
                token: token.clone(),
                text: "结果".into(),
            });
            let mut effects = c
                .dispatch(Action::ProcessedCommitted {
                    token: token.clone(),
                    persisted: failure != "processed",
                })
                .1;
            if failure != "processed" {
                effects.extend(
                    c.dispatch(Action::DeliveryPrepared {
                        token: token.clone(),
                        safe: failure != "guard",
                    })
                    .1,
                );
            }
            if failure == "attempt" {
                effects.extend(
                    c.dispatch(Action::DeliveryAttemptSaved {
                        token,
                        persisted: false,
                    })
                    .1,
                );
            }
            assert!(
                effects
                    .iter()
                    .all(|effect| !matches!(effect, Effect::Deliver { .. }))
            );
            let state = c.snapshot();
            assert_eq!(state.phase, Phase::Result);
            assert!(state.current.unwrap().raw.is_some());
        }
    }

    #[test]
    fn cancellation_after_native_dispatch_is_unknown_and_cannot_replay() {
        let mut c = Controller::new(ready());
        let token = begin(&mut c, StartSource::Shortcut);
        raw(&mut c, &token);
        c.dispatch(Action::RawCommitted {
            token: token.clone(),
            persisted: true,
        });
        c.dispatch(Action::Rewritten {
            token: token.clone(),
            text: "结果".into(),
        });
        c.dispatch(Action::ProcessedCommitted {
            token: token.clone(),
            persisted: true,
        });
        c.dispatch(Action::DeliveryPrepared {
            token: token.clone(),
            safe: true,
        });
        c.dispatch(Action::DeliveryAttemptSaved {
            token: token.clone(),
            persisted: true,
        });
        let effects = c.dispatch(Action::Cancel).1;
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::Deliver { .. }))
        );
        assert_eq!(
            c.snapshot().current.unwrap().delivery,
            DeliveryStatus::Unknown
        );
        assert_eq!(
            c.dispatch(Action::Delivered {
                token,
                status: DeliveryStatus::SentUnconfirmed
            })
            .0,
            DispatchResult::Invalid
        );
    }

    #[test]
    fn empty_asr_and_invalid_output_never_proceed_and_tick_removes_expired_text() {
        let mut c = Controller::new(ready());
        let token = begin(&mut c, StartSource::Ui);
        c.dispatch(Action::Stop { target: None });
        assert!(
            c.dispatch(Action::RawReady {
                token,
                raw: "  ".into(),
                now_ms: 100
            })
            .1
            .is_empty()
        );
        assert_eq!(c.snapshot().error.unwrap().code, "no_speech");
        for text in [
            String::new(),
            "bad\u{0001}text".into(),
            "x".repeat(12 * 1024 + 1),
        ] {
            let token = begin(&mut c, StartSource::Ui);
            raw(&mut c, &token);
            c.dispatch(Action::RawCommitted {
                token: token.clone(),
                persisted: true,
            });
            assert!(c.dispatch(Action::Rewritten { token, text }).1.is_empty());
            assert_eq!(c.snapshot().error.unwrap().code, "invalid_output");
            assert!(c.snapshot().current.unwrap().raw.is_some());
        }
        c.dispatch(Action::Tick {
            now_ms: 100 + 24 * 3_600_000,
        });
        assert!(c.snapshot().history.is_empty());
        assert!(c.snapshot().current.is_none());
    }
}
