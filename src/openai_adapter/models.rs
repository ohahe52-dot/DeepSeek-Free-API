//! Tạo response danh sách model OpenAI
//!
//! Tạo tĩnh response OpenAI /models dựa trên DeepSeek model_types + model_aliases.

use crate::openai_adapter::types::{OpenAIModel, OpenAIModelList};

const MODEL_CREATED: u64 = 1_090_108_800;
const MODEL_OWNED_BY: &str = "deepseek-web (proxied by https://github.com/NIyueeE)";

/// Tạo danh sách model theo model_types + aliases
pub fn list(
    model_types: &[String],
    max_input_tokens: &[u32],
    max_output_tokens: &[u32],
    aliases: &[String],
) -> OpenAIModelList {
    let mut data: Vec<OpenAIModel> = model_types
        .iter()
        .enumerate()
        .map(|(idx, ty)| {
            let input = max_input_tokens.get(idx).copied();
            let output = max_output_tokens.get(idx).copied();
            make_model(&format!("deepseek-{}", ty), input, output)
        })
        .collect();

    // Thêm model alias (khớp index với model_types, hỗ trợ nhiều alias ngăn bằng dấu phẩy)
    for (i, alias) in aliases.iter().enumerate() {
        if let Some(_ty) = model_types.get(i) {
            let input = max_input_tokens.get(i).copied();
            let output = max_output_tokens.get(i).copied();
            for alias in split_model_aliases(alias) {
                data.push(make_model(alias, input, output));
            }
        }
    }

    OpenAIModelList {
        object: "list",
        data,
    }
}

/// Truy vấn một model
pub fn get(
    model_types: &[String],
    max_input_tokens: &[u32],
    max_output_tokens: &[u32],
    aliases: &[String],
    id: &str,
) -> Option<OpenAIModel> {
    let target = id.to_lowercase();

    // Tìm trong model_types trước
    if let Some((idx, ty)) = model_types
        .iter()
        .enumerate()
        .find(|(_, ty)| format!("deepseek-{}", ty).to_lowercase() == target)
    {
        let input = max_input_tokens.get(idx).copied();
        let output = max_output_tokens.get(idx).copied();
        return Some(make_model(&format!("deepseek-{}", ty), input, output));
    }

    // Sau đó tìm trong aliases (khớp index với model_types, hỗ trợ nhiều alias ngăn bằng dấu phẩy)
    for (i, alias) in aliases.iter().enumerate() {
        if let Some(_ty) = model_types.get(i) {
            for alias in split_model_aliases(alias) {
                if alias.to_lowercase() == target {
                    let input = max_input_tokens.get(i).copied();
                    let output = max_output_tokens.get(i).copied();
                    return Some(make_model(alias, input, output));
                }
            }
        }
    }

    None
}

fn split_model_aliases(alias: &str) -> impl Iterator<Item = &str> {
    alias.split(',').map(str::trim).filter(|a| !a.is_empty())
}

fn make_model(id: &str, input: Option<u32>, output: Option<u32>) -> OpenAIModel {
    OpenAIModel {
        id: id.to_string(),
        object: "model",
        created: MODEL_CREATED,
        owned_by: MODEL_OWNED_BY,
        max_input_tokens: input,
        max_output_tokens: output,
        context_length: input,
        context_window: input,
        max_context_length: input,
        max_tokens: output,
        max_completion_tokens: output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_splits_comma_aliases() {
        let models = list(
            &["default".to_string()],
            &[1024],
            &[2048],
            &["deepseek-v4-flash, deepseek-v4-flash-nothinking".to_string()],
        );
        let ids = models.data.into_iter().map(|m| m.id).collect::<Vec<_>>();

        assert!(
            ids.contains(&"deepseek-default".to_string()),
            "base model missing"
        );
        assert!(
            ids.contains(&"deepseek-v4-flash".to_string()),
            "first alias missing"
        );
        assert!(
            ids.contains(&"deepseek-v4-flash-nothinking".to_string()),
            "second alias missing"
        );
    }

    #[test]
    fn get_finds_split_alias() {
        let model = get(
            &["default".to_string()],
            &[1024],
            &[2048],
            &["deepseek-v4-flash, deepseek-v4-flash-nothinking".to_string()],
            "deepseek-v4-flash-nothinking",
        )
        .unwrap();

        assert_eq!(model.id, "deepseek-v4-flash-nothinking");
    }
}
