//! Chuyển response OpenAI - map stream SSE DeepSeek thành format response OpenAI
//!
//! Luồng dữ liệu: sse_parser -> state -> converter -> tool_parser
//! - Chỉ fragment THINK / RESPONSE được map thành text người dùng thấy
//! - obfuscation được inject động ở bước serialize SSE cuối

mod converter;
mod sse_parser;
mod state;
mod tool_parser;

pub(crate) use tool_parser::{TOOL_CALL_END, TOOL_CALL_START, TagConfig};

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use log::{debug, info, trace, warn};
use pin_project_lite::pin_project;
use rand::RngExt;
use tokio::time::Sleep;

use crate::openai_adapter::{
    OpenAIAdapterError,
    types::{
        ChatCompletionsResponse, ChatCompletionsResponseChunk, Choice, ChunkChoice, Delta,
        FunctionCall, MessageResponse, ToolCall, Usage,
    },
};

static CHATCMPL_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_chatcmpl_id() -> String {
    let n = CHATCMPL_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("chatcmpl-{:016x}", n)
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

const OBFUSCATION_TARGET_LEN: usize = 512;
const OBFUSCATION_MIN_PAD: usize = 16;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
const FINISH_STOP: &str = "stop";
const FINISH_TOOL_CALLS: &str = "tool_calls";

fn random_padding(len: usize) -> String {
    if len == 0 {
        return String::new();
    }
    let byte_len = (len * 3).div_ceil(4);
    let mut bytes = vec![0u8; byte_len];
    rand::rng().fill(&mut bytes);
    let s = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    s[..len].to_string()
}

pub(crate) fn sse_serialize(
    chunk: &ChatCompletionsResponseChunk,
) -> Result<Bytes, OpenAIAdapterError> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"data: ");
    serde_json::to_writer(&mut buf, chunk).map_err(OpenAIAdapterError::from)?;
    buf.extend_from_slice(b"\n\n");
    Ok(Bytes::from(buf))
}

fn find_stop_pos(content: &str, stop: &[String]) -> Option<usize> {
    stop.iter().filter_map(|s| content.find(s)).min()
}

/// Kiểu stream dùng nội bộ trong RepairStream
type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send>>;

/// Kiểu closure sửa lỗi gọi công cụ
pub(crate) type RepairFn = Arc<
    dyn Fn(
            String,
        )
            -> Pin<Box<dyn Future<Output = Result<Vec<ToolCall>, OpenAIAdapterError>> + Send>>
        + Send
        + Sync,
>;

/// Thực hiện sửa tool_calls: parse stream byte ds_core để trích text, rồi chuyển thành ToolCall có cấu trúc
pub(crate) async fn execute_tool_repair(
    ds_stream: Pin<Box<dyn Stream<Item = Result<Bytes, crate::ds_core::CoreError>> + Send>>,
    tag_config: &TagConfig,
) -> Result<Vec<ToolCall>, OpenAIAdapterError> {
    let sse = sse_parser::SseStream::new(ds_stream);
    let state_stream = state::StateStream::new(sse);
    futures::pin_mut!(state_stream);

    let mut text = String::new();
    while let Some(frame) = state_stream.next().await {
        if let state::DsFrame::ContentDelta(t) = frame? {
            text.push_str(&t);
            if text.len() > tool_parser::MAX_XML_BUF_LEN {
                return Err(OpenAIAdapterError::Internal(
                    "Model sửa trả output quá dài, bỏ sửa".into(),
                ));
            }
        }
    }

    let wrapped = if tool_parser::contains_start_tag_with(&text, tag_config) {
        text.trim().to_string()
    } else {
        format!(
            "{}{}{}",
            tool_parser::TOOL_CALL_START,
            text.trim(),
            tool_parser::TOOL_CALL_END
        )
    };

    let (calls, _) = tool_parser::parse_tool_calls_with(&wrapped, tag_config).ok_or_else(|| {
        OpenAIAdapterError::Internal(format!(
            "Model sửa trả kết quả không parse được thành tool call: {}",
            &text[..text.len().min(200)]
        ))
    })?;

    // Model sửa có thể trả kết quả rỗng, kiểm tra sớm
    let trimmed = text.trim();
    if trimmed == "[]" || trimmed == "{}" {
        return Err(OpenAIAdapterError::Internal(
            "Model sửa trả kết quả rỗng".into(),
        ));
    }
    Ok(calls)
}

enum RepairState {
    Forwarding,
    Repairing {
        future: Pin<Box<dyn Future<Output = Result<Vec<ToolCall>, OpenAIAdapterError>> + Send>>,
    },
    RepairFailed(String),
    Done,
}

