//! Parse model - map field OpenAI model sang cờ năng lực ds_core
//!
//! Dùng registry inject từ ngoài để map động alias model sang model_type.

use std::collections::HashMap;

use crate::openai_adapter::types::WebSearchOptions;

/// Kết quả parse model
pub(crate) struct ModelResolution {
    /// model_type mà ds_core dùng
    pub model_type: String,
    pub thinking_enabled: bool,
    pub search_enabled: bool,
}

/// Parse cấu hình model theo model_id và tham số mở rộng
///
/// thinking_enabled bật khi reasoning_effort khác "none".
/// Model id kết thúc bằng `-nothinking` luôn tắt thinking.
/// Nếu reasoning_effort không được truyền và model không có `-nothinking`, mặc định xử lý như "high".
/// search_enabled bật mặc định (backend DeepSeek inject system prompt mạnh hơn khi ở chế độ search).
/// Có thể override bằng web_search_options tường minh.
pub(crate) fn resolve(
    registry: &HashMap<String, String>,
    model_id: &str,
    reasoning_effort: Option<&str>,
    web_search_options: Option<&WebSearchOptions>,
) -> Result<ModelResolution, String> {
    let key = model_id.to_lowercase();
    let nothinking = key.ends_with("-nothinking");
    let base_key = key.strip_suffix("-nothinking");
    let model_type = registry
        .get(&key)
        .or_else(|| base_key.and_then(|base| registry.get(base)))
        .cloned()
        .ok_or_else(|| format!("Mô hình không được hỗ trợ: {}", model_id))?;

    let reasoning_effort = reasoning_effort.unwrap_or("high");
    let thinking_enabled = !nothinking && reasoning_effort != "none";

    let search_enabled = web_search_options.map(|_| true).unwrap_or(true);

    Ok(ModelResolution {
        model_type,
        thinking_enabled,
        search_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> HashMap<String, String> {
        HashMap::from([
            ("deepseek-default".to_string(), "default".to_string()),
            ("deepseek-v4-flash".to_string(), "default".to_string()),
        ])
    }

    #[test]
    fn nothinking_suffix_disables_thinking_for_base_model() {
        let res = resolve(&registry(), "deepseek-default-nothinking", None, None).unwrap();

        assert_eq!(res.model_type, "default");
        assert!(!res.thinking_enabled);
    }

    #[test]
    fn nothinking_suffix_disables_thinking_for_alias() {
        let res = resolve(
            &registry(),
            "deepseek-v4-flash-nothinking",
            Some("high"),
            None,
        )
        .unwrap();

        assert_eq!(res.model_type, "default");
        assert!(!res.thinking_enabled);
    }
}
