//! Parse công cụ - kiểm tra tools/tool_choice và tạo text inject vào prompt
//!
//! Vì ds_core không hỗ trợ function calling gốc, module này hạ cấp định nghĩa công cụ
//! thành mô tả ngôn ngữ tự nhiên rồi nối vào prompt để hướng dẫn output của model.

use crate::openai_adapter::response::{TOOL_CALL_END, TOOL_CALL_START};
use crate::openai_adapter::types::{
    AllowedTools, AllowedToolsChoice, ChatCompletionsRequest, CustomTool, CustomToolFormat,
    FunctionDefinition, Tool, ToolChoice,
};

/// Ngữ cảnh công cụ sau khi trích xuất
pub(crate) struct ToolContext {
    /// Mẫu định dạng + quy tắc + ví dụ (nằm trước định nghĩa công cụ)
    pub format_block: Option<String>,
    /// Text định nghĩa công cụ sau khi format
    pub defs_text: Option<String>,
    /// Chỉ dẫn hành vi thêm theo tool_choice / parallel_tool_calls
    pub instruction_text: Option<String>,
}

fn has_tools(req: &ChatCompletionsRequest) -> bool {
    req.tools.as_ref().map(|t| !t.is_empty()).unwrap_or(false)
}

/// Trích xuất và kiểm tra thông tin công cụ từ request
///
/// Khi tool_choice là none, trả về ToolContext rỗng và không tạo text inject.
pub(crate) fn extract(req: &ChatCompletionsRequest) -> Result<ToolContext, String> {
    let default_choice = if has_tools(req) {
        ToolChoice::Mode("auto".to_string())
    } else {
        ToolChoice::Mode("none".to_string())
    };
    let tool_choice = req.tool_choice.as_ref().unwrap_or(&default_choice);

    validate_tool_choice(tool_choice, req.tools.as_deref())?;

    if matches!(tool_choice, ToolChoice::Mode(m) if m == "none") {
        return Ok(ToolContext {
            format_block: None,
            defs_text: None,
            instruction_text: None,
        });
    }

    let mut instruction_lines = Vec::new();

    match tool_choice {
        ToolChoice::Mode(mode) => {
            if mode == "required" {
                instruction_lines
                    .push("**Lưu ý: bạn phải gọi một hoặc nhiều công cụ.**".to_string());
            }
        }
        ToolChoice::AllowedTools(AllowedToolsChoice { allowed_tools, .. }) => {
            build_allowed_tools_instruction(allowed_tools, &mut instruction_lines);
        }
        ToolChoice::Named(named) => {
            instruction_lines.push(format!(
                "**Lưu ý: bạn phải gọi công cụ '{}'.**",
                named.function.name
            ));
        }
        ToolChoice::Custom(custom) => {
            instruction_lines.push(format!(
                "**Lưu ý: bạn phải gọi công cụ tùy chỉnh '{}'.**",
                custom.custom.name
            ));
        }
    }

    if req.parallel_tool_calls == Some(false) {
        instruction_lines.push("**Lưu ý: mỗi lần chỉ được gọi một công cụ.**".to_string());
    }

    let format_block = has_tools(req).then(|| build_tool_instruction_block(req));

    let defs_text = if has_tools(req) {
        let mut lines = vec!["Bạn có thể dùng các công cụ sau:".to_string()];
        for (i, tool) in req.tools.as_ref().unwrap().iter().enumerate() {
            lines.push(format_tool(tool, i)?);
        }
        Some(lines.join("\n"))
    } else {
        None
    };

    let instruction_text = if instruction_lines.is_empty() {
        None
    } else {
        Some(instruction_lines.join("\n"))
    };

    Ok(ToolContext {
        format_block,
        defs_text,
        instruction_text,
    })
}

