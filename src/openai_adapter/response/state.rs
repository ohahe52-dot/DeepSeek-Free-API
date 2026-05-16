//! State machine DeepSeek Patch - parse thao tác path p/o/v và tạo frame delta
//!
//! Module này khớp thuật toán parse SSR delta của frontend chat.deepseek.com (DeltaParser + lớp rm):
//! - `p` / `o` được giữ xuyên event (event sau có thể bỏ qua)
//! - Giá trị mặc định của `o` là "SET"
//! - BATCH phân rã đệ quy, path con được prefix bằng path cha (dùng parser con độc lập)
//! - APPEND với string = `+=`, không có ngữ nghĩa thay snapshot

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use pin_project_lite::pin_project;

use log::{trace, warn};

use crate::openai_adapter::OpenAIAdapterError;

use super::sse_parser::SseEvent;

const FRAG_THINK: &str = "THINK";
const FRAG_RESPONSE: &str = "RESPONSE";

/// Một frame delta parse từ stream DeepSeek
#[derive(Debug, Clone)]
pub enum DsFrame {
    /// event: ready, dùng để tạo delta.role = assistant
    Role,
    /// Text append từ fragment THINK
    ThinkDelta(String),
    /// Text append từ fragment RESPONSE
    ContentDelta(String),
    /// Thay đổi response/status
    Status(String),
    /// Giá trị accumulated_token_usage
    Usage(u32),
}

#[derive(Debug, Default)]
struct Fragment {
    ty: String,
    content: String,
}

/// Duy trì trạng thái patch của response DeepSeek, tạo frame delta cho converter tiêu thụ
///
/// current_path / current_op giữ xuyên event, mặc định khớp DeltaParser frontend:
/// - `current_op` mặc định "SET" (snapshot ban đầu, update status... khi không có `o` rõ ràng)
/// - `current_path` mặc định None (snapshot ban đầu không có `p` thì vào xử lý đặc biệt)
#[derive(Debug, Default)]
pub struct DsState {
    current_path: Option<String>,
    current_op: Option<String>,
    fragments: Vec<Fragment>,
    status: Option<String>,
    accumulated_token_usage: Option<u32>,
}

impl DsState {
    /// Tiêu thụ một event SSE, trả về không hoặc nhiều frame delta
    pub fn apply_event(&mut self, evt: &SseEvent) -> Vec<DsFrame> {
        let mut frames = Vec::new();

        if let Some("ready") = evt.event.as_deref() {
            frames.push(DsFrame::Role);
        }

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&evt.data) {
            frames.extend(self.apply_patch_value(val));
        }

