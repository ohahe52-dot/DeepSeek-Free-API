use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::str;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dashmap::DashMap;
use futures_util::StreamExt;
use mimalloc::MiMalloc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const MAX_INPUT_TOKENS: usize = 500_000;
const CHUNK_TOKEN_LIMIT: usize = 12_000;
const APPROX_CHARS_PER_TOKEN: usize = 4;
const MAX_INPUT_BYTES: usize = MAX_INPUT_TOKENS * APPROX_CHARS_PER_TOKEN * 2;
const CACHE_TTL: Duration = Duration::from_secs(86_400);

type SseResult = Result<Event, Infallible>;

#[derive(Clone)]
struct AppState {
    client: Client,
    worker_url: Arc<str>,
    worker_admin_accounts_url: Arc<str>,
    cache: Arc<DashMap<String, CacheEntry>>,
    api_keys: Arc<DashMap<String, ApiKeyEntry>>,
    require_api_key: bool,
    admin_token: Option<Arc<str>>,
    notifier: broadcast::Sender<()>,
}

#[derive(Clone)]
struct CacheEntry {
    value: Arc<str>,
    created_at: SystemTime,
    expires_at: Instant,
}

#[derive(Serialize)]
struct CacheInfo {
    key: String,
    created_at_unix: u64,
    expires_in_seconds: u64,
}

#[derive(Clone)]
struct ApiKeyEntry {
    hash: String,
    prefix: String,
    name: String,
    created_at: SystemTime,
}

#[derive(Serialize)]
struct ApiKeyInfo {
    id: String,
    prefix: String,
    name: String,
    created_at_unix: u64,
}

#[derive(Deserialize)]
struct CreateApiKeyRequest {
    name: Option<String>,
}

#[derive(Serialize)]
struct CreateApiKeyResponse {
    id: String,
    key: String,
    prefix: String,
    name: String,
    created_at_unix: u64,
}

#[derive(Debug)]
enum AppError {
    BadRequest(String),
    Unauthorized(String),
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Unauthorized(message) => (StatusCode::UNAUTHORIZED, message),
            Self::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(8080);

    let worker_url = std::env::var("WORKER_URL")
        .unwrap_or_else(|_| "http://worker.railway.internal:8080/v1/compress".to_string());
    let worker_admin_accounts_url = std::env::var("WORKER_ADMIN_ACCOUNTS_URL")
        .unwrap_or_else(|_| worker_url.replace("/v1/compress", "/api/admin/accounts"));
    let admin_token = std::env::var("ADMIN_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(Arc::<str>::from);
    let require_api_key = std::env::var("REQUIRE_API_KEY")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    let api_keys = Arc::new(load_api_keys_from_env());

    let client = Client::builder()
        .http2_adaptive_window(true)
        .pool_max_idle_per_host(128)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .build()
        .map_err(|e| AppError::Internal(format!("build HTTP client failed: {e}")))?;

    let (notifier, _) = broadcast::channel(128);
    let state = AppState {
        client,
        worker_url: Arc::from(worker_url),
        worker_admin_accounts_url: Arc::from(worker_admin_accounts_url),
        cache: Arc::new(DashMap::new()),
        api_keys,
        require_api_key,
        admin_token,
        notifier,
    };

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/health", get(|| async { "ok" }))
        .route("/v1/compress", post(compress))
        .route(
            "/api/admin/accounts",
            get(admin_accounts).post(admin_add_accounts),
        )
        .route(
            "/api/admin/accounts/{index}",
            axum::routing::delete(admin_delete_account),
        )
        .route("/api/admin/cache", get(admin_cache))
        .route(
            "/api/admin/cache/{key}",
            axum::routing::delete(admin_delete_cache),
        )
        .route(
            "/api/admin/api-keys",
            get(admin_api_keys).post(admin_create_api_key),
        )
        .route(
            "/api/admin/api-keys/{id}",
            axum::routing::delete(admin_delete_api_key),
        )
        .route("/api/admin/events", get(admin_events))
        .layer(DefaultBodyLimit::max(MAX_INPUT_BYTES))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| AppError::Internal(format!("bind {addr} failed: {e}")))?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| AppError::Internal(format!("server failed: {e}")))?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn dashboard() -> Html<&'static str> {
    Html(include_str!("../static/dashboard.html"))
}