fn validate_tool_choice(tc: &ToolChoice, tools: Option<&[Tool]>) -> Result<(), String> {
    match tc {
        ToolChoice::Mode(mode) => {
            if !matches!(mode.as_str(), "none" | "auto" | "required") {
                return Err(format!("Chế độ tool_choice không hợp lệ: {}", mode));
            }
            if matches!(mode.as_str(), "auto" | "required")
                && tools.map(|t| t.is_empty()).unwrap_or(true)
            {
                return Err(
                    "Khi tool_choice là 'auto' hoặc 'required' thì phải cung cấp tools".into(),
                );
            }
            Ok(())
        }
        ToolChoice::Named(_) | ToolChoice::Custom(_) => {
            if tools.is_none() {
                return Err(
                    "Khi tool_choice chỉ định công cụ cụ thể thì phải cung cấp tools".into(),
                );
            }
            Ok(())
        }
        ToolChoice::AllowedTools(AllowedToolsChoice { allowed_tools, .. }) => {
            if tools.is_none() {
                return Err(
                    "Khi tool_choice chỉ định allowed_tools thì phải cung cấp tools".into(),
                );
            }
            if !matches!(allowed_tools.mode.as_str(), "auto" | "required") {
                return Err(format!(
                    "allowed_tools.mode phải là 'auto' hoặc 'required', nhận được: {}",
                    allowed_tools.mode
                ));
            }
            Ok(())
        }
    }
}

fn build_allowed_tools_instruction(allowed_tools: &AllowedTools, lines: &mut Vec<String>) {
    if let Some(tool_list) = &allowed_tools.tools {
        let names: Vec<String> = tool_list
            .iter()
            .filter_map(|v| v.get("function").and_then(|f| f.get("name")))
            .filter_map(|n| n.as_str().map(|s| s.to_string()))
            .collect();
        if !names.is_empty() {
            lines.push(format!(
                "**Lưu ý:** bạn chỉ được chọn trong các công cụ được phép sau: {}.",
                names.join(", ")
            ));
        }
    }

    if allowed_tools.mode == "required" {
        lines.push("**Lưu ý: bạn phải gọi một hoặc nhiều công cụ.**".to_string());
    }
}

fn format_tool(tool: &Tool, idx: usize) -> Result<String, String> {
    match tool.ty.as_str() {
        "function" => {
            let func = tool.function.as_ref().ok_or_else(|| {
                format!(
                    "tools[{}] có type 'function' thì phải cung cấp định nghĩa function",
                    idx
                )
            })?;
            format_function(func)
        }
        "custom" => {
            let custom = tool.custom.as_ref().ok_or_else(|| {
                format!(
                    "tools[{}] có type 'custom' thì phải cung cấp định nghĩa custom",
                    idx
                )
            })?;
            Ok(format_custom(custom))
        }
        _ => Err(format!(
            "tools[{}] có type không được hỗ trợ: {}",
            idx, tool.ty
        )),
    }
}

fn format_function(func: &FunctionDefinition) -> Result<String, String> {
    if func.name.trim().is_empty() {
        return Err("function trong tools thiếu trường bắt buộc 'name'".into());
    }
    let params = serde_json::to_string(&func.parameters).unwrap_or_else(|_| "{}".into());
    let call_example = format!(
        "{TOOL_CALL_START}[{{\"name\": \"{}\", \"arguments\": {}}}]{TOOL_CALL_END}",
        func.name, params
    );
    let desc = func.description.as_deref().unwrap_or("").trim();
    let desc_block = if desc.is_empty() {
        "  Không có mô tả".to_string()
    } else {
        format!("~~~markdown\n  {}\n~~~\n", desc)
    };
    Ok(format!(
        "- **{}** (function):\n  - Cách gọi: `{}`\n  - Mô tả ngắn:\n{}",
        func.name, call_example, desc_block,
    ))
}

