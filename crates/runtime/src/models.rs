//! Immutable models: complete hash + header + real worker load before publication.
//! No user-supplied download endpoints and no partially published inventory.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};
use voice_core::{AppError, Config};

const MAX_MODEL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const DISK_RESERVE: u64 = 256 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 1024 * 1024;
const NETWORK_WAIT: Duration = Duration::from_secs(5);
const CANCEL_POLL: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub engine: String,
    pub bytes: u64,
    pub sha256: String,
    pub source: String,
    pub license: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub compatibility: String,
}
#[derive(Clone)]
pub struct ModelRecord {
    pub info: ModelInfo,
    pub path: PathBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredModel {
    info: ModelInfo,
    filename: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct DownloadInfo {
    pub id: String,
    pub filename: String,
    pub bytes: u64,
    pub sha256: String,
    pub source: String,
    pub license: String,
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub compatibility: String,
}
#[derive(Deserialize)]
struct Catalog {
    models: Vec<DownloadInfo>,
}

fn failure(code: &str, message: &str) -> AppError {
    AppError::new(code, "models", true, message)
}
pub fn catalog() -> Vec<DownloadInfo> {
    serde_json::from_str::<Catalog>(include_str!("../../../models.lock.json"))
        .expect("bundled model catalog")
        .models
}
/// Startup facts only: available memory can change before the next task.
/// The supervisor performs its own live resource check before loading a model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HardwareFacts {
    pub architecture: String,
    pub logical_cpus: Option<usize>,
    pub total_memory_bytes: Option<u64>,
    pub available_memory_bytes: Option<u64>,
    pub available_disk_bytes: Option<u64>,
    pub available_backends: Vec<String>,
    pub memory_source: String,
    pub gpu_status: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRecommendation {
    pub engine: String,
    pub model_id: Option<String>,
    pub reason: String,
    pub estimated_memory_bytes: Option<u64>,
    pub estimate_source: String,
}
const GIB: u64 = 1024 * 1024 * 1024;
const RECOMMENDATION_ESTIMATE: &str = "粗略预算：权重×2，加转写 1 GiB 或整理 1.5 GiB 的运行与系统余量；整理按默认 4096 上下文参考，实际加载校验优先";
#[cfg(any(target_os = "linux", test))]
fn memory_value(contents: &str, name: &str) -> Option<u64> {
    let line = contents.lines().find_map(|line| line.strip_prefix(name))?;
    let mut fields = line.split_whitespace();
    let bytes = fields.next()?.parse::<u64>().ok()?.checked_mul(1024)?;
    if fields.next()? != "kB" || fields.next().is_some() {
        return None;
    }
    Some(bytes)
}
#[cfg(any(target_os = "linux", test))]
fn memory_facts(contents: &str) -> (Option<u64>, Option<u64>) {
    let total = memory_value(contents, "MemTotal:");
    let mut available = memory_value(contents, "MemAvailable:");
    if total
        .zip(available)
        .is_some_and(|(total, available)| available > total)
    {
        available = None;
    }
    (total, available)
}
/// Reads only local CPU/memory/filesystem facts. No network, model download,
/// GPU probing, worker spawning, or setting/remote-mode mutation occurs here.
pub fn probe_hardware(model_directory: &Path) -> HardwareFacts {
    #[cfg(target_os = "linux")]
    let (total_memory_bytes, available_memory_bytes) = fs::read_to_string("/proc/meminfo")
        .ok()
        .map_or((None, None), |contents| memory_facts(&contents));
    #[cfg(not(target_os = "linux"))]
    let (total_memory_bytes, available_memory_bytes) = (None, None);
    HardwareFacts {
        architecture: std::env::consts::ARCH.into(),
        logical_cpus: thread::available_parallelism().ok().map(usize::from),
        total_memory_bytes,
        available_memory_bytes,
        available_disk_bytes: fs2::available_space(model_directory).ok(),
        available_backends: vec!["cpu".into()],
        memory_source: if cfg!(target_os = "linux") {
            "Linux /proc/meminfo 的 MemTotal / MemAvailable"
        } else {
            "此平台尚未接入内存探测，数值保持未知"
        }
        .into(),
        gpu_status: "未探测；当前构建仅启用 CPU".into(),
    }
}
fn recommendation_budget(model: &ModelInfo) -> Option<u64> {
    let reserve = if model.engine == "gguf" {
        GIB + GIB / 2
    } else {
        GIB
    };
    model.bytes.checked_mul(2)?.checked_add(reserve)
}
fn fitting_model<'a>(
    inventory: &'a [ModelInfo],
    engine: &str,
    available: Option<u64>,
) -> Option<(&'a ModelInfo, u64)> {
    let available = available?;
    inventory
        .iter()
        .filter(|model| model.engine == engine)
        .filter_map(|model| {
            recommendation_budget(model)
                .filter(|budget| *budget <= available)
                .map(|budget| (model, budget))
        })
        .min_by_key(|(model, _)| model.bytes)
}
/// Advice only. Candidates come from the already-verified inventory, and a
/// suggestion never selects a model, enables remote access, or changes Config.
pub fn recommend(facts: &HardwareFacts, inventory: &[ModelInfo]) -> Vec<ModelRecommendation> {
    let asr = match fitting_model(inventory,"whisper",facts.available_memory_bytes) {
        Some((model,budget)) => ModelRecommendation { engine:"whisper".into(),model_id:Some(model.id.clone()),
            reason:format!("按启动时可用内存，优先考虑较小的已验证转写模型「{}」；语种仍以模型元信息为准。",model.name),
            estimated_memory_bytes:Some(budget),estimate_source:RECOMMENDATION_ESTIMATE.into() },
        None => ModelRecommendation { engine:"whisper".into(),model_id:None,
            reason:if facts.available_memory_bytes.is_none(){"可用内存未知，暂不推荐具体模型；可手动导入较小的多语 Tiny/Base，并以实际加载结果为准。"}
                else{"没有符合启动时内存预算的已验证转写模型；可考虑较小的多语 Tiny/Base，关闭占用资源的程序后再验证。"}.into(),
            estimated_memory_bytes:None,estimate_source:RECOMMENDATION_ESTIMATE.into() },
    };
    let low_or_unknown = facts
        .total_memory_bytes
        .is_none_or(|total| total <= 8 * GIB)
        || facts
            .available_memory_bytes
            .is_none_or(|available| available < 3 * GIB);
    let local = if low_or_unknown {
        None
    } else {
        fitting_model(inventory, "gguf", facts.available_memory_bytes)
    };
    let llm = match local {
        Some((model,budget)) => ModelRecommendation { engine:"gguf".into(),model_id:Some(model.id.clone()),
            reason:format!("按启动时可用内存，可考虑较小的已验证本地整理模型「{}」；较大上下文会增加内存占用。",model.name),
            estimated_memory_bytes:Some(budget),estimate_source:RECOMMENDATION_ESTIMATE.into() },
        None => ModelRecommendation { engine:"remote".into(),model_id:None,
            reason:if facts.total_memory_bytes.is_none()||facts.available_memory_bytes.is_none(){"内存数据不完整，暂不推荐本地整理模型。可手动配置远程整理，或先验证较小的本地模型；不会自动切换模式。"}
                else if low_or_unknown{"总内存不超过 8 GiB 或启动时可用内存较少，建议考虑远程整理以减少本机占用，或手动验证较小模型；使用远程仍需自行配置并确认发送内容。"}
                else{"当前没有符合启动时预算的已验证本地整理模型。可考虑较小的 1–3B Q4 模型或手动配置远程整理；不会自动下载或切换模式。"}.into(),
            estimated_memory_bytes:None,estimate_source:RECOMMENDATION_ESTIMATE.into() },
    };
    vec![asr, llm]
}

fn populate_model_metadata(info: &mut ModelInfo, verified_now: bool) {
    // Only already-verified bytes (hash + size) can identify catalog metadata.
    // Keep IDs stable so old configurations continue to reference the same file.
    if let Some(known) = catalog()
        .into_iter()
        .find(|model| model.sha256 == info.sha256 && model.bytes == info.bytes)
    {
        info.version = known.revision;
        info.language = known.language;
        info.compatibility = known.compatibility;
    }
    if info.version.is_empty() {
        info.version = format!("sha256:{}", info.sha256);
    }
    if info.language.is_empty() {
        info.language = "未声明".into();
    }
    if info.compatibility.is_empty() {
        info.compatibility = if verified_now {
            "本机 CPU 加载已验证；其他平台或后端未验证"
        } else {
            "旧索引未记录兼容范围；请以当前构建实际加载验证为准"
        }
        .into();
    }
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_filename(filename: &str) -> bool {
    let mut components = Path::new(filename).components();
    matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !filename.starts_with('.')
        && !filename.contains(['/', '\\', ':'])
}
fn valid_info(info: &ModelInfo) -> bool {
    !info.id.is_empty()
        && info.id.len() <= 128
        && info
            .id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        && info.name.len() <= 512
        && info.source.len() <= 4096
        && info.license.len() <= 1024
        && info.version.len() <= 128
        && info.language.len() <= 512
        && info.compatibility.len() <= 4096
        && matches!(info.engine.as_str(), "whisper" | "gguf")
        && (1024..=MAX_MODEL_BYTES).contains(&info.bytes)
        && valid_hash(&info.sha256)
}
fn check_space(available: u64, required: u64) -> Result<(), AppError> {
    if required
        .checked_add(DISK_RESERVE)
        .is_none_or(|needed| available < needed)
    {
        return Err(failure("model_disk", "磁盘空间不足以安全处理模型"));
    }
    Ok(())
}
fn check_cancel(cancelled: &AtomicBool) -> Result<(), AppError> {
    if cancelled.load(Ordering::Acquire) {
        Err(failure("cancelled", "模型下载已取消"))
    } else {
        Ok(())
    }
}
fn format_matches(path: &Path, engine: &str) -> Result<(), AppError> {
    let mut magic = [0; 4];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map_err(|_| failure("model_format", "模型头部不可读"))?;
    if !matches!(
        (engine, magic),
        ("gguf", [b'G', b'G', b'U', b'F']) | ("whisper", [b'l', b'm', b'g', b'g'])
    ) {
        return Err(failure("model_format", "模型格式与所选引擎不匹配"));
    }
    Ok(())
}
fn hash_file(path: &Path, expected_size: Option<u64>) -> Result<(u64, String), AppError> {
    hash_file_checked(path, expected_size, None)
}
fn hash_file_checked(
    path: &Path,
    expected_size: Option<u64>,
    cancelled: Option<&AtomicBool>,
) -> Result<(u64, String), AppError> {
    if let Some(cancelled) = cancelled {
        check_cancel(cancelled)?;
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|_| failure("model_read", "无法读取模型文件"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(failure("model_read", "模型必须是目录内的普通文件"));
    }
    let mut input = fs::File::open(path).map_err(|_| failure("model_read", "无法读取模型文件"))?;
    let size = input
        .metadata()
        .map_err(|_| failure("model_read", "无法读取模型文件"))?
        .len();
    if !(1024..=MAX_MODEL_BYTES).contains(&size) || expected_size.is_some_and(|n| n != size) {
        return Err(failure("model_size", "模型大小不符或超限"));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65536];
    let mut read = 0_u64;
    loop {
        if let Some(cancelled) = cancelled {
            check_cancel(cancelled)?;
        }
        let count = input
            .read(&mut buffer)
            .map_err(|_| failure("model_read", "无法完整读取模型"))?;
        if count == 0 {
            break;
        }
        read += count as u64;
        if read > size {
            return Err(failure("model_changed", "模型在校验时发生变化"));
        }
        hasher.update(&buffer[..count]);
    }
    if read != size {
        return Err(failure("model_changed", "模型在校验时发生变化"));
    }
    Ok((size, format!("{:x}", hasher.finalize())))
}

fn read_index(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failure("model_index", "模型目录不可读")),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_INDEX_BYTES
    {
        return Err(failure(
            "model_index",
            "模型目录文件无效或超限，原文件已保留",
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(MAX_INDEX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| failure("model_index", "模型目录不可读"))?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        return Err(failure("model_index", "模型目录超限"));
    }
    Ok(Some(bytes))
}
fn revision(bytes: Option<&[u8]>) -> Option<[u8; 32]> {
    bytes.map(|b| Sha256::digest(b).into())
}
fn mutation_lock(directory: &Path) -> Result<fs::File, AppError> {
    let path = directory.join(".model-index.lock");
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(failure("model_index", "模型目录锁文件无效"));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|_| failure("model_index", "无法锁定模型目录"))?;
    fs2::FileExt::try_lock_exclusive(&file)
        .map_err(|_| failure("model_index_busy", "其他操作正在更新模型目录，请稍后重试"))?;
    Ok(file)
}
fn staged_file(directory: &Path) -> Result<tempfile::NamedTempFile, AppError> {
    // Creation and lock acquisition share the mutation lock with crash cleanup.
    let _directory_lock = mutation_lock(directory)?;
    let staged = tempfile::Builder::new()
        .prefix(".voice-model-")
        .suffix(".part")
        .tempfile_in(directory)
        .map_err(|_| failure("model_copy", "无法准备模型临时文件"))?;
    fs2::FileExt::try_lock_exclusive(staged.as_file())
        .map_err(|_| failure("model_copy", "无法保护模型临时文件"))?;
    Ok(staged)
}
fn cleanup_interrupted(directory: &Path) -> Result<(), AppError> {
    let _directory_lock = mutation_lock(directory)?;
    for entry in
        fs::read_dir(directory).map_err(|_| failure("model_directory", "无法检查未完成模型"))?
    {
        let entry = entry.map_err(|_| failure("model_directory", "无法检查未完成模型"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(".voice-model-") || !name.ends_with(".part") {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| failure("model_directory", "无法检查未完成模型"))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|_| failure("model_directory", "无法清理未完成模型"))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => fs::remove_file(&path)
                .map_err(|_| failure("model_directory", "无法清理未完成模型"))?,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                return Err(failure(
                    "model_directory",
                    "无法确认模型临时文件是否仍在使用",
                ));
            }
        }
    }
    Ok(())
}
struct SaveFailure {
    error: AppError,
    committed: bool,
}

pub struct ModelManager {
    directory: PathBuf,
    entries: Vec<Arc<ModelRecord>>,
    index_revision: Option<[u8; 32]>,
    blocking_error: Option<AppError>,
}
impl ModelManager {
    pub fn open(directory: PathBuf) -> Result<Self, AppError> {
        if let Ok(metadata) = fs::symlink_metadata(&directory)
            && (!metadata.is_dir() || metadata.file_type().is_symlink())
        {
            return Err(failure(
                "model_directory",
                "模型目录不能是符号链接或普通文件",
            ));
        }
        fs::create_dir_all(&directory)
            .map_err(|_| failure("model_directory", "无法创建模型目录"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .map_err(|_| failure("model_directory", "无法保护模型目录"))?;
        }
        let bytes = read_index(&directory.join("index.json"))?;
        let mut entries = Vec::new();
        if let Some(bytes) = bytes.as_ref() {
            let stored: Vec<StoredModel> = serde_json::from_slice(bytes)
                .map_err(|_| failure("model_index", "模型目录损坏，原文件已保留"))?;
            let (mut ids, mut filenames) = (HashSet::new(), HashSet::new());
            for mut entry in stored {
                if !valid_filename(&entry.filename)
                    || !valid_info(&entry.info)
                    || !ids.insert(entry.info.id.clone())
                    || !filenames.insert(entry.filename.clone())
                {
                    return Err(failure("model_index", "模型目录包含重复或无效条目"));
                }
                let path = directory.join(entry.filename);
                let (size, hash) = hash_file(&path, Some(entry.info.bytes))?;
                if hash != entry.info.sha256 || size != entry.info.bytes {
                    return Err(failure("model_checksum", "已导入模型校验失败"));
                }
                format_matches(&path, &entry.info.engine)?;
                populate_model_metadata(&mut entry.info, false);
                entries.push(Arc::new(ModelRecord {
                    info: entry.info,
                    path,
                }));
            }
        }
        cleanup_interrupted(&directory)?;
        Ok(Self {
            directory,
            entries,
            index_revision: revision(bytes.as_deref()),
            blocking_error: None,
        })
    }
    /// Strict validation with a diagnosable, read-only fallback for damaged
    /// index/entry data. Unsafe or inaccessible directories still return Err.
    /// Degraded startup does not repair, clean up, replace or delete files.
    pub fn open_for_runtime(directory: PathBuf) -> Result<(Self, Option<AppError>), AppError> {
        match Self::open(directory.clone()) {
            Ok(manager) => Ok((manager, None)),
            Err(mut error) => {
                if !matches!(
                    error.code.as_str(),
                    "model_index"
                        | "model_checksum"
                        | "model_read"
                        | "model_size"
                        | "model_changed"
                        | "model_format"
                ) {
                    return Err(error);
                }
                // Strict open already created/protected the directory. Recheck
                // independently before interpreting a failure as damaged data.
                let metadata = fs::symlink_metadata(&directory)
                    .map_err(|_| failure("model_directory", "模型目录不可访问"))?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(failure("model_directory", "模型目录不可访问或不安全"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o777 != 0o700 {
                        return Err(failure("model_directory", "模型目录权限无法确认"));
                    }
                }
                fs::read_dir(&directory)
                    .map_err(|_| failure("model_directory", "模型目录不可访问"))?;
                error.retryable = false;
                error.message = format!(
                    "{}；模型目录已进入只读模式，原文件已保留。请修复索引或模型后重启应用，本次运行禁止导入、下载和删除",
                    error.message
                );
                Ok((
                    Self {
                        directory,
                        entries: Vec::new(),
                        index_revision: None,
                        blocking_error: Some(error.clone()),
                    },
                    Some(error),
                ))
            }
        }
    }
    pub fn blocking_error(&self) -> Option<&AppError> {
        self.blocking_error.as_ref()
    }
    fn ensure_writable(&self) -> Result<(), AppError> {
        match &self.blocking_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
    pub fn inventory(&self) -> Vec<ModelInfo> {
        self.entries.iter().map(|m| m.info.clone()).collect()
    }
    pub fn resolve(&self, id: &str, engine: &str) -> Result<Arc<ModelRecord>, AppError> {
        self.ensure_writable()?;
        self.entries
            .iter()
            .find(|m| m.info.id == id && m.info.engine == engine)
            .cloned()
            .ok_or_else(|| failure("model_missing", "请选择已验证的模型"))
    }
    fn save(&mut self) -> Result<(), SaveFailure> {
        self.ensure_writable().map_err(|error| SaveFailure {
            error,
            committed: false,
        })?;
        let before_commit = |error| SaveFailure {
            error,
            committed: false,
        };
        let current = read_index(&self.directory.join("index.json")).map_err(before_commit)?;
        if revision(current.as_deref()) != self.index_revision {
            return Err(before_commit(failure(
                "model_index_changed",
                "模型目录已被其他操作修改，请重新打开",
            )));
        }
        let records: Vec<_> = self
            .entries
            .iter()
            .map(|m| StoredModel {
                info: m.info.clone(),
                filename: m
                    .path
                    .file_name()
                    .expect("internal model filename")
                    .to_string_lossy()
                    .into_owned(),
            })
            .collect();
        let bytes = serde_json::to_vec_pretty(&records)
            .map_err(|_| before_commit(failure("model_index", "无法编码模型目录")))?;
        if bytes.len() as u64 > MAX_INDEX_BYTES {
            return Err(before_commit(failure("model_index", "模型目录条目超限")));
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)
            .map_err(|_| before_commit(failure("model_index", "无法保存模型目录")))?;
        file.write_all(&bytes)
            .and_then(|_| file.as_file().sync_all())
            .map_err(|_| before_commit(failure("model_index", "无法刷盘模型目录")))?;
        file.persist(self.directory.join("index.json"))
            .map_err(|_| before_commit(failure("model_index", "无法原子提交模型目录")))?;
        // The index rename has committed. A directory-fsync failure must never
        // trigger removal of model bytes that this new index already references.
        self.index_revision = revision(Some(&bytes));
        #[cfg(unix)]
        fs::File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|_| SaveFailure {
                error: failure(
                    "model_index_durability",
                    "目录已提交但刷盘未确认，请检查模型列表",
                ),
                committed: true,
            })?;
        Ok(())
    }
    pub fn import(
        &mut self,
        source: &Path,
        engine: &str,
        verify: impl FnOnce(&Path) -> Result<(), AppError>,
    ) -> Result<ModelInfo, AppError> {
        self.import_cancellable(source, engine, &AtomicBool::new(false), verify)
    }
    pub fn import_cancellable(
        &mut self,
        source: &Path,
        engine: &str,
        cancelled: &AtomicBool,
        verify: impl FnOnce(&Path) -> Result<(), AppError>,
    ) -> Result<ModelInfo, AppError> {
        self.ensure_writable()?;
        check_cancel(cancelled)?;
        if !matches!(engine, "whisper" | "gguf") {
            return Err(failure("model_engine", "模型引擎无效"));
        }
        let mut input =
            fs::File::open(source).map_err(|_| failure("model_read", "无法读取所选模型"))?;
        let metadata = input
            .metadata()
            .map_err(|_| failure("model_read", "无法读取所选模型"))?;
        if !metadata.is_file() || !(1024..=MAX_MODEL_BYTES).contains(&metadata.len()) {
            return Err(failure("model_size", "所选模型不是受支持的单文件模型"));
        }
        check_space(
            fs2::available_space(&self.directory)
                .map_err(|_| failure("model_disk", "无法检查可用磁盘"))?,
            metadata.len(),
        )?;
        let mut staged = staged_file(&self.directory)?;
        let mut copied = 0_u64;
        let mut buffer = [0; 65536];
        loop {
            check_cancel(cancelled)?;
            let count = input
                .read(&mut buffer)
                .map_err(|_| failure("model_read", "模型读取失败"))?;
            if count == 0 {
                break;
            }
            copied += count as u64;
            if copied > metadata.len() {
                return Err(failure("model_changed", "模型在导入时发生变化"));
            }
            staged
                .write_all(&buffer[..count])
                .map_err(|_| failure("model_copy", "模型导入失败"))?;
        }
        let after = input
            .metadata()
            .map_err(|_| failure("model_read", "无法读取模型状态"))?;
        if copied != metadata.len()
            || after.len() != metadata.len()
            || after.modified().ok() != metadata.modified().ok()
        {
            return Err(failure("model_changed", "模型在导入时发生变化"));
        }
        self.publish_staged(staged, engine, None, Some(cancelled), verify)
    }
    fn publish_staged(
        &mut self,
        staged: tempfile::NamedTempFile,
        engine: &str,
        expected: Option<&DownloadInfo>,
        cancelled: Option<&AtomicBool>,
        verify: impl FnOnce(&Path) -> Result<(), AppError>,
    ) -> Result<ModelInfo, AppError> {
        self.ensure_writable()?;
        staged
            .as_file()
            .sync_all()
            .map_err(|_| failure("model_copy", "模型刷盘失败"))?;
        let (size, hash) =
            hash_file_checked(staged.path(), expected.map(|entry| entry.bytes), cancelled)?;
        if expected.is_some_and(|entry| hash != entry.sha256) {
            return Err(failure("model_checksum", "下载模型校验失败，未激活"));
        }
        format_matches(staged.path(), engine)?;
        verify(staged.path())?;
        let (_, after_verify) = hash_file_checked(staged.path(), Some(size), cancelled)?;
        if after_verify != hash {
            return Err(failure("model_changed", "模型在加载验证期间发生变化"));
        }
        if let Some(cancelled) = cancelled {
            check_cancel(cancelled)?;
        }
        let _mutation_lock = mutation_lock(&self.directory)?;
        let current = read_index(&self.directory.join("index.json"))?;
        if revision(current.as_deref()) != self.index_revision {
            return Err(failure(
                "model_index_changed",
                "模型目录已被其他操作修改，请重新打开",
            ));
        }
        if let Some(existing) = self
            .entries
            .iter()
            .find(|m| m.info.sha256 == hash && m.info.engine == engine)
        {
            // A previous real-load verification is not proof the indexed bytes
            // still exist unchanged. Resolve duplicates only after full rehash.
            let (existing_size, existing_hash) = hash_file(&existing.path, Some(size))?;
            if existing_size != size || existing_hash != hash {
                return Err(failure("model_checksum", "已存在模型校验失败"));
            }
            return Ok(existing.info.clone());
        }
        let known = catalog()
            .into_iter()
            .find(|c| c.sha256 == hash && c.bytes == size);
        let mut info = ModelInfo {
            id: known
                .as_ref()
                .map_or_else(|| format!("import-{hash}"), |c| c.id.clone()),
            name: known
                .as_ref()
                .map_or_else(|| format!("导入模型 {}", &hash[..8]), |c| c.id.clone()),
            engine: engine.into(),
            bytes: size,
            sha256: hash.clone(),
            source: known
                .as_ref()
                .map_or_else(|| "用户本地导入".into(), |c| c.source.clone()),
            license: known
                .as_ref()
                .map_or_else(|| "用户提供，许可待用户确认".into(), |c| c.license.clone()),
            version: String::new(),
            language: String::new(),
            compatibility: String::new(),
        };
        populate_model_metadata(&mut info, true);
        if self.entries.iter().any(|m| m.info.id == info.id) {
            return Err(failure("model_index", "模型标识冲突"));
        }
        let path = self.directory.join(format!(
            "{hash}.{}",
            if engine == "gguf" { "gguf" } else { "bin" }
        ));
        // Never overwrite an existing immutable model. A crash orphan can only
        // be adopted if its complete bytes match the verified staging file.
        let created = match fs::symlink_metadata(&path) {
            Ok(_) => {
                let (old_size, old_hash) = hash_file(&path, Some(size))
                    .map_err(|_| failure("model_publish", "发布位置已存在无效文件，未覆盖"))?;
                if old_size != size || old_hash != hash {
                    return Err(failure("model_publish", "发布位置存在不同文件，未覆盖"));
                }
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                staged
                    .persist_noclobber(&path)
                    .map_err(|_| failure("model_publish", "无法发布已校验的模型，未覆盖旧文件"))?;
                true
            }
            Err(_) => return Err(failure("model_publish", "无法访问模型发布位置")),
        };
        self.entries.push(Arc::new(ModelRecord {
            info: info.clone(),
            path: path.clone(),
        }));
        if let Err(error) = self.save() {
            if !error.committed {
                self.entries.pop();
                if created {
                    let _ = fs::remove_file(path);
                }
            }
            return Err(error.error);
        }
        Ok(info)
    }
    pub fn delete(&mut self, id: &str, config: &Config) -> Result<(), AppError> {
        self.ensure_writable()?;
        let _mutation_lock = mutation_lock(&self.directory)?;
        if config.asr.model_id == id || config.llm.local.model_id == id {
            return Err(failure("model_selected", "不能删除当前选用的模型"));
        }
        let index = self
            .entries
            .iter()
            .position(|m| m.info.id == id)
            .ok_or_else(|| failure("model_missing", "模型不存在"))?;
        if Arc::strong_count(&self.entries[index]) > 1 {
            return Err(failure("model_in_use", "模型正在运行，暂不能删除"));
        }
        let model = self.entries.remove(index);
        if let Err(error) = self.save() {
            if !error.committed {
                self.entries.insert(index, model);
            }
            return Err(error.error);
        }
        fs::remove_file(&model.path)
            .map_err(|_| failure("model_delete", "目录已更新，但模型文件删除失败"))?;
        Ok(())
    }
    pub fn download(
        &mut self,
        id: &str,
        cancelled: &AtomicBool,
        verify: impl FnOnce(&Path) -> Result<(), AppError>,
    ) -> Result<ModelInfo, AppError> {
        self.ensure_writable()?;
        check_cancel(cancelled)?;
        let entry = catalog()
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| failure("model_source", "模型不在内置下载清单中"))?;
        if !(1024..=MAX_MODEL_BYTES).contains(&entry.bytes)
            || !valid_hash(&entry.sha256)
            || !approved_source(&entry.source, false)
        {
            return Err(failure("model_source", "内置模型清单条目无效"));
        }
        check_space(
            fs2::available_space(&self.directory)
                .map_err(|_| failure("model_disk", "无法检查磁盘空间"))?,
            entry.bytes,
        )?;
        let mut part = staged_file(&self.directory)?;
        let receiver = start_download(entry.source.clone(), entry.bytes);
        receive_download(&receiver, &mut part, entry.bytes, cancelled)?;
        check_cancel(cancelled)?;
        let engine = if entry.filename.ends_with(".gguf") {
            "gguf"
        } else {
            "whisper"
        };
        self.publish_staged(part, engine, Some(&entry), Some(cancelled), |path| {
            check_cancel(cancelled)?;
            verify(path)?;
            check_cancel(cancelled)
        })
    }
}

fn approved_source(source: &str, signed_redirect: bool) -> bool {
    let Ok(url) = url::Url::parse(source) else {
        return false;
    };
    let host = url.host_str().unwrap_or("");
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.port_or_known_default() == Some(443)
        && (signed_redirect || url.query().is_none())
        && ["huggingface.co", "hf.co"]
            .iter()
            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}
enum DownloadEvent {
    Chunk(Vec<u8>),
    Finished,
}
fn start_download(source: String, expected_size: u64) -> Receiver<Result<DownloadEvent, AppError>> {
    let (sender, receiver) = mpsc::sync_channel(2);
    thread::spawn(move || {
        let result = (|| {
            let policy = reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() > 5 || !approved_source(attempt.url().as_str(), true) {
                    attempt.error("untrusted model redirect")
                } else {
                    attempt.follow()
                }
            });
            // blocking reqwest applies this wait limit to headers and each
            // Response::read. A stalled network worker cannot linger 1800 s.
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(policy)
                .connect_timeout(NETWORK_WAIT)
                .timeout(NETWORK_WAIT)
                .build()
                .map_err(|_| failure("model_network", "无法初始化模型下载"))?;
            let mut response = client
                .get(source)
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .send()
                .map_err(|_| failure("model_network", "模型下载失败，可主动重试"))?
                .error_for_status()
                .map_err(|_| failure("model_http", "模型服务器拒绝下载"))?;
            if response
                .content_length()
                .is_some_and(|n| n != expected_size)
            {
                return Err(failure("model_size", "服务器模型大小不符"));
            }
            let mut written = 0_u64;
            loop {
                let mut buffer = vec![0; 65536];
                let count = response
                    .read(&mut buffer)
                    .map_err(|_| failure("model_download", "模型下载中断，未完成文件已清理"))?;
                if count == 0 {
                    break;
                }
                written += count as u64;
                if written > expected_size {
                    return Err(failure("model_size", "下载模型超过清单大小"));
                }
                buffer.truncate(count);
                if sender.send(Ok(DownloadEvent::Chunk(buffer))).is_err() {
                    return Ok(());
                }
            }
            if written != expected_size {
                return Err(failure("model_size", "下载模型长度与清单不符"));
            }
            let _ = sender.send(Ok(DownloadEvent::Finished));
            Ok(())
        })();
        if let Err(error) = result {
            let _ = sender.send(Err(error));
        }
    });
    receiver
}
fn receive_download(
    receiver: &Receiver<Result<DownloadEvent, AppError>>,
    output: &mut impl Write,
    expected_size: u64,
    cancelled: &AtomicBool,
) -> Result<(), AppError> {
    let deadline = Instant::now() + Duration::from_secs(1800);
    let mut written = 0_u64;
    loop {
        check_cancel(cancelled)?;
        if Instant::now() >= deadline {
            return Err(failure("model_timeout", "模型下载超时，未完成文件已清理"));
        }
        match receiver.recv_timeout(CANCEL_POLL) {
            Ok(Ok(DownloadEvent::Chunk(bytes))) => {
                written = written
                    .checked_add(bytes.len() as u64)
                    .filter(|n| *n <= expected_size)
                    .ok_or_else(|| failure("model_size", "下载模型超过清单大小"))?;
                output
                    .write_all(&bytes)
                    .map_err(|_| failure("model_disk", "写入模型失败"))?;
            }
            Ok(Ok(DownloadEvent::Finished)) if written == expected_size => return Ok(()),
            Ok(Ok(DownloadEvent::Finished)) => {
                return Err(failure("model_size", "下载模型长度与清单不符"));
            }
            Ok(Err(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(failure("model_download", "模型下载中断，未完成文件已清理"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn fixture(directory: &Path, marker: u8) -> PathBuf {
        let path = directory.join(format!("fixture-{marker}.gguf"));
        let mut bytes = vec![marker; 2048];
        bytes[..4].copy_from_slice(b"GGUF");
        fs::write(&path, bytes).unwrap();
        path
    }
    fn manager(directory: &Path) -> ModelManager {
        ModelManager::open(directory.join("models")).unwrap()
    }
    fn no_parts(directory: &Path) {
        assert!(fs::read_dir(directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "part")
        }));
    }
    fn verify(path: &Path) -> Result<(), AppError> {
        format_matches(path, "gguf")
    }

    #[test]
    fn imports_hash_complete_file_loads_before_publish_and_reopens() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path(), 7);
        let mut manager = manager(temp.path());
        let called = Cell::new(false);
        let info = manager
            .import(&source, "gguf", |path| {
                assert!(!manager_index(path).exists());
                assert!(path.extension().is_some_and(|e| e == "part"));
                called.set(true);
                verify(path)
            })
            .unwrap();
        assert!(called.get());
        assert_eq!(info.sha256, hash_file(&source, None).unwrap().1);
        let record = manager.resolve(&info.id, "gguf").unwrap();
        assert_eq!(fs::read(&record.path).unwrap(), fs::read(source).unwrap());
        assert_eq!(
            ModelManager::open(manager.directory.clone())
                .unwrap()
                .inventory()
                .len(),
            1
        );
        no_parts(&manager.directory);
    }
    fn manager_index(staged: &Path) -> PathBuf {
        staged.parent().unwrap().join("index.json")
    }

    #[test]
    fn invalid_format_and_failed_load_never_publish() {
        let temp = tempfile::tempdir().unwrap();
        let invalid = temp.path().join("invalid.bin");
        fs::write(&invalid, vec![0; 2048]).unwrap();
        let mut manager = manager(temp.path());
        assert_eq!(
            manager
                .import(&invalid, "gguf", |_| panic!("must reject format first"))
                .unwrap_err()
                .code,
            "model_format"
        );
        let source = fixture(temp.path(), 8);
        assert_eq!(
            manager
                .import(&source, "gguf", |_| Err(failure(
                    "test_load",
                    "simulated load failure"
                )))
                .unwrap_err()
                .code,
            "test_load"
        );
        assert!(manager.inventory().is_empty());
        assert!(!manager.directory.join("index.json").exists());
        no_parts(&manager.directory);
    }
    #[test]
    fn callback_mutation_and_download_checksum_failure_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path(), 9);
        let mut manager = manager(temp.path());
        let error = manager
            .import(&source, "gguf", |path| {
                let mut bytes = fs::read(path).unwrap();
                bytes[100] ^= 1;
                fs::write(path, bytes).unwrap();
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.code, "model_changed");
        let mut staged = tempfile::NamedTempFile::new_in(&manager.directory).unwrap();
        staged.write_all(&fs::read(&source).unwrap()).unwrap();
        let expected = DownloadInfo {
            id: "test".into(),
            filename: "test.gguf".into(),
            bytes: 2048,
            sha256: "0".repeat(64),
            source: "https://huggingface.co/test".into(),
            license: "test".into(),
            revision: "test-revision".into(),
            language: "未声明".into(),
            compatibility: "synthetic test only".into(),
        };
        assert_eq!(
            manager
                .publish_staged(staged, "gguf", Some(&expected), None, |_| panic!(
                    "checksum must fail first"
                ))
                .unwrap_err()
                .code,
            "model_checksum"
        );
        assert!(manager.inventory().is_empty());
        no_parts(&manager.directory);
    }
    #[test]
    fn oversized_sparse_source_and_disk_budget_fail_before_copy() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("oversized.gguf");
        fs::File::create(&source)
            .unwrap()
            .set_len(MAX_MODEL_BYTES + 1)
            .unwrap();
        let mut manager = manager(temp.path());
        assert_eq!(
            manager
                .import(&source, "gguf", |_| panic!("size must fail first"))
                .unwrap_err()
                .code,
            "model_size"
        );
        assert!(check_space(DISK_RESERVE + 1023, 1024).is_err());
        assert!(check_space(u64::MAX, u64::MAX).is_err());
        assert!(check_space(DISK_RESERVE + 1024, 1024).is_ok());
        no_parts(&manager.directory);
    }
    #[test]
    fn publication_collision_does_not_overwrite_and_cleans_stage() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path(), 10);
        let mut manager = manager(temp.path());
        let hash = hash_file(&source, None).unwrap().1;
        let collision = manager.directory.join(format!("{hash}.gguf"));
        fs::create_dir(&collision).unwrap();
        fs::write(collision.join("keep"), b"untouched").unwrap();
        assert_eq!(
            manager.import(&source, "gguf", verify).unwrap_err().code,
            "model_publish"
        );
        assert_eq!(fs::read(collision.join("keep")).unwrap(), b"untouched");
        assert!(manager.inventory().is_empty());
        assert!(!manager.directory.join("index.json").exists());
        no_parts(&manager.directory);
    }
    #[test]
    fn malformed_index_is_never_defaulted_or_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("models");
        fs::create_dir(&directory).unwrap();
        let index = directory.join("index.json");
        fs::write(&index, b"{broken").unwrap();
        assert!(ModelManager::open(directory.clone()).is_err());
        assert_eq!(fs::read(&index).unwrap(), b"{broken");
        fs::remove_file(&index).unwrap();
        let mut manager = ModelManager::open(directory).unwrap();
        fs::write(&index, b"externally-corrupted").unwrap();
        let source = fixture(temp.path(), 11);
        assert_eq!(
            manager.import(&source, "gguf", verify).unwrap_err().code,
            "model_index_changed"
        );
        assert_eq!(fs::read(&index).unwrap(), b"externally-corrupted");
        assert!(manager.inventory().is_empty());
        no_parts(&manager.directory);
    }
    #[test]
    fn selected_and_referenced_models_cannot_be_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path(), 12);
        let mut manager = manager(temp.path());
        let info = manager.import(&source, "gguf", verify).unwrap();
        let mut config = Config::default();
        config.asr.model_id = info.id.clone();
        assert_eq!(
            manager.delete(&info.id, &config).unwrap_err().code,
            "model_selected"
        );
        config.asr.model_id.clear();
        config.llm.local.model_id = info.id.clone();
        assert_eq!(
            manager.delete(&info.id, &config).unwrap_err().code,
            "model_selected"
        );
        config.llm.local.model_id.clear();
        let reference = manager.resolve(&info.id, "gguf").unwrap();
        let path = reference.path.clone();
        assert_eq!(
            manager.delete(&info.id, &config).unwrap_err().code,
            "model_in_use"
        );
        drop(reference);
        manager.delete(&info.id, &config).unwrap();
        assert!(!path.exists());
        assert!(
            ModelManager::open(manager.directory.clone())
                .unwrap()
                .inventory()
                .is_empty()
        );
    }
    #[test]
    fn duplicate_import_is_loaded_again_and_corrupt_existing_file_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path(), 13);
        let mut manager = manager(temp.path());
        let info = manager.import(&source, "gguf", verify).unwrap();
        let calls = Cell::new(0);
        let again = manager
            .import(&source, "gguf", |path| {
                calls.set(calls.get() + 1);
                verify(path)
            })
            .unwrap();
        assert_eq!(info.id, again.id);
        assert_eq!(calls.get(), 1);
        assert_eq!(manager.inventory().len(), 1);
        let path = manager.resolve(&info.id, "gguf").unwrap().path.clone();
        let mut bytes = fs::read(&path).unwrap();
        bytes[100] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            manager.import(&source, "gguf", verify).unwrap_err().code,
            "model_checksum"
        );
        assert!(ModelManager::open(manager.directory.clone()).is_err());
    }
    #[test]
    fn index_paths_and_duplicate_ids_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path(), 14);
        let mut manager = manager(temp.path());
        let info = manager.import(&source, "gguf", verify).unwrap();
        let index = manager.directory.join("index.json");
        let bytes = fs::read(&index).unwrap();
        let original: Vec<StoredModel> = serde_json::from_slice(&bytes).unwrap();
        for filename in [
            "/",
            "../outside.gguf",
            "/tmp/outside.gguf",
            "C:\\outside.gguf",
            "..",
            ".hidden",
        ] {
            let entries = vec![StoredModel {
                info: info.clone(),
                filename: filename.into(),
            }];
            fs::write(&index, serde_json::to_vec(&entries).unwrap()).unwrap();
            assert!(
                ModelManager::open(manager.directory.clone()).is_err(),
                "accepted {filename}"
            );
        }
        let filename = original[0].filename.clone();
        let duplicates = vec![
            StoredModel {
                info: info.clone(),
                filename: filename.clone(),
            },
            StoredModel { info, filename },
        ];
        fs::write(&index, serde_json::to_vec(&duplicates).unwrap()).unwrap();
        assert!(ModelManager::open(manager.directory.clone()).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn storage_file_and_index_symlinks_cannot_escape_directory() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path(), 15);
        let mut manager = manager(temp.path());
        let info = manager.import(&source, "gguf", verify).unwrap();
        let path = manager.resolve(&info.id, "gguf").unwrap().path.clone();
        fs::remove_file(&path).unwrap();
        symlink(&source, &path).unwrap();
        assert!(ModelManager::open(manager.directory.clone()).is_err());
        let index = manager.directory.join("index.json");
        fs::remove_file(&index).unwrap();
        let external_index = temp.path().join("external.json");
        fs::write(&external_index, b"[]").unwrap();
        symlink(&external_index, &index).unwrap();
        assert!(ModelManager::open(manager.directory.clone()).is_err());
        assert_eq!(fs::read(&external_index).unwrap(), b"[]");
        let alias = temp.path().join("alias");
        symlink(&manager.directory, &alias).unwrap();
        assert!(ModelManager::open(alias).is_err());
    }
    #[test]
    fn restart_cleans_unlocked_crash_parts_but_preserves_active_stages() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(temp.path());
        let active = staged_file(&manager.directory).unwrap();
        let active_path = active.path().to_owned();
        let stale = manager.directory.join(".voice-model-crashed.part");
        fs::write(&stale, b"unfinished").unwrap();
        let unrelated = manager.directory.join("user-data.part");
        fs::write(&unrelated, b"preserve").unwrap();
        ModelManager::open(manager.directory.clone()).unwrap();
        assert!(!stale.exists());
        assert!(active_path.exists());
        assert_eq!(fs::read(unrelated).unwrap(), b"preserve");
    }
    #[test]
    fn cancellation_does_not_wait_for_blocked_network_reader() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let flag = Arc::new(AtomicBool::new(false));
        let shared = flag.clone();
        let cancel = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            shared.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let mut output = Vec::new();
        assert_eq!(
            receive_download(&receiver, &mut output, 2048, &flag)
                .unwrap_err()
                .code,
            "cancelled"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(output.is_empty());
        drop(sender);
        cancel.join().unwrap();
    }
    #[test]
    fn download_chunks_are_bounded_and_length_mismatch_never_finishes() {
        for (bytes, size) in [(vec![0; 10], 9), (vec![0; 8], 9)] {
            let (sender, receiver) = mpsc::sync_channel(2);
            sender.send(Ok(DownloadEvent::Chunk(bytes))).unwrap();
            sender.send(Ok(DownloadEvent::Finished)).unwrap();
            assert_eq!(
                receive_download(&receiver, &mut Vec::new(), size, &AtomicBool::new(false))
                    .unwrap_err()
                    .code,
                "model_size"
            );
        }
        let (sender, receiver) = mpsc::sync_channel(2);
        sender.send(Ok(DownloadEvent::Chunk(vec![1; 9]))).unwrap();
        sender.send(Ok(DownloadEvent::Finished)).unwrap();
        let mut output = Vec::new();
        receive_download(&receiver, &mut output, 9, &AtomicBool::new(false)).unwrap();
        assert_eq!(output, vec![1; 9]);
    }
    #[test]
    fn runtime_damaged_index_is_read_only_and_preserves_all_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("models");
        fs::create_dir(&directory).unwrap();
        let index = directory.join("index.json");
        let weight = directory.join("original.gguf");
        let crash_part = directory.join(".voice-model-preserve-evidence.part");
        fs::write(&index, b"{broken index").unwrap();
        fs::write(&weight, b"original model evidence").unwrap();
        fs::write(&crash_part, b"unfinished evidence").unwrap();
        assert!(ModelManager::open(directory.clone()).is_err());
        let (mut manager, warning) = ModelManager::open_for_runtime(directory.clone()).unwrap();
        let warning = warning.expect("startup must expose blocking diagnosis");
        assert_eq!(warning.code, "model_index");
        assert!(!warning.retryable);
        assert!(warning.message.contains("只读"));
        assert!(manager.inventory().is_empty());
        assert_eq!(manager.blocking_error(), Some(&warning));
        let source = fixture(temp.path(), 31);
        assert_eq!(
            manager
                .import(&source, "gguf", |_| panic!("read-only must not load"))
                .unwrap_err(),
            warning
        );
        assert_eq!(
            manager
                .download("not-in-catalog", &AtomicBool::new(false), |_| panic!(
                    "read-only must not load"
                ))
                .unwrap_err(),
            warning
        );
        assert_eq!(
            manager.delete("any-id", &Config::default()).unwrap_err(),
            warning
        );
        assert!(manager.resolve("any-id", "gguf").is_err());
        assert_eq!(fs::read(index).unwrap(), b"{broken index");
        assert_eq!(fs::read(weight).unwrap(), b"original model evidence");
        assert_eq!(fs::read(crash_part).unwrap(), b"unfinished evidence");
        assert_eq!(fs::read_dir(directory).unwrap().count(), 3);
    }
    #[test]
    fn runtime_bad_weight_is_diagnosable_without_destroying_index_or_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path(), 32);
        let mut original = manager(temp.path());
        let info = original.import(&source, "gguf", verify).unwrap();
        let path = original.resolve(&info.id, "gguf").unwrap().path.clone();
        let index = original.directory.join("index.json");
        let index_before = fs::read(&index).unwrap();
        let mut changed = fs::read(&path).unwrap();
        changed[20] ^= 1;
        fs::write(&path, &changed).unwrap();
        assert!(ModelManager::open(original.directory.clone()).is_err());
        let (mut degraded, warning) =
            ModelManager::open_for_runtime(original.directory.clone()).unwrap();
        let warning = warning.unwrap();
        assert_eq!(warning.code, "model_checksum");
        assert!(degraded.inventory().is_empty());
        assert_eq!(
            degraded.delete(&info.id, &Config::default()).unwrap_err(),
            warning
        );
        assert_eq!(fs::read(index).unwrap(), index_before);
        assert_eq!(fs::read(path).unwrap(), changed);
    }
    #[test]
    fn runtime_keeps_directory_failures_fatal_and_normal_open_writable() {
        let temp = tempfile::tempdir().unwrap();
        let not_directory = temp.path().join("not-directory");
        fs::write(&not_directory, b"preserve").unwrap();
        assert!(ModelManager::open_for_runtime(not_directory.clone()).is_err());
        assert!(ModelManager::open_for_runtime(not_directory.join("child")).is_err());
        assert_eq!(fs::read(not_directory).unwrap(), b"preserve");
        let (mut manager, warning) =
            ModelManager::open_for_runtime(temp.path().join("valid")).unwrap();
        assert!(warning.is_none());
        assert!(manager.blocking_error().is_none());
        let source = fixture(temp.path(), 33);
        assert!(manager.import(&source, "gguf", verify).is_ok());
    }
    #[test]
    fn old_index_metadata_is_enriched_in_memory_without_rewriting_old_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path(), 41);
        let mut manager = manager(temp.path());
        let info = manager.import(&source, "gguf", verify).unwrap();
        assert_eq!(info.version, format!("sha256:{}", info.sha256));
        assert_eq!(info.language, "未声明");
        assert!(info.compatibility.contains("本机 CPU"));
        let index = manager.directory.join("index.json");
        let mut old: serde_json::Value =
            serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
        let metadata = old[0]["info"].as_object_mut().unwrap();
        for field in ["version", "language", "compatibility"] {
            metadata.remove(field);
        }
        let old_bytes = serde_json::to_vec(&old).unwrap();
        fs::write(&index, &old_bytes).unwrap();
        let reopened = ModelManager::open(manager.directory.clone()).unwrap();
        let upgraded = &reopened.inventory()[0];
        assert_eq!(upgraded.id, info.id);
        assert_eq!(upgraded.version, format!("sha256:{}", info.sha256));
        assert_eq!(upgraded.language, "未声明");
        assert!(upgraded.compatibility.contains("旧索引"));
        assert_eq!(fs::read(index).unwrap(), old_bytes);
    }
    #[test]
    fn pinned_catalog_revision_language_and_cpu_scope_survive_projection() {
        for entry in catalog() {
            assert_eq!(entry.revision.len(), 40);
            assert!(entry.revision.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(!entry.language.is_empty());
            assert!(entry.compatibility.contains("CPU"));
            let mut info = ModelInfo {
                id: "preserve-existing-id".into(),
                name: entry.id.clone(),
                engine: if entry.filename.ends_with(".gguf") {
                    "gguf"
                } else {
                    "whisper"
                }
                .into(),
                bytes: entry.bytes,
                sha256: entry.sha256.clone(),
                source: entry.source.clone(),
                license: entry.license.clone(),
                version: String::new(),
                language: String::new(),
                compatibility: String::new(),
            };
            populate_model_metadata(&mut info, false);
            assert_eq!(info.id, "preserve-existing-id");
            assert_eq!(info.version, entry.revision);
            assert_eq!(info.language, entry.language);
            assert_eq!(info.compatibility, entry.compatibility);
            assert!(valid_info(&info));
        }
    }
    #[test]
    fn memory_parser_keeps_missing_invalid_overflow_and_inconsistent_values_unknown() {
        assert_eq!(
            memory_facts("MemTotal: 16777216 kB\nMemAvailable: 4194304 kB\n"),
            (Some(16 * GIB), Some(4 * GIB))
        );
        for text in [
            "",
            "MemTotal: invalid kB\nMemAvailable: 12 MB",
            "MemTotal: 18446744073709551615 kB",
        ] {
            assert_eq!(memory_facts(text), (None, None));
        }
        assert_eq!(
            memory_facts("MemTotal: 10 kB\nMemAvailable: 20 kB"),
            (Some(10240), None)
        );
    }
    fn recommendation_fixture() -> (HardwareFacts, Vec<ModelInfo>) {
        let facts = HardwareFacts {
            architecture: "test-cpu".into(),
            logical_cpus: Some(4),
            total_memory_bytes: Some(16 * GIB),
            available_memory_bytes: Some(4 * GIB),
            available_disk_bytes: Some(10 * GIB),
            available_backends: vec!["cpu".into()],
            memory_source: "fixture".into(),
            gpu_status: "unknown".into(),
        };
        let inventory = catalog()
            .into_iter()
            .map(|entry| ModelInfo {
                id: entry.id.clone(),
                name: entry.id,
                engine: if entry.filename.ends_with(".gguf") {
                    "gguf"
                } else {
                    "whisper"
                }
                .into(),
                bytes: entry.bytes,
                sha256: entry.sha256,
                source: entry.source,
                license: entry.license,
                version: entry.revision,
                language: entry.language,
                compatibility: entry.compatibility,
            })
            .collect();
        (facts, inventory)
    }
    #[test]
    fn recommendations_respect_available_memory_and_only_refer_to_verified_inventory() {
        let (mut facts, inventory) = recommendation_fixture();
        let hints = recommend(&facts, &inventory);
        assert_eq!(hints[0].engine, "whisper");
        assert_eq!(hints[1].engine, "gguf");
        assert!(
            hints
                .iter()
                .filter_map(|hint| hint.estimated_memory_bytes)
                .all(|budget| budget <= 4 * GIB)
        );
        assert!(
            hints
                .iter()
                .filter_map(|hint| hint.model_id.as_ref())
                .all(|id| inventory.iter().any(|model| &model.id == id))
        );
        facts.total_memory_bytes = Some(64 * GIB);
        facts.available_memory_bytes = Some(2 * GIB);
        let hints = recommend(&facts, &inventory);
        assert_eq!(hints[1].engine, "remote");
        assert!(hints[1].model_id.is_none());
        facts.total_memory_bytes = Some(8 * GIB);
        facts.available_memory_bytes = Some(7 * GIB);
        assert_eq!(recommend(&facts, &inventory)[1].engine, "remote");
        assert!(
            recommend(&facts, &[])
                .iter()
                .all(|hint| hint.model_id.is_none())
        );
    }
    #[test]
    fn unknown_resources_do_not_invent_local_support_and_advice_is_deterministic() {
        let (mut facts, inventory) = recommendation_fixture();
        facts.total_memory_bytes = None;
        facts.available_memory_bytes = None;
        let first = recommend(&facts, &inventory);
        assert_eq!(first, recommend(&facts, &inventory));
        assert!(
            first
                .iter()
                .all(|hint| hint.model_id.is_none() && hint.estimated_memory_bytes.is_none())
        );
        assert_eq!(first[1].engine, "remote");
        assert!(first[1].reason.contains("不会自动切换"));
    }
    #[test]
    fn hardware_probe_is_read_only_and_does_not_claim_gpu_verification() {
        let temp = tempfile::tempdir().unwrap();
        let facts = probe_hardware(temp.path());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
        assert_eq!(facts.architecture, std::env::consts::ARCH);
        assert_eq!(facts.available_backends, vec!["cpu"]);
        assert!(facts.gpu_status.contains("未探测"));
        let (_, mut inventory) = recommendation_fixture();
        inventory[0].bytes = u64::MAX;
        assert!(recommendation_budget(&inventory[0]).is_none());
    }
    #[test]
    fn only_official_https_sources_and_signed_official_redirects_are_allowed() {
        assert!(approved_source(
            "https://huggingface.co/owner/model/resolve/hash/model.gguf",
            false
        ));
        assert!(approved_source(
            "https://cas-bridge.xethub.hf.co/object?Expires=123&Signature=synthetic",
            true
        ));
        for url in [
            "http://huggingface.co/model",
            "https://evil.invalid/model",
            "https://huggingface.co.evil.invalid/model",
            "https://user:secret@huggingface.co/model",
            "https://huggingface.co:444/model",
            "https://huggingface.co/model#fragment",
        ] {
            assert!(!approved_source(url, true), "accepted {url}");
        }
        assert!(!approved_source(
            "https://huggingface.co/model?url=custom",
            false
        ));
    }
}
