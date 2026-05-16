//! Module xác thực - phát hành/kiểm tra JWT + giới hạn lỗi đăng nhập

use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use super::store::StoreManager;

type HmacSha256 = Hmac<Sha256>;

// ── JWT ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: String,
    pub iat: u64,
    pub exp: u64,
}

const JWT_HEADER: &str = r#"{"alg":"HS256","typ":"JWT"}"#;
const JWT_EXPIRY_SECS: u64 = 24 * 3600;

fn base64url_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn base64url_decode(data: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(data)
        .ok()
}

/// Phát hành JWT
pub async fn sign_jwt(store: &StoreManager) -> Option<String> {
    let secret = store.jwt_secret().await?;
    let now = epoch_secs();

    let payload = serde_json::to_vec(&TokenClaims {
        sub: "admin".to_string(),
        iat: now,
        exp: now + JWT_EXPIRY_SECS,
    })
    .ok()?;

    let header_b64 = base64url_encode(JWT_HEADER.as_bytes());
    let payload_b64 = base64url_encode(&payload);
    let signing_input = format!("{}.{}", header_b64, payload_b64);

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(signing_input.as_bytes());
    let sig_b64 = base64url_encode(&mac.finalize().into_bytes());

    let token = format!("{}.{}", signing_input, sig_b64);

    // Cập nhật jwt_issued_at (dùng để thu hồi token cũ)
    store.set_jwt_issued_at(now).await;
    Some(token)
}

/// Kiểm tra JWT, trả về có hợp lệ không
pub async fn verify_jwt(store: &StoreManager, token: &str) -> bool {
    let Some(secret) = store.jwt_secret().await else {
        return false;
    };

    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return false;
    }

    // Kiểm tra chữ ký HMAC-SHA256
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(signing_input.as_bytes());
    let expected = mac.finalize().into_bytes();

    let Some(sig_bytes) = base64url_decode(parts[2]) else {
        return false;
    };

    // CtOutput deref về [u8], có thể so sánh trực tiếp
    if &*expected != sig_bytes.as_slice() {
        return false;
    }

    // Parse payload
    let Some(payload_bytes) = base64url_decode(parts[1]) else {
        return false;
    };

    #[derive(Deserialize)]
    struct JwtPayload {
        sub: String,
        iat: u64,
        exp: u64,
    }

    let payload: JwtPayload = match serde_json::from_slice(&payload_bytes) {
        Ok(p) => p,
        Err(_) => return false,
    };
    // sub chỉ dùng để kiểm tra deserialize, không cần đọc
    let _ = payload.sub;

    // Kiểm tra hết hạn (leeway 60 giây, khớp hành vi jsonwebtoken cũ)
    let now = epoch_secs();
    if now > payload.exp + 60 {
        return false;
    }

    // Kiểm tra thu hồi: iat của token phải >= jwt_issued_at đã lưu
    // Khi đổi mật khẩu, jwt_issued_at được cập nhật để token cũ hết hiệu lực
    if let Some(min_iat) = store.jwt_issued_at().await
        && payload.iat < min_iat
    {
        return false;
    }

    true
}

// ── Giới hạn lỗi đăng nhập ─────────────────────────────────────────────────

/// Số lần thất bại tối đa
const MAX_FAILURES: u64 = 5;
/// Thời lượng khóa
const LOCKOUT_SECS: u64 = 300; // 5 phút

pub struct LoginLimiter {
    fail_count: AtomicU64,
    locked_until: AtomicU64, // epoch secs, 0 nghĩa là chưa khóa
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self {
            fail_count: AtomicU64::new(0),
            locked_until: AtomicU64::new(0),
        }
    }

    /// Kiểm tra có bị khóa không
    pub fn is_locked(&self) -> bool {
        let until = self.locked_until.load(Ordering::Relaxed);
        if until == 0 {
            return false;
        }
        if epoch_secs() >= until {
            // Khóa đã hết hạn, reset
            self.locked_until.store(0, Ordering::Relaxed);
            self.fail_count.store(0, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Ghi nhận một lần thất bại
    pub fn record_failure(&self) {
        let count = self.fail_count.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= MAX_FAILURES {
            self.locked_until
                .store(epoch_secs() + LOCKOUT_SECS, Ordering::Relaxed);
        }
    }

    /// Ghi nhận thành công, reset bộ đếm
    pub fn record_success(&self) {
        self.fail_count.store(0, Ordering::Relaxed);
        self.locked_until.store(0, Ordering::Relaxed);
    }

    /// Số giây khóa còn lại
    pub fn remaining_lock_secs(&self) -> u64 {
        let until = self.locked_until.load(Ordering::Relaxed);
        if until == 0 {
            return 0;
        }
        let now = epoch_secs();
        until.saturating_sub(now)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Hàm quản lý cấp cao ────────────────────────────────────────────────────

/// Đặt mật khẩu admin lần đầu, trả về JWT token
pub async fn setup_admin(
    store: &StoreManager,
    limiter: &LoginLimiter,
    password: &str,
) -> Result<String, String> {
    if store.has_password().await {
        return Err("Mật khẩu đã được đặt, hãy dùng API đăng nhập".into());
    }

    if limiter.is_locked() {
        return Err(format!(
            "Quá nhiều yêu cầu, hãy thử lại sau {} giây",
            limiter.remaining_lock_secs()
        ));
    }

    if password.len() < 6 {
        limiter.record_failure();
        return Err("Mật khẩu phải có tối thiểu 6 ký tự".into());
    }

    let password_hash = super::store::hash_password(password);
    let jwt_secret = super::store::generate_hex_secret();
    store
        .save_admin(password_hash, jwt_secret, 0)
        .await
        .map_err(|e| format!("Lưu thất bại: {}", e))?;

    sign_jwt(store)
        .await
        .ok_or_else(|| "Không ký được JWT".into())
}

/// Đăng nhập bằng mật khẩu, trả về JWT token
pub async fn login_admin(
    store: &StoreManager,
    limiter: &LoginLimiter,
    password: &str,
) -> Result<String, String> {
    if !store.has_password().await {
        return Err("Chưa đặt mật khẩu, hãy dùng API setup trước".into());
    }

    if limiter.is_locked() {
        return Err(format!(
            "Đăng nhập sai quá nhiều lần, hãy thử lại sau {} giây",
            limiter.remaining_lock_secs()
        ));
    }

    if store.verify_password(password).await {
        limiter.record_success();
        sign_jwt(store)
            .await
            .ok_or_else(|| "Không ký được JWT".into())
    } else {
        limiter.record_failure();
        Err("Mật khẩu không đúng".into())
    }
}
