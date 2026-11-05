#![cfg(target_os = "linux")]
//! Real actor/config/recovery integration with isolated paths. These tests never
//! start capture, load a model, contact a remote provider, or access system keys.

use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use voice_core::{Config, DeliveryStatus, RecoveryStore};
use voice_platform::ipc::SessionSocket;
use voice_runtime::{
    credentials::CredentialStore,
    service::{Runtime, Snapshot, UiCommand},
};

struct Case {
    runtime: Runtime,
    data: PathBuf,
    _directory: tempfile::TempDir,
}

impl Drop for Case {
    fn drop(&mut self) {
        self.runtime.shutdown();
    }
}

impl Case {
    fn start(prepare: impl FnOnce(&Path)) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        let workers = directory.path().join("empty-workers");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&workers).unwrap();
        prepare(&data);
        let socket =
            SessionSocket::bind_path(&directory.path().join("private/control.sock")).unwrap();
        let runtime = Runtime::start(data.clone(), workers, socket, |_| {}).unwrap();
        let case = Self {
            runtime,
            data,
            _directory: directory,
        };
        let _ = case.snapshot();
        case
    }

    fn snapshot(&self) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.runtime.snapshot() {
                Ok(snapshot) => return snapshot,
                Err(error) => {
                    assert_eq!(error.code, "starting");
                    assert!(Instant::now() < deadline, "actor initialization timed out");
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    fn save(
        &self,
        config: Config,
        remote_confirmed: bool,
    ) -> Result<serde_json::Value, voice_core::AppError> {
        self.runtime.execute(UiCommand::SaveConfig {
            config: Box::new(config),
            remote_confirmed,
            key: None,
            session_only: true,
        })
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn unknown_configuration_schema_startup_never_overwrites_the_file() {
    let original = "schema_version = 99\n# preserve future configuration\n";
    let case = Case::start(|data| fs::write(data.join("config.toml"), original).unwrap());
    assert_eq!(
        case.snapshot().state.error.unwrap().code,
        "unsupported_schema"
    );
    assert!(!case.snapshot().ready);
    assert!(case.save(Config::default(), false).is_err());
    assert_eq!(
        fs::read_to_string(case.data.join("config.toml")).unwrap(),
        original
    );
}

#[test]
fn damaged_remote_confirmation_store_does_not_prevent_local_settings() {
    let original = b"damaged remote confirmation file";
    let case = Case::start(|data| {
        Config::default().save(data.join("config.toml")).unwrap();
        fs::write(data.join("confirmed-endpoints.json"), original).unwrap();
    });
    assert_eq!(case.snapshot().config.llm.mode, voice_core::LlmMode::Local);
    assert!(!case.snapshot().remote_confirmed);
    assert!(!case.snapshot().credential_present);
    assert!(!case.snapshot().detail.is_empty());
    let mut config = case.snapshot().config;
    config.audio.max_seconds = 17;
    case.save(config.clone(), false).unwrap();
    assert_eq!(Config::load(case.data.join("config.toml")).unwrap(), config);
    assert_eq!(
        fs::read(case.data.join("confirmed-endpoints.json")).unwrap(),
        original
    );
}

#[test]
fn saving_valid_settings_commits_disk_and_actor_snapshot_together() {
    let case = Case::start(|_| {});
    let mut config = case.snapshot().config;
    config.audio.max_seconds = 12;
    config.recovery.max_entries = 2;
    config.recovery.ttl_hours = 4;
    config.recovery.persist = false;
    case.save(config.clone(), false).unwrap();
    assert_eq!(case.snapshot().config, config);
    assert_eq!(Config::load(case.data.join("config.toml")).unwrap(), config);
    assert!(case.snapshot().recent.is_empty());
}

#[test]
fn failed_recovery_mode_change_does_not_leave_new_config_on_disk() {
    let case = Case::start(|data| {
        let mut config = Config::default();
        config.recovery.persist = false;
        config.save(data.join("config.toml")).unwrap();
    });
    let original = case.snapshot().config;
    // The disabled store has not opened this path. A corrupt target makes the
    // later enable operation fail without permissions tricks or global state.
    fs::write(case.data.join("recovery.sqlite3"), b"not a sqlite database").unwrap();
    let mut changed = original.clone();
    changed.recovery.persist = true;
    assert!(case.save(changed, false).is_err());
    assert_eq!(case.snapshot().config, original);
    assert_eq!(
        Config::load(case.data.join("config.toml")).unwrap(),
        original
    );
}

#[test]
fn failed_config_save_preserves_old_endpoint_confirmation_without_reading_keys() {
    let profile = "synthetic-confirmation-profile";
    let endpoint = "https://example.com/v1";
    let case = Case::start(|data| {
        let mut config = Config::default();
        // Local mode avoids even metadata lookups in the system credential
        // store. The saved remote draft is enough to exercise consent rollback.
        config.llm.remote.base_url = endpoint.into();
        config.llm.remote.profile_id = profile.into();
        config.llm.remote.model = "fixture-model".into();
        config.save(data.join("config.toml")).unwrap();
        CredentialStore::new(data.join("confirmed-endpoints.json"))
            .unwrap()
            .confirm_endpoint(profile, endpoint)
            .unwrap();
    });
    let config = case.snapshot().config;
    let replacement = "schema_version = 99\n# external future version\n";
    fs::write(case.data.join("config.toml"), replacement).unwrap();
    assert!(case.save(config.clone(), false).is_err());
    assert_eq!(case.snapshot().config, config);
    assert_eq!(
        fs::read_to_string(case.data.join("config.toml")).unwrap(),
        replacement
    );
    let credentials = CredentialStore::new(case.data.join("confirmed-endpoints.json")).unwrap();
    assert!(credentials.is_confirmed(profile, endpoint).unwrap());
}

#[test]
fn deleting_and_clearing_history_updates_actor_cache_and_database() {
    let case = Case::start(|data| {
        let config = Config::default();
        config.save(data.join("config.toml")).unwrap();
        let mut store = RecoveryStore::open(data.join("recovery.sqlite3"), true).unwrap();
        store
            .commit_raw("older", "synthetic older 原文", &config, now_ms() - 1)
            .unwrap();
        store
            .commit_raw("newer", "synthetic newer 原文", &config, now_ms())
            .unwrap();
        store
            .mark_delivery("newer", DeliveryStatus::Attempting, now_ms())
            .unwrap();
    });
    assert_eq!(case.snapshot().recent.len(), 2);
    assert_eq!(case.snapshot().recent[0].delivery, DeliveryStatus::Unknown);
    case.runtime
        .execute(UiCommand::DeleteResult {
            job_id: "older".into(),
        })
        .unwrap();
    assert_eq!(case.snapshot().recent.len(), 1);
    assert_eq!(case.snapshot().recent[0].job_id, "newer");
    case.runtime.execute(UiCommand::ClearHistory).unwrap();
    assert!(case.snapshot().recent.is_empty());
    assert!(case.snapshot().state.history.is_empty());
    assert!(case.snapshot().state.current.is_none());
    let mut store = RecoveryStore::open(case.data.join("recovery.sqlite3"), true).unwrap();
    assert!(store.list(now_ms()).unwrap().is_empty());
}
