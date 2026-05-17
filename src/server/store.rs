//! Lưu trữ bền vững - dữ liệu admin/auth dựa trên Config + đọc/ghi nguyên tử stats.json

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::{info, warn};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::Config;

/// Dữ liệu quản lý stats.json
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct StatsStore {
    pub total_requests: u64,
    pub success_requests: u64,
    pub failed_requests: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    /// Thống kê theo model (khôi phục được sau restart)
    #[serde(default)]
    pub model_stats: std::collections::HashMap<String, ModelStatsData>,
    /// Thống kê theo API Key (khôi phục được sau restart, key là tiền tố đã che)
    #[serde(default)]
    pub key_stats: std::collections::HashMap<String, KeyStatsData>,
    /// N log request gần nhất (khôi phục được sau restart)
    #[serde(default)]
    pub request_logs: Vec<RequestLogData>,
}

/// Dữ liệu thống kê model đã lưu
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ModelStatsData {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub requests: u64,
}

/// Dữ liệu thống kê API Key đã lưu
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KeyStatsData {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub requests: u64,
}

/// Mục log request đã lưu
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RequestLogData {
    pub timestamp: u64,
    pub request_id: String,
    pub model: String,
    pub api_key: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_ms: u64,
    pub success: bool,
}

/// Trình quản lý lưu trữ runtime (admin + api_keys -> Config, stats -> stats.json)
pub struct StoreManager {
    config_path: PathBuf,
    config: Arc<RwLock<Config>>,
    base_dir: PathBuf,
    pub stats: Arc<RwLock<StatsStore>>,
}

impl StoreManager {
    pub fn new(base_dir: &Path, config_path: &Path, config: Arc<RwLock<Config>>) -> Self {
        let stats_path = base_dir.join("stats.json");
        let stats = if stats_path.exists() {
            match fs::read_to_string(&stats_path) {
                Ok(content) if !content.trim().is_empty() => {
                    match serde_json::from_str::<StatsStore>(&content) {
                        Ok(s) => {
                            info!(target: "store", "Đã tải stats.json");
                            s
                        }
                        Err(e) => {
                            warn!(target: "store", "Phân tích stats.json thất bại: {}, dùng giá trị 0", e);
                            StatsStore::default()
                        }
                    }
                }
                Ok(_) => {
                    info!(target: "store", "stats.json trống, dùng giá trị 0");
                    StatsStore::default()
                }
                Err(e) => {
                    warn!(target: "store", "Đọc stats.json thất bại: {}, dùng giá trị 0", e);
                    StatsStore::default()
                }
            }
        } else {
            info!(target: "store", "stats.json không tồn tại, dùng giá trị 0");
            StatsStore::default()
        };

        Self {
            config_path: config_path.to_path_buf(),
            config,
            base_dir: base_dir.to_path_buf(),
            stats: Arc::new(RwLock::new(stats)),
        }
    }

    /// Kiểm tra đã đặt mật khẩu chưa
    pub async fn has_password(&self) -> bool {
        !self.config.read().await.admin.password_hash.is_empty()
    }

    /// Xác thực mật khẩu
    pub async fn verify_password(&self, plain: &str) -> bool {
        let guard = self.config.read().await;
        bcrypt::verify(plain, &guard.admin.password_hash).unwrap_or(false)
    }

    /// Lấy khóa JWT
    pub async fn jwt_secret(&self) -> Option<String> {
        let guard = self.config.read().await;
        if guard.admin.jwt_secret.is_empty() {
            None
        } else {
            Some(guard.admin.jwt_secret.clone())
        }
    }

    /// Lấy thời điểm phát hành JWT gần nhất (dùng để thu hồi token cũ)
    pub async fn jwt_issued_at(&self) -> Option<u64> {
        let guard = self.config.read().await;
        let iat = guard.admin.jwt_issued_at;
        (iat > 0).then_some(iat)
    }

    /// Cập nhật jwt_issued_at và lưu bền vững
    #[allow(dead_code)]
    pub async fn set_jwt_issued_at(&self, iat: u64) {
        let mut guard = self.config.write().await;
        guard.admin.jwt_issued_at = iat;
        let _ = guard.save(&self.config_path);
    }

    /// Lưu cấu hình admin (hash mật khẩu, khóa JWT...)
    pub async fn save_admin(
        &self,
        password_hash: String,
        jwt_secret: String,
        jwt_issued_at: u64,
    ) -> anyhow::Result<()> {
        let mut guard = self.config.write().await;
        guard.admin.password_hash = password_hash;
        guard.admin.jwt_secret = jwt_secret;
        guard.admin.jwt_issued_at = jwt_issued_at;
        guard.save(&self.config_path)?;
        Ok(())
    }

    /// Kiểm tra API Key có hợp lệ không
    pub async fn is_valid_api_key(&self, key: &str) -> bool {
        let guard = self.config.read().await;
        guard.api_keys.iter().any(|k| k.key == key)
    }

    /// Tải dữ liệu thống kê đã lưu
    pub async fn load_stats(&self) -> StatsStore {
        self.stats.read().await.clone()
    }

    /// Lưu stats.json
    pub async fn save_stats(&self, store: &StatsStore) -> anyhow::Result<()> {
        let path = self.base_dir.join("stats.json");
        write_json_file(&path, store)?;
        *self.stats.write().await = store.clone();
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Ghi file JSON nguyên tử: ghi .tmp trước rồi rename
fn write_json_file<T: Serialize>(path: &Path, data: &T) -> anyhow::Result<()> {
    let tmp_path = path.with_extension("tmp");
    let json = serde_json::to_string_pretty(data)?;
    fs::write(&tmp_path, &json)?;
    fs::rename(&tmp_path, path)?;
    // Đặt quyền file 0600 (chỉ owner được đọc/ghi)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Tạo chuỗi hex ngẫu nhiên (32 byte = 64 ký tự hex)
pub fn generate_hex_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    hex::encode(&bytes)
}

/// Hash mật khẩu bằng bcrypt
pub fn hash_password(plain: &str) -> String {
    bcrypt::hash(plain, 12).expect("bcrypt hash không nên thất bại")
}

// Helper mã hóa hex (tránh thêm dependency)
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}