pin_project! {
    /// Stream sửa lỗi gọi công cụ: sau ToolCallStream, trước StopDetectStream
    ///
    /// Khi ToolCallStream trả Err(ToolCallRepairNeeded),
    /// bỏ stream upstream (nhả tài khoản), gửi request sửa qua repair_fn,
    /// rồi gửi tool_calls đã sửa cho client.
    struct RepairStream {
        #[pin]
        inner: Option<ChunkStream>,
        repair_fn: Option<RepairFn>,
        state: RepairState,
        model: String,
        #[pin]
        keepalive_deadline: Sleep,
    }
}

impl RepairStream {
    fn new(inner: ChunkStream, repair_fn: RepairFn, model: String) -> Self {
        Self {
            inner: Some(inner),
            repair_fn: Some(repair_fn),
            state: RepairState::Forwarding,
            model,
            keepalive_deadline: tokio::time::sleep_until(
                tokio::time::Instant::now() + KEEPALIVE_INTERVAL,
            ),
        }
    }
}

impl Stream for RepairStream {
    type Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        loop {
            match this.state {
                RepairState::Forwarding => {
                    match this.inner.as_mut().as_pin_mut().map(|p| p.poll_next(cx)) {
                        Some(Poll::Ready(Some(Ok(chunk)))) => {
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        Some(Poll::Ready(Some(Err(OpenAIAdapterError::ToolCallRepairNeeded(
                            tool_text,
                        ))))) => {
                            warn!(
                                target: "adapter",
                                "RepairStream bắt được yêu cầu sửa: len={}",
                                tool_text.len()
                            );
                            trace!(target: "adapter", ">>> repair: accepting tool_text len={}", tool_text.len());
                            drop(this.inner.as_mut().get_mut().take());
                            if let Some(f) = this.repair_fn.take() {
                                let future = f(tool_text);
                                *this.state = RepairState::Repairing { future };
                            } else {
                                *this.state =
                                    RepairState::RepairFailed("no repair function".into());
                            }
                            continue;
                        }
                        Some(Poll::Ready(Some(Err(e)))) => {
                            return Poll::Ready(Some(Err(e)));
                        }
                        Some(Poll::Ready(None)) | None => {
                            return Poll::Ready(None);
                        }
                        Some(Poll::Pending) => {
                            return Poll::Pending;
                        }
                    }
                }

                RepairState::Repairing { future } => match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(calls)) => {
                        info!(
                            target: "adapter",
                            "Sửa tool_calls thành công: {} tool call",
                            calls.len()
                        );
                        trace!(target: "adapter", ">>> repair: success {} calls", calls.len());
                        *this.state = RepairState::Done;
                        return Poll::Ready(Some(Ok(converter::make_chunk(
                            this.model,
                            Delta {
                                tool_calls: Some(calls),
                                ..Default::default()
                            },
                            Some(FINISH_TOOL_CALLS),
                        ))));
                    }
                    Poll::Ready(Err(e)) => {
                        warn!(target: "adapter", "Sửa tool_calls thất bại: {}", e);
                        *this.state = RepairState::RepairFailed(format!("Sửa thất bại: {}", e));
                        continue;
                    }
                    Poll::Pending => {
                        if this.keepalive_deadline.as_mut().poll(cx).is_ready() {
                            trace!(target: "adapter", ">>> keepalive(repair): gửi delta công cụ rỗng");
                            this.keepalive_deadline
                                .as_mut()
                                .reset(tokio::time::Instant::now() + KEEPALIVE_INTERVAL);
                            return Poll::Ready(Some(Ok(ChatCompletionsResponseChunk {
                                id: "chatcmpl-keepalive".into(),
                                object: "chat.completion.chunk",
                                created: 0,
                                model: this.model.clone(),
                                choices: vec![ChunkChoice {
                                    index: 0,
                                    delta: Delta {
                                        tool_calls: Some(vec![ToolCall {
                                            id: String::new(),
                                            ty: "function".into(),
                                            function: Some(FunctionCall {
                                                name: String::new(),
                                                arguments: String::new(),
                                            }),
                                            custom: None,
                                            index: 0,
                                        }]),
                                        ..Default::default()
                                    },
                                    finish_reason: None,
                                    logprobs: None,
                                }],
                                usage: None,
                                service_tier: None,
                                system_fingerprint: None,
                            })));
                        }
                        return Poll::Pending;
                    }
                },

                RepairState::RepairFailed(msg) => {
                    let msg = std::mem::take(msg);
                    return Poll::Ready(Some(Err(OpenAIAdapterError::Internal(msg))));
                }

                RepairState::Done => return Poll::Ready(None),
            }
        }
    }
}

