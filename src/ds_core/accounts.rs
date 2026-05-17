//! Quản lý pool tài khoản - cân bằng tải nhiều tài khoản
//!
//! Mặc định 1 account = 1 session = 1 concurrency. Có thể tăng giới hạn theo cấu hình,
//! nhưng vẫn ưu tiên xoay ngang nhiều tài khoản.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::time::SystemTime;

use dashmap::DashMap;
use futures::TryStreamExt;
use log::{debug, error, info, warn};
use tokio::sync::RwLock;

use crate::config::Account as AccountConfig;
use crate::ds_core::client::{ClientError, CompletionPayload, DsClient, LoginPayload};
use crate::ds_core::pow::{PowError, PowSolver};

/// Enum trạng thái tài khoản
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountState {
    Idle = 0,
    Busy = 1,
    Error = 2,
    Invalid = 3,
}

impl AccountState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Idle,
            1 => Self::Busy,
            2 => Self::Error,
            _ => Self::Invalid,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Error => "error",
            Self::Invalid => "invalid",
        }
    }
}

/// Thông tin trạng thái tài khoản
#[derive(serde::Serialize)]
pub struct AccountStatus {
    pub email: String,
    pub mobile: String,
    pub state: String,
    pub active_count: usize,
    pub max_concurrent: usize,
    /// Timestamp nhả cuối cùng (ms), 0 nghĩa là chưa từng dùng
    pub last_released_ms: i64,
    /// Số lần đăng nhập thất bại liên tiếp
    pub error_count: u8,
}

pub struct Account {
    token: std::sync::RwLock<Arc<str>>,
    email: String,
    mobile: String,
    state: AtomicU8,
    /// Số request/session đang giữ tài khoản này.
    active_count: AtomicUsize,
    /// Timestamp lần nhả tài khoản gần nhất (ms), dùng để xét cooldown
    last_released: AtomicI64,
    /// Số lần đăng nhập thất bại liên tiếp
    error_count: AtomicU8,
    /// Thông tin đăng nhập gốc (dùng để đăng nhập lại)
    creds: AccountConfig,
}

/// Giới hạn lỗi đăng nhập liên tiếp, đạt ngưỡng thì đánh dấu Invalid
const MAX_ERROR_COUNT: u8 = 3;

impl Account {
    pub fn token(&self) -> Arc<str> {
        self.token.read().unwrap().clone()
    }

    pub fn display_id(&self) -> &str {
        if self.email.is_empty() {
            &self.mobile
        } else {
            &self.email
        }
    }

    pub fn state(&self) -> AccountState {
        AccountState::from_u8(self.state.load(Ordering::Relaxed))
    }

    pub fn is_busy(&self) -> bool {
        self.active_count.load(Ordering::Relaxed) > 0
    }

    fn visible_state(&self) -> AccountState {
        match self.state() {
            AccountState::Error => AccountState::Error,
            AccountState::Invalid => AccountState::Invalid,
            _ if self.is_busy() => AccountState::Busy,
            _ => AccountState::Idle,
        }
    }

    fn can_accept_more(&self, max_concurrent: usize) -> bool {
        matches!(self.state(), AccountState::Idle | AccountState::Busy)
            && self.active_count.load(Ordering::Relaxed) < max_concurrent
    }