/// Tạo block chỉ dẫn gọi công cụ: mẫu -> quy tắc -> ví dụ đúng động
fn build_tool_instruction_block(req: &ChatCompletionsRequest) -> String {
    let mut lines: Vec<String> = Vec::new();

    // Mẫu
    lines.push("**Định dạng gọi công cụ - hãy tuân thủ nghiêm ngặt:**".into());
    lines.push(String::new());
    lines.push("Bọc mảng JSON bằng thẻ gọi công cụ:".into());
    lines.push(String::new());
    lines.push(format!(
        "{TOOL_CALL_START}[{{\"name\": \"tên_công_cụ\", \"arguments\": {{JSON_tham_số}}}}]{TOOL_CALL_END}"
    ));
    lines.push(String::new());

    // Quy tắc
    lines.push("**Quy tắc:**".into());
    lines.push(String::new());
    lines.push(
        "**Cốt lõi: khi quyết định gọi công cụ, phản hồi chỉ được chứa chính văn bản gọi công cụ; cấm mọi giải thích, tiền tố, tóm tắt, lời chào hoặc nội dung thừa.**".into(),
    );
    lines.push(String::new());
    lines.push(format!("1. Mảng JSON phải bắt đầu bằng `{TOOL_CALL_START}` và kết thúc bằng `{TOOL_CALL_END}`; mảng phải được **bọc trọn vẹn** trong thẻ."));
    lines.push("2. Mọi lệnh gọi công cụ phải nằm trong **một** mảng JSON; nhiều lệnh gọi phân tách bằng dấu phẩy.".into());
    lines.push(format!(
        "3. Sau khi xuất `{TOOL_CALL_END}`, **dừng ngay**; không thêm văn bản, thẻ XML hoặc chú thích phía sau."
    ));
    lines.push("4. Không bọc lệnh gọi công cụ trong code block markdown.".into());
    lines.push(
        "5. Giá trị tham số kiểu chuỗi phải được bọc bằng **dấu nháy kép** (chuẩn JSON).".into(),
    );
    lines.push(format!(
        "6. Khi quyết định gọi công cụ, **ký tự không trắng đầu tiên** trong output phải là `{TOOL_CALL_START}`."
    ));
    lines.push(format!(
        "7. Toàn bộ phản hồi **chỉ được có một khối `{TOOL_CALL_START}`**; không xuất lặp nhiều khối `{TOOL_CALL_START}`."
    ));
    lines.push(format!(
        "8. **Nhắc lại:** toàn bộ phản hồi chỉ được có một khối `{TOOL_CALL_START}`. Nếu đã xuất một khối `{TOOL_CALL_START}`, tuyệt đối không xuất khối thứ hai."
    ));
    lines.push(format!(
        "9. **Nhắc lại:** cấm xuất bất kỳ chữ nào trước `{TOOL_CALL_START}`, gồm giải thích, xác nhận, tóm tắt, lời chào."
    ));
    lines.push(
        "10. Không đặt phản hồi cuối hoặc lệnh gọi công cụ trong nội dung suy nghĩ.".to_string(),
    );
    lines.push(
        "11. **Nhắc lại:** nội dung suy nghĩ (trong thẻ <think>) chỉ dùng cho suy luận nội bộ; không đặt phản hồi cuối hoặc lệnh gọi công cụ trong thẻ <think>.".to_string(),
    );
    lines.push(String::new());

    let tool_names: Vec<String> = req
        .tools
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter_map(|t| t.function.as_ref().map(|f| f.name.clone()))
        .collect();
    let a = tool_names.first().map(|s| s.as_str()).unwrap_or("tool_a");

    // Ví dụ đúng (dùng tên công cụ thật, có tham số thật)
    lines.push("**Ví dụ đúng:**".into());
    lines.push(String::new());

    // Ví dụ A: một công cụ
    lines.push("**Ví dụ A** - gọi một công cụ:".into());
    lines.push(format!(
        "{TOOL_CALL_START}[{{\"name\": \"{a}\", \"arguments\": {}}}]{TOOL_CALL_END}",
        example_args(a)
    ));
    lines.push(String::new());

    // Ví dụ B: hai công cụ song song
    if tool_names.len() >= 2 {
        let items: Vec<String> = tool_names[..2]
            .iter()
            .map(|n| format!("{{\"name\": \"{n}\", \"arguments\": {}}}", example_args(n)))
            .collect();
        lines.push("**Ví dụ B** - gọi nhiều công cụ cùng lúc (một mảng chứa mọi lệnh gọi):".into());
        lines.push(String::new());
        lines.push(format!(
            "{TOOL_CALL_START}[{}]{TOOL_CALL_END}",
            items.join(", ")
        ));
        lines.push(String::new());
    }

    // Ví dụ C: ba công cụ song song
    if tool_names.len() >= 3 {
        let items: Vec<String> = tool_names[..3]
            .iter()
            .map(|n| format!("{{\"name\": \"{n}\", \"arguments\": {}}}", example_args(n)))
            .collect();
        lines.push(
            "**Ví dụ C** - gọi ba công cụ cùng lúc (mọi lệnh gọi nằm trong một mảng):".into(),
        );
        lines.push(String::new());
        lines.push(format!(
            "{TOOL_CALL_START}[{}]{TOOL_CALL_END}",
            items.join(", ")
        ));
        lines.push(String::new());
    }

    // Ví dụ D: tham số lồng nhau (object/array vẫn là JSON chuẩn)
    if !tool_names.is_empty() {
        let d_name = tool_names.first().map(|s| s.as_str()).unwrap_or("tool_a");
        lines.push("**Ví dụ D** - tham số là object/array lồng nhau (vẫn là JSON chuẩn):".into());
        lines.push(String::new());
        lines.push(format!(
            "{TOOL_CALL_START}[{{\"name\": \"{d_name}\", \"arguments\": {}}}]{TOOL_CALL_END}",
            example_nested_args(d_name)
        ));
        lines.push(String::new());
    }

    lines.join("\n")
}

