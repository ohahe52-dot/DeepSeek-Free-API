//! Handler route API quản trị - đăng nhập/đặt mật khẩu, trạng thái pool, thống kê request, danh sách model, xem cấu hình

use axum::{
    body::Body,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use super::handlers::AppState;
use crate::config::Config;

// ── Kiểu request/response ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SetupRequest {
    pub password: String,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub token: String,
}

#[derive(Serialize)]
pub struct AdminStatusResponse {
    pub accounts: Vec<crate::ds_core::AccountStatus>,
    pub total: usize,
    pub idle: usize,
    pub busy: usize,
    pub error: usize,
    pub invalid: usize,
}

#[derive(Serialize)]
pub struct AdminStatsResponse {
    #[serde(flatten)]
    pub stats: super::stats::StatsSnapshot,
}

#[derive(Serialize)]
pub struct AdminConfigResponse {
    pub server: ServerConfigView,
    pub deepseek: DeepSeekConfigView,
    pub context: crate::config::ContextConfig,
    pub accounts: Vec<AccountView>,
    pub proxy: ProxyConfigView,
    pub admin: AdminConfigView,
    pub api_keys: Vec<ApiKeyEntryView>,
}

#[derive(Serialize)]
pub struct ServerConfigView {
    pub host: String,
    pub port: u16,
    pub cors_origins: Vec<String>,
}

#[derive(Serialize)]
pub struct DeepSeekConfigView {
    pub api_base: String,
    pub wasm_url: String,
    pub user_agent: String,
    pub client_version: String,
    pub client_platform: String,
    pub client_locale: String,
    pub model_types: Vec<String>,
    pub max_input_tokens: Vec<u32>,
    pub max_output_tokens: Vec<u32>,
    pub input_character_limits: Vec<u32>,
    pub max_concurrent_per_account: usize,
    pub model_aliases: Vec<String>,
    pub tool_call: ToolCallTagConfigView,
}

#[derive(Serialize)]
pub struct ToolCallTagConfigView {
    pub extra_starts: Vec<String>,
    pub extra_ends: Vec<String>,
}

#[derive(Serialize)]
pub struct ProxyConfigView {
    pub url: Option<String>,
}

#[derive(Serialize)]
pub struct AdminConfigView {
    pub password_set: bool,
    pub jwt_issued_at: u64,
}

#[derive(Serialize)]
pub struct ApiKeyEntryView {
    pub key: String,
    pub description: String,
}
#[derive(Serialize)]
pub struct AccountView {
    pub email: String,
    pub mobile: String,
    pub area_code: String,
    pub password: String,
}

// ── Che dữ liệu ────────────────────────────────────────────────────────────

