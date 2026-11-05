use crate::{AppError, Config};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    NotAttempted,
    Preview,
    Attempting,
    SentUnconfirmed,
    Manual,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub job_id: String,
    pub raw: String,
    pub processed: Option<String>,
    pub config: Config,
    pub created_at_ms: i64,
    pub delivery: DeliveryStatus,
    pub preview_required: bool,
}

fn recovery_error() -> AppError {
    AppError::new(
        "recovery_unavailable",
        "recovery",
        true,
        "恢复存储不可用，请保留当前预览并检查存储空间和权限",
    )
}

pub(crate) fn create_private_directory(path: &Path) -> Result<(), AppError> {
    if path.exists() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|_| recovery_error())
}

/// Single-threaded store: put behind the controller's mutex when shared.
/// Callers supply wall-clock milliseconds for deterministic TTL and crash tests.
pub struct RecoveryStore {
    path: PathBuf,
    connection: Option<Connection>,
    memory: BTreeMap<String, Record>,
    max_entries: usize,
    ttl_ms: i64,
    unresolved_primary: bool,
}

impl RecoveryStore {
    /// Memory fallback retains the real primary destination without opening,
    /// clearing or replacing an unavailable database. Explicit persistence
    /// changes must still resolve that primary store instead of bypassing it.
    pub fn in_memory(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_owned(),
            connection: None,
            memory: BTreeMap::new(),
            max_entries: 5,
            ttl_ms: 24 * 60 * 60 * 1000,
            unresolved_primary: true,
        }
    }

    pub fn open(path: impl AsRef<Path>, persist: bool) -> Result<Self, AppError> {
        let path = path.as_ref().to_owned();
        let connection = if persist || path.exists() {
            Some(Self::open_connection(&path)?)
        } else {
            None
        };
        let mut store = Self {
            path,
            connection,
            memory: BTreeMap::new(),
            max_entries: 5,
            ttl_ms: 24 * 60 * 60 * 1000,
            unresolved_primary: false,
        };
        if persist {
            // An attempt has no consumption receipt. Never replay after restart.
            store
                .connection
                .as_ref()
                .unwrap()
                .execute(
                    "UPDATE records SET delivery='\"unknown\"' WHERE delivery='\"attempting\"'",
                    [],
                )
                .map_err(|_| recovery_error())?;
        } else {
            store.purge()?;
            store.connection = None;
        }
        Ok(store)
    }

    fn open_connection(path: &Path) -> Result<Connection, AppError> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        create_private_directory(parent)?;
        // Pre-create with private permissions before SQLite can write text.
        if !path.exists() {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path).map_err(|_| recovery_error())?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|_| recovery_error())?;
        }
        let connection = Connection::open(path).map_err(|_| recovery_error())?;
        connection
            .busy_timeout(std::time::Duration::from_millis(500))
            .map_err(|_| recovery_error())?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|_| recovery_error())?;
        if !(0..=1).contains(&version) {
            return Err(AppError::new(
                "unsupported_recovery_schema",
                "recovery",
                false,
                "恢复数据库版本较新，请使用兼容版本",
            ));
        }
        connection.execute_batch("PRAGMA secure_delete=ON; PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS records(job_id TEXT PRIMARY KEY NOT NULL,raw TEXT NOT NULL,processed TEXT,config TEXT NOT NULL,created_at_ms INTEGER NOT NULL,delivery TEXT NOT NULL); PRAGMA user_version=1;").map_err(|_| recovery_error())?;
        Ok(connection)
    }

    pub fn is_persistent(&self) -> bool {
        self.connection.is_some()
    }

    pub fn set_limits(&mut self, max_entries: usize, ttl_hours: u32) -> Result<(), AppError> {
        if !(1..=5).contains(&max_entries) || !(1..=24).contains(&ttl_hours) {
            return Err(AppError::new(
                "invalid_recovery_limits",
                "recovery",
                false,
                "恢复记录最多 5 条且保留最长 24 小时",
            ));
        }
        self.max_entries = max_entries;
        self.ttl_ms = i64::from(ttl_hours) * 60 * 60 * 1000;
        Ok(())
    }

    pub fn commit_raw(
        &mut self,
        job_id: &str,
        raw: &str,
        config: &Config,
        now_ms: i64,
    ) -> Result<Record, AppError> {
        config.validate()?;
        if job_id.is_empty() || job_id.len() > 128 || raw.trim().is_empty() {
            return Err(AppError::new(
                "invalid_raw_record",
                "recovery",
                false,
                "原文记录无效",
            ));
        }
        self.expire(now_ms)?;
        let record = Record {
            job_id: job_id.into(),
            raw: raw.into(),
            processed: None,
            config: config.clone(),
            created_at_ms: now_ms,
            delivery: DeliveryStatus::NotAttempted,
            preview_required: true,
        };
        if let Some(connection) = self.connection.as_mut() {
            let transaction = connection.transaction().map_err(|_| recovery_error())?;
            let exists: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM records WHERE job_id=?1)",
                    [job_id],
                    |row| row.get(0),
                )
                .map_err(|_| recovery_error())?;
            if exists {
                return Err(AppError::new(
                    "record_exists",
                    "recovery",
                    false,
                    "任务原文已提交，不能覆盖",
                ));
            }
            transaction.execute("INSERT INTO records(job_id,raw,processed,config,created_at_ms,delivery) VALUES(?1,?2,NULL,?3,?4,?5)", params![job_id, raw, encode(config)?, now_ms, encode(&record.delivery)?]).map_err(|_| recovery_error())?;
            transaction.execute("DELETE FROM records WHERE job_id NOT IN (SELECT job_id FROM records ORDER BY created_at_ms DESC,rowid DESC LIMIT ?1)", [self.max_entries as i64]).map_err(|_| recovery_error())?;
            transaction.commit().map_err(|_| recovery_error())?;
        } else {
            if self.memory.contains_key(job_id) {
                return Err(AppError::new(
                    "record_exists",
                    "recovery",
                    false,
                    "任务原文已提交，不能覆盖",
                ));
            }
            self.memory.insert(job_id.into(), record.clone());
            self.trim_memory();
        }
        Ok(record)
    }

    pub fn commit_processed(
        &mut self,
        job_id: &str,
        text: &str,
        config: &Config,
        now_ms: i64,
    ) -> Result<(), AppError> {
        config.validate()?;
        if text.trim().is_empty() {
            return Err(AppError::new(
                "invalid_processed_record",
                "recovery",
                false,
                "处理结果为空",
            ));
        }
        self.expire(now_ms)?;
        if let Some(connection) = self.connection.as_mut() {
            let transaction = connection.transaction().map_err(|_| recovery_error())?;
            let changed = transaction
                .execute(
                    "UPDATE records SET processed=?1,config=?2 WHERE job_id=?3",
                    params![text, encode(config)?, job_id],
                )
                .map_err(|_| recovery_error())?;
            if changed == 0 {
                return Err(missing_record());
            }
            transaction.commit().map_err(|_| recovery_error())?;
        } else {
            let record = self.memory.get_mut(job_id).ok_or_else(missing_record)?;
            record.processed = Some(text.into());
            record.config = config.clone();
        }
        Ok(())
    }

    pub fn mark_delivery(
        &mut self,
        job_id: &str,
        delivery: DeliveryStatus,
        now_ms: i64,
    ) -> Result<(), AppError> {
        self.expire(now_ms)?;
        if let Some(connection) = self.connection.as_mut() {
            let changed = connection
                .execute(
                    "UPDATE records SET delivery=?1 WHERE job_id=?2",
                    params![encode(&delivery)?, job_id],
                )
                .map_err(|_| recovery_error())?;
            if changed == 0 {
                return Err(missing_record());
            }
        } else {
            self.memory
                .get_mut(job_id)
                .ok_or_else(missing_record)?
                .delivery = delivery;
        }
        Ok(())
    }

    pub fn list(&mut self, now_ms: i64) -> Result<Vec<Record>, AppError> {
        self.expire(now_ms)?;
        let mut records = if let Some(connection) = self.connection.as_ref() {
            let mut statement = connection.prepare("SELECT job_id,raw,processed,config,created_at_ms,delivery FROM records ORDER BY created_at_ms DESC,rowid DESC LIMIT ?1").map_err(|_| recovery_error())?;
            let rows = statement
                .query_map([self.max_entries as i64], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .map_err(|_| recovery_error())?;
            let mut output = Vec::new();
            for row in rows {
                let (job_id, raw, processed, config, created_at_ms, delivery) =
                    row.map_err(|_| recovery_error())?;
                output.push(Record {
                    job_id,
                    raw,
                    processed,
                    config: decode(&config)?,
                    created_at_ms,
                    delivery: decode(&delivery)?,
                    preview_required: true,
                });
            }
            output
        } else {
            self.memory.values().cloned().collect()
        };
        records.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| b.job_id.cmp(&a.job_id))
        });
        records.truncate(self.max_entries);
        Ok(records)
    }

    pub fn delete(&mut self, job_id: &str) -> Result<(), AppError> {
        if let Some(connection) = self.connection.as_ref() {
            connection
                .execute("DELETE FROM records WHERE job_id=?1", [job_id])
                .map_err(|_| recovery_error())?;
            connection
                .execute_batch("VACUUM;")
                .map_err(|_| recovery_error())?;
        } else {
            self.memory.remove(job_id);
        }
        Ok(())
    }

    pub fn purge(&mut self) -> Result<(), AppError> {
        if self.unresolved_primary {
            let connection = Self::open_connection(&self.path)?;
            connection
                .execute_batch("PRAGMA secure_delete=ON; DELETE FROM records; VACUUM;")
                .map_err(|_| recovery_error())?;
            self.unresolved_primary = false;
        }
        if let Some(connection) = self.connection.as_ref() {
            connection
                .execute_batch("PRAGMA secure_delete=ON; DELETE FROM records; VACUUM;")
                .map_err(|_| recovery_error())?;
        }
        self.memory.clear();
        Ok(())
    }

    pub fn set_persist(&mut self, persist: bool) -> Result<(), AppError> {
        if !persist && self.unresolved_primary {
            self.purge()?;
            return Ok(());
        }
        if persist == self.is_persistent() {
            return Ok(());
        }
        if persist {
            let connection = Self::open_connection(&self.path)?;
            // Any pre-existing attempts are unknown, including files restored externally.
            connection
                .execute(
                    "UPDATE records SET delivery='\"unknown\"' WHERE delivery='\"attempting\"'",
                    [],
                )
                .map_err(|_| recovery_error())?;
            self.connection = Some(connection);
            self.memory.clear();
            self.unresolved_primary = false;
        } else {
            self.purge()?;
            self.connection = None;
        }
        Ok(())
    }

    fn expire(&mut self, now_ms: i64) -> Result<(), AppError> {
        let cutoff = now_ms.saturating_sub(self.ttl_ms);
        if let Some(connection) = self.connection.as_ref() {
            connection
                .execute("DELETE FROM records WHERE created_at_ms<=?1", [cutoff])
                .map_err(|_| recovery_error())?;
            connection.execute("DELETE FROM records WHERE job_id NOT IN (SELECT job_id FROM records ORDER BY created_at_ms DESC,rowid DESC LIMIT ?1)", [self.max_entries as i64]).map_err(|_| recovery_error())?;
        } else {
            self.memory
                .retain(|_, record| record.created_at_ms > cutoff);
            self.trim_memory();
        }
        Ok(())
    }

    fn trim_memory(&mut self) {
        while self.memory.len() > self.max_entries {
            let oldest = self
                .memory
                .values()
                .min_by(|a, b| {
                    a.created_at_ms
                        .cmp(&b.created_at_ms)
                        .then_with(|| a.job_id.cmp(&b.job_id))
                })
                .map(|r| r.job_id.clone());
            if let Some(id) = oldest {
                self.memory.remove(&id);
            }
        }
    }

    pub fn secure_delete_enabled(&self) -> Result<bool, AppError> {
        self.connection.as_ref().map_or(Ok(true), |connection| {
            connection
                .pragma_query_value(None, "secure_delete", |row| row.get::<_, i64>(0))
                .map(|v| v == 1)
                .map_err(|_| recovery_error())
        })
    }

    pub fn contains(&self, job_id: &str) -> Result<bool, AppError> {
        if let Some(connection) = self.connection.as_ref() {
            connection
                .query_row("SELECT 1 FROM records WHERE job_id=?1", [job_id], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()
                .map(|v| v.is_some())
                .map_err(|_| recovery_error())
        } else {
            Ok(self.memory.contains_key(job_id))
        }
    }
}

