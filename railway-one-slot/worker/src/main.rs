use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ds_free_api::config::{
    Account as AccountConfig, AdminConfig, ContextConfig, DeepSeekConfig, ProxyConfig, ServerConfig,
};
use ds_free_api::{ChatCompletionsRequest, ChatOutput, Config, OpenAIAdapter};
use futures_util::StreamExt;
use mimalloc::MiMalloc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const MAX_CHUNK_BYTES: usize = 256 * 1024;
const DEFAULT_MODEL: &str = "deepseek-default";
const DEFAULT_COMPRESSION_PROMPT: &str = "You are a lossless context compressor. Compress the user text into compact notes. Preserve facts, numbers, names, code, API details, decisions, constraints, and unresolved questions. Remove filler and repetition. Return compressed text only.";

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

type SseResult = Result<Event, Infallible>;

#[derive(Clone)]
struct WorkerState {
    adapter: Arc<OpenAIAdapter>,
    model: Arc<str>,
    compression_prompt: Arc<str>,
}

#[derive(Serialize)]
struct AccountInfo {
    index: usize,
    label: String,
    email: String,
    mobile: String,
    state: String,
    busy_for_seconds: Option<u64>,
    active_count: usize,
    max_concurrent: usize,
    last_released_ms: i64,
    error_count: u8,
}

#[derive(Deserialize)]
struct AddAccountsRequest {
    accounts: String,
}

#[derive(Serialize)]
struct AddAccountsResponse {
    added: usize,
    total: usize,
    errors: Vec<String>,
}

#[derive(Debug)]
enum WorkerError {
    BadRequest(String),
    Busy,
    Conflict(String),
    Internal(String),
}

impl IntoResponse for WorkerError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Busy => (
                StatusCode::SERVICE_UNAVAILABLE,
                "all accounts are busy".to_string(),
            ),
            Self::Conflict(message) => (StatusCode::CONFLICT, message),
            Self::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

#[tokio::main]
async fn main() -> Result<(), WorkerError> {
    env_logger::init();

    let port = env_u16("PORT", 8080);
    let config = config_from_env(port)?;
    let adapter = OpenAIAdapter::new(&config)
        .await
        .map_err(|e| WorkerError::Internal(format!("init DeepSeek adapter failed: {e}")))?;

    let state = WorkerState {
        adapter: Arc::new(adapter),
        model: Arc::from(env_string("COMPRESSION_MODEL", DEFAULT_MODEL)),
        compression_prompt: Arc::from(env_string("COMPRESSION_PROMPT", DEFAULT_COMPRESSION_PROMPT)),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/models", get(models))
        .route("/models/{id}", get(model))
        .route("/v1/models", get(models))
        .route("/v1/models/{id}", get(model))
        .route("/chat/completions", post(chat_completions))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/compress", post(compress))
        .route(
            "/api/admin/accounts",
            get(admin_accounts).post(admin_add_accounts),
        )
        .route(
            "/api/admin/accounts/{index}",
            axum::routing::delete(admin_delete_account),
        )
        .layer(DefaultBodyLimit::max(MAX_CHUNK_BYTES))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| WorkerError::Internal(format!("bind {addr} failed: {e}")))?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| WorkerError::Internal(format!("server failed: {e}")))?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn health(State(state): State<WorkerState>) -> Json<serde_json::Value> {
    let accounts = sorted_accounts(&state);
    let idle = accounts.iter().filter(|a| a.state == "idle").count();
    let busy = accounts.iter().filter(|a| a.state == "busy").count();
    let error = accounts.iter().filter(|a| a.state == "error").count();
    let invalid = accounts.iter().filter(|a| a.state == "invalid").count();

    Json(json!({
        "ok": true,
        "accounts": accounts.len(),
        "idle": idle,
        "busy": busy,
        "error": error,
        "invalid": invalid
    }))
}

async fn admin_accounts(State(state): State<WorkerState>) -> Json<serde_json::Value> {
    Json(json!({ "accounts": sorted_accounts(&state) }))
}