        frames
    }

    /// Áp dụng một event `p/o/v` (gồm giữ xuyên event + phân rã BATCH + xử lý snapshot ban đầu)
    fn apply_patch_value(&mut self, val: serde_json::Value) -> Vec<DsFrame> {
        // 1. Giữ p/o xuyên event
        if let Some(p) = val.get("p").and_then(|v| v.as_str()) {
            self.current_path = Some(p.to_string());
        }
        if let Some(o) = val.get("o").and_then(|v| v.as_str()) {
            self.current_op = Some(o.to_string());
        }

        let op = self.current_op.as_deref().unwrap_or("SET").to_string();
        let path = self.current_path.as_deref().unwrap_or("").to_string();

        let Some(v) = val.get("v") else {
            return Vec::new();
        };

        // 2. Snapshot ban đầu: không có path và v chứa response (khởi tạo trạng thái đầy đủ từ frontend)
        if self.current_path.is_none()
            && let Some(response) = v.get("response")
        {
            return self.apply_initial_snapshot(response);
        }

        // 3. Phân rã BATCH: dùng parser con độc lập, không làm bẩn path/op tầng ngoài
        if op == "BATCH" {
            if v.is_array() {
                return self.apply_batch(&path, v);
            }
            // v không phải mảng nhưng mang BATCH: op là phần sót từ event trước; event này thực chất là SET.
            // (Trong stream thật, event status/usage sẽ có o="SET" rõ ràng; đây là xử lý phòng thủ.)
            return self.apply_path(&path, "SET", v);
        }

        // 4. Một thao tác SET / APPEND
        self.apply_path(&path, &op, v)
    }

    /// Phân rã BATCH đệ quy. Dùng trạng thái parser con cục bộ (sub_path / sub_op),
    /// không sửa self.current_path / self.current_op, giữ nguyên trạng thái tầng ngoài.
    fn apply_batch(&mut self, parent_path: &str, v: &serde_json::Value) -> Vec<DsFrame> {
        let mut frames = Vec::new();
        let Some(arr) = v.as_array() else {
            return frames;
        };

        // Trạng thái parser con độc lập (khớp DeltaParser frontend: BATCH tạo parser mới)
        let (mut sub_path, mut sub_op) = (String::new(), "SET".to_string());

        for item in arr {
            if let Some(p) = item.get("p").and_then(|v| v.as_str()) {
                sub_path = p.to_string();
            }
            if let Some(o) = item.get("o").and_then(|v| v.as_str()) {
                sub_op = o.to_string();
            }

            let Some(v) = item.get("v") else {
                continue;
            };

            if sub_op == "BATCH" {
                // BATCH lồng nhau: ghép path đầy đủ rồi đệ quy
                let nested = if parent_path.is_empty() {
                    sub_path.clone()
                } else if sub_path.is_empty() {
                    parent_path.to_string()
                } else {
                    format!("{}/{}", parent_path, sub_path)
                };
                frames.extend(self.apply_batch(&nested, v));
            } else {
                let full_path = if parent_path.is_empty() {
                    sub_path.clone()
                } else if sub_path.is_empty() {
                    parent_path.to_string()
                } else {
                    format!("{}/{}", parent_path, sub_path)
                };
                frames.extend(self.apply_path(&full_path, &sub_op, v));
            }
        }

        frames
    }

    fn apply_initial_snapshot(&mut self, response: &serde_json::Value) -> Vec<DsFrame> {
        let mut frames = Vec::new();

        // status
        if let Some(s) = response.get("status").and_then(|v| v.as_str()) {
            self.status = Some(s.to_string());
        }

        // token usage
        if let Some(n) = response
            .get("accumulated_token_usage")
            .and_then(|v| v.as_u64())
        {
            self.accumulated_token_usage = Some(u32::try_from(n).unwrap_or(u32::MAX));
        }

        // fragments
        if let Some(arr) = response.get("fragments").and_then(|f| f.as_array()) {
            self.fragments.clear();
            for frag in arr {
                let Some(ty) = frag.get("type").and_then(|t| t.as_str()) else {
                    continue;
                };
                let content = frag
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                self.fragments.push(Fragment {
                    ty: ty.to_string(),
                    content: content.clone(),
                });
                if !content.is_empty() {
                    match ty {
                        FRAG_THINK => frames.push(DsFrame::ThinkDelta(content)),
                        FRAG_RESPONSE => frames.push(DsFrame::ContentDelta(content)),
                        _ => {}
                    }
                }
            }
        }

        frames
    }

    fn apply_path(&mut self, path: &str, op: &str, val: &serde_json::Value) -> Vec<DsFrame> {
        let mut frames = Vec::new();

        match path {
            "response/status" => {
                if let Some(s) = val.as_str() {
                    self.status = Some(s.to_string());
                    if s == "FINISHED" || s == "INCOMPLETE" {
                        let has_response = self
                            .fragments
                            .iter()
                            .any(|f| f.ty == "RESPONSE" && !f.content.is_empty());
                        if !has_response && s == "FINISHED" {
                            warn!(
                                target: "adapter",
                                "State machine FINISHED nhưng không có nội dung RESPONSE: fragments={:?}, status={:?}, accumulated_token_usage={:?}",
                                self.fragments.iter().map(|f| format!("{}/{}", f.ty, f.content.len())).collect::<Vec<_>>(),
                                self.status, self.accumulated_token_usage
                            );
                        }
                    }
                    frames.push(DsFrame::Status(s.to_string()));
                }
            }
            "response/accumulated_token_usage" | "accumulated_token_usage" => {
                if let Some(n) = val.as_u64() {
                    let u = u32::try_from(n).unwrap_or(u32::MAX);
                    self.accumulated_token_usage = Some(u);
                    frames.push(DsFrame::Usage(u));
                }
            }
            "response/fragments/-1/content" => {
                if let Some(s) = val.as_str()
                    && let Some(frag) = self.fragments.last_mut()
                {
                    match frag.ty.as_str() {
                        FRAG_THINK => {
                            frag.content.push_str(s);
                            frames.push(DsFrame::ThinkDelta(s.to_string()));
                        }
                        FRAG_RESPONSE => {
                            frag.content.push_str(s);
                            frames.push(DsFrame::ContentDelta(s.to_string()));
                        }
                        _ => {}
                    }
                }
            }
            "response/fragments" if op == "APPEND" => {
                if let Some(arr) = val.as_array() {
                    for item in arr {
                        if let Some(ty) = item.get("type").and_then(|t| t.as_str()) {
                            let content = item
                                .get("content")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            self.fragments.push(Fragment {
                                ty: ty.to_string(),
                                content: content.clone(),
                            });
                            if !content.is_empty() {
                                match ty {
                                    FRAG_THINK => frames.push(DsFrame::ThinkDelta(content)),
                                    FRAG_RESPONSE => frames.push(DsFrame::ContentDelta(content)),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        frames
    }
}

pin_project! {
    // Stream wrapper áp dụng state machine patch lên stream event SSE
    pub struct StateStream<S> {
        #[pin]
        inner: S,
        state: DsState,
        pending: Vec<DsFrame>,
    }
}

impl<S> StateStream<S> {
    /// Tạo wrapper stream trạng thái
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            state: DsState::default(),
            pending: Vec::new(),
        }
    }
}

impl<S, E> Stream for StateStream<S>
where
    S: Stream<Item = Result<SseEvent, E>>,
    E: Into<OpenAIAdapterError>,
{
    type Item = Result<DsFrame, OpenAIAdapterError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if let Some(frame) = this.pending.pop() {
            return Poll::Ready(Some(Ok(frame)));
        }

        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(evt))) => {
                    let frames = this.state.apply_event(&evt);
                    if frames.is_empty() {
                        continue;
                    }
                    let mut frames = frames;
                    let first = frames.remove(0);
                    trace!(target: "adapter", ">>> state: {}", trace_frame(&first));
                    // Đẩy frame còn lại vào pending theo thứ tự xuôi (do push rồi pop sẽ đảo, nên extend đảo)
                    this.pending.extend(frames.into_iter().rev());
                    return Poll::Ready(Some(Ok(first)));
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e.into())));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Dùng cho log TRACE: cắt text dài, biến thể khác Debug trực tiếp
fn trace_frame(frame: &DsFrame) -> String {
    const MAX_LEN: usize = 60;
    match frame {
        DsFrame::ContentDelta(s) | DsFrame::ThinkDelta(s) => {
            let ty = if matches!(frame, DsFrame::ContentDelta(_)) {
                "ContentDelta"
            } else {
                "ThinkDelta"
            };
            if s.len() > MAX_LEN {
                format!("{}(\"{}\")", ty, &s[..MAX_LEN])
            } else {
                format!("{:?}", frame)
            }
        }
        _ => format!("{:?}", frame),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_content_with_explicit_append() {
        let mut state = DsState::default();
        state.fragments.push(Fragment {
            ty: "RESPONSE".into(),
            content: "".into(),
        });
        let evt = SseEvent {
            event: None,
            data: r#"{"p":"response/fragments/-1/content","o":"APPEND","v":"hello"}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert!(matches!(&frames[0], DsFrame::ContentDelta(s) if s == "hello"));
    }

    #[test]
    fn append_content_with_bare_v_after_path_set() {
        let mut state = DsState::default();
        state.fragments.push(Fragment {
            ty: "RESPONSE".into(),
            content: "hello".into(),
        });
        // Mô phỏng event trước đã đặt path và op=APPEND
        state.current_path = Some("response/fragments/-1/content".into());
        state.current_op = Some("APPEND".into());
        let evt = SseEvent {
            event: None,
            data: r#"{"v":" world"}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert!(matches!(&frames[0], DsFrame::ContentDelta(s) if s == " world"));
    }

    #[test]
    fn snapshot_then_append() {
        let mut state = DsState::default();
        let evt = SseEvent {
            event: None,
            data: r#"{"v":{"response":{"fragments":[{"type":"THINK","content":"hi"}]}}}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert!(matches!(&frames[0], DsFrame::ThinkDelta(s) if s == "hi"));
    }

    #[test]
    fn ready_event() {
        let mut state = DsState::default();
        let frames = state.apply_event(&SseEvent {
            event: Some("ready".into()),
            data: "{}".into(),
        });
        assert!(matches!(frames[0], DsFrame::Role));
    }

    #[test]
    fn batch_accumulated_token_usage() {
        let mut state = DsState::default();
        let evt = SseEvent {
            event: None,
            data: r#"{"p":"response","o":"BATCH","v":[{"p":"accumulated_token_usage","v":41},{"p":"quasi_status","v":"FINISHED"}]}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert!(matches!(
            &frames[0],
            DsFrame::Usage(u) if *u == 41
        ));
    }

    #[test]
    fn batch_fragment_level_with_path_prepending() {
        let mut state = DsState::default();
        state.fragments.push(Fragment {
            ty: "RESPONSE".into(),
            content: "hello".into(),
        });
        let evt = SseEvent {
            event: None,
            data: r#"{"p":"response/fragments/-1","o":"BATCH","v":[{"p":"content","o":"APPEND","v":"[reference:3]"},{"p":"references","o":"SET","v":[{"id":5,"type":"TOOL_OPEN"}]}]}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert_eq!(frames.len(), 1);
        assert!(matches!(&frames[0], DsFrame::ContentDelta(s) if s == "[reference:3]"));
        assert_eq!(
            state.fragments.last().unwrap().content,
            "hello[reference:3]"
        );
    }

    #[test]
    fn batch_fragment_bare_v_array_continues_batch() {
        let mut state = DsState::default();
        state.fragments.push(Fragment {
            ty: "RESPONSE".into(),
            content: "hello world".into(),
        });
        state.current_path = Some("response/fragments/-1".into());
        state.current_op = Some("BATCH".into());
        let evt = SseEvent {
            event: None,
            data: r#"{"v":[{"p":"content","o":"APPEND","v":"[reference:1]"},{"p":"references","v":[{"id":6,"type":"TOOL_OPEN"}]}]}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert_eq!(frames.len(), 1);
        assert!(matches!(&frames[0], DsFrame::ContentDelta(s) if s == "[reference:1]"));
        assert_eq!(
            state.fragments.last().unwrap().content,
            "hello world[reference:1]"
        );
    }

    #[test]
    fn incomplete_status_with_finish_event() {
        let mut state = DsState::default();
        let evt = SseEvent {
            event: None,
            data: r#"{"p":"response/status","v":"INCOMPLETE"}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert_eq!(frames.len(), 1);
        assert!(matches!(&frames[0], DsFrame::Status(s) if s == "INCOMPLETE"));
    }

    #[test]
    fn batch_decomposition_preserves_outer_path() {
        // Sau khi BATCH kết thúc, path/op tầng ngoài giữ nguyên
        let mut state = DsState::default();
        // Mô phỏng một đoạn hội thoại bình thường trước: đặt path+op, rồi BATCH, đảm bảo path/op phục hồi sau BATCH
        state.current_path = Some("response/fragments/-1".into());
        state.current_op = Some("BATCH".into());
        let evt = SseEvent {
            event: None,
            data: r#"{"v":[{"p":"content","o":"APPEND","v":"x"}]}"#.into(),
        };
        state.apply_event(&evt);
        assert_eq!(state.current_path.as_deref(), Some("response/fragments/-1"));
        assert_eq!(state.current_op.as_deref(), Some("BATCH"));
    }

    #[test]
    fn complex_tool_search_with_think_and_response() {
        let mut state = DsState::default();

        // Snapshot ban đầu: fragment THINK
        let evt = SseEvent {
            event: None,
            data: r#"{"v":{"response":{"fragments":[{"type":"THINK","content":"suy nghi"}]}}}"#
                .into(),
        };
        state.apply_event(&evt);
        assert_eq!(state.fragments.len(), 1);

        // TOOL_SEARCH APPEND
        let evt = SseEvent {
            event: None,
            data: r#"{"p":"response/fragments","o":"APPEND","v":[{"id":3,"type":"TOOL_SEARCH","content":null,"queries":[{"query":"q"}],"results":[]}]}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert!(frames.is_empty()); // TOOL_SEARCH không tạo nội dung nhìn thấy
        assert_eq!(state.fragments.len(), 2);
        assert_eq!(state.fragments[1].ty, "TOOL_SEARCH");

        // TOOL_OPEN APPEND
        let evt = SseEvent {
            event: None,
            data: r#"{"p":"response/fragments","o":"APPEND","v":[{"id":4,"type":"TOOL_OPEN","status":"WIP","result":{"url":"https://x.com","title":"t","snippet":"s"},"reference":{"id":3,"type":"TOOL_SEARCH"}}]}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert!(frames.is_empty());
        assert_eq!(state.fragments.len(), 3);

        // THINK APPEND mới
        let evt = SseEvent {
            event: None,
            data:
                r#"{"p":"response/fragments","o":"APPEND","v":[{"type":"THINK","content":"tiep tuc"}]}"#
                    .into(),
        };
        let frames = state.apply_event(&evt);
        assert_eq!(frames.len(), 1);
        assert!(matches!(&frames[0], DsFrame::ThinkDelta(s) if s == "tiep tuc"));
        assert_eq!(state.fragments.len(), 4);

        // RESPONSE APPEND
        let evt = SseEvent {
            event: None,
            data:
                r#"{"p":"response/fragments","o":"APPEND","v":[{"type":"RESPONSE","content":""}]}"#
                    .into(),
        };
        let frames = state.apply_event(&evt);
        assert!(frames.is_empty());
        assert_eq!(state.fragments.len(), 5);

        // RESPONSE content
        let evt = SseEvent {
            event: None,
            data: r#"{"p":"response/fragments/-1/content","o":"APPEND","v":"hello"}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert!(matches!(&frames[0], DsFrame::ContentDelta(s) if s == "hello"));

        // FINISHED
        let evt = SseEvent {
            event: None,
            data: r#"{"p":"response/status","v":"FINISHED"}"#.into(),
        };
        let frames = state.apply_event(&evt);
        assert!(matches!(&frames[0], DsFrame::Status(s) if s == "FINISHED"));
    }
}