pin_project! {
    struct StopDetectStream<S> {
        #[pin]
        inner: S,
        stop: Vec<String>,
        stopped: bool,
        sent_len: usize,
        buffer: String,
        include_obfuscation: bool,
    }
}

impl<S> Stream for StopDetectStream<S>
where
    S: Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>>,
{
    type Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(mut chunk))) => {
                    if *this.stopped {
                        if chunk.choices.is_empty() && chunk.usage.is_some() {
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        // Cho phép nâng finish_reason từ stop thành tool_calls
                        if let Some(choice) = chunk.choices.first_mut()
                            && choice.delta.content.is_none()
                            && choice.delta.reasoning_content.is_none()
                            && choice.delta.tool_calls.is_none()
                            && choice.finish_reason == Some(FINISH_TOOL_CALLS)
                        {
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                        continue;
                    }

                    if !this.stop.is_empty()
                        && let Some(choice) = chunk.choices.first_mut()
                        && let Some(ref content) = choice.delta.content
                    {
                        this.buffer.push_str(content);
                        if let Some(pos) = find_stop_pos(this.buffer, this.stop) {
                            trace!(target: "adapter", ">>> stop: truncate at {}", pos);
                            let truncated = &this.buffer[*this.sent_len..pos];
                            if truncated.is_empty() {
                                choice.delta.content = None;
                            } else {
                                choice.delta.content = Some(truncated.to_string());
                            }
                            choice.finish_reason = Some(FINISH_STOP);
                            *this.stopped = true;
                            this.buffer.clear();
                            *this.sent_len = pos;
                        } else {
                            *this.sent_len = this.buffer.len();
                        }
                    }
                    if *this.include_obfuscation && !chunk.choices.is_empty() {
                        let without = serde_json::to_string(&chunk)
                            .map_err(|e| OpenAIAdapterError::Internal(format!("json: {}", e)))?;
                        let overhead = r#","obfuscation":"""#.len();
                        let pad_len = if without.len() + overhead < OBFUSCATION_TARGET_LEN {
                            OBFUSCATION_TARGET_LEN - without.len() - overhead
                        } else {
                            OBFUSCATION_MIN_PAD
                        };
                        if let Some(choice) = chunk.choices.first_mut() {
                            choice.delta.obfuscation = Some(random_padding(pad_len));
                        }
                    }
                    return Poll::Ready(Some(Ok(chunk)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Tham số response stream (giảm số tham số của stream())
pub(crate) struct StreamCfg {
    pub include_usage: bool,
    pub include_obfuscation: bool,
    pub stop: Vec<String>,
    pub prompt_tokens: u32,
    pub repair_fn: Option<RepairFn>,
    pub tag_config: Arc<TagConfig>,
}

/// Response stream: chuyển stream byte ds_core thành stream ChatCompletionsResponseChunk
pub(crate) fn stream<S>(ds_stream: S, model: String, cfg: StreamCfg) -> ChunkStream
where
    S: Stream<Item = Result<Bytes, crate::ds_core::CoreError>> + Send + 'static,
{
    debug!(
        target: "adapter",
        "Dựng phản hồi streaming: model={}, include_usage={}, include_obfuscation={}, stop_count={}, repair={}",
        model, cfg.include_usage, cfg.include_obfuscation, cfg.stop.len(), cfg.repair_fn.is_some()
    );
    let sse = sse_parser::SseStream::new(ds_stream);
    let state_stream = state::StateStream::new(sse);
    let converted = converter::ConverterStream::new(
        state_stream,
        model.clone(),
        cfg.include_usage,
        cfg.include_obfuscation,
        cfg.prompt_tokens,
    );
    let tool_parsed = tool_parser::ToolCallStream::new(converted, model.clone(), cfg.tag_config);
    let tool_boxed: Pin<
        Box<dyn Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send>,
    > = Box::pin(tool_parsed);

    let after_repair: Pin<
        Box<dyn Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send>,
    > = if let Some(f) = cfg.repair_fn {
        Box::pin(RepairStream::new(tool_boxed, f, model))
    } else {
        tool_boxed
    };

    let stop_detect = StopDetectStream {
        inner: after_repair,
        stop: cfg.stop,
        stopped: false,
        sent_len: 0,
        buffer: String::new(),
        include_obfuscation: cfg.include_obfuscation,
    };
    Box::pin(stop_detect)
}

/// Response không stream: collector downstream của stream(), chỉ tái tổ hợp, không có logic đặc biệt
///
/// Luôn giữ kiểu thu thập và tái tổ hợp từ stream():
/// - Mọi xử lý lõi (sửa, chuyển đổi, stop sequence) đều nằm trong stream()
/// - Hàm này chỉ gom event output của stream() và tái tổ hợp thành một ChatCompletionsResponse JSON
/// - Không thêm logic độc lập với stream() vào hàm này
pub(crate) async fn aggregate<S>(
    ds_stream: S,
    model: String,
    cfg: StreamCfg,
) -> Result<ChatCompletionsResponse, OpenAIAdapterError>
where
    S: Stream<Item = Result<Bytes, crate::ds_core::CoreError>> + Send + 'static,
{
    debug!(target: "adapter", "Dựng phản hồi không streaming: model={}, stop_count={}", model, cfg.stop.len());
    let chunk_stream = stream(
        ds_stream,
        model.clone(),
        StreamCfg {
            include_usage: true,
            include_obfuscation: false,
            ..cfg
        },
    );
    futures::pin_mut!(chunk_stream);

    let mut id = String::new();
    let mut created = 0u64;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Option<Vec<ToolCall>> = None;
    let mut usage = None;
    let mut finish_reason: Option<&'static str> = None;

    while let Some(res) = chunk_stream.next().await {
        let chunk = res?;

        if id.is_empty() {
            id = chunk.id;
            created = chunk.created;
        }

        if let Some(u) = chunk.usage {
            usage = Some(Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            });
        }

        if let Some(choice) = chunk.choices.into_iter().next() {
            if finish_reason.is_none() {
                finish_reason = choice.finish_reason;
            }
            if let Some(c) = choice.delta.content {
                content.push_str(&c);
            }
            if let Some(r) = choice.delta.reasoning_content {
                reasoning.push_str(&r);
            }
            if let Some(tc) = choice.delta.tool_calls
                && !tc.is_empty()
            {
                tool_calls = Some(tc);
            }
        }
    }

    let reasoning_content = if reasoning.is_empty() {
        None
    } else {
        Some(reasoning)
    };

    let has_tool_calls = tool_calls.is_some();
    let message_content = if content.is_empty() && !has_tool_calls {
        warn!(
            target: "adapter",
            "Nội dung phản hồi tổng hợp rỗng: model={}, finish_reason={:?}, has_tool_calls={}, usage={:?}",
            model, finish_reason, tool_calls.is_some(), usage
        );
        None
    } else {
        Some(content)
    };
    let final_reason = if has_tool_calls {
        Some(FINISH_TOOL_CALLS)
    } else {
        finish_reason
    };

    let completion = ChatCompletionsResponse {
        id,
        object: "chat.completion",
        created,
        model,
        choices: vec![Choice {
            index: 0,
            message: MessageResponse {
                role: "assistant",
                content: message_content,
                reasoning_content,
                refusal: None,
                annotations: None,
                audio: None,
                function_call: None,
                tool_calls,
            },
            finish_reason: final_reason,
            logprobs: None,
        }],
        usage,
        service_tier: None,
        system_fingerprint: None,
    };

    debug!(
        target: "adapter",
        "Tổng hợp phản hồi không streaming hoàn tất: finish_reason={:?}, has_tool_calls={}, usage={:?}",
        completion.choices[0].finish_reason,
        completion.choices[0].message.tool_calls.is_some(),
        completion.usage
    );
    Ok(completion)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use futures::StreamExt;

    use super::*;

    fn default_tag_config() -> Arc<TagConfig> {
        Arc::new(TagConfig::from_config(&Default::default()))
    }

    fn sse_bytes(body: &str) -> Result<Bytes, crate::ds_core::CoreError> {
        Ok(Bytes::from(body.to_string()))
    }

    fn tool_span(content: &str) -> String {
        format!(
            "{}{}{}",
            tool_parser::TOOL_CALL_START,
            content,
            tool_parser::TOOL_CALL_END
        )
    }

    /// Cắt nội dung thành chuỗi frame DS SSE dạng stream, mô phỏng output theo ký tự (mỗi ~3 ký tự một mảnh)
    /// - pieces: các cặp (nội dung, loại fragment) theo thứ tự; tự thêm event fragment mới khi loại đổi
    fn make_ds_stream(
        pieces: &[(&str, &str)],
        usage_tokens: Option<u32>,
    ) -> Vec<Result<Bytes, crate::ds_core::CoreError>> {
        let mut frames = vec![sse_bytes("event: ready\ndata: {}\n\n")];

        for (idx, (content, frag_type)) in pieces.iter().enumerate() {
            let is_first = idx == 0;
            let prev_type = if idx > 0 {
                Some(pieces[idx - 1].1)
            } else {
                None
            };
            let type_changed = prev_type != Some(*frag_type);

            if is_first {
                // Fragment đầu: khai báo khi tạo response
                frames.push(sse_bytes(&format!(
                    "data: {{\"v\":{{\"response\":{{\"fragments\":[{{\"type\":\"{frag_type}\",\"content\":\"\"}}]}}}}}}\n\n"
                )));
            } else if type_changed {
                // Loại fragment đổi: APPEND fragment mới vào mảng fragments
                frames.push(sse_bytes(&format!(
                    "data: {{\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{{\"type\":\"{frag_type}\",\"content\":\"\"}}]}}\n\n"
                )));
            }

            // Cắt mỗi 3 ký tự một mảnh
            let mut i = 0;
            while i < content.len() {
                let mut end = (i + 3).min(content.len());
                while !content.is_char_boundary(end) {
                    end -= 1;
                }
                let piece = &content[i..end];
                let escaped = piece.replace('"', "\\\"");
                frames.push(sse_bytes(&format!(
                    "data: {{\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\"{escaped}\"}}\n\n"
                )));
                i = end;
            }
        }

        if let Some(tokens) = usage_tokens {
            frames.push(sse_bytes(&format!(
                "data: {{\"p\":\"response\",\"o\":\"BATCH\",\"v\":[{{\"p\":\"accumulated_token_usage\",\"v\":{tokens}}},{{\"p\":\"quasi_status\",\"v\":\"FINISHED\"}}]}}\n\n"
            )));
        }

        frames.push(sse_bytes(
            "data: {\"p\":\"response/status\",\"o\":\"SET\",\"v\":\"FINISHED\"}\n\n",
        ));

        frames
    }

    #[tokio::test]
    async fn aggregate_plain_text() {
        let frames = make_ds_stream(&[("hello world", "RESPONSE")], Some(41));
        let stream = futures::stream::iter(frames);
        let resp = aggregate(
            stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.object, "chat.completion");
        assert_eq!(resp.model, "deepseek-default");
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some("hello world"));
        assert_eq!(resp.choices[0].finish_reason, Some("stop"));
        assert_eq!(resp.usage.as_ref().unwrap().completion_tokens, 41);
    }

    #[tokio::test]
    async fn aggregate_thinking() {
        let frames = make_ds_stream(&[("thinking", "THINK"), ("answer", "RESPONSE")], None);
        let stream = futures::stream::iter(frames);
        let resp = aggregate(
            stream,
            "deepseek-expert".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )
        .await
        .unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.reasoning_content.as_deref(), Some("thinking"));
        assert_eq!(msg.content.as_deref(), Some("answer"));
        assert_eq!(resp.choices[0].finish_reason, Some("stop"));
    }

    #[tokio::test]
    async fn aggregate_tool_calls() {
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "beijing"}}]"#);
        let frames = make_ds_stream(&[(&tool_xml, "RESPONSE")], None);
        let stream = futures::stream::iter(frames);
        let resp = aggregate(
            stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )
        .await
        .unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some(""));
        let calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].ty, "function");
        assert_eq!(calls[0].function.as_ref().unwrap().name, "get_weather");
        assert_eq!(
            calls[0].function.as_ref().unwrap().arguments,
            r#"{"city":"beijing"}"#
        );
        assert_eq!(resp.choices[0].finish_reason, Some("tool_calls"));
    }
    use std::pin::Pin;

    fn to_bytes_stream(
        st: ChunkStream,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes, OpenAIAdapterError>> + Send>> {
        Box::pin(st.map(|r| r.and_then(|c| sse_serialize(&c))))
    }

    async fn collect_chunks(
        st: Pin<Box<dyn Stream<Item = Result<Bytes, OpenAIAdapterError>> + Send>>,
    ) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        let mut st = st;
        while let Some(res) = st.next().await {
            let text = String::from_utf8(res.unwrap().to_vec()).unwrap();
            let json = text
                .strip_prefix("data: ")
                .unwrap()
                .strip_suffix("\n\n")
                .unwrap();
            out.push(serde_json::from_str(json).unwrap());
        }
        out
    }

    #[tokio::test]
    async fn stream_plain_text() {
        let frames = make_ds_stream(&[("hi", "RESPONSE")], None);
        let bytes_stream = futures::stream::iter(frames);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (plain_text) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!("chunk[{i}]:\n{}", serde_json::to_string_pretty(c).unwrap());
        }
        println!("===================================\n");
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        // Tất cả content gộp lại phải là "hi"
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(all_content, "hi");
        // finish_reason cuối
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "stop"
        );
    }

    #[tokio::test]
    async fn stream_include_usage() {
        let frames = make_ds_stream(&[("x", "RESPONSE")], Some(12));
        let bytes_stream = futures::stream::iter(frames);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "m".into(),
            super::StreamCfg {
                include_usage: true,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (include_usage) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!("chunk[{i}]:\n{}", serde_json::to_string_pretty(c).unwrap());
        }
        println!("======================================\n");
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        // Tất cả content gộp lại phải là "x"
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(all_content, "x");
        // usage chunk
        let usage_chunk = chunks
            .iter()
            .find(|c| c["usage"]["completion_tokens"].as_i64() == Some(12));
        assert!(usage_chunk.is_some(), "should have usage chunk");
        // finish_reason nằm trong chunk cuối có choices
        let finish_chunk = chunks.iter().rev().find(|c| {
            c["choices"].as_array().map_or(false, |a| !a.is_empty())
                && c["choices"][0]["finish_reason"].as_str().is_some()
        });
        assert_eq!(finish_chunk.unwrap()["choices"][0]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn stream_tool_calls() {
        let tool_xml = tool_span(r#"[{"name": "f", "arguments": {}}]"#);
        let frames = make_ds_stream(&[(&tool_xml, "RESPONSE")], None);
        let bytes_stream = futures::stream::iter(frames);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (tool_calls) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!("chunk[{i}]:\n{}", serde_json::to_string_pretty(c).unwrap());
        }
        println!("===================================\n");
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk");
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert!(
            !all_content.contains(tool_parser::TOOL_CALL_START),
            "content should not contain tool_calls tags"
        );
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[tokio::test]
    async fn stream_fragmented_tool_calls_with_thinking() {
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "Hanoi"}}]"#);
        let frames = make_ds_stream(&[("dang suy nghi", "THINK"), (&tool_xml, "RESPONSE")], None);
        let bytes_stream = futures::stream::iter(frames);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (fragmented_tool_calls_with_thinking) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!("chunk[{i}]:\n{}", serde_json::to_string_pretty(c).unwrap());
        }
        println!("============================================================\n");
        assert!(chunks.len() >= 3);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        // reasoning_content phải chứa nội dung suy luận
        let all_reasoning: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["reasoning_content"].as_str())
            .collect();
        assert!(
            all_reasoning.contains("dang suy nghi"),
            "should contain dang suy nghi"
        );
        // Một chunk phải chứa tool_calls
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk");
        let tc_chunk = chunks
            .iter()
            .find(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some())
            .unwrap();
        let calls = tc_chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"city":"Hanoi"}"#);
        // finish
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[tokio::test]
    async fn stream_with_tool_search_and_open() {
        let fixture = "event: ready\ndata: {}\n\n\
            data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"THINK\",\"content\":\"suy nghi\"}]}}}\n\n\
            data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":3,\"type\":\"TOOL_SEARCH\",\"content\":null,\"queries\":[{\"query\":\"q\"}],\"results\":[],\"stage_id\":1}]}\n\n\
            data: {\"p\":\"response/fragments/-2/results\",\"o\":\"SET\",\"v\":[{\"url\":\"https://example.com\",\"title\":\"ex\",\"snippet\":\"snip\"}]}\n\n\
            data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":4,\"type\":\"TOOL_OPEN\",\"status\":\"WIP\",\"result\":{\"url\":\"https://open.com\",\"title\":\"open\",\"snippet\":\"open-snippet\"},\"reference\":{\"id\":3,\"type\":\"TOOL_SEARCH\"},\"stage_id\":1}]}\n\n\
            data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"THINK\",\"content\":\"tiep tuc\"}]}\n\n\
            data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"RESPONSE\",\"content\":\"\"}]}\n\n\
            data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\"hello\"}\n\n\
            data: {\"p\":\"response/status\",\"o\":\"SET\",\"v\":\"FINISHED\"}\n\n";
        let bytes_stream = futures::stream::iter(vec![sse_bytes(fixture)]);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (tool_search_and_open) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!("chunk[{i}]:\n{}", serde_json::to_string_pretty(c).unwrap());
        }
        println!("=============================================\n");
        assert!(chunks.len() >= 3);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        // Tất cả reasoning gộp lại phải chứa "suy nghi" và "tiep tuc"
        let all_reasoning: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["reasoning_content"].as_str())
            .collect();
        assert!(
            all_reasoning.contains("suy nghi"),
            "should contain suy nghi"
        );
        assert!(
            all_reasoning.contains("tiep tuc"),
            "should contain tiep tuc"
        );
        // Tất cả content gộp lại phải là "hello"
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(all_content, "hello");
        // finish_reason
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "stop"
        );
    }

    #[tokio::test]
    async fn stream_include_obfuscation() {
        let frames = make_ds_stream(
            &[(
                "day la mot doan van ban tieng Viet du dai de kiem thu obfuscation",
                "RESPONSE",
            )],
            None,
        );
        let bytes_stream = futures::stream::iter(frames);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: true,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (include_obfuscation) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!(
                "chunk[{i}] len={}:\n{}",
                serde_json::to_string(c).unwrap().len(),
                serde_json::to_string_pretty(c).unwrap()
            );
        }
        println!("============================================\n");
        assert!(chunks.len() >= 2);
        // Mọi chunk có choices và content phải được padding động gần độ dài mục tiêu
        for c in &chunks {
            if c["choices"][0]["delta"]["content"].as_str().is_some()
                || c["choices"][0]["finish_reason"].as_str().is_some()
            {
                assert!(
                    c["choices"][0]["delta"]["obfuscation"].as_str().is_some(),
                    "chunk with content or finish_reason should have obfuscation"
                );
                let len = serde_json::to_string(c).unwrap().len();
                assert!(
                    len >= 490 && len <= 530,
                    "chunk len {} out of expected 490..=530 range",
                    len
                );
            }
        }
        // Nội dung đầy đủ
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert!(
            all_content.contains("van ban tieng Viet du dai"),
            "should contain expected text, got {all_content:?}"
        );
        // finish_reason
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "stop"
        );
    }

    #[tokio::test]
    async fn aggregate_tool_calls_with_leading_text() {
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "beijing"}}]"#);
        let frames = make_ds_stream(
            &[
                ("Duoc, toi se ho tro ban.", "RESPONSE"),
                (&tool_xml, "RESPONSE"),
            ],
            None,
        );
        let stream = futures::stream::iter(frames);
        let resp = aggregate(
            stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )
        .await
        .unwrap();
        let msg = &resp.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some("Duoc, toi se ho tro ban."));
        let calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.as_ref().unwrap().name, "get_weather");
        assert_eq!(
            calls[0].function.as_ref().unwrap().arguments,
            r#"{"city":"beijing"}"#
        );
        assert_eq!(resp.choices[0].finish_reason, Some("tool_calls"));
    }

    #[tokio::test]
    async fn stream_tool_calls_with_leading_text_fragmented() {
        let tool_xml = tool_span(
            r#"[{"name": "astrbot_execute_shell", "arguments": {"command": "cat /data/astrbot/skills/doubao-image-gen/SKILL.md"}}]"#,
        );
        let frames = make_ds_stream(
            &[
                ("Duoc, toi se giup ban tao anh bang Doubao.", "RESPONSE"),
                (&tool_xml, "RESPONSE"),
            ],
            None,
        );
        let bytes_stream = futures::stream::iter(frames);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (tool_calls with leading text, fragmented) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!("chunk[{i}]:\n{}", serde_json::to_string_pretty(c).unwrap());
        }
        println!("====================================================================\n");
        // Kiểm tra ngữ nghĩa lõi: leading text + tool_calls + finish_reason
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        // Tất cả content gộp lại phải chứa leading text
        let all_content: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert!(
            all_content.contains("Duoc, toi se giup ban tao anh bang Doubao"),
            "should contain leading text, got {all_content:?}"
        );
        // Một chunk phải chứa tool_calls
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk");
        let tc_chunk = chunks
            .iter()
            .find(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some())
            .unwrap();
        let calls = tc_chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "astrbot_execute_shell");
        // finish
        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    }

    #[tokio::test]
    async fn stream_tool_calls_with_leading_text_multi_chunk_fragments() {
        let tool_xml = tool_span(r#"[{"name": "f", "arguments": {}}]"#);
        let frames = make_ds_stream(
            &[("Toi se kiem tra.", "RESPONSE"), (&tool_xml, "RESPONSE")],
            None,
        );
        let bytes_stream = futures::stream::iter(frames);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (leading text + multi-chunk JSON fragments) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!("chunk[{i}]:\n{}", serde_json::to_string_pretty(c).unwrap());
        }
        println!("=============================================================\n");
        // Phải output: role, leading text, tool_calls, finish
        for (i, c) in chunks.iter().enumerate() {
            eprintln!(
                "chunk[{}] content={:?} tool_calls={:?} finish={:?}",
                i,
                c["choices"][0]["delta"]["content"],
                c["choices"][0]["delta"]["tool_calls"],
                c["choices"][0]["finish_reason"]
            );
        }
        // Phải có chunk tool_calls
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk but didn't");
        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    }

    #[tokio::test]
    async fn stream_tool_calls_with_thinking_then_leading_text_then_fragmented_json() {
        // Kịch bản production đầy đủ nhất: thinking -> leading text -> tool_calls bị chia mảnh
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "beijing"}}]"#);
        let frames = make_ds_stream(
            &[
                (
                    "Nguoi dung muon xem thoi tiet, toi can goi cong cu",
                    "THINK",
                ),
                ("Duoc, toi se kiem tra giup ban.", "RESPONSE"),
                (&tool_xml, "RESPONSE"),
            ],
            None,
        );
        let bytes_stream = futures::stream::iter(frames);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (thinking + leading + fragmented JSON) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!("chunk[{i}]:\n{}", serde_json::to_string_pretty(c).unwrap());
        }
        println!("=============================================================\n");
        for (i, c) in chunks.iter().enumerate() {
            eprintln!(
                "chunk[{}] content={:?} reasoning={:?} tool_calls={:?} finish={:?}",
                i,
                c["choices"][0]["delta"]["content"],
                c["choices"][0]["delta"]["reasoning_content"],
                c["choices"][0]["delta"]["tool_calls"],
                c["choices"][0]["finish_reason"]
            );
        }
        // Phải có chunk tool_calls
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk but didn't");
        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    }

    #[tokio::test]
    async fn stream_tool_calls_json_split_right_after_tag() {
        let tool_xml = tool_span(r#"[{"name": "f", "arguments": {}}]"#);
        let frames = make_ds_stream(&[("Duoc.", "RESPONSE"), (&tool_xml, "RESPONSE")], None);
        let bytes_stream = futures::stream::iter(frames);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "m".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (JSON split right after tool_call) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!("chunk[{i}]:\n{}", serde_json::to_string_pretty(c).unwrap());
        }
        println!("=============================================================\n");
        let has_tool_calls = chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some());
        assert!(has_tool_calls, "should have a tool_calls chunk but didn't");
        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    }

    #[tokio::test]
    async fn stream_tool_calls_no_leading_text() {
        let tool_xml = tool_span(r#"[{"name": "get_weather", "arguments": {"city": "beijing"}}]"#);
        let frames = make_ds_stream(&[(&tool_xml, "RESPONSE")], None);
        let bytes_stream = futures::stream::iter(frames);
        let chunks = collect_chunks(to_bytes_stream(super::stream(
            bytes_stream,
            "deepseek-default".into(),
            super::StreamCfg {
                include_usage: false,
                include_obfuscation: false,
                stop: vec![],
                prompt_tokens: 0,
                repair_fn: None,
                tag_config: default_tag_config(),
            },
        )))
        .await;
        println!("\n=== STREAM CHUNKS (tool_calls, no leading text) ===");
        for (i, c) in chunks.iter().enumerate() {
            println!("chunk[{i}]:\n{}", serde_json::to_string_pretty(c).unwrap());
        }
        println!("===================================================\n");
        // Phải có role chunk + tool_calls chunk + finish chunk
        for (i, c) in chunks.iter().enumerate() {
            eprintln!(
                "chunk[{}] content={:?} tool_calls={:?} finish={:?}",
                i,
                c["choices"][0]["delta"]["content"],
                c["choices"][0]["delta"]["tool_calls"],
                c["choices"][0]["finish_reason"]
            );
        }
        assert!(
            chunks.len() >= 2,
            "expected at least 2 chunks, got {}",
            chunks.len()
        );
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        // Tìm chunk có tool_calls
        let tc_idx = chunks
            .iter()
            .position(|c| c["choices"][0]["delta"]["tool_calls"].as_array().is_some())
            .expect("should have a chunk with tool_calls");
        let tc_chunk = &chunks[tc_idx];
        let calls = tc_chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"city":"beijing"}"#);
        // finish_reason của chunk cuối phải là tool_calls
        let last = chunks.last().unwrap();
        assert_eq!(
            last["choices"][0]["finish_reason"], "tool_calls",
            "finish_reason should be tool_calls, got {:?}",
            last["choices"][0]["finish_reason"]
        );
    }
}