async fn admin_add_accounts(
    State(state): State<WorkerState>,
    Json(req): Json<AddAccountsRequest>,
) -> Result<Json<AddAccountsResponse>, WorkerError> {
    let imported = parse_account_lines(&req.accounts);
    if imported.is_empty() {
        return Err(WorkerError::BadRequest(
            "no usable accounts in request".to_string(),
        ));
    }

    let mut added = 0usize;
    let mut errors = Vec::new();
    for account in imported {
        let label = account_label(&account);
        match state.adapter.add_account(&account).await {
            Ok(_) => added += 1,
            Err(e) => errors.push(format!("{label}: {e}")),
        }
    }

    let total = state.adapter.account_statuses().len();
    Ok(Json(AddAccountsResponse {
        added,
        total,
        errors,
    }))
}

async fn admin_delete_account(
    State(state): State<WorkerState>,
    Path(index): Path<usize>,
) -> Result<StatusCode, WorkerError> {
    let accounts = sorted_accounts(&state);
    let Some(account) = accounts.get(index) else {
        return Err(WorkerError::BadRequest(
            "account index not found".to_string(),
        ));
    };

    let id = if account.email.is_empty() {
        account.mobile.as_str()
    } else {
        account.email.as_str()
    };

    state
        .adapter
        .remove_account(id)
        .await
        .map_err(|e| WorkerError::Conflict(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

async fn models(State(state): State<WorkerState>) -> Response {
    let bytes = serde_json::to_vec(&state.adapter.list_models().await).unwrap();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(bytes))
        .unwrap()
}

async fn model(State(state): State<WorkerState>, Path(id): Path<String>) -> Response {
    match state.adapter.get_model(&id).await {
        Some(model) => {
            let bytes = serde_json::to_vec(&model).unwrap();
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(bytes))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({ "error": format!("model not found: {id}") }).to_string(),
            ))
            .unwrap(),
    }
}

async fn chat_completions(
    State(state): State<WorkerState>,
    Json(req): Json<ChatCompletionsRequest>,
) -> Result<Response, WorkerError> {
    let request_id = next_request_id();
    let stream_requested = req.stream;
    let result = state
        .adapter
        .chat_completions(req, &request_id)
        .await
        .map_err(|e| WorkerError::Internal(e.to_string()))?;

    match result.data {
        ChatOutput::Stream(stream) => {
            let (tx, rx) = mpsc::channel(256);
            tokio::spawn(stream_openai_chunks(stream, tx));
            Ok(Sse::new(ReceiverStream::new(rx))
                .keep_alive(KeepAlive::default())
                .into_response())
        }
        ChatOutput::Json(json) => {
            if stream_requested {
                return Err(WorkerError::Internal(
                    "stream request returned JSON response".to_string(),
                ));
            }
            let bytes = serde_json::to_vec(&json)
                .map_err(|e| WorkerError::Internal(format!("serialize response failed: {e}")))?;
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(bytes))
                .unwrap())
        }
    }
}

async fn compress(State(state): State<WorkerState>, body: Bytes) -> Result<Response, WorkerError> {
    if body.is_empty() {
        return Err(WorkerError::BadRequest("payload is empty".to_string()));
    }

    let text = std::str::from_utf8(&body)
        .map_err(|_| WorkerError::BadRequest("payload must be valid UTF-8 text".to_string()))?;
    let req = build_compression_request(&state, text)?;
    let request_id = next_request_id();

    let result = state
        .adapter
        .chat_completions(req, &request_id)
        .await
        .map_err(|e| {
            let message = e.to_string();
            if message.contains("no available account") {
                WorkerError::Busy
            } else {
                WorkerError::Internal(message)
            }
        })?;

    let ChatOutput::Stream(stream) = result.data else {
        return Err(WorkerError::Internal(
            "compression request did not return a stream".to_string(),
        ));
    };

    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(stream_text_deltas(stream, tx));

    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

async fn stream_openai_chunks(
    mut stream: ds_free_api::openai_adapter::ChunkStream,
    tx: mpsc::Sender<SseResult>,
) {
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                let Ok(data) = serde_json::to_string(&chunk) else {
                    let _ = tx
                        .send(Ok(Event::default()
                            .data(json!({ "error": "serialize stream chunk failed" }).to_string())))
                        .await;
                    return;
                };
                if tx.send(Ok(Event::default().data(data))).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Ok(Event::default()
                        .data(json!({ "error": e.to_string() }).to_string())))
                    .await;
                return;
            }
        }
    }

    let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
}

