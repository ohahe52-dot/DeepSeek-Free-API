//! Format response lỗi HTTP - hỗ trợ JSON lỗi tương thích OpenAI và Anthropic
//!
//! Map lỗi adapter sang format response lỗi chuẩn.

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::fmt;

use crate::anthropic_compat::AnthropicCompatError;
use crate::openai_adapter::OpenAIAdapterError;

/// Body response lỗi tương thích OpenAI
#[derive(Debug, Serialize)]
pub struct OpenAIErrorBody {
    error: OpenAIErrorDetail,
}

#[derive(Debug, Serialize)]
struct OpenAIErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: &'static str,
    code: &'static str,
}

/// Body response lỗi tương thích Anthropic
#[derive(Debug, Serialize)]
pub struct AnthropicErrorBody {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
}

/// Kiểu lỗi tầng server
#[derive(Debug)]
pub enum ServerError {
    /// Lỗi adapter OpenAI
    Adapter(OpenAIAdapterError),
    /// Lỗi tầng tương thích Anthropic
    Anthropic(AnthropicCompatError),
    /// Chưa xác thực (API token không hợp lệ)
    Unauthorized,
    /// Tài nguyên không tồn tại
    NotFound(String),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Adapter(e) => write!(f, "{}", e),
            Self::Anthropic(e) => write!(f, "{}", e),
            Self::Unauthorized => write!(f, "API token không hợp lệ"),
            Self::NotFound(id) => write!(f, "Mô hình '{}' không tồn tại", id),
        }
    }
}

impl From<OpenAIAdapterError> for ServerError {
    fn from(e: OpenAIAdapterError) -> Self {
        Self::Adapter(e)
    }
}

impl From<AnthropicCompatError> for ServerError {
    fn from(e: AnthropicCompatError) -> Self {
        Self::Anthropic(e)
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        match &self {
            Self::Anthropic(e) => anthropic_error_response(e),
            _ => openai_error_response(&self),
        }
    }
}

fn openai_error_response(err: &ServerError) -> Response {
    let (status, error_type, code) = match err {
        ServerError::Adapter(e) => {
            let status =
                StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let (error_type, code) = match e {
                OpenAIAdapterError::BadRequest(_) => ("invalid_request_error", "bad_request"),
                OpenAIAdapterError::Overloaded => ("server_error", "overloaded"),
                OpenAIAdapterError::ProviderError(_) => ("server_error", "provider_error"),
                OpenAIAdapterError::Internal(_) | OpenAIAdapterError::ToolCallRepairNeeded(_) => {
                    ("server_error", "internal_error")
                }
            };
            (status, error_type, code)
        }
        ServerError::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid_api_token",
        ),
        ServerError::NotFound(_) => (
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
        ),
        // Lỗi Anthropic không đi tới nhánh này
        ServerError::Anthropic(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "internal_error",
        ),
    };

    let body = OpenAIErrorBody {
        error: OpenAIErrorDetail {
            message: err.to_string(),
            error_type,
            code,
        },
    };

    log::debug!(target: "http::response", "{} error: {}", status, body.error.message);

    let mut resp = (status, Json(body)).into_response();
    if status == StatusCode::TOO_MANY_REQUESTS {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
    }
    resp
}

fn anthropic_error_response(err: &AnthropicCompatError) -> Response {
    let status =
        StatusCode::from_u16(err.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let error_type = match err {
        AnthropicCompatError::BadRequest(_) => "invalid_request_error",
        AnthropicCompatError::Overloaded => "overloaded_error",
        AnthropicCompatError::Internal(_) => "api_error",
    };

    let body = AnthropicErrorBody {
        error_type,
        message: err.to_string(),
    };

    log::debug!(target: "http::response", "{} Anthropic error: {}", status, body.message);

    let mut resp = (status, Json(body)).into_response();
    if status == StatusCode::TOO_MANY_REQUESTS {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
    }
    resp
}
