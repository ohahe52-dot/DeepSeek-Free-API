//! Map response Anthropic - map OpenAI ChatCompletion thành Anthropic Message
//!
//! Module facade: khai báo module con, xuất kiểu dùng chung và helper.
//! `MessagesResponse` / `Usage` được định nghĩa trong `types.rs` (cùng module với kiểu request).

mod aggregate;
mod stream;

pub(crate) use aggregate::from_chat_completions;
pub(crate) use stream::from_chat_completion_stream;

/// Block nội dung response - định nghĩa trong `types.rs` là `ResponseContentBlock`, alias ở đây để giữ tương thích module con
pub(crate) use crate::anthropic_compat::types::ResponseContentBlock as ContentBlock;

// ============================================================================
// Helper dùng chung
// ============================================================================

pub(crate) fn finish_reason_map(reason: &str) -> String {
    match reason {
        "stop" => "end_turn".to_string(),
        "tool_calls" => "tool_use".to_string(),
        _ => reason.to_string(),
    }
}

/// OpenAI id có format chatcmpl-xxx, map thành msg_xxx
pub(crate) fn map_id(openai_id: &str) -> String {
    openai_id
        .strip_prefix("chatcmpl-")
        .map(|hex| format!("msg_{}", hex))
        .or_else(|| {
            openai_id
                .strip_prefix("call_")
                .map(|suffix| format!("toolu_{}", suffix))
        })
        .unwrap_or_else(|| format!("msg_{}", openai_id))
}
