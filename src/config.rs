//! Module tải cấu hình - điểm vào cấu hình thống nhất
//!
//! Hỗ trợ tham số dòng lệnh `-c <path>`; giá trị mặc định xem trong các hàm bên dưới.
//! Mục bị comment trong config.toml dùng giá trị mặc định trong code.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Cấu trúc gốc cấu hình ứng dụng
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Pool tài khoản (bắt buộc, có thể rỗng - thêm qua bảng quản trị sau khi khởi động)
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Cấu hình DeepSeek
    #[serde(default)]
    pub deepseek: DeepSeekConfig,
    /// Context compression and summary cache
    #[serde(default)]
    pub context: ContextConfig,
    /// Cấu hình HTTP server (bắt buộc)
    pub server: ServerConfig,
    /// Cấu hình proxy (tùy chọn, dùng để vượt WAF)
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// Cấu hình admin (hash mật khẩu bcrypt, khóa JWT..., do bảng quản trị quản lý)
    #[serde(default)]
    pub admin: AdminConfig,
    /// Danh sách API Key (do bảng quản trị quản lý)
    #[serde(default)]
    pub api_keys: Vec<ApiKeyEntry>,
}

/// Cấu hình admin
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AdminConfig {
    /// Mật khẩu đã hash bằng bcrypt
    #[serde(default)]
    pub password_hash: String,
    /// Khóa ký JWT (giá trị ngẫu nhiên 32 byte mã hóa hex)
    #[serde(default)]
    pub jwt_secret: String,
    /// Thời điểm phát hành JWT gần nhất (dùng để thu hồi token cũ)
    #[serde(default)]
    pub jwt_issued_at: u64,
    /// Đổi mật khẩu: mật khẩu cũ dạng rõ (chỉ nhận qua PUT, không ghi xuống config.toml)
    #[serde(default, skip_serializing)]
    pub old_password: String,
    /// Đổi mật khẩu: mật khẩu mới dạng rõ (chỉ nhận qua PUT, không ghi xuống config.toml)
    #[serde(default, skip_serializing)]
    pub new_password: String,
}

/// Mục API Key
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiKeyEntry {
    pub key: String,
    pub description: String,
}

/// Cấu hình proxy
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProxyConfig {
    /// Proxy URL, ví dụ http://127.0.0.1:7890 hoặc socks5://127.0.0.1:7891
    pub url: Option<String>,
}

/// Context compression config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContextConfig {
    /// Enable non-blocking summary cache.
    #[serde(default = "default_context_enabled")]
    pub enabled: bool,
    /// Use cached summary when old text reaches this size.
    #[serde(default = "default_context_trigger_chars")]
    pub trigger_chars: usize,
    /// Start background summary before trigger so cache is ready later.
    #[serde(default = "default_context_prewarm_chars")]
    pub prewarm_chars: usize,
    /// Keep recent messages verbatim.
    #[serde(default = "default_context_keep_last_messages")]
    pub keep_last_messages: usize,
    /// Split old context for parallel background summaries.
    #[serde(default = "default_context_chunk_chars")]
    pub chunk_chars: usize,
    /// Max concurrent background summary jobs.
    #[serde(default = "default_context_summary_workers")]
    pub summary_workers: usize,
    /// Limit summary text injected into the final request.
    #[serde(default = "default_context_summary_max_chars")]
    pub summary_max_chars: usize,
    /// In-memory cache TTL.
    #[serde(default = "default_context_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
    /// Delay background jobs so the foreground chat gets the account first.
    #[serde(default = "default_context_background_delay_ms")]
    pub background_delay_ms: u64,
    /// ds_core model_type used for background summaries.
    #[serde(default = "default_context_summary_model_type")]
    pub summary_model_type: String,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            enabled: default_context_enabled(),
            trigger_chars: default_context_trigger_chars(),
            prewarm_chars: default_context_prewarm_chars(),
            keep_last_messages: default_context_keep_last_messages(),
            chunk_chars: default_context_chunk_chars(),
            summary_workers: default_context_summary_workers(),
            summary_max_chars: default_context_summary_max_chars(),
            cache_ttl_secs: default_context_cache_ttl_secs(),
            background_delay_ms: default_context_background_delay_ms(),
            summary_model_type: default_context_summary_model_type(),
        }
    }
}

