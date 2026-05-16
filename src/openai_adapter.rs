//! Tầng adapter giao thức OpenAI - chuyển đổi hai chiều giữa OpenAI JSON và định dạng nội bộ ds_core
//!
//! Module này chuyển HTTP request tương thích OpenAI sang định dạng nội bộ ds_core,
//! rồi chuyển response của ds_core sang JSON tương thích OpenAI.
//!
//! Chỉ xuất ra giao diện tối thiểu: OpenAIAdapter, OpenAIAdapterError

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::ds_core::{CoreError, DeepSeekCore};
use std::collections::HashMap;

mod models;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod types;

pub use types::{ChatCompletionsRequest, ChatCompletionsResponse, ChatCompletionsResponseChunk};

/// Kiểu response dạng stream (stream byte SSE)
pub type StreamResponse = Pin<Box<dyn Stream<Item = Result<Bytes, OpenAIAdapterError>> + Send>>;

/// Stream struct response dạng stream
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send>>;

/// Output thống nhất của Chat Completions
pub enum ChatOutput {
    Stream(ChunkStream),
    Json(ChatCompletionsResponse),
}

/// Wrapper kết quả chung của tầng adapter: chứa kết quả request và định danh tài khoản
pub struct ChatResult<T> {
    pub data: T,
    pub account_id: String,
    pub prompt_tokens: u32,
}

/// Adapter OpenAI
pub struct OpenAIAdapter {
    ds_core: Arc<DeepSeekCore>,
    model_types: tokio::sync::RwLock<Vec<String>>,
    model_registry: tokio::sync::RwLock<HashMap<String, String>>,
    model_aliases: tokio::sync::RwLock<Vec<String>>,
    max_input_tokens: tokio::sync::RwLock<Vec<u32>>,
    max_output_tokens: tokio::sync::RwLock<Vec<u32>>,
    tag_config: tokio::sync::RwLock<Arc<response::TagConfig>>,
    /// Encoder tiktoken BPE được cache (tránh tạo lại cho mỗi request)
    bpe: Option<Arc<tiktoken_rs::CoreBPE>>,
}

impl OpenAIAdapter {
    /// Tạo instance adapter
    pub async fn new(config: &crate::config::Config) -> Result<Self, OpenAIAdapterError> {
        let ds_core = Arc::new(DeepSeekCore::new(config).await?);
        let model_registry = config.deepseek.model_registry();
        // Khởi tạo trước tiktoken BPE (tránh tạo lại bảng từ cho mỗi request)
        let bpe = tiktoken_rs::cl100k_base().ok().map(Arc::new);

        Ok(Self {
            ds_core,
            model_types: tokio::sync::RwLock::new(config.deepseek.model_types.clone()),
            model_registry: tokio::sync::RwLock::new(model_registry),
            model_aliases: tokio::sync::RwLock::new(config.deepseek.model_aliases.clone()),
            max_input_tokens: tokio::sync::RwLock::new(config.deepseek.max_input_tokens.clone()),
            max_output_tokens: tokio::sync::RwLock::new(config.deepseek.max_output_tokens.clone()),
            tag_config: tokio::sync::RwLock::new(Arc::new(response::TagConfig::from_config(
                &config.deepseek.tool_call,
            ))),
            bpe,
        })
    }

