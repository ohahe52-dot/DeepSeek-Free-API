use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use mimalloc::MiMalloc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const MAX_CHUNK_BYTES: usize = 256 * 1024;
const STATE_IDLE: u8 = 0;
const STATE_BUSY: u8 = 1;
const STATE_ERROR: u8 = 2;

type SseResult = Result<Event, Infallible>;

#[derive(Clone)]
struct WorkerState {
    client: Client,
    upstream_base: Arc<str>,
    accounts: Arc<RwLock<Vec<Arc<Account>>>>,
}

#[derive(Debug, Clone, Copy)]
enum AccountState {
    Idle,
    Busy(Instant),
    Error,
}

struct Account {
    label: Arc<str>,
    token: Arc<str>,
    state: AtomicU8,
    busy_since: Mutex<Option<Instant>>,
    semaphore: Arc<Semaphore>,
}

struct AccountLease {
    account: Arc<Account>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Serialize)]
struct ProviderRequest<'a> {
    text: &'a str,
    stream: bool,
}

#[derive(Serialize)]
struct AccountInfo {
    index: usize,
    label: String,
    state: &'static str,
    busy_for_seconds: Option<u64>,
}

#[derive(Deserialize)]
struct AddAccountsRequest {
    accounts: String,
}

#[derive(Serialize)]
struct AddAccountsResponse {
    added: usize,
    total: usize,
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

impl Account {
    fn new(label: impl Into<Arc<str>>, token: impl Into<Arc<str>>) -> Self {
        Self {
            label: label.into(),
            token: token.into(),
            state: AtomicU8::new(STATE_IDLE),
            busy_since: Mutex::new(None),
            semaphore: Arc::new(Semaphore::new(1)),
        }
    }

    async fn snapshot(&self) -> AccountState {
        match self.state.load(Ordering::Acquire) {
            STATE_IDLE => AccountState::Idle,
            STATE_BUSY => {
                AccountState::Busy(self.busy_since.lock().await.unwrap_or_else(Instant::now))
            }
            _ => AccountState::Error,
        }
    }

    async fn mark_busy(&self) {
        *self.busy_since.lock().await = Some(Instant::now());
        self.state.store(STATE_BUSY, Ordering::Release);
    }

    async fn mark_idle(&self) {
        *self.busy_since.lock().await = None;
        self.state.store(STATE_IDLE, Ordering::Release);
    }

