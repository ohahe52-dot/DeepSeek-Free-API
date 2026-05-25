//! Parse SSE - cắt stream byte ds_core thành event SSE độc lập

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use pin_project_lite::pin_project;

use log::{trace, warn};

use crate::openai_adapter::OpenAIAdapterError;

/// Một event SSE
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

pin_project! {
    // Bọc stream byte tầng dưới, cắt thành event SSE độc lập
    pub struct SseStream<S> {
        #[pin]
        inner: S,
        text_buf: String,
        raw_buf: Vec<u8>,
    }
}

impl<S> SseStream<S> {
    /// Tạo wrapper stream SSE
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            text_buf: String::new(),
            raw_buf: Vec::new(),
        }
    }
}

impl<S, E> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: std::fmt::Display,
{
    type Item = Result<SseEvent, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.raw_buf.extend_from_slice(&bytes);
                    decode_utf8_prefix(this.raw_buf, this.text_buf);
                    if let Some(evt) = try_pop_event(this.text_buf) {
                        trace!(target: "adapter", "<<< {} {}", evt.event.as_deref().unwrap_or("-"), evt.data);
                        return Poll::Ready(Some(Ok(evt)));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    warn!(target: "adapter", "Lỗi stream SSE: {}", e);
                    return Poll::Ready(Some(Err(OpenAIAdapterError::Internal(format!(
                        "Lỗi stream SSE: {}",
                        e
                    )))));
                }
                Poll::Ready(None) => {
                    decode_utf8_prefix(this.raw_buf, this.text_buf);
                    if !this.raw_buf.is_empty() {
                        this.text_buf
                            .push_str(&String::from_utf8_lossy(this.raw_buf));
                        this.raw_buf.clear();
                    }
                    return try_pop_event(this.text_buf)
                        .map_or(Poll::Ready(None), |evt| Poll::Ready(Some(Ok(evt))));
                }
                Poll::Pending => {
                    decode_utf8_prefix(this.raw_buf, this.text_buf);
                    if let Some(evt) = try_pop_event(this.text_buf) {
                        return Poll::Ready(Some(Ok(evt)));
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}

/// Chuyển tiền tố UTF-8 hoàn chỉnh từ raw_buf sang text_buf, giữ byte chưa hoàn chỉnh trong raw_buf
fn decode_utf8_prefix(raw: &mut Vec<u8>, text: &mut String) {
    if raw.is_empty() {
        return;
    }
    match std::str::from_utf8(raw) {
        Ok(s) => {
            text.push_str(s);
            raw.clear();
        }
        Err(e) => {
            let up_to = e.valid_up_to();
            if up_to > 0 {
                // valid_up_to() đảm bảo tiền tố này là UTF-8 hợp lệ
                text.push_str(
                    std::str::from_utf8(&raw[..up_to])
                        .expect("prefix after valid_up_to() must be valid UTF-8"),
                );
                raw.drain(..up_to);
            }
        }
    }
}

/// Trích block event SSE đầu tiên được phân tách bằng \n\n từ buf
fn try_pop_event(buf: &mut String) -> Option<SseEvent> {
    let pos = buf.find("\n\n")?;
    let tail = buf.split_off(pos);
    let block = std::mem::take(buf);
    *buf = tail;
    if buf.starts_with("\n\n") {
        buf.drain(..2);
    }

    let mut event = None;
    let mut data = String::new();
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.trim_start());
        }
    }
    Some(SseEvent { event, data })
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    #[tokio::test]
    async fn split_simple_events() {
        let input = Bytes::from("event: ready\ndata: {}\n\nevent: finish\ndata: {}\n\n");
        let stream = SseStream::new(futures::stream::iter(vec![Ok::<_, std::io::Error>(input)]));
        let events: Vec<_> = stream.map(|r| r.unwrap()).collect().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("ready"));
        assert_eq!(events[0].data, "{}");
        assert_eq!(events[1].event.as_deref(), Some("finish"));
    }

    #[tokio::test]
    async fn split_across_chunks() {
        let parts: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from("event: ready\ndata: {}")),
            Ok(Bytes::from("\n\nevent: finish\ndata: {}\n\n")),
        ];
        let stream = SseStream::new(futures::stream::iter(parts));
        let events: Vec<_> = stream.map(|r| r.unwrap()).collect().await;
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn multi_line_data_is_joined_with_newlines() {
        let input = Bytes::from("event: ready\ndata: first\ndata: second\n\n");
        let stream = SseStream::new(futures::stream::iter(vec![Ok::<_, std::io::Error>(input)]));
        let events: Vec<_> = stream.map(|r| r.unwrap()).collect().await;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "first\nsecond");
    }

    #[tokio::test]
    async fn incomplete_utf8_is_completed_by_later_chunk() {
        let parts: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(&[0x64, 0x61, 0x74, 0x61, 0x3a, 0x20, 0xe2, 0x82])),
            Ok(Bytes::from_static(&[0xac, 0x0a, 0x0a])),
        ];
        let stream = SseStream::new(futures::stream::iter(parts));
        let events: Vec<_> = stream.map(|r| r.unwrap()).collect().await;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "€");
    }
}