fn mask_config(config: &Config) -> AdminConfigResponse {
    AdminConfigResponse {
        server: ServerConfigView {
            host: config.server.host.clone(),
            port: config.server.port,
            cors_origins: config.server.cors_origins.clone(),
        },
        deepseek: DeepSeekConfigView {
            api_base: config.deepseek.api_base.clone(),
            wasm_url: config.deepseek.wasm_url.clone(),
            user_agent: config.deepseek.user_agent.clone(),
            client_version: config.deepseek.client_version.clone(),
            client_platform: config.deepseek.client_platform.clone(),
            client_locale: config.deepseek.client_locale.clone(),
            model_types: config.deepseek.model_types.clone(),
            max_input_tokens: config.deepseek.max_input_tokens.clone(),
            max_output_tokens: config.deepseek.max_output_tokens.clone(),
            input_character_limits: config.deepseek.input_character_limits.clone(),
            max_concurrent_per_account: config.deepseek.max_concurrent_per_account,
            model_aliases: config.deepseek.model_aliases.clone(),
            tool_call: ToolCallTagConfigView {
                extra_starts: config.deepseek.tool_call.extra_starts.clone(),
                extra_ends: config.deepseek.tool_call.extra_ends.clone(),
            },
        },
        context: config.context.clone(),
        proxy: ProxyConfigView {
            url: config.proxy.url.clone(),
        },
        admin: AdminConfigView {
            password_set: !config.admin.password_hash.is_empty(),
            jwt_issued_at: config.admin.jwt_issued_at,
        },
        api_keys: config
            .api_keys
            .iter()
            .map(|k| ApiKeyEntryView {
                key: k.key.clone(),
                description: k.description.clone(),
            })
            .collect(),
        accounts: config
            .accounts
            .iter()
            .map(|a| AccountView {
                email: a.email.clone(),
                mobile: a.mobile.clone(),
                area_code: a.area_code.clone(),
                password: a.password.clone(),
            })
            .collect(),
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────

/// POST /admin/api/setup - đặt mật khẩu lần đầu
pub(crate) async fn admin_setup(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    let req: SetupRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Định dạng yêu cầu sai: {}", e),
            );
        }
    };

    match super::auth::setup_admin(&state.store, &state.login_limiter, &req.password).await {
        Ok(token) => json_response(&LoginResponse { token }),
        Err(msg) => {
            let status = if msg.contains("đã được đặt") {
                StatusCode::FORBIDDEN
            } else if msg.contains("Quá nhiều") {
                StatusCode::TOO_MANY_REQUESTS
            } else if msg.contains("tối thiểu 6") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            error_response(status, &msg)
        }
    }
}

pub(crate) async fn admin_login(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    let req: LoginRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Định dạng yêu cầu sai: {}", e),
            );
        }
    };

    match super::auth::login_admin(&state.store, &state.login_limiter, &req.password).await {
        Ok(token) => json_response(&LoginResponse { token }),
        Err(msg) => {
            let status = if msg.contains("quá nhiều") || msg.contains("Quá nhiều") {
                StatusCode::TOO_MANY_REQUESTS
            } else if msg.contains("Chưa đặt mật khẩu") {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::UNAUTHORIZED
            };
            error_response(status, &msg)
        }
    }
}

/// GET /admin/api/status
pub(crate) async fn admin_status(State(state): State<AppState>) -> Response {
    let statuses = state.adapter.account_statuses();
    let total = statuses.len();
    let busy = statuses.iter().filter(|a| a.state == "busy").count();
    let idle = statuses.iter().filter(|a| a.state == "idle").count();
    let error = statuses.iter().filter(|a| a.state == "error").count();
    let invalid = statuses.iter().filter(|a| a.state == "invalid").count();

    let resp = AdminStatusResponse {
        accounts: statuses,
        total,
        idle,
        busy,
        error,
        invalid,
    };
    json_response(&resp)
}

/// GET /admin/api/stats
pub(crate) async fn admin_stats(State(state): State<AppState>) -> Response {
    let snapshot = state.stats.snapshot();
    let resp = AdminStatsResponse { stats: snapshot };
    json_response(&resp)
}

/// GET /admin/api/models
pub(crate) async fn admin_models(State(state): State<AppState>) -> Response {
    let models = state.adapter.list_models().await;
    json_response(&models)
}

/// GET /admin/api/config
pub(crate) async fn admin_config(State(state): State<AppState>) -> Response {
    let config = state.config.read().await;
    let config_view = mask_config(&config);
    json_response(&config_view)
}

