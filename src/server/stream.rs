//! Cầu nối stream SSE - chuyển Stream generic thành axum Body
//!
//! Hỗ trợ cả response stream OpenAI và Anthropic.

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt};

// ---------------------------------------------------------------------------
// SseBody
// ---------------------------------------------------------------------------

/// Wrapper body response SSE (generic)
pub struct SseBody<S> {
    inner: S,
    extra_headers: Vec<(String, String)>,
}

impl<S, E> SseBody<S>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + Sync + 'static,
{
    pub fn new(stream: S) -> Self {
        Self {
            inner: stream,
            extra_headers: Vec::new(),
        }
    }

    /// Thêm header response tùy chỉnh
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers
            .push((name.to_string(), value.to_string()));
        self
    }
}

impl<S, E> IntoResponse for SseBody<S>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + Sync + 'static,
{
    fn into_response(self) -> Response {
        let body = Body::from_stream(self.inner.map(|result| {
            result.map_err(|e| {
                log::error!(target: "http::response", "SSE stream error: {}", e);
                std::io::Error::other(e.to_string())
            })
        }));

        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive")
            .header("X-Accel-Buffering", "no");

        for (name, value) in self.extra_headers {
            builder = builder.header(&name, &value);
        }

        builder.body(body).unwrap().into_response()
    }
}