fn default_context_enabled() -> bool {
    true
}

fn default_context_trigger_chars() -> usize {
    24_000
}

fn default_context_prewarm_chars() -> usize {
    16_000
}

fn default_context_keep_last_messages() -> usize {
    12
}

fn default_context_chunk_chars() -> usize {
    12_000
}

fn default_context_summary_workers() -> usize {
    2
}

fn default_context_summary_max_chars() -> usize {
    3_000
}

fn default_context_cache_ttl_secs() -> u64 {
    86_400
}

fn default_context_background_delay_ms() -> u64 {
    1_000
}

fn default_context_summary_model_type() -> String {
    "default".to_string()
}

/// Cấu hình một tài khoản
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Account {
    /// Email (chọn một trong email hoặc mobile)
    pub email: String,
    /// Số điện thoại (chọn một trong email hoặc mobile)
    pub mobile: String,
    /// Mã vùng (dùng kèm mobile, ví dụ "+86")
    pub area_code: String,
    /// Mật khẩu
    pub password: String,
}

/// Cấu hình client DeepSeek
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeepSeekConfig {
    /// Base URL API
    #[serde(default = "default_api_base")]
    pub api_base: String,
    /// URL đầy đủ của file WASM (cần cho tính PoW, version có thể đổi)
    #[serde(default = "default_wasm_url")]
    pub wasm_url: String,
    /// Header User-Agent
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Header X-Client-Version (dùng cho model expert và tính năng liên quan)
    #[serde(default = "default_client_version")]
    pub client_version: String,
    /// Header X-Client-Platform
    #[serde(default = "default_client_platform")]
    pub client_platform: String,
    /// Header X-Client-Locale
    #[serde(default = "default_client_locale")]
    pub client_locale: String,
    /// Danh sách loại model hỗ trợ; mỗi loại tự map thành model_id OpenAI: deepseek-<type>
    #[serde(default = "default_model_types")]
    pub model_types: Vec<String>,
    /// Giới hạn token input của từng loại model (khớp index với model_types)
    #[serde(default = "default_max_input_tokens")]
    pub max_input_tokens: Vec<u32>,
    /// Giới hạn token output của từng loại model (khớp index với model_types)
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: Vec<u32>,
    /// Giới hạn số ký tự input mỗi lần của từng loại model (khớp index với model_types)
    /// Vượt giới hạn này: model expert dùng ghi session theo chunk, model khác tự fallback sang gửi inline
    #[serde(default = "default_input_character_limits")]
    pub input_character_limits: Vec<u32>,
    /// Cấu hình tag gọi công cụ (tag fallback tùy chỉnh)
    #[serde(default)]
    pub tool_call: ToolCallTagConfig,
    /// Alias model: khớp index với model_types, có thể có nhiều alias ngăn bằng dấu phẩy
    /// Ví dụ model_types = ["default", "expert"], model_aliases = ["deepseek-v4-flash, deepseek-v4-flash-nothinking", "deepseek-v4-pro"]
    #[serde(default = "default_model_aliases")]
    pub model_aliases: Vec<String>,
}

/// Cấu hình tag gọi công cụ
///
/// Fuzzy match tích hợp: `｜`(U+FF5C)<->`|`, `▁`(U+2581)<->`_`, tự xử lý đa số biến thể sai ở cấp ký tự.
/// Danh sách extra ở đây dùng cho tag có format khác hẳn (ví dụ `<tool_call>`),
/// tức các trường hợp fuzzy match không bao phủ được.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCallTagConfig {
    /// Tag bắt đầu bổ sung (đã có `<|tool▁calls▁begin|>` + fuzzy match; chỉ thêm biến thể format khác hẳn)
    #[serde(default = "default_tool_call_starts")]
    pub extra_starts: Vec<String>,
    /// Tag kết thúc bổ sung (đã có `<|tool▁calls▁end|>` + fuzzy match; chỉ thêm biến thể format khác hẳn)
    #[serde(default = "default_tool_call_ends")]
    pub extra_ends: Vec<String>,
}

