//! Module lõi DeepSeek - lớp chuyển đổi từ OpenAI API sang DeepSeek
//!
//! Chỉ xuất ra giao diện tối thiểu: DeepSeekCore, CoreError, ChatRequest

mod accounts;
mod client;
mod completions;
mod pow;

pub use accounts::AccountStatus;
pub use accounts::PoolError;
pub use completions::{ChatRequest, ChatResponse, FilePayload};

use crate::config::Config;
use accounts::AccountPool;
use client::{ClientError, DsClient};
use pow::{PowError, PowSolver};

/// Kiểu lỗi tầng lõi
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Quá tải dịch vụ: mọi tài khoản đều bận hoặc không khỏe
    #[error("no available account")]
    Overloaded,

    /// Tính PoW thất bại
    #[error("proof of work failed: {0}")]
    ProofOfWorkFailed(#[from] PowError),

    /// Lỗi nhà cung cấp: mạng, nghiệp vụ, token hết hiệu lực...
    #[error("provider: {0}")]
    ProviderError(String),

    /// Lỗi xử lý stream: mất kết nối...
    #[error("stream error: {0}")]
    Stream(String),
}

impl From<ClientError> for CoreError {
    fn from(e: ClientError) -> Self {
        CoreError::ProviderError(e.to_string())
    }
}

pub struct DeepSeekCore {
    completions: crate::ds_core::completions::Completions,
}

impl DeepSeekCore {
    pub async fn new(config: &Config) -> Result<Self, CoreError> {
        let client = DsClient::new(
            config.deepseek.api_base.clone(),
            config.deepseek.wasm_url.clone(),
            config.deepseek.user_agent.clone(),
            config.deepseek.client_version.clone(),
            config.deepseek.client_platform.clone(),
            config.deepseek.client_locale.clone(),
            config.proxy.url.as_deref(),
        )?;

        let wasm_bytes = client.get_wasm().await?;
        let solver = PowSolver::new(&wasm_bytes)?;

        let pool = AccountPool::new(config.deepseek.max_concurrent_per_account);
        pool.init(config.accounts.clone(), &client, &solver)
            .await
            .map_err(|e| match e {
                accounts::PoolError::AllAccountsFailed => {
                    CoreError::ProviderError("Tất cả tài khoản khởi tạo thất bại".to_string())
                }
                accounts::PoolError::Client(e) => CoreError::ProviderError(e.to_string()),
                accounts::PoolError::Pow(e) => CoreError::ProofOfWorkFailed(e),
                accounts::PoolError::Validation(msg) => {
                    CoreError::ProviderError(format!("Lỗi cấu hình: {}", msg))
                }
                other => CoreError::ProviderError(other.to_string()),
            })?;

        let completions = crate::ds_core::completions::Completions::new(
            client,
            solver,
            pool,
            config.deepseek.model_types.clone(),
            config.deepseek.input_character_limits.clone(),
        )
        .await;

        Ok(Self { completions })
    }

    /// Gửi yêu cầu hội thoại, trả về stream byte SSE + định danh tài khoản
    ///
    /// Tự nhả tài khoản khi stream kết thúc hoặc bị hủy
    pub async fn v0_chat(
        &self,
        req: ChatRequest,
        request_id: &str,
    ) -> Result<ChatResponse, CoreError> {
        self.completions.v0_chat(req, request_id).await
    }

    pub fn account_statuses(&self) -> Vec<AccountStatus> {
        self.completions.account_statuses()
    }

    /// Thêm tài khoản động
    pub async fn add_account(&self, creds: &crate::config::Account) -> Result<String, PoolError> {
        self.completions.add_account(creds).await
    }

    /// Xóa tài khoản động
    pub async fn remove_account(&self, email_or_mobile: &str) -> Result<String, PoolError> {
        self.completions.remove_account(email_or_mobile).await
    }

    /// Đánh dấu tài khoản sang trạng thái Error
    pub fn mark_error(&self, email_or_mobile: &str) {
        self.completions.mark_error(email_or_mobile);
    }

    /// Đăng nhập lại thủ công tài khoản chỉ định
    pub async fn re_login_single(&self, email_or_mobile: &str) -> Result<(), String> {
        self.completions.re_login_single(email_or_mobile).await
    }

    /// Tắt an toàn: dọn session của mọi tài khoản
    pub async fn shutdown(&self) {
        self.completions.shutdown().await;
    }

    pub async fn reload_config(&self, config: &Config) -> Result<(), CoreError> {
        self.completions.reload_config(config).await
    }
}
