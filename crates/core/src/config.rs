use crate::{AppError, invalid_config};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::Path};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub audio: AudioConfig,
    pub asr: AsrConfig,
    pub llm: LlmConfig,
    pub delivery: DeliveryConfig,
    pub recovery: RecoveryConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: 1,
            audio: AudioConfig::default(),
            asr: AsrConfig::default(),
            llm: LlmConfig::default(),
            delivery: DeliveryConfig::default(),
            recovery: RecoveryConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub device_id: String,
    pub max_seconds: u32,
}
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device_id: "default".into(),
            max_seconds: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrBackend {
    Cpu,
    Auto,
    Vulkan,
    Metal,
    Cuda,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    pub model_id: String,
    pub language: Language,
    pub threads: usize,
    pub backend: AsrBackend,
    pub timeout_seconds: u32,
}
impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            model_id: String::new(),
            language: Language::Auto,
            threads: 0,
            backend: AsrBackend::Auto,
            timeout_seconds: 120,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Auto,
    Zh,
    En,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmMode {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteProfile {
    Cleanup,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    pub mode: LlmMode,
    pub profile: RewriteProfile,
    pub local: LocalLlmConfig,
    pub remote: RemoteLlmConfig,
    pub glossary: Vec<String>,
}
impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            mode: LlmMode::Local,
            profile: RewriteProfile::Cleanup,
            local: LocalLlmConfig::default(),
            remote: RemoteLlmConfig::default(),
            glossary: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalLlmConfig {
    pub model_id: String,
    pub context_tokens: u32,
    pub threads: usize,
    pub gpu_layers: u32,
    pub timeout_seconds: u32,
}
impl Default for LocalLlmConfig {
    fn default() -> Self {
        Self {
            model_id: String::new(),
            context_tokens: 4096,
            threads: 0,
            gpu_layers: 0,
            timeout_seconds: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteLlmConfig {
    pub base_url: String,
    pub model: String,
    pub profile_id: String,
    pub timeout_seconds: u32,
}
impl Default for RemoteLlmConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            profile_id: String::new(),
            timeout_seconds: 15,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    AutoPaste,
    Preview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliveryConfig {
    pub mode: DeliveryMode,
    pub shortcut: String,
    pub sensitive_apps: Vec<String>,
}
impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            mode: DeliveryMode::AutoPaste,
            shortcut: "platform_default".into(),
            sensitive_apps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecoveryConfig {
    pub persist: bool,
    pub max_entries: usize,
    pub ttl_hours: u32,
}
impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            persist: true,
            max_entries: 5,
            ttl_hours: 24,
        }
    }
}

fn safe_string(value: &str, max: usize) -> bool {
    value.len() <= max && !value.chars().any(char::is_control)
}

impl Config {
    /// Structure validation accepts an unconfigured first-run template.
    /// Model format, installed backends and model context limits need inventory checks.
    pub fn validate(&self) -> Result<(), AppError> {
        if self.schema_version != 1 {
            return Err(AppError::new(
                "unsupported_schema",
                "config",
                false,
                "不支持此配置版本，请保留原文件并使用兼容版本",
            ));
        }
        if !(1..=60).contains(&self.audio.max_seconds)
            || !safe_string(&self.audio.device_id, 1024)
            || self.audio.device_id.is_empty()
        {
            return Err(invalid_config("录音时长或设备标识无效"));
        }
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        if self.asr.threads > cpus || self.llm.local.threads > cpus {
            return Err(invalid_config("线程数超出本机可用逻辑核数"));
        }
        if !(1..=600).contains(&self.asr.timeout_seconds)
            || !(1..=600).contains(&self.llm.local.timeout_seconds)
            || !(5..=60).contains(&self.llm.remote.timeout_seconds)
        {
            return Err(invalid_config("推理超时超出允许范围"));
        }
        if !(2048..=16384).contains(&self.llm.local.context_tokens)
            || self.llm.local.gpu_layers > 512
        {
            return Err(invalid_config("本地上下文或 GPU 层数超出允许范围"));
        }
        if !(1..=5).contains(&self.recovery.max_entries)
            || !(1..=24).contains(&self.recovery.ttl_hours)
        {
            return Err(invalid_config("恢复记录最多 5 条且保留最长 24 小时"));
        }
        if [
            &self.asr.model_id,
            &self.llm.local.model_id,
            &self.llm.remote.model,
            &self.llm.remote.profile_id,
            &self.delivery.shortcut,
        ]
        .iter()
        .any(|s| !safe_string(s, 1024))
            || self.delivery.shortcut.trim().is_empty()
        {
            return Err(invalid_config("模型、档案或快捷键标识无效"));
        }
        if self.llm.glossary.len() > 256
            || self.llm.glossary.iter().any(|s| !safe_string(s, 256))
            || self.llm.glossary.iter().map(String::len).sum::<usize>() > 8192
            || self.delivery.sensitive_apps.len() > 256
            || self
                .delivery
                .sensitive_apps
                .iter()
                .any(|s| !safe_string(s, 256))
        {
            return Err(invalid_config("术语表或敏感应用列表超限"));
        }
        let url = &self.llm.remote.base_url;
        if !url.is_empty() {
            let rest = url
                .strip_prefix("https://")
                .ok_or_else(|| invalid_config("远程地址必须使用 HTTPS"))?;
            let authority = rest.split('/').next().unwrap_or_default();
            if authority.is_empty()
                || url.contains(['@', '?', '#'])
                || !safe_string(url, 4096)
                || url.chars().any(char::is_whitespace)
                || url.trim_end_matches('/').ends_with("/chat/completions")
            {
                return Err(invalid_config(
                    "远程基地址无效，请填写不含凭据、查询或接口后缀的 HTTPS 基地址",
                ));
            }
        }
        Ok(())
    }

    pub fn validate_ready(&self) -> Result<(), AppError> {
        self.validate()?;
        if self.asr.model_id.is_empty()
            || match self.llm.mode {
                LlmMode::Local => self.llm.local.model_id.is_empty(),
                LlmMode::Remote => {
                    self.llm.remote.base_url.is_empty()
                        || self.llm.remote.model.is_empty()
                        || self.llm.remote.profile_id.is_empty()
                }
            }
        {
            return Err(AppError::new(
                "setup_required",
                "config",
                false,
                "请先选择 ASR 模型并完成 LLM 配置",
            ));
        }
        Ok(())
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let raw = fs::read_to_string(path).map_err(|_| {
            AppError::new(
                "config_read_failed",
                "config",
                true,
                "无法读取配置，请检查文件权限",
            )
        })?;
        let value: toml::Value =
            toml::from_str(&raw).map_err(|_| invalid_config("配置格式无效，原文件已保留"))?;
        if value
            .get("schema_version")
            .and_then(toml::Value::as_integer)
            != Some(1)
        {
            return Err(AppError::new(
                "unsupported_schema",
                "config",
                false,
                "不支持此配置版本，原文件已保留",
            ));
        }
        let config: Config =
            toml::from_str(&raw).map_err(|_| invalid_config("配置字段无效，原文件已保留"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), AppError> {
        self.validate()?;
        let path = path.as_ref();
        if path.exists() {
            Self::load(path)?;
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        crate::recovery::create_private_directory(parent)?;
        let raw = toml::to_string_pretty(self).map_err(|_| invalid_config("无法编码配置"))?;
        let write_error = || {
            AppError::new(
                "config_write_failed",
                "config",
                true,
                "配置保存失败，原配置未被覆盖",
            )
        };
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|_| write_error())?;
        temporary
            .write_all(raw.as_bytes())
            .map_err(|_| write_error())?;
        temporary.as_file().sync_all().map_err(|_| write_error())?;
        temporary.persist(path).map_err(|_| write_error())?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| {
                AppError::new(
                    "config_sync_failed",
                    "config",
                    true,
                    "配置已替换，但目录刷盘失败，请检查存储设备",
                )
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_run_and_ready_have_distinct_contracts() {
        let mut c = Config::default();
        assert!(c.validate().is_ok());
        assert!(c.validate_ready().is_err());
        c.asr.model_id = "base".into();
        c.llm.local.model_id = "qwen".into();
        assert!(c.validate_ready().is_ok());
        c.audio.max_seconds = 61;
        assert!(c.validate().is_err());
    }
    #[test]
    fn save_roundtrip_and_unknown_schema_never_overwrites() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let c = Config::default();
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        let newer = "schema_version = 2\n";
        fs::write(&path, newer).unwrap();
        assert_eq!(c.save(&path).unwrap_err().code, "unsupported_schema");
        assert_eq!(fs::read_to_string(path).unwrap(), newer);
    }
    #[test]
    fn remote_endpoint_and_resource_boundaries() {
        let mut c = Config::default();
        for invalid in [
            "http://example.com/v1",
            "https://user:password@example.com",
            "https://example.com/v1?key=x",
            "https://example.com/v1/chat/completions/",
        ] {
            c.llm.remote.base_url = invalid.into();
            assert!(c.validate().is_err());
        }
        c.llm.remote.base_url = "https://example.com/custom/v1/".into();
        assert!(c.validate().is_ok());
        c.recovery.max_entries = 6;
        assert!(c.validate().is_err());
        c.recovery.max_entries = 5;
        c.llm.local.context_tokens = 2047;
        assert!(c.validate().is_err());
    }
}