async fn compress(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    require_client_api_key(&state, &headers)?;

    let cache_key = cache_key(&body);
    if let Some(hit) = cache_get(&state.cache, &cache_key) {
        let (tx, rx) = mpsc::channel::<SseResult>(4);
        let _ = tx
            .send(Ok(Event::default()
                .event("compressed")
                .data(hit.to_string())))
            .await;
        let _ = tx
            .send(Ok(Event::default().event("done").data("[DONE]")))
            .await;
        return Ok(Sse::new(ReceiverStream::new(rx))
            .keep_alive(KeepAlive::default())
            .into_response());
    }

    let chunks = split_semantic(&body)?;
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(stream_from_worker(state, cache_key, chunks, tx));

    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

async fn stream_from_worker(
    state: AppState,
    cache_key: String,
    chunks: Vec<Bytes>,
    tx: mpsc::Sender<SseResult>,
) {
    let mut compressed = String::new();
    let total = chunks.len();

    for (idx, chunk) in chunks.into_iter().enumerate() {
        let response = state
            .client
            .post(state.worker_url.as_ref())
            .header("content-type", "text/plain; charset=utf-8")
            .header("x-chunk-index", idx.to_string())
            .header("x-chunk-total", total.to_string())
            .body(chunk)
            .send()
            .await;

        let Ok(response) = response else {
            let _ = tx
                .send(Ok(Event::default()
                    .event("error")
                    .data("worker request failed")))
                .await;
            return;
        };

        if !response.status().is_success() {
            let _ = tx
                .send(Ok(Event::default()
                    .event("error")
                    .data(format!("worker status {}", response.status()))))
                .await;
            return;
        }

        let mut sse_buffer = String::new();
        let mut stream = response.bytes_stream();
        while let Some(next) = stream.next().await {
            let Ok(bytes) = next else {
                let _ = tx
                    .send(Ok(Event::default()
                        .event("error")
                        .data("worker stream failed")))
                    .await;
                return;
            };

            sse_buffer.push_str(&String::from_utf8_lossy(&bytes));
            for data in drain_sse_data(&mut sse_buffer) {
                if data == "[DONE]" {
                    continue;
                }
                compressed.push_str(&data);
                if tx
                    .send(Ok(Event::default().event("delta").data(data)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }

    state.cache.insert(
        cache_key,
        CacheEntry {
            value: Arc::from(compressed),
            created_at: SystemTime::now(),
            expires_at: Instant::now() + CACHE_TTL,
        },
    );
    let _ = state.notifier.send(());

    let _ = tx
        .send(Ok(Event::default().event("done").data("[DONE]")))
        .await;
}

async fn admin_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, AppError> {
    require_admin(&state, &headers, &query)?;

    let response = state
        .client
        .get(state.worker_admin_accounts_url.as_ref())
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("worker admin request failed: {e}")))?;

    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| AppError::Internal(format!("worker admin body failed: {e}")))?;

    build_json_proxy_response(status, bytes)
}

async fn admin_add_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Response, AppError> {
    require_admin(&state, &headers, &query)?;

    let response = state
        .client
        .post(state.worker_admin_accounts_url.as_ref())
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("worker add accounts failed: {e}")))?;

    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| AppError::Internal(format!("worker add accounts body failed: {e}")))?;
    let _ = state.notifier.send(());

    build_json_proxy_response(status, bytes)
}

async fn admin_delete_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Path(index): Path<usize>,
) -> Result<StatusCode, AppError> {
    require_admin(&state, &headers, &query)?;

    let url = format!(
        "{}/{}",
        state.worker_admin_accounts_url.trim_end_matches('/'),
        index
    );
    let response = state
        .client
        .delete(url)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("worker delete account failed: {e}")))?;
    let _ = state.notifier.send(());

    Ok(response.status())
}