    fn try_claim(&self, max_concurrent: usize) -> bool {
        let max_concurrent = max_concurrent.max(1);
        loop {
            let state = self.state();
            if !matches!(state, AccountState::Idle | AccountState::Busy) {
                return false;
            }

            let active = self.active_count.load(Ordering::Relaxed);
            if active >= max_concurrent {
                return false;
            }

            if active == 0
                && state == AccountState::Idle
                && self
                    .state
                    .compare_exchange(
                        AccountState::Idle as u8,
                        AccountState::Busy as u8,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_err()
            {
                continue;
            }

            if self
                .active_count
                .compare_exchange(active, active + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                if self.state() == AccountState::Busy {
                    return true;
                }
                self.release_claim();
                return false;
            }
        }
    }

    fn release_claim(&self) {
        let previous = self.active_count.fetch_sub(1, Ordering::Relaxed);
        if previous <= 1 {
            self.active_count.store(0, Ordering::Relaxed);
            self.state
                .compare_exchange(
                    AccountState::Busy as u8,
                    AccountState::Idle as u8,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .ok();
            self.touch_last_released();
        }
    }

    fn touch_last_released(&self) {
        let d = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let now_ms = (d.as_secs() * 1000 + u64::from(d.subsec_millis())) as i64;
        self.last_released.store(now_ms, Ordering::Relaxed);
    }

    /// Tạo tài khoản trạng thái Invalid (dùng khi init thất bại, vẫn thêm vào pool để frontend hiển thị)
    fn new_invalid(creds: AccountConfig) -> Self {
        Self {
            token: std::sync::RwLock::new(String::new().into()),
            email: creds.email.clone(),
            mobile: creds.mobile.clone(),
            state: AtomicU8::new(AccountState::Invalid as u8),
            active_count: AtomicUsize::new(0),
            last_released: AtomicI64::new(0),
            error_count: AtomicU8::new(MAX_ERROR_COUNT),
            creds,
        }
    }
}

/// Khi đang giữ, tài khoản được đánh dấu busy; tự nhả khi Drop
pub struct AccountGuard {
    account: Arc<Account>,
}

impl AccountGuard {
    pub fn account(&self) -> &Account {
        &self.account
    }
}

impl Drop for AccountGuard {
    fn drop(&mut self) {
        self.account.release_claim();
    }
}

pub struct AccountPool {
    /// key = display_id (email or mobile), value = Account
    accounts: DashMap<String, Arc<Account>>,
    client: RwLock<Option<DsClient>>,
    solver: RwLock<Option<PowSolver>>,
    max_concurrent_per_account: AtomicUsize,
}

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// Mọi tài khoản init thất bại (không có tài khoản khả dụng)
    #[error("Tất cả tài khoản khởi tạo thất bại")]
    AllAccountsFailed,

    /// Lỗi client downstream (mạng, API...)
    #[error("Lỗi client: {0}")]
    Client(#[from] ClientError),

    /// Tính PoW thất bại (lỗi chạy WASM)
    #[error("Tính PoW thất bại: {0}")]
    Pow(#[from] PowError),

    /// Kiểm tra cấu hình tài khoản thất bại
    #[error("Lỗi cấu hình tài khoản: {0}")]
    Validation(String),

    /// Tài khoản đã tồn tại
    #[error("Tài khoản đã tồn tại: {0}")]
    AlreadyExists(String),

    /// Tài khoản không tồn tại
    #[error("Tài khoản không tồn tại: {0}")]
    NotFound(String),

    /// Tài khoản đang được dùng, không thể xóa
    #[error("Tài khoản đang được sử dụng: {0}")]
    AccountBusy(String),
}

impl AccountPool {
    pub fn new(max_concurrent_per_account: usize) -> Self {
        Self {
            accounts: DashMap::new(),
            client: RwLock::new(None),
            solver: RwLock::new(None),
            max_concurrent_per_account: AtomicUsize::new(max_concurrent_per_account.max(1)),
        }
    }

    fn max_concurrent_per_account(&self) -> usize {
        self.max_concurrent_per_account
            .load(Ordering::Relaxed)
            .max(1)
    }

    pub fn set_max_concurrent_per_account(&self, value: usize) {
        self.max_concurrent_per_account
            .store(value.max(1), Ordering::Relaxed);
    }

    pub async fn init(
        &self,
        creds: Vec<AccountConfig>,
        client: &DsClient,
        solver: &PowSolver,
    ) -> Result<(), PoolError> {
        if creds.is_empty() {
            return Ok(());
        }

        use futures::future::join_all;
        use std::sync::Arc;
        use tokio::sync::Semaphore;

        // Giới hạn số init đồng thời để tránh gây áp lực lên DeepSeek và pool kết nối cục bộ
        let semaphore = Arc::new(Semaphore::new(13));
        let futures: Vec<_> = creds
            .into_iter()
            .map(|creds| {
                let client = client.clone();
                let solver = solver.clone();
                let sem = semaphore.clone();
                async move {
                    let _permit = sem.acquire().await.expect("Semaphore chưa đóng");
                    let display_id = if creds.email.is_empty() {
                        creds.mobile.clone()
                    } else {
                        creds.email.clone()
                    };
                    let account = match init_account(&creds, &client, &solver).await {
                        Ok(account) => {
                            info!(target: "ds_core::accounts", "Tài khoản {} khởi tạo thành công", display_id);
                            account
                        }
                        Err(e) => {
                            warn!(target: "ds_core::accounts", "Tài khoản {} khởi tạo thất bại: {}", display_id, e);
                            // Dù init thất bại vẫn thêm vào pool, đánh dấu Invalid để frontend hiển thị
                            Account::new_invalid(creds.clone())
                        }
                    };
                    Some((display_id, Arc::new(account)))
                }
            })
            .collect();

        let results: Vec<(String, Arc<Account>)> =
            join_all(futures).await.into_iter().flatten().collect();
        let idle_count = results
            .iter()
            .filter(|(_, a)| a.visible_state() == AccountState::Idle)
            .count();

        for (id, account) in &results {
            self.accounts.insert(id.clone(), Arc::clone(account));
        }

        if idle_count == 0 {
            warn!(target: "ds_core::accounts", "Tất cả tài khoản khởi tạo thất bại: tài khoản có thể bị tắt hoặc thông tin đăng nhập sai");
        } else if results.len() > 1 && idle_count < results.len() {
            warn!(target: "ds_core::accounts", "{}/{} tài khoản không khả dụng", results.len() - idle_count, results.len());
        }
        Ok(())
    }

    /// Thêm tài khoản động (init runtime)
    pub async fn add_account(
        &self,
        creds: &AccountConfig,
        client: &DsClient,
        solver: &PowSolver,
    ) -> Result<String, PoolError> {
        let display_id = if creds.email.is_empty() {
            creds.mobile.clone()
        } else {
            creds.email.clone()
        };

        // Kiểm tra đã tồn tại chưa (DashMap O(1))
        if self.accounts.contains_key(&display_id) {
            return Err(PoolError::AlreadyExists(display_id));
        }

        let account = init_account(creds, client, solver).await?;
        let _id = account.display_id().to_string();
        self.accounts.insert(display_id.clone(), Arc::new(account));
        info!(target: "ds_core::accounts", "Thêm động tài khoản {} thành công", display_id);
        Ok(display_id)
    }

    /// Xóa tài khoản động (chỉ xóa tài khoản rảnh)
    pub async fn remove_account(&self, email_or_mobile: &str) -> Result<String, PoolError> {
        let account = self
            .accounts
            .get(email_or_mobile)
            .ok_or_else(|| PoolError::NotFound(email_or_mobile.to_string()))?;

        if account.is_busy() {
            return Err(PoolError::AccountBusy(email_or_mobile.to_string()));
        }

        // Cũng cho phép xóa tài khoản trạng thái Error/Invalid
        drop(account);
        let (_, removed) = self
            .accounts
            .remove(email_or_mobile)
            .ok_or_else(|| PoolError::NotFound(email_or_mobile.to_string()))?;
        let id = removed.display_id().to_string();
        info!(target: "ds_core::accounts", "Xóa động tài khoản {}", id);
        Ok(id)
    }

    /// Lấy tài khoản khả dụng rảnh lâu nhất, có chờ: nếu không có tài khoản khả dụng thì chờ tối đa `timeout_ms` ms
    pub async fn get_account_with_wait(&self, timeout_ms: u64) -> Option<AccountGuard> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            if let Some(g) = self.get_account() {
                return Some(g);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    /// Lấy tài khoản khả dụng rảnh lâu nhất (không chờ, trả về ngay)
    ///
    /// Duyệt mọi tài khoản, chọn tài khoản đã hết cooldown và rảnh lâu nhất để tối đa hóa khoảng cách giữa các lần dùng.
    /// DashMap đọc không khóa, không chặn request đồng thời.
    pub fn get_account(&self) -> Option<AccountGuard> {
        if self.accounts.is_empty() {
            return None;
        }

        let d = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let now_ms = (d.as_secs() * 1000 + u64::from(d.subsec_millis())) as i64;

        let max_concurrent = self.max_concurrent_per_account();
        let mut candidates: Vec<(bool, i64, Arc<Account>)> = self
            .accounts
            .iter()
            .filter_map(|entry| {
                let account = entry.value();
                if account.can_accept_more(max_concurrent) {
                    let idle = now_ms - account.last_released.load(Ordering::Relaxed);
                    Some((!account.is_busy(), idle, Arc::clone(account)))
                } else {
                    None
                }
            })
            .collect();

        candidates.sort_unstable_by_key(|(unused, idle, _)| {
            (std::cmp::Reverse(*unused), std::cmp::Reverse(*idle))
        });

        for (_, _, account) in candidates {
            if account.try_claim(max_concurrent) {
                return Some(AccountGuard { account });
            }
        }

        None
    }

    /// Lấy trạng thái chi tiết của mọi tài khoản
    pub fn account_statuses(&self) -> Vec<AccountStatus> {
        let max_concurrent = self.max_concurrent_per_account();
        self.accounts
            .iter()
            .map(|entry| {
                let a = entry.value();
                AccountStatus {
                    email: a.email.clone(),
                    mobile: a.mobile.clone(),
                    state: a.visible_state().as_str().to_string(),
                    active_count: a.active_count.load(Ordering::Relaxed),
                    max_concurrent,
                    last_released_ms: a.last_released.load(Ordering::Relaxed),
                    error_count: a.error_count.load(Ordering::Relaxed),
                }
            })
            .collect()
    }

    /// Tắt an toàn (luồng mới không có session bền vững, không cần dọn)
    pub async fn shutdown(&self, _client: &DsClient) {}

    /// Lưu client và solver cho tác vụ khôi phục
    pub async fn set_client_solver(&self, client: DsClient, solver: PowSolver) {
        *self.client.write().await = Some(client);
        *self.solver.write().await = Some(solver);
    }

    /// Đánh dấu tài khoản sang trạng thái Error (gọi khi request thất bại)
    pub fn mark_error(&self, email_or_mobile: &str) {
        if let Some(entry) = self.accounts.get(email_or_mobile) {
            let account = entry.value();
            if account.state() != AccountState::Invalid {
                account
                    .state
                    .store(AccountState::Error as u8, Ordering::Relaxed);
            }
            warn!(target: "ds_core::accounts", "Đánh dấu tài khoản {} là Error", account.display_id());
        }
    }

    /// Đăng nhập lại thủ công tài khoản chỉ định (admin kích hoạt)
    /// Thành công -> Idle, thất bại -> error_count++, >=3 thì Invalid
    pub async fn re_login_single(&self, email_or_mobile: &str) -> Result<(), String> {
        let client_opt = self.client.read().await.clone();
        let solver_opt = self.solver.read().await.clone();
        let (Some(client), Some(solver)) = (client_opt, solver_opt) else {
            return Err("client/solver chưa được khởi tạo".to_string());
        };

        let account = self
            .accounts
            .get(email_or_mobile)
            .ok_or_else(|| format!("Tài khoản {} không tồn tại", email_or_mobile))?;
        let account = account.value();

        // Chỉ cho phép đăng nhập lại tài khoản trạng thái Error/Invalid
        let state = account.state();
        if state != AccountState::Error && state != AccountState::Invalid {
            return Err(format!(
                "Trạng thái tài khoản là {}, chỉ Error/Invalid mới được đăng nhập lại",
                state.as_str()
            ));
        }
        if account.is_busy() {
            return Err(format!(
                "Tài khoản {} đang được sử dụng, thử lại sau",
                account.display_id()
            ));
        }

        Self::re_login_account(account, &client, &solver).await;

        // Kiểm tra trạng thái sau đăng nhập lại
        let new_state = account.state();
        if new_state == AccountState::Idle {
            Ok(())
        } else {
            Err(format!(
                "Đăng nhập lại thất bại, trạng thái hiện tại: {}",
                new_state.as_str()
            ))
        }
    }

    /// Thử đăng nhập lại tài khoản trạng thái Error
    /// Thành công -> Idle, thất bại -> error_count++, >=3 thì Invalid
    async fn re_login_account(account: &Account, client: &DsClient, solver: &PowSolver) {
        let display_id = account.display_id().to_string();
        match try_init_account(&account.creds, client, solver).await {
            Ok(new_account) => {
                // Cập nhật token
                *account.token.write().unwrap() = new_account.token.read().unwrap().clone();
                account
                    .state
                    .store(AccountState::Idle as u8, Ordering::Relaxed);
                account.error_count.store(0, Ordering::Relaxed);
                info!(target: "ds_core::accounts", "Tài khoản {} đăng nhập lại thành công", display_id);
            }
            Err(e) => {
                let count = account.error_count.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= MAX_ERROR_COUNT {
                    account
                        .state
                        .store(AccountState::Invalid as u8, Ordering::Relaxed);
                    error!(target: "ds_core::accounts", "Tài khoản {} đăng nhập lại thất bại liên tiếp {} lần, đánh dấu Invalid: {}", display_id, count, e);
                } else {
                    warn!(target: "ds_core::accounts", "Tài khoản {} đăng nhập lại thất bại ({} lần): {}", display_id, count, e);
                }
            }
        }
    }

    /// Khởi động tác vụ khôi phục nền: quét tài khoản Error mỗi 60 giây và thử đăng nhập lại
    pub fn start_recovery_task(self: &Arc<Self>) {
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

                let client_opt = pool.client.read().await.clone();
                let solver_opt = pool.solver.read().await.clone();
                let (Some(client), Some(solver)) = (client_opt, solver_opt) else {
                    continue;
                };

                for entry in pool.accounts.iter() {
                    let account = entry.value();
                    if account.state() == AccountState::Error && !account.is_busy() {
                        Self::re_login_account(account, &client, &solver).await;
                    }
                }
            }
        });
    }
}

async fn init_account(
    creds: &AccountConfig,
    client: &DsClient,
    solver: &PowSolver,
) -> Result<Account, PoolError> {
    try_init_account(creds, client, solver).await
}

async fn try_init_account(
    creds: &AccountConfig,
    client: &DsClient,
    solver: &PowSolver,
) -> Result<Account, PoolError> {
    // Kiểm tra: email và mobile phải có ít nhất một giá trị
    if creds.email.is_empty() && creds.mobile.is_empty() {
        return Err(PoolError::Validation(
            "email và mobile không được đồng thời để trống".to_string(),
        ));
    }

    let login_payload = LoginPayload {
        email: if creds.email.is_empty() {
            None
        } else {
            Some(creds.email.clone())
        },
        mobile: if creds.mobile.is_empty() {
            None
        } else {
            Some(creds.mobile.clone())
        },
        password: creds.password.clone(),
        area_code: if creds.area_code.is_empty() {
            None
        } else {
            Some(creds.area_code.clone())
        },
        device_id: String::new(),
        os: "web".to_string(),
    };

    let login_data = client.login(&login_payload).await?;
    debug!(
        target: "ds_core::client",
        "Phản hồi đăng nhập: code={}, msg={}, user_id={}, email={:?}, mobile={:?}",
        login_data.code,
        login_data.msg,
        login_data.user.id,
        login_data.user.email,
        login_data.user.mobile_number
    );
    let token = login_data.user.token;

    let display_id = if creds.email.is_empty() {
        &creds.mobile
    } else {
        &creds.email
    };

    // Health check: tạo session tạm -> gửi test completion -> xóa session
    let session_id = client.create_session(&token).await?;
    if let Err(e) = health_check(&token, &session_id, client, solver, "default", display_id).await {
        // Dù health check thất bại cũng phải dọn session
        let _ = client.delete_session(&token, &session_id).await;
        return Err(e);
    }
    let _ = client.delete_session(&token, &session_id).await;

    Ok(Account {
        token: std::sync::RwLock::new(token.into()),
        email: creds.email.clone(),
        mobile: creds.mobile.clone(),
        state: AtomicU8::new(AccountState::Idle as u8),
        active_count: AtomicUsize::new(0),
        last_released: AtomicI64::new(0),
        error_count: AtomicU8::new(0),
        creds: creds.clone(),
    })
}

async fn health_check(
    token: &str,
    session_id: &str,
    client: &DsClient,
    solver: &PowSolver,
    model_type: &str,
    display_id: &str,
) -> Result<(), PoolError> {
    let start = std::time::Instant::now();
    let challenge = client
        .create_pow_challenge(token, "/api/v0/chat/completion")
        .await?;

    let result = solver.solve(&challenge)?;
    let pow_header = result.to_header();

    let payload = CompletionPayload {
        chat_session_id: session_id.to_string(),
        parent_message_id: None,
        model_type: model_type.to_string(),
        prompt: "Chỉ trả lời `Hello, world!`".to_string(),
        ref_file_ids: vec![],
        thinking_enabled: false,
        search_enabled: false,
        preempt: false,
    };

    let mut stream = client.completion(token, &pow_header, &payload).await?;
    // Tiêu thụ stream và kiểm tra có nhận SSE bình thường không (tài khoản khỏe phải có event ready/response)
    let mut data = Vec::new();
    while let Some(chunk) = stream.try_next().await? {
        data.extend_from_slice(&chunk);
    }

    let text = String::from_utf8_lossy(&data);

    // Phát hiện tài khoản bất thường (muted / rate limit...)
    if text.contains(r#""biz_code":"#) {
        error!(
            target: "ds_core::accounts",
            "health_check phát hiện lỗi nghiệp vụ: account={}, response={}",
            display_id,
            text.lines().find(|l| l.contains("biz_code")).unwrap_or(&text)
        );
        return Err(PoolError::Validation(
            "Tài khoản bất thường (muted/limited)".into(),
        ));
    }

    // Kiểm tra stream SSE có kết thúc bình thường không
    if !text.contains(r#""FINISHED""#) && !text.contains(r#""INCOMPLETE""#) {
        return Err(PoolError::Validation(
            "Stream SSE không kết thúc bình thường".into(),
        ));
    }

    debug!(
        target: "ds_core::accounts",
        "health_check hoàn tất model_type={} account={} elapsed={:?}",
        model_type, display_id, start.elapsed()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_config(email: &str) -> AccountConfig {
        AccountConfig {
            email: email.to_string(),
            mobile: String::new(),
            area_code: String::new(),
            password: "pass".to_string(),
        }
    }

    fn idle_account(email: &str, last_released_ms: i64) -> Arc<Account> {
        Arc::new(Account {
            token: std::sync::RwLock::new(Arc::<str>::from("token")),
            email: email.to_string(),
            mobile: String::new(),
            state: AtomicU8::new(AccountState::Idle as u8),
            active_count: AtomicUsize::new(0),
            last_released: AtomicI64::new(last_released_ms),
            error_count: AtomicU8::new(0),
            creds: account_config(email),
        })
    }

    #[test]
    fn get_account_claims_distinct_idle_accounts_without_waiting() {
        let pool = AccountPool::new(1);
        let ids = [
            "user0@example.com",
            "user1@example.com",
            "user2@example.com",
        ];

        for (idx, id) in ids.iter().enumerate() {
            let id = *id;
            let last_released_ms = i64::try_from(idx).expect("test index fits i64");
            pool.accounts
                .insert(id.to_string(), idle_account(id, last_released_ms));
        }

        let guard_a = pool
            .get_account()
            .expect("first idle account should be claimed");
        let guard_b = pool
            .get_account()
            .expect("second idle account should be claimed");
        let guard_c = pool
            .get_account()
            .expect("third idle account should be claimed");

        let mut claimed = [
            guard_a.account().display_id().to_string(),
            guard_b.account().display_id().to_string(),
            guard_c.account().display_id().to_string(),
        ];
        claimed.sort();
        let expected = [
            "user0@example.com".to_string(),
            "user1@example.com".to_string(),
            "user2@example.com".to_string(),
        ];

        assert_eq!(
            claimed, expected,
            "pool should claim each idle account once before reporting exhaustion"
        );
        assert!(
            pool.get_account().is_none(),
            "pool should be exhausted while all guards are held"
        );
    }

    #[test]
    fn get_account_allows_configured_parallel_claims_per_account() {
        let pool = AccountPool::new(2);
        pool.accounts.insert(
            "user0@example.com".to_string(),
            idle_account("user0@example.com", 0),
        );

        let guard_a = pool.get_account().expect("first claim should use account");
        let guard_b = pool
            .get_account()
            .expect("second claim should share account when limit is 2");

        assert_eq!(guard_a.account().display_id(), "user0@example.com");
        assert_eq!(guard_b.account().display_id(), "user0@example.com");
        assert_eq!(guard_a.account().visible_state(), AccountState::Busy);
        assert_eq!(guard_a.account().active_count.load(Ordering::Relaxed), 2);
        assert!(
            pool.get_account().is_none(),
            "pool should be exhausted after configured per-account limit"
        );

        drop(guard_a);
        assert_eq!(guard_b.account().active_count.load(Ordering::Relaxed), 1);
        drop(guard_b);
        let status = pool.account_statuses();
        assert_eq!(status[0].state, "idle");
        assert_eq!(status[0].active_count, 0);
        assert_eq!(status[0].max_concurrent, 2);
    }
}
