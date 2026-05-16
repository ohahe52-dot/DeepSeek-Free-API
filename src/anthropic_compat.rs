//! Tầng tương thích giao thức Anthropic - cung cấp API tương thích Anthropic dựa trên openai_adapter
//!
//! Module này không truy cập ds_core trực tiếp; mọi dữ liệu đi qua openai_adapter rồi được map định dạng.
//! Luồng request: Anthropic JSON -> ChatCompletionsRequest -> openai_adapter -> response map về định dạng Anthropic.

mod models;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod types;

pub use types::{MessagesRequest, MessagesResponse, MessagesResponseChunk};

/// Kiểu response Anthropic dạng stream (stream struct)
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<MessagesResponseChunk, AnthropicCompatError>> + Send>>;

/// Kiểu response Anthropic dạng stream (stream byte SSE)
pub type StreamResponse = Pin<Box<dyn Stream<Item = Result<Bytes, AnthropicCompatError>> + Send>>;

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::Stream;
use log::debug;

use crate::openai_adapter::{ChatOutput, ChatResult, OpenAIAdapter, OpenAIAdapterError};

/// Output thống nhất Anthropic (tương ứng ChatOutput của openai_adapter)
pub enum AnthropicOutput {
    Stream(ChunkStream),
    Json(MessagesResponse),
}

/// Tầng tương thích Anthropic
pub struct AnthropicCompat {
    openai_adapter: Arc<OpenAIAdapter>,
}

impl AnthropicCompat {
    /// Tạo instance tầng tương thích
    pub fn new(openai_adapter: Arc<OpenAIAdapter>) -> Self {
        Self { openai_adapter }
    }

    /// POST /v1/messages (điểm vào thống nhất)
    ///
    /// Map request Anthropic thành ChatCompletionsRequest, ủy quyền cho openai_adapter,
    /// rồi map kết quả rẽ nhánh stream của OpenAI về định dạng Anthropic.
    pub async fn messages(
        &self,
        req: MessagesRequest,
        request_id: &str,
    ) -> Result<ChatResult<AnthropicOutput>, AnthropicCompatError> {
        debug!(target: "anthropic_compat", "Đã nhận yêu cầu messages");
        let chat_req = request::into_chat_completions(req);
        let result = self
            .openai_adapter
            .chat_completions(chat_req, request_id)
            .await?;
        let data = match result.data {
            ChatOutput::Stream(stream) => {
                AnthropicOutput::Stream(response::from_chat_completion_stream(stream))
            }
            ChatOutput::Json(json) => {
                let msg = response::from_chat_completions(&json);
                AnthropicOutput::Json(msg)
            }
        };
        Ok(ChatResult {
            data,
            account_id: result.account_id,
            prompt_tokens: result.prompt_tokens,
        })
    }

    /// GET /v1/models
    ///
    /// Trả về danh sách model định dạng Anthropic.
    pub async fn list_models(&self) -> models::AnthropicModelList {
        debug!(target: "anthropic_compat", "Đã nhận yêu cầu danh sách mô hình");
        models::list(&self.openai_adapter.list_models().await)
    }

    /// GET /v1/models/{model_id}
    ///
    /// Trả về chi tiết model chỉ định theo định dạng Anthropic.
    pub async fn get_model(&self, model_id: &str) -> Option<models::AnthropicModel> {
        debug!(target: "anthropic_compat", "Truy vấn mô hình: {}", model_id);
        models::get(&self.openai_adapter.list_models().await, model_id)
    }
}

/// Kiểu lỗi tầng tương thích Anthropic
#[derive(Debug, thiserror::Error)]
pub enum AnthropicCompatError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("service overloaded")]
    Overloaded,
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<OpenAIAdapterError> for AnthropicCompatError {
    fn from(e: OpenAIAdapterError) -> Self {
        match e {
            OpenAIAdapterError::BadRequest(msg) => Self::BadRequest(msg),
            OpenAIAdapterError::Overloaded => Self::Overloaded,
            OpenAIAdapterError::ProviderError(msg)
            | OpenAIAdapterError::Internal(msg)
            | OpenAIAdapterError::ToolCallRepairNeeded(msg) => Self::Internal(msg),
        }
    }
}

impl AnthropicCompatError {
    /// Trả về HTTP status tương ứng
    #[must_use]
    pub fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::Overloaded => 429,
            Self::Internal(_) => 500,
        }
    }
}