/// PUT /admin/api/config - cập nhật và hot reload cấu hình
pub(crate) async fn admin_put_config(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    let mut new_config: Config = match serde_json::from_slice(&body) {
        Ok(c) => c,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Phân tích JSON thất bại: {}", e),
            );
        }
    };

    // Validate
    if let Err(e) = new_config.validate() {
        return error_response(StatusCode::BAD_REQUEST, &e.to_string());
    }
    // Merge: empty/"***" passwords keep existing values from current config;
    // API keys match by `id` (stable identifier), falling back to description for old-format migration.
    {
        let current = state.config.read().await;
        for a in &mut new_config.accounts {
            if (a.password.is_empty() || a.password == "***")
                && let Some(existing) = current
                    .accounts
                    .iter()
                    .find(|e| e.email == a.email && e.mobile == a.mobile)
            {
                a.password.clone_from(&existing.password);
            }
        }
        // Cấu hình admin: password_hash/jwt_secret rỗng thì giữ giá trị cũ (frontend không trả các field này)
        if new_config.admin.password_hash.is_empty() {
            new_config
                .admin
                .password_hash
                .clone_from(&current.admin.password_hash);
        }
        if new_config.admin.jwt_secret.is_empty() {
            new_config
                .admin
                .jwt_secret
                .clone_from(&current.admin.jwt_secret);
        }
        // Đổi mật khẩu: frontend gửi old_password + new_password
        if !new_config.admin.old_password.is_empty() || !new_config.admin.new_password.is_empty() {
            if new_config.admin.old_password.is_empty() || new_config.admin.new_password.is_empty()
            {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "Đổi mật khẩu cần cung cấp cả mật khẩu cũ và mật khẩu mới",
                );
            }
            if !bcrypt::verify(&new_config.admin.old_password, &current.admin.password_hash)
                .unwrap_or(false)
            {
                return error_response(StatusCode::BAD_REQUEST, "Mật khẩu cũ không đúng");
            }
            new_config.admin.password_hash =
                super::store::hash_password(&new_config.admin.new_password);
            new_config.admin.jwt_secret = super::store::generate_hex_secret();
            new_config.admin.jwt_issued_at += 1;
        }
    }

    // Persist
    {
        let mut guard = state.config.write().await;
        *guard = new_config.clone();
        if let Err(e) = guard.save(&state.config_path) {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Lưu thất bại: {}", e),
            );
        }
    }

    // Hot-reload adapter config.
    if let Err(e) = state.adapter.reload_config(&new_config).await {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Hot reload failed: {}", e),
        );
    }
    json_response(&serde_json::json!({"ok": true}))
}

#[derive(Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    50
}

/// GET /admin/api/logs - lấy log request gần nhất
pub(crate) async fn admin_logs(
    Query(query): Query<LogsQuery>,
    State(state): State<AppState>,
) -> Response {
    let logs = state.stats.recent_logs(query.limit);
    json_response(&logs)
}

#[derive(Deserialize)]
pub struct RuntimeLogsQuery {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_runtime_limit")]
    pub limit: usize,
}

fn default_runtime_limit() -> usize {
    100
}

/// GET /admin/api/runtime-logs - truy vấn log runtime có phân trang
pub(crate) async fn admin_runtime_logs(Query(query): Query<RuntimeLogsQuery>) -> Response {
    let (total, logs) = super::runtime_log::query_logs(query.offset, query.limit).await;
    json_response(&serde_json::json!({
        "total": total,
        "offset": query.offset,
        "limit": query.limit,
        "logs": logs,
    }))
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn json_response<T: Serialize>(data: &T) -> Response {
    match serde_json::to_vec(data) {
        Ok(bytes) => match Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
        {
            Ok(response) => response,
            Err(error) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Tạo phản hồi JSON thất bại: {}", error),
            ),
        },
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Phân tích JSON thất bại: {}", error),
        ),
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({"error": message});
    match serde_json::to_vec(&body) {
        Ok(bytes) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| simple_error_response(status, message)),
        Err(_) => simple_error_response(status, message),
    }
}

fn simple_error_response(status: StatusCode, message: &str) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], message.to_string())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::json_response;
    use axum::{body::to_bytes, http::{StatusCode, header}};
    use serde::ser::{Error as _, Serialize, Serializer};

    struct BrokenSerialize;

    impl Serialize for BrokenSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(S::Error::custom("broken serialize"))
        }
    }

    #[tokio::test]
    async fn json_response_returns_200_for_valid_payload() {
        let response = json_response(&serde_json::json!({"ok": true}));

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn json_response_returns_500_when_serialization_fails() {
        let response = json_response(&BrokenSerialize);
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(text.contains("broken serialize"));
    }
}