async fn admin_cache(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Vec<CacheInfo>>, AppError> {
    require_admin(&state, &headers, &query)?;
    Ok(Json(cache_entries(&state.cache)))
}

async fn admin_delete_cache(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Path(key): Path<String>,
) -> Result<StatusCode, AppError> {
    require_admin(&state, &headers, &query)?;
    state.cache.remove(&key);
    let _ = state.notifier.send(());
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_api_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Vec<ApiKeyInfo>>, AppError> {
    require_admin(&state, &headers, &query)?;
    Ok(Json(api_key_entries(&state.api_keys)))
}

async fn admin_create_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(req): Json<CreateApiKeyRequest>,
) -> Result<Json<CreateApiKeyResponse>, AppError> {
    require_admin(&state, &headers, &query)?;

    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("API key")
        .to_string();
    let key = new_api_key();
    let hash = hash_secret(&key);
    let id = hash[..16].to_string();
    let prefix = key_prefix(&key);
    let created_at = SystemTime::now();
    let created_at_unix = unix_seconds(created_at);

    state.api_keys.insert(
        id.clone(),
        ApiKeyEntry {
            hash,
            prefix: prefix.clone(),
            name: name.clone(),
            created_at,
        },
    );
    let _ = state.notifier.send(());

    Ok(Json(CreateApiKeyResponse {
        id,
        key,
        prefix,
        name,
        created_at_unix,
    }))
}

fn build_json_proxy_response(status: StatusCode, bytes: Bytes) -> Result<Response, AppError> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(bytes))
        .map_err(|e| AppError::Internal(format!("build proxied JSON response failed: {e}")))
}

async fn admin_delete_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    require_admin(&state, &headers, &query)?;
    state.api_keys.remove(&id);
    let _ = state.notifier.send(());
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    require_admin(&state, &headers, &query)?;

    let (tx, rx) = mpsc::channel::<SseResult>(32);
    let mut notify = state.notifier.subscribe();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = notify.recv() => {}
            }

            let accounts = fetch_accounts_json(&state).await;
            let cache = cache_entries(&state.cache);
            let api_keys = api_key_entries(&state.api_keys);
            let payload = json!({
                "accounts": accounts,
                "cache": cache,
                "api_keys": api_keys,
            });

            if tx
                .send(Ok(Event::default()
                    .event("state")
                    .data(payload.to_string())))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()))
}

async fn fetch_accounts_json(state: &AppState) -> serde_json::Value {
    match state
        .client
        .get(state.worker_admin_accounts_url.as_ref())
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response
            .json::<serde_json::Value>()
            .await
            .unwrap_or_else(|_| json!({ "accounts": [] })),
        Ok(response) => json!({
            "accounts": [],
            "error": format!("worker status {}", response.status())
        }),
        Err(e) => json!({
            "accounts": [],
            "error": e.to_string()
        }),
    }
}

fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<(), AppError> {
    let Some(expected) = state.admin_token.as_deref() else {
        return Ok(());
    };

    let provided = query
        .get("admin_token")
        .map(String::as_str)
        .or_else(|| header_value(headers, "x-admin-token"))
        .or_else(|| bearer_token(headers));

    if provided == Some(expected) {
        Ok(())
    } else {
        Err(AppError::Unauthorized("admin token required".to_string()))
    }
}