    fn mark_error(&self) {
        self.state.store(STATE_ERROR, Ordering::Release);
    }
}

#[tokio::main]
async fn main() -> Result<(), WorkerError> {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(8080);

    let upstream_base = std::env::var("UPSTREAM_BASE_URL")
        .unwrap_or_else(|_| "https://provider.example.com".to_string());

    let accounts = load_accounts()?;
    let client = Client::builder()
        .http2_adaptive_window(true)
        .pool_max_idle_per_host(128)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .build()
        .map_err(|e| WorkerError::Internal(format!("build HTTP client failed: {e}")))?;

    let state = WorkerState {
        client,
        upstream_base: Arc::from(upstream_base),
        accounts: Arc::new(RwLock::new(accounts)),
    };

    let app = Router::new()
        .route("/health", get(health))
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
    let mut idle = 0usize;
    let mut busy = 0usize;
    let mut error = 0usize;
    let accounts = state.accounts.read().await;

    for account in accounts.iter() {
        match account.snapshot().await {
            AccountState::Idle => idle += 1,
            AccountState::Busy(since) => {
                let _busy_for = since.elapsed();
                busy += 1;
            }
            AccountState::Error => error += 1,
        }
    }

    Json(json!({
        "ok": true,
        "accounts": accounts.len(),
        "idle": idle,
        "busy": busy,
        "error": error
    }))
}

async fn admin_accounts(State(state): State<WorkerState>) -> Json<serde_json::Value> {
    let account_pool = state.accounts.read().await;
    let mut accounts = Vec::with_capacity(account_pool.len());

    for (idx, account) in account_pool.iter().enumerate() {
        let (state_name, busy_for_seconds) = match account.snapshot().await {
            AccountState::Idle => ("idle", None),
            AccountState::Busy(since) => ("busy", Some(since.elapsed().as_secs())),
            AccountState::Error => ("error", None),
        };

        accounts.push(AccountInfo {
            index: idx,
            label: account.label.to_string(),
            state: state_name,
            busy_for_seconds,
        });
    }

    Json(json!({ "accounts": accounts }))
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

    let mut accounts = state.accounts.write().await;
    let start = accounts.len();
    for (offset, credential) in imported.into_iter().enumerate() {
        let label = account_label(&credential, start + offset);
        accounts.push(Arc::new(Account::new(label, credential)));
    }

    Ok(Json(AddAccountsResponse {
        added: accounts.len() - start,
        total: accounts.len(),
    }))
}

async fn admin_delete_account(
    State(state): State<WorkerState>,
    Path(index): Path<usize>,
) -> Result<StatusCode, WorkerError> {
    let mut accounts = state.accounts.write().await;
    let Some(account) = accounts.get(index).cloned() else {
        return Err(WorkerError::BadRequest(
            "account index not found".to_string(),
        ));
    };

    if matches!(account.snapshot().await, AccountState::Busy(_)) {
        return Err(WorkerError::Conflict("account is busy".to_string()));
    }

    accounts.remove(index);
    Ok(StatusCode::NO_CONTENT)
}

async fn compress(
    State(state): State<WorkerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, WorkerError> {
    if body.is_empty() {
        return Err(WorkerError::BadRequest("payload is empty".to_string()));
    }
    let lease = acquire_account(&state).await?;
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(run_provider_stream(state, headers, body, lease, tx));

    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

async fn run_provider_stream(
    state: WorkerState,
    headers: HeaderMap,
    body: Bytes,
    lease: AccountLease,
    tx: mpsc::Sender<SseResult>,
) {
    let text = String::from_utf8_lossy(&body);
    let chunk_index = headers
        .get("x-chunk-index")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0");

    let url = format!("{}/conversation", state.upstream_base.trim_end_matches('/'));
    let response = state
        .client
        .post(&url)
        .bearer_auth(lease.account.token.as_ref())
        .json(&ProviderRequest {
            text: &text,
            stream: true,
        })
        .send()
        .await;

    let mut conversation_id = None;
    match response {
        Ok(response) if response.status().is_success() => {
            conversation_id = response
                .headers()
                .get("x-conversation-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);

            let mut stream = response.bytes_stream();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(bytes) => {
                        let data = String::from_utf8_lossy(&bytes).to_string();
                        if tx
                            .send(Ok(Event::default()
                                .event("delta")
                                .id(chunk_index.to_string())
                                .data(data)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        lease.account.mark_error();
                        let _ = tx
                            .send(Ok(Event::default().event("error").data(e.to_string())))
                            .await;
                        break;
                    }
                }
            }
        }
        Ok(response) => {
            lease.account.mark_error();
            let _ = tx
                .send(Ok(Event::default()
                    .event("error")
                    .data(format!("upstream status {}", response.status()))))
                .await;
        }
        Err(e) => {
            lease.account.mark_error();
            let _ = tx
                .send(Ok(Event::default().event("error").data(e.to_string())))
                .await;
        }
    }

    let _ = tx
        .send(Ok(Event::default().event("done").data("[DONE]")))
        .await;
    tokio::spawn(cleanup_conversation(
        state.client,
        state.upstream_base,
        conversation_id,
        lease,
    ));
}

async fn cleanup_conversation(
    client: Client,
    upstream_base: Arc<str>,
    conversation_id: Option<String>,
    lease: AccountLease,
) {
    let base = upstream_base.trim_end_matches('/');
    let url = conversation_id.map_or_else(
        || format!("{base}/conversation"),
        |id| format!("{base}/conversation/{id}"),
    );

    let _ = client
        .delete(url)
        .bearer_auth(lease.account.token.as_ref())
        .send()
        .await;

    if matches!(lease.account.snapshot().await, AccountState::Busy(_)) {
        lease.account.mark_idle().await;
    }
}

async fn acquire_account(state: &WorkerState) -> Result<AccountLease, WorkerError> {
    let accounts = state.accounts.read().await;
    for account in accounts.iter() {
        if !matches!(account.snapshot().await, AccountState::Idle) {
            continue;
        }
        if let Ok(permit) = account.semaphore.clone().try_acquire_owned() {
            account.mark_busy().await;
            return Ok(AccountLease {
                account: Arc::clone(account),
                _permit: permit,
            });
        }
    }
    Err(WorkerError::Busy)
}

fn load_accounts() -> Result<Vec<Arc<Account>>, WorkerError> {
    let raw = std::env::var("ACCOUNTS").unwrap_or_default();

    let accounts: Vec<Arc<Account>> = raw
        .split([',', '\n'])
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .enumerate()
        .map(|(idx, token)| {
            Arc::new(Account::new(
                account_label(token, idx),
                Arc::<str>::from(token),
            ))
        })
        .collect();

    Ok(accounts)
}

fn parse_account_lines(raw: &str) -> Vec<Arc<str>> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(Arc::<str>::from)
        .collect()
}

fn account_label(credential: &str, index: usize) -> String {
    if !credential
        .chars()
        .any(|ch| matches!(ch, '|' | ':' | ';' | ',' | '\t'))
    {
        return format!("Account {:02}", index + 1);
    }

    let head = credential
        .split(|ch| matches!(ch, '|' | ':' | ';' | ',' | '\t'))
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match head {
        Some(value) if value.len() <= 64 => value.to_string(),
        Some(value) => format!("{}...", value.chars().take(61).collect::<String>()),
        None => format!("Account {:02}", index + 1),
    }
}