async fn stream_text_deltas(
    mut stream: ds_free_api::openai_adapter::ChunkStream,
    tx: mpsc::Sender<SseResult>,
) {
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                for choice in chunk.choices {
                    if let Some(content) = choice.delta.content
                        && !content.is_empty()
                        && tx
                            .send(Ok(Event::default().event("delta").data(content)))
                            .await
                            .is_err()
                    {
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Ok(Event::default().event("error").data(e.to_string())))
                    .await;
                return;
            }
        }
    }

    let _ = tx
        .send(Ok(Event::default().event("done").data("[DONE]")))
        .await;
}

fn build_compression_request(
    state: &WorkerState,
    text: &str,
) -> Result<ChatCompletionsRequest, WorkerError> {
    let value = json!({
        "model": state.model.as_ref(),
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": [
            { "role": "system", "content": state.compression_prompt.as_ref() },
            { "role": "user", "content": text }
        ]
    });

    serde_json::from_value(value)
        .map_err(|e| WorkerError::Internal(format!("build compression request failed: {e}")))
}

fn sorted_accounts(state: &WorkerState) -> Vec<AccountInfo> {
    let mut accounts = state
        .adapter
        .account_statuses()
        .into_iter()
        .map(|account| {
            let label = if account.email.is_empty() {
                account.mobile.clone()
            } else {
                account.email.clone()
            };
            AccountInfo {
                index: 0,
                label,
                email: account.email,
                mobile: account.mobile,
                state: account.state,
                busy_for_seconds: None,
                active_count: account.active_count,
                max_concurrent: account.max_concurrent,
                last_released_ms: account.last_released_ms,
                error_count: account.error_count,
            }
        })
        .collect::<Vec<_>>();

    accounts.sort_by(|a, b| a.label.cmp(&b.label));
    for (index, account) in accounts.iter_mut().enumerate() {
        account.index = index;
    }
    accounts
}

fn config_from_env(port: u16) -> Result<Config, WorkerError> {
    let mut deepseek = DeepSeekConfig::default();
    deepseek.api_base = env_string("UPSTREAM_BASE_URL", &deepseek.api_base);
    deepseek.wasm_url = env_string("WASM_URL", &deepseek.wasm_url);
    deepseek.user_agent = env_string("USER_AGENT", &deepseek.user_agent);
    deepseek.max_concurrent_per_account = env_usize(
        "MAX_CONCURRENT_PER_ACCOUNT",
        deepseek.max_concurrent_per_account,
    );

    Ok(Config {
        accounts: load_accounts_from_env(),
        deepseek,
        context: ContextConfig::default(),
        server: ServerConfig {
            host: "0.0.0.0".to_string(),
            port,
            cors_origins: Vec::new(),
        },
        proxy: ProxyConfig {
            url: std::env::var("PROXY_URL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        },
        admin: AdminConfig::default(),
        api_keys: Vec::new(),
    })
}

fn load_accounts_from_env() -> Vec<AccountConfig> {
    std::env::var("ACCOUNTS")
        .ok()
        .map(|raw| parse_account_lines(&raw))
        .unwrap_or_default()
}

fn parse_account_lines(raw: &str) -> Vec<AccountConfig> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(parse_account_line)
        .collect()
}

fn parse_account_line(line: &str) -> Option<AccountConfig> {
    let sep = ['|', ':', ';', '\t', ',']
        .into_iter()
        .find(|sep| line.contains(*sep))?;

    let parts = line.split(sep).map(str::trim).collect::<Vec<_>>();
    if parts.len() >= 3 && parts[0].starts_with('+') {
        let password = parts[2..].join(&sep.to_string());
        return Some(AccountConfig {
            email: String::new(),
            mobile: parts[1].to_string(),
            area_code: parts[0].to_string(),
            password,
        });
    }

    if parts.len() >= 2 {
        let password = parts[1..].join(&sep.to_string());
        let login = parts[0].to_string();
        let is_email = login.contains('@');
        return Some(AccountConfig {
            email: if is_email {
                login.clone()
            } else {
                String::new()
            },
            mobile: if is_email { String::new() } else { login },
            area_code: String::new(),
            password,
        });
    }

    None
}

fn account_label(account: &AccountConfig) -> String {
    if account.email.is_empty() {
        account.mobile.clone()
    } else {
        account.email.clone()
    }
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_u16(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn next_request_id() -> String {
    let id = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("worker-{id}")
}