/// Trả về chuỗi tham số ví dụ theo tên công cụ.
fn example_args(name: &str) -> String {
    let args: &str = match name {
        "Read" | "read_file" => r#""file_path": "/path/to/file""#,
        "Bash" | "execute_command" | "exec_command" => r#""command": "ls -la""#,
        "Write" | "write_to_file" => r#""file_path": "/path/to/file", "content": "hello""#,
        "Edit" => r#""file_path": "/path/to/file", "old_string": "foo", "new_string": "bar""#,
        "Glob" => r#""pattern": "**/*.rs", "path": "."#,
        "search_files" => r#""query": "TODO", "path": "."#,
        "get_weather" => r#""city": "Beijing""#,
        "get_time" => r#""timezone": "Asia/Shanghai""#,
        "list_files" => r#""path": "."#,
        _ => r#""key": "value""#,
    };
    format!("{{{args}}}")
}

/// Trả về ví dụ tham số lồng nhau (giá trị tham số là array hoặc object).
fn example_nested_args(name: &str) -> String {
    match name {
        "Edit" => r#"{"file_path": "/path/to/file", "edits": [{"old_string": "foo", "new_string": "bar"}, {"old_string": "x", "new_string": "y"}]}"#.into(),
        _ => r#"{"config": {"enabled": true, "items": ["a", "b"]}}"#.into(),
    }
}

fn format_custom(custom: &CustomTool) -> String {
    let desc = custom.description.as_deref().unwrap_or("").trim();
    let method = match &custom.format {
        Some(CustomToolFormat::Text) => "text".into(),
        Some(CustomToolFormat::Grammar { grammar }) => {
            format!("grammar(syntax: {})", grammar.syntax)
        }
        None => "Không ràng buộc".into(),
    };
    format!(
        "- **{}** (custom):\n  - Cách gọi: `{}`\n  - Mô tả ngắn: {}",
        custom.name,
        method,
        if desc.is_empty() {
            "Không có mô tả"
        } else {
            desc
        },
    )
}