impl Default for ToolCallTagConfig {
    fn default() -> Self {
        Self {
            extra_starts: default_tool_call_starts(),
            extra_ends: default_tool_call_ends(),
        }
    }
}

fn default_tool_call_starts() -> Vec<String> {
    vec![
        "<|tool_call_begin|>".into(),
        "<tool_calls>".into(),
        "<tool_call>".into(),
    ]
}

fn default_tool_call_ends() -> Vec<String> {
    vec![
        "<|tool_call_end|>".into(),
        "</tool_calls>".into(),
        "</tool_call>".into(),
    ]
}

impl Default for DeepSeekConfig {
    fn default() -> Self {
        Self {
            api_base: default_api_base(),
            wasm_url: default_wasm_url(),
            user_agent: default_user_agent(),
            client_version: default_client_version(),
            client_platform: default_client_platform(),
            client_locale: default_client_locale(),
            model_types: default_model_types(),
            max_input_tokens: default_max_input_tokens(),
            max_output_tokens: default_max_output_tokens(),
            input_character_limits: default_input_character_limits(),
            tool_call: ToolCallTagConfig::default(),
            model_aliases: default_model_aliases(),
        }
    }
}

fn default_model_types() -> Vec<String> {
    vec![
        "default".to_string(),
        "expert".to_string(),
        "vision".to_string(),
    ]
}

fn default_max_input_tokens() -> Vec<u32> {
    vec![1_048_576, 1_048_576, 1_048_576]
}

fn default_max_output_tokens() -> Vec<u32> {
    vec![384_000, 384_000, 384_000]
}

fn default_input_character_limits() -> Vec<u32> {
    vec![2_621_440, 163_840, 2_621_440]
}

fn default_model_aliases() -> Vec<String> {
    vec![
        "deepseek-v4-flash, deepseek-v4-flash-nothinking, deepseek-v4-flash-search, deepseek-v4-flash-search-nothinking".to_string(),
        "deepseek-v4-pro, deepseek-v4-pro-nothinking, deepseek-v4-pro-search, deepseek-v4-pro-search-nothinking".to_string(),
        "deepseek-v4-vision, deepseek-v4-vision-nothinking".to_string(),
    ]
}

pub(crate) fn builtin_model_aliases(model_type: &str) -> &'static [&'static str] {
    match model_type {
        "default" => &[
            "deepseek-v4-flash",
            "deepseek-v4-flash-nothinking",
            "deepseek-v4-flash-search",
            "deepseek-v4-flash-search-nothinking",
        ],
        "expert" => &[
            "deepseek-v4-pro",
            "deepseek-v4-pro-nothinking",
            "deepseek-v4-pro-search",
            "deepseek-v4-pro-search-nothinking",
        ],
        "vision" => &["deepseek-v4-vision", "deepseek-v4-vision-nothinking"],
        _ => &[],
    }
}

impl DeepSeekConfig {
    /// Tạo map registry model OpenAI
    #[must_use]
    pub fn model_registry(&self) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        for (i, ty) in self.model_types.iter().enumerate() {
            map.insert(format!("deepseek-{}", ty).to_lowercase(), ty.clone());
            for alias in builtin_model_aliases(ty) {
                map.insert((*alias).to_string(), ty.clone());
            }
            if let Some(alias) = self.model_aliases.get(i) {
                for alias in split_model_aliases(alias) {
                    map.insert(alias.to_lowercase(), ty.clone());
                }
            }
        }
        map
    }
}

fn split_model_aliases(alias: &str) -> impl Iterator<Item = &str> {
    alias.split(',').map(str::trim).filter(|a| !a.is_empty())
}