fn require_client_api_key(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    if !state.require_api_key && state.api_keys.is_empty() {
        return Ok(());
    }

    let provided = bearer_token(headers).or_else(|| header_value(headers, "x-api-key"));
    let Some(key) = provided else {
        return Err(AppError::Unauthorized("api key required".to_string()));
    };

    let hash = hash_secret(key);
    if state
        .api_keys
        .iter()
        .any(|entry| entry.hash.as_str() == hash)
    {
        Ok(())
    } else {
        Err(AppError::Unauthorized("invalid api key".to_string()))
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = header_value(headers, "authorization")?;
    value.strip_prefix("Bearer ").map(str::trim)
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().filter(|v| !v.is_empty())
}

fn load_api_keys_from_env() -> DashMap<String, ApiKeyEntry> {
    let keys = DashMap::new();
    let Ok(raw) = std::env::var("API_KEYS") else {
        return keys;
    };

    for (idx, key) in raw
        .split(',')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .enumerate()
    {
        let hash = hash_secret(key);
        let id = hash[..16].to_string();
        keys.insert(
            id,
            ApiKeyEntry {
                hash,
                prefix: key_prefix(key),
                name: format!("Env key {}", idx + 1),
                created_at: SystemTime::now(),
            },
        );
    }

    keys
}

fn new_api_key() -> String {
    format!("sk-gw-{}-{}", Uuid::new_v4(), Uuid::new_v4())
}

fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

fn key_prefix(key: &str) -> String {
    let prefix = key.chars().take(12).collect::<String>();
    format!("{prefix}...")
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn split_semantic(body: &Bytes) -> Result<Vec<Bytes>, AppError> {
    let text = str::from_utf8(body)
        .map_err(|_| AppError::BadRequest("payload must be valid UTF-8 text".to_string()))?;
    if text.is_empty() {
        return Err(AppError::BadRequest("payload is empty".to_string()));
    }

    let max_chars = CHUNK_TOKEN_LIMIT * APPROX_CHARS_PER_TOKEN;
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        start = skip_leading_whitespace(text, start);
        if start >= text.len() {
            break;
        }

        let hard_end = char_budget_end(text, start, max_chars);
        if hard_end >= text.len() {
            chunks.push(body.slice(start..text.len()));
            break;
        }

        let window = &text[start..hard_end];
        let boundary = nearest_semantic_boundary(window)
            .filter(|end| *end > 0)
            .unwrap_or(window.len());
        let end = start + boundary;

        chunks.push(body.slice(start..end));
        start = end;
    }

    Ok(chunks)
}

fn char_budget_end(text: &str, start: usize, max_chars: usize) -> usize {
    let tail = &text[start..];
    tail.char_indices()
        .nth(max_chars)
        .map(|(idx, _)| start + idx)
        .unwrap_or(text.len())
}

fn nearest_semantic_boundary(window: &str) -> Option<usize> {
    let paragraph = window.rfind("\n\n").map(|idx| idx + 2);
    let sentence = window.rfind('.').map(|idx| idx + 1);
    paragraph.into_iter().chain(sentence).max()
}

fn skip_leading_whitespace(text: &str, mut idx: usize) -> usize {
    while idx < text.len() {
        let Some(ch) = text[idx..].chars().next() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        idx += ch.len_utf8();
    }
    idx
}

fn drain_sse_data(buffer: &mut String) -> Vec<String> {
    let mut events = Vec::new();
    while let Some(pos) = buffer.find("\n\n") {
        let frame = buffer[..pos].to_string();
        buffer.drain(..pos + 2);

        let mut data = String::new();
        for line in frame.lines() {
            if let Some(value) = line.strip_prefix("data:") {
                data.push_str(value.trim_start());
            }
        }
        if !data.is_empty() {
            events.push(data);
        }
    }
    events
}

fn cache_key(body: &Bytes) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    hex::encode(hasher.finalize())
}

fn cache_get(cache: &DashMap<String, CacheEntry>, key: &str) -> Option<Arc<str>> {
    let entry = cache.get(key)?;
    if entry.expires_at > Instant::now() {
        return Some(entry.value.clone());
    }
    drop(entry);
    cache.remove(key);
    None
}

fn cache_entries(cache: &DashMap<String, CacheEntry>) -> Vec<CacheInfo> {
    let now = Instant::now();
    let mut expired = Vec::new();
    let mut entries = Vec::new();

    for item in cache.iter() {
        if item.expires_at <= now {
            expired.push(item.key().clone());
            continue;
        }
        entries.push(CacheInfo {
            key: item.key().clone(),
            created_at_unix: item
                .created_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            expires_in_seconds: item.expires_at.saturating_duration_since(now).as_secs(),
        });
    }

    for key in expired {
        cache.remove(&key);
    }

    entries.sort_by(|a, b| a.key.cmp(&b.key));
    entries
}

fn api_key_entries(api_keys: &DashMap<String, ApiKeyEntry>) -> Vec<ApiKeyInfo> {
    let mut entries = api_keys
        .iter()
        .map(|item| ApiKeyInfo {
            id: item.key().clone(),
            prefix: item.prefix.clone(),
            name: item.name.clone(),
            created_at_unix: unix_seconds(item.created_at),
        })
        .collect::<Vec<_>>();

    entries.sort_by(|a, b| a.created_at_unix.cmp(&b.created_at_unix));
    entries
}