    /// POST /v1/chat/completions (điểm vào thống nhất)
    ///
    /// Kiểm tra tham số, tạo ChatML prompt, rồi rẽ nhánh theo cờ stream:
    /// - stream=true  -> trả về stream byte SSE
    /// - stream=false -> gom stream SSE thành một JSON object rồi trả về
    pub async fn chat_completions(
        &self,
        mut req: ChatCompletionsRequest,
        request_id: &str,
    ) -> Result<ChatResult<ChatOutput>, OpenAIAdapterError> {
        log::debug!(target: "adapter", "req={} adapter bắt đầu xử lý: model={}, stream={}", request_id, req.model, req.stream);
        use crate::openai_adapter::types::{
            FunctionCallOption, NamedFunction, NamedToolChoice, Tool, ToolChoice,
        };

        // Tương thích functions / function_call đời cũ -> tools / tool_choice
        if req.tools.as_ref().map(|t| t.is_empty()).unwrap_or(true)
            && let Some(functions) = req.functions.clone()
            && !functions.is_empty()
        {
            req.tools = Some(
                functions
                    .into_iter()
                    .map(|f| Tool {
                        ty: "function".to_string(),
                        function: Some(f),
                        custom: None,
                    })
                    .collect(),
            );
        }
        if req.tool_choice.is_none()
            && let Some(fc) = req.function_call.clone()
        {
            req.tool_choice = Some(match fc {
                FunctionCallOption::Mode(mode) => ToolChoice::Mode(mode),
                FunctionCallOption::Named(named) => ToolChoice::Named(NamedToolChoice {
                    ty: "function".to_string(),
                    function: NamedFunction { name: named.name },
                }),
            });
        }

        let norm = request::normalize::apply(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let tool_ctx = request::tools::extract(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let prompt = request::prompt::build(&req, &tool_ctx);
        let registry = self.model_registry.read().await;
        let model_res = request::resolver::resolve(
            &registry,
            &req.model,
            req.reasoning_effort.as_deref(),
            req.web_search_options.as_ref(),
        )
        .map_err(OpenAIAdapterError::BadRequest)?;

        let prompt_tokens = self
            .bpe
            .as_ref()
            .map(|bpe| {
                u32::try_from(bpe.encode_with_special_tokens(&prompt).len())
                    .expect("token count exceeds u32::MAX")
            })
            .unwrap_or(0);

        let file_result = request::files::extract(&req);
        let chat_req = crate::ds_core::ChatRequest {
            prompt,
            thinking_enabled: model_res.thinking_enabled,
            search_enabled: model_res.search_enabled || file_result.has_http_urls,
            model_type: model_res.model_type,
            files: file_result.files,
        };

        let chat_resp = self.try_chat(chat_req, request_id).await?;
        let account_id = chat_resp.account_id;

        // Chuẩn bị thông tin định nghĩa công cụ cho mô hình sửa
        let tool_defs = req.tools.as_ref().map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.function.as_ref())
                .map(|f| {
                    format!(
                        "- {}: {}",
                        f.name,
                        serde_json::to_string(&f.parameters).unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        });

        if req.stream {
            let repair_fn = self.create_repair_fn(request_id, tool_defs.clone()).await;
            let s = response::stream(
                chat_resp.stream,
                req.model,
                response::StreamCfg {
                    include_usage: norm.include_usage,
                    include_obfuscation: norm.include_obfuscation,
                    stop: norm.stop,
                    prompt_tokens,
                    repair_fn: Some(repair_fn),
                    tag_config: self.tag_config.read().await.clone(),
                },
            );
            Ok(ChatResult {
                data: ChatOutput::Stream(s),
                account_id,
                prompt_tokens,
            })
        } else {
            let repair_fn = self.create_repair_fn(request_id, tool_defs).await;
            let json = response::aggregate(
                chat_resp.stream,
                req.model,
                response::StreamCfg {
                    include_usage: true,
                    include_obfuscation: false,
                    stop: norm.stop,
                    prompt_tokens,
                    repair_fn: Some(repair_fn),
                    tag_config: self.tag_config.read().await.clone(),
                },
            )
            .await?;
            Ok(ChatResult {
                data: ChatOutput::Json(json),
                account_id,
                prompt_tokens,
            })
        }
    }

    /// Helper nội bộ: retry có backoff với `Overloaded` (v0_chat đã đổi tài khoản, đây là lớp dự phòng cấp pool)
    pub(crate) async fn try_chat(
        &self,
        req: crate::ds_core::ChatRequest,
        request_id: &str,
    ) -> Result<crate::ds_core::ChatResponse, CoreError> {
        const MAX_RETRIES: usize = 2;
        const BASE_DELAY_MS: u64 = 2000;

        for attempt in 0..MAX_RETRIES {
            match self.ds_core.v0_chat(req.clone(), request_id).await {
                Ok(resp) => {
                    if attempt > 0 {
                        log::info!(target: "adapter", "req={} retry lần {} thành công", request_id, attempt);
                    }
                    return Ok(resp);
                }
                Err(CoreError::Overloaded) if attempt + 1 < MAX_RETRIES => {
                    let delay = BASE_DELAY_MS * (1 << attempt);
                    log::warn!(target: "adapter", "req={} Overloaded, retry lần {} chờ {}ms", request_id, attempt + 1, delay);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Err(e) => return Err(e),
            }
        }
        log::warn!(target: "adapter", "req={} cả {} lần retry đều thất bại, bỏ cuộc", request_id, MAX_RETRIES);
        Err(CoreError::Overloaded)
    }

    /// GET /v1/models
    pub async fn list_models(&self) -> types::OpenAIModelList {
        let model_types = self.model_types.read().await;
        let max_input = self.max_input_tokens.read().await;
        let max_output = self.max_output_tokens.read().await;
        let aliases = self.model_aliases.read().await;
        models::list(&model_types, &max_input, &max_output, &aliases)
    }

    /// GET /v1/models/{model_id}
    pub async fn get_model(&self, model_id: &str) -> Option<types::OpenAIModel> {
        let model_types = self.model_types.read().await;
        let max_input = self.max_input_tokens.read().await;
        let max_output = self.max_output_tokens.read().await;
        let aliases = self.model_aliases.read().await;
        models::get(&model_types, &max_input, &max_output, &aliases, model_id)
    }

    /// Stream SSE DeepSeek gốc (không chuyển qua giao thức OpenAI)
    ///
    /// Dùng để phân tích stream: so sánh response gốc với bản chuyển đổi OpenAI để tìm lỗi chuyển đổi
    pub async fn raw_chat_completions_stream(
        &self,
        body: &[u8],
        request_id: &str,
    ) -> Result<ChatResult<StreamResponse>, OpenAIAdapterError> {
        let chat_req: ChatCompletionsRequest = serde_json::from_slice(body)
            .map_err(|e| OpenAIAdapterError::BadRequest(format!("bad request: {}", e)))?;
        let registry = self.model_registry.read().await;
        let model_res = request::resolver::resolve(
            &registry,
            &chat_req.model,
            chat_req.reasoning_effort.as_deref(),
            chat_req.web_search_options.as_ref(),
        )
        .map_err(OpenAIAdapterError::BadRequest)?;
        let ds_req = crate::ds_core::ChatRequest {
            prompt: request::prompt::build(
                &chat_req,
                &request::tools::extract(&chat_req).map_err(OpenAIAdapterError::BadRequest)?,
            ),
            thinking_enabled: model_res.thinking_enabled,
            search_enabled: model_res.search_enabled,
            model_type: model_res.model_type,
            files: vec![],
        };
        let chat_resp = self.try_chat(ds_req, request_id).await?;
        let data = Box::pin(
            chat_resp
                .stream
                .map(|r| r.map_err(OpenAIAdapterError::from)),
        );
        Ok(ChatResult {
            data,
            account_id: chat_resp.account_id,
            prompt_tokens: 0,
        })
    }

    /// Lấy trạng thái pool tài khoản ds_core
    pub fn account_statuses(&self) -> Vec<crate::ds_core::AccountStatus> {
        self.ds_core.account_statuses()
    }

    /// Thêm tài khoản động
    pub async fn add_account(
        &self,
        creds: &crate::config::Account,
    ) -> Result<String, crate::ds_core::PoolError> {
        self.ds_core.add_account(creds).await
    }

    /// Xóa tài khoản động
    pub async fn remove_account(
        &self,
        email_or_mobile: &str,
    ) -> Result<String, crate::ds_core::PoolError> {
        self.ds_core.remove_account(email_or_mobile).await
    }

    /// Đánh dấu tài khoản sang trạng thái Error
    pub fn mark_error(&self, email_or_mobile: &str) {
        self.ds_core.mark_error(email_or_mobile);
    }

    /// Đăng nhập lại thủ công tài khoản chỉ định
    pub async fn re_login_single(&self, email_or_mobile: &str) -> Result<(), String> {
        self.ds_core.re_login_single(email_or_mobile).await
    }
}

impl OpenAIAdapter {
    /// Đồng bộ tài khoản hàng loạt: so sánh pool hiện tại với cấu hình đích rồi thêm/xóa phần khác biệt
    pub(crate) async fn sync_accounts(&self, new_accounts: &[crate::config::Account]) {
        let old_statuses = self.account_statuses();
        let old_ids: Vec<String> = old_statuses
            .iter()
            .map(|a| {
                if a.email.is_empty() {
                    a.mobile.clone()
                } else {
                    a.email.clone()
                }
            })
            .collect();

        let mut _added = 0usize;
        let mut _failed = 0usize;
        for acct in new_accounts {
            let id = if acct.email.is_empty() {
                &acct.mobile
            } else {
                &acct.email
            };
            if !old_ids.contains(id) {
                match self.add_account(acct).await {
                    Ok(_) => _added += 1,
                    Err(e) => {
                        log::warn!(target: "adapter", "Đồng bộ thêm tài khoản {} thất bại: {}", id, e);
                        _failed += 1;
                    }
                }
            }
        }

        let mut _removed = 0usize;
        let new_ids: Vec<&str> = new_accounts
            .iter()
            .map(|a| {
                if a.email.is_empty() {
                    a.mobile.as_str()
                } else {
                    a.email.as_str()
                }
            })
            .collect();
        for old_id in &old_ids {
            if !new_ids.contains(&old_id.as_str()) && !old_id.is_empty() {
                match self.remove_account(old_id).await {
                    Ok(_) => _removed += 1,
                    Err(e) => {
                        log::warn!(target: "adapter", "Đồng bộ xóa tài khoản {} thất bại: {}", old_id, e);
                    }
                }
            }
        }
    }

    /// Tắt an toàn
    pub async fn shutdown(&self) {
        self.ds_core.shutdown().await;
    }

    pub async fn reload_config(&self, new_config: &crate::config::Config) -> Result<(), CoreError> {
        // Sync accounts
        self.sync_accounts(&new_config.accounts).await;
        // Rebuild model registry
        let registry = new_config.deepseek.model_registry();
        *self.model_registry.write().await = registry;
        *self.model_types.write().await = new_config.deepseek.model_types.clone();
        *self.model_aliases.write().await = new_config.deepseek.model_aliases.clone();
        *self.max_input_tokens.write().await = new_config.deepseek.max_input_tokens.clone();
        *self.max_output_tokens.write().await = new_config.deepseek.max_output_tokens.clone();
        *self.tag_config.write().await = Arc::new(response::TagConfig::from_config(
            &new_config.deepseek.tool_call,
        ));
        // Rebuild DsClient if needed (deepseek/proxy changes)
        self.ds_core.reload_config(new_config).await
    }

    pub(crate) async fn create_repair_fn(
        &self,
        request_id: &str,
        tool_defs: Option<String>,
    ) -> response::RepairFn {
        use std::sync::atomic::{AtomicU16, Ordering};
        let core = self.ds_core.clone();
        let req_id = request_id.to_string();
        let seq = Arc::new(AtomicU16::new(0));
        let tag_config = self.tag_config.read().await.clone();
        let tools_info = tool_defs.unwrap_or_default();
        Arc::new(move |tool_text: String| {
            let core = core.clone();
            let req_id = req_id.clone();
            let seq = seq.clone();
            let tag_config = tag_config.clone();
            let tools_info = tools_info.clone();
            Box::pin(async move {
                use crate::ds_core::ChatRequest;
                let n = seq.fetch_add(1, Ordering::Relaxed);
                let repair_req_id = format!("{}-repair-{}", req_id, n);
                let mut prompt = String::new();
                if !tools_info.is_empty() {
                    prompt.push_str(&format!("Định nghĩa công cụ khả dụng:\n{}\n\n", tools_info));
                }
                prompt.push_str(&format!(
                    "Hãy trích xuất nội dung trong code block sau và chuyển thành mảng JSON gọi công cụ hợp lệ.\
                     \nMỗi phần tử phải có trường \"name\" (chuỗi) và \"arguments\" (object).\
                     \nChỉ xuất chính mảng JSON, không thêm code fence hoặc giải thích.\
                     \nLưu ý: dấu nháy và ký tự xuống dòng trong giá trị chuỗi phải được escape bằng dấu gạch chéo ngược (ví dụ \\\" và \\n).\
                     \n\nNội dung cần sửa:\n~~~\n{tool_text}\n~~~"
                ));
                let req = ChatRequest {
                    prompt,
                    thinking_enabled: false,
                    search_enabled: false,
                    model_type: "default".to_string(),
                    files: vec![],
                };
                log::debug!(
                    target: "adapter",
                    "{} gửi yêu cầu sửa: len={}", repair_req_id, tool_text.len()
                );
                let resp = core
                    .v0_chat(req, &repair_req_id)
                    .await
                    .map_err(OpenAIAdapterError::from)?;
                response::execute_tool_repair(resp.stream, &tag_config).await
            })
        })
    }
}

/// Kiểu lỗi adapter
#[derive(Debug, thiserror::Error)]
pub enum OpenAIAdapterError {
    /// Lỗi định dạng request
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Dịch vụ quá tải, không có tài khoản ds_core khả dụng
    #[error("service overloaded")]
    Overloaded,

    /// Lỗi nhà cung cấp upstream (mạng, nghiệp vụ...)
    #[error("provider error: {0}")]
    ProviderError(String),

    /// Lỗi nội bộ (serialize, chuyển đổi stream...)
    #[error("internal error: {0}")]
    Internal(String),

    /// Parse marker tool_calls thất bại, mang text gốc trong `{TOOL_CALL_START}...{TOOL_CALL_END}`
    #[error("tool_calls repair needed: {0}")]
    ToolCallRepairNeeded(String),
}

impl From<CoreError> for OpenAIAdapterError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Overloaded => Self::Overloaded,
            CoreError::ProofOfWorkFailed(err) => {
                Self::Internal(format!("proof of work failed: {}", err))
            }
            CoreError::ProviderError(msg) => Self::ProviderError(msg),
            CoreError::Stream(msg) => Self::Internal(msg),
        }
    }
}

impl From<serde_json::Error> for OpenAIAdapterError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(format!("json serialization failed: {}", e))
    }
}

impl OpenAIAdapterError {
    /// Trả về HTTP status tương ứng
    #[must_use]
    pub fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::Overloaded => 429,
            Self::ProviderError(_) => 502,
            Self::Internal(_) | Self::ToolCallRepairNeeded(_) => 500,
        }
    }
}