/// Cấu hình HTTP server (bắt buộc)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    /// Địa chỉ lắng nghe
    pub host: String,
    /// Cổng lắng nghe
    pub port: u16,
    /// Danh sách Origin được CORS cho phép, mặc định ["http://localhost:22217"]
    /// Đặt ["*"] để cho phép tất cả (không khuyến nghị cho production)
    #[serde(default = "default_cors_origins")]
    pub cors_origins: Vec<String>,
}

fn default_cors_origins() -> Vec<String> {
    vec!["http://localhost:22217".to_string()]
}

/// Base URL API mặc định
fn default_api_base() -> String {
    "https://chat.deepseek.com/api/v0".to_string()
}

/// URL file WASM mặc định (version có thể đổi, nên khai báo rõ trong file cấu hình)
fn default_wasm_url() -> String {
    "https://fe-static.deepseek.com/chat/static/sha3_wasm_bg.7b9ca65ddd.wasm".to_string()
}

/// User-Agent mặc định
fn default_user_agent() -> String {
    "DeepSeek/2.0.4 Android/35".to_string()
}

/// X-Client-Version mặc định
fn default_client_version() -> String {
    "2.1.0".to_string()
}

/// X-Client-Platform mặc định
fn default_client_platform() -> String {
    "android".to_string()
}

/// X-Client-Locale mặc định
fn default_client_locale() -> String {
    "zh_CN".to_string()
}

