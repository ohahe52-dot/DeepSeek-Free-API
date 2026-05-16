//! Kiểm tra request và thu gọn giá trị mặc định
//!
//! Trách nhiệm: kiểm tra field bắt buộc, format message, và chuẩn hóa tham số tùy chọn cho nội bộ.

use crate::openai_adapter::types::{ChatCompletionsRequest, StopSequence};

pub(crate) struct NormalizedParams {
    pub include_usage: bool,
    pub include_obfuscation: bool,
    pub stop: Vec<String>,
}

/// Thu gọn và trả về tham số đã chuẩn hóa
///
/// Quy tắc kiểm tra:
/// - model không được rỗng
/// - messages không được rỗng
/// - message role=tool phải có tool_call_id
/// - message role=function phải có name
pub(crate) fn apply(req: &ChatCompletionsRequest) -> Result<NormalizedParams, String> {
    if req.model.trim().is_empty() {
        return Err("Thiếu trường bắt buộc 'model'".into());
    }

    if req.messages.is_empty() {
        return Err("Thiếu trường bắt buộc 'messages'".into());
    }

    for (i, msg) in req.messages.iter().enumerate() {
        match msg.role.as_str() {
            "tool" if msg.tool_call_id.is_none() => {
                return Err(format!(
                    "messages[{}] có role 'tool' thì phải cung cấp 'tool_call_id'",
                    i
                ));
            }
            "function" if msg.name.is_none() => {
                return Err(format!(
                    "messages[{}] có role 'function' thì phải cung cấp 'name'",
                    i
                ));
            }
            _ => {}
        }
    }

    let include_usage = req
        .stream_options
        .as_ref()
        .map(|o| o.include_usage)
        .unwrap_or(false);

    let include_obfuscation = req
        .stream_options
        .as_ref()
        .map(|o| o.include_obfuscation)
        .unwrap_or(true);

    let stop = match &req.stop {
        Some(StopSequence::Single(s)) => vec![s.clone()],
        Some(StopSequence::Multiple(v)) => v.clone(),
        None => Vec::new(),
    };

    Ok(NormalizedParams {
        include_usage,
        include_obfuscation,
        stop,
    })
}
