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

    // Thêm model alias (khớp index với model_types)
    for (i, alias) in aliases.iter().enumerate() {
        if let Some(_ty) = model_types.get(i) {
            let input = max_input_tokens.get(i).copied();
            let output = max_output_tokens.get(i).copied();
            data.push(make_model(alias, input, output));
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

    // Sau đó tìm trong aliases (khớp index với model_types)
    for (i, alias) in aliases.iter().enumerate() {
        if alias.to_lowercase() == target
            && let Some(_ty) = model_types.get(i)
        {
            let input = max_input_tokens.get(i).copied();
            let output = max_output_tokens.get(i).copied();
            return Some(make_model(&target, input, output));
        }
    }

    None
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