fn missing_record() -> AppError {
    AppError::new(
        "record_expired",
        "recovery",
        false,
        "原文记录不存在或已过期",
    )
}
fn encode<T: Serialize>(value: &T) -> Result<String, AppError> {
    serde_json::to_string(value).map_err(|_| recovery_error())
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, AppError> {
    serde_json::from_str(value).map_err(|_| recovery_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    const HOUR: i64 = 60 * 60 * 1000;
    #[test]
    fn crash_reopen_retains_raw_marks_attempt_unknown_and_never_replays() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recovery.sqlite3");
        let config = Config::default();
        {
            let mut store = RecoveryStore::open(&path, true).unwrap();
            assert!(store.secure_delete_enabled().unwrap());
            store
                .commit_raw("job", "original 原文", &config, 100)
                .unwrap();
            store
                .mark_delivery("job", DeliveryStatus::Attempting, 101)
                .unwrap();
        }
        let mut reopened = RecoveryStore::open(&path, true).unwrap();
        let records = reopened.list(102).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].raw, "original 原文");
        assert_eq!(records[0].delivery, DeliveryStatus::Unknown);
        assert!(records[0].preview_required);
    }
    #[test]
    fn memory_and_sqlite_share_retention_and_raw_clock() {
        for persist in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = RecoveryStore::open(dir.path().join("db"), persist).unwrap();
            let config = Config::default();
            for i in 0..6 {
                store
                    .commit_raw(&format!("job{i}"), "raw", &config, i)
                    .unwrap();
            }
            assert_eq!(store.list(6).unwrap().len(), 5);
            assert!(!store.contains("job0").unwrap());
            store
                .commit_processed("job5", "processed", &config, 23 * HOUR)
                .unwrap();
            assert_eq!(store.list(24 * HOUR + 5).unwrap().len(), 0); // Processing cannot extend TTL.
        }
    }
    #[test]
    fn disabling_persistence_and_starting_disabled_clear_existing_library() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let config = Config::default();
        let mut store = RecoveryStore::open(&path, true).unwrap();
        store
            .commit_raw("old", "sensitive synthetic", &config, 1)
            .unwrap();
        store.set_persist(false).unwrap();
        assert!(store.list(2).unwrap().is_empty());
        store
            .commit_raw("memory", "new transient", &config, 2)
            .unwrap();
        drop(store);
        let mut reopened = RecoveryStore::open(&path, true).unwrap();
        assert!(reopened.list(3).unwrap().is_empty());
        reopened
            .commit_raw("old", "clear on disabled startup", &config, 3)
            .unwrap();
        drop(reopened);
        let mut disabled = RecoveryStore::open(&path, false).unwrap();
        assert!(disabled.list(4).unwrap().is_empty());
        drop(disabled);
        assert!(
            RecoveryStore::open(&path, true)
                .unwrap()
                .list(4)
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn duplicate_raw_cannot_replace_text_or_reset_submission_time() {
        for persist in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = RecoveryStore::open(dir.path().join("db"), persist).unwrap();
            let config = Config::default();
            store.commit_raw("same", "original", &config, 1).unwrap();
            assert!(store.commit_raw("same", "replacement", &config, 2).is_err());
            let rows = store.list(2).unwrap();
            assert_eq!(rows[0].raw, "original");
            assert_eq!(rows[0].created_at_ms, 1);
        }
    }

    #[test]
    fn purge_removes_plaintext_from_database_and_new_schema_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let mut store = RecoveryStore::open(&path, true).unwrap();
        let marker = "SYNTHETIC-RAW-SHOULD-BE-PURGED-7421";
        store
            .commit_raw("job", marker, &Config::default(), 100)
            .unwrap();
        store.purge().unwrap();
        drop(store);
        assert!(
            !fs::read(&path)
                .unwrap()
                .windows(marker.len())
                .any(|bytes| bytes == marker.as_bytes())
        );
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);
        assert!(RecoveryStore::open(&path, true).is_err());
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            99
        );
    }

    #[test]
    fn memory_fallback_keeps_primary_failure_visible_for_enable_and_disable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recovery.sqlite3");
        let original = b"unavailable original database bytes";
        fs::write(&path, original).unwrap();
        let mut store = RecoveryStore::in_memory(&path);
        store
            .commit_raw(
                "memory",
                "raw survives unavailable recovery",
                &Config::default(),
                100,
            )
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(store.set_persist(true).is_err());
        assert!(store.set_persist(false).is_err());
        assert!(store.purge().is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            store.list(101).unwrap()[0].raw,
            "raw survives unavailable recovery"
        );
    }
}