impl Config {
    /// Tải cấu hình từ path chỉ định
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Self = toml::de::from_str(&content)?;
        config.dedup_accounts();
        config.apply_env_overrides()?;
        config.validate()?;
        Ok(config)
    }

    fn apply_env_overrides(&mut self) -> Result<(), ConfigError> {
        let platform_port = std::env::var("PORT").ok();
        let port = std::env::var("DS_PORT")
            .ok()
            .or_else(|| platform_port.clone());

        if let Some(port) = port {
            self.server.port = port.parse::<u16>().map_err(|_| {
                ConfigError::Validation(format!("PORT/DS_PORT không hợp lệ: {port}"))
            })?;
        }

        if let Ok(host) = std::env::var("DS_HOST") {
            let host = host.trim();
            if !host.is_empty() {
                self.server.host = host.to_string();
            }
        } else if platform_port.is_some() {
            self.server.host = "0.0.0.0".to_string();
        }

        if let Ok(origins) = std::env::var("DS_CORS_ORIGINS") {
            let origins = origins
                .split(',')
                .map(str::trim)
                .filter(|origin| !origin.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            if !origins.is_empty() {
                self.server.cors_origins = origins;
            }
        }

        Ok(())
    }

    /// Khử trùng lặp theo email (ưu tiên) hoặc mobile, giữ tài khoản xuất hiện đầu tiên
    fn dedup_accounts(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.accounts.retain(|a| {
            let key = if a.email.is_empty() {
                a.mobile.clone()
            } else {
                a.email.clone()
            };
            seen.insert(key)
        });
    }

    /// Parse tham số dòng lệnh và tải cấu hình
    ///
    /// Hỗ trợ `-c <path>` để chỉ định path file cấu hình, mặc định dùng `config.toml`
    /// Cũng hỗ trợ biến môi trường `DS_CONFIG_PATH` (ưu tiên: `-c` > `DS_CONFIG_PATH` > mặc định)
    /// Nếu file không tồn tại và không được chỉ định rõ bằng `-c`, tự tạo cấu hình tối thiểu
    /// Trả về (cấu hình đã tải, path file cấu hình)
    pub fn load_with_args(
        args: impl Iterator<Item = String>,
    ) -> Result<(Self, PathBuf), ConfigError> {
        let mut explicit_c = false;
        let mut config_path = None;
        let mut iter = args.skip(1); // Bỏ qua tên chương trình

        while let Some(arg) = iter.next() {
            if arg == "-c" {
                explicit_c = true;
                if let Some(path) = iter.next() {
                    config_path = Some(path);
                } else {
                    return Err(ConfigError::Cli(
                        "Tham số -c cần đường dẫn cấu hình".to_string(),
                    ));
                }
            }
        }

        let path: PathBuf = config_path
            .map(PathBuf::from)
            .or_else(|| std::env::var("DS_CONFIG_PATH").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("config.toml"));

        if !path.exists() {
            if explicit_c {
                return Err(ConfigError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "Tệp cấu hình được chỉ định không tồn tại: {}",
                        path.display()
                    ),
                )));
            }
            // Tự tạo cấu hình tối thiểu
            let default = Config {
                accounts: Vec::new(),
                deepseek: DeepSeekConfig::default(),
                context: ContextConfig::default(),
                server: ServerConfig {
                    host: "127.0.0.1".into(),
                    port: 22217,
                    cors_origins: default_cors_origins(),
                },
                proxy: ProxyConfig::default(),
                admin: AdminConfig::default(),
                api_keys: Vec::new(),
            };
            if let Some(parent) = path.parent() {
                let parent_str = parent.as_os_str();
                if !parent_str.is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            default.save(&path)?;
            log::info!(target: "config", "Đã tạo tệp cấu hình mặc định: {}", path.display());
            let mut default = default;
            default.apply_env_overrides()?;
            default.validate()?;
            return Ok((default, path));
        }

        let config = Self::load(&path)?;
        Ok((config, path))
    }
    /// Kiểm tra tính hợp lệ của cấu hình
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if self.deepseek.model_types.is_empty() {
            return Err(ConfigError::Validation(
                "model_types không được để trống".to_string(),
            ));
        }
        let n = self.deepseek.model_types.len();
        if self.deepseek.max_input_tokens.len() != n {
            return Err(ConfigError::Validation(format!(
                "Độ dài max_input_tokens ({}) phải khớp độ dài model_types ({})",
                self.deepseek.max_input_tokens.len(),
                n
            )));
        }
        if self.deepseek.max_output_tokens.len() != n {
            return Err(ConfigError::Validation(format!(
                "Độ dài max_output_tokens ({}) phải khớp độ dài model_types ({})",
                self.deepseek.max_output_tokens.len(),
                n
            )));
        }
        if self.deepseek.input_character_limits.len() != n {
            return Err(ConfigError::Validation(format!(
                "Độ dài input_character_limits ({}) phải khớp độ dài model_types ({})",
                self.deepseek.input_character_limits.len(),
                n
            )));
        }
        if self.context.enabled {
            if self.context.keep_last_messages == 0 {
                return Err(ConfigError::Validation(
                    "context.keep_last_messages must be > 0".to_string(),
                ));
            }
            if self.context.trigger_chars < self.context.prewarm_chars {
                return Err(ConfigError::Validation(
                    "context.trigger_chars must be >= context.prewarm_chars".to_string(),
                ));
            }
            if self.context.chunk_chars == 0 || self.context.summary_workers == 0 {
                return Err(ConfigError::Validation(
                    "context.chunk_chars and context.summary_workers must be > 0".to_string(),
                ));
            }
        }
        let mut seen_keys = std::collections::HashSet::new();
        for k in &self.api_keys {
            if !seen_keys.insert(&k.key) {
                let prefix = if k.key.len() > 12 {
                    &k.key[..12]
                } else {
                    &k.key
                };
                return Err(ConfigError::Validation(format!(
                    "API key bị trùng: {}...",
                    prefix
                )));
            }
        }
        Ok(())
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let toml_str = toml::to_string_pretty(self).map_err(ConfigError::TomlSerialization)?;
        let tmp = path.as_ref().with_extension("toml.tmp");
        std::fs::write(&tmp, &toml_str)?;
        std::fs::rename(&tmp, path.as_ref())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(path.as_ref(), perms)?;
        }
        Ok(())
    }
}

/// Kiểu lỗi tải cấu hình
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Lỗi IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("Lỗi phân tích TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("Lỗi xác thực cấu hình: {0}")]
    Validation(String),
    #[error("Lỗi tham số dòng lệnh: {0}")]
    Cli(String),
    #[error("Lỗi serialize TOML: {0}")]
    TomlSerialization(#[from] toml::ser::Error),
}
