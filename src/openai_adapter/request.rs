//! Parse request OpenAI - hạ cấp OpenAI ChatCompletion request thành ds_core::ChatRequest
//!
//! Giới hạn hiện tại:
//! - Hội thoại nhiều lượt được nén thành một prompt bằng tag gốc DeepSeek
//! - Định nghĩa tool được nhúng vào block `<think>` không đóng sau `<｜Assistant｜>` cuối cùng

pub(crate) mod files;
pub(crate) mod normalize;
pub(crate) mod prompt;
pub(crate) mod resolver;
pub(crate) mod tools;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai_adapter::OpenAIAdapterError;
    use crate::openai_adapter::types::{
        ChatCompletionsRequest, FunctionCallOption, NamedFunction, NamedToolChoice, Tool,
        ToolChoice,
    };

    fn default_registry() -> std::collections::HashMap<String, String> {
        crate::config::DeepSeekConfig::default().model_registry()
    }

    /// prepare dùng cho test, mô phỏng logic parse nội bộ của adapter
    #[derive(Debug)]
    struct TestRequest {
        prompt: String,
        thinking_enabled: bool,
        search_enabled: bool,
        stream: bool,
        include_usage: bool,
        include_obfuscation: bool,
        stop: Vec<String>,
    }

    fn parse_json(val: serde_json::Value) -> Result<TestRequest, OpenAIAdapterError> {
        let mut req: ChatCompletionsRequest = serde_json::from_value(val)
            .map_err(|e| OpenAIAdapterError::BadRequest(format!("bad request: {}", e)))?;
        let registry = default_registry();

        if req.tools.as_ref().map(|t| t.is_empty()).unwrap_or(true)
            && let Some(functions) = req.functions.clone()
            && !functions.is_empty()
        {
            req.tools = Some(
                functions
                    .into_iter()
                    .map(|f| Tool {
                        ty: "function".to_string(),
                        function: Some(f),
                        custom: None,
                    })
                    .collect(),
            );
        }
        if req.tool_choice.is_none()
            && let Some(fc) = req.function_call.clone()
        {
            req.tool_choice = Some(match fc {
                FunctionCallOption::Mode(mode) => ToolChoice::Mode(mode),
                FunctionCallOption::Named(named) => ToolChoice::Named(NamedToolChoice {
                    ty: "function".to_string(),
                    function: NamedFunction { name: named.name },
                }),
            });
        }

        let norm = normalize::apply(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let tool_ctx = tools::extract(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let prompt = prompt::build(&req, &tool_ctx);
        let model_res = resolver::resolve(
            &registry,
            &req.model,
            req.reasoning_effort.as_deref(),
            req.web_search_options.as_ref(),
        )
        .map_err(OpenAIAdapterError::BadRequest)?;

        println!("\n=== PARSED REQUEST ===");
        println!("prompt:\n{}", prompt);
        println!(
            "thinking={} search={}",
            model_res.thinking_enabled, model_res.search_enabled
        );
        println!("======================\n");

        Ok(TestRequest {
            prompt,
            thinking_enabled: model_res.thinking_enabled,
            search_enabled: model_res.search_enabled,
            stream: req.stream,
            include_usage: norm.include_usage,
            include_obfuscation: norm.include_obfuscation,
            stop: norm.stop,
        })
    }

    #[test]
    fn basic_chat() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "system", "content": "Ban la mot tro ly huu ich." },
                { "role": "user", "content": "Xin chao" }
            ]
        });
        let req = parse_json(body).unwrap();
        assert!(!req.prompt.is_empty());
    }

    #[test]
    fn tool_conversation() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "Thoi tiet Ha Noi the nao?" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_abc123",
                            "type": "function",
                            "function": { "name": "get_weather", "arguments": "{\"city\":\"Hanoi\"}" }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_abc123",
                    "content": "Ha Noi hom nay nang, 25°C."
                },
                { "role": "user", "content": "Cam on" }
            ]
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("get_weather"));
    }

    #[test]
    fn reasoning_and_search_flags() {
        let body = serde_json::json!({
            "model": "deepseek-expert",
            "messages": [
                { "role": "user", "content": "Phan tich dien toan luong tu" }
            ],
            "reasoning_effort": "high",
            "web_search_options": { "search_context_size": "high" }
        });
        let req = parse_json(body).unwrap();
        assert!(req.thinking_enabled);
        assert!(req.search_enabled);
    }

    // Trường hợp lỗi normalize
    #[test]
    fn missing_model() {
        let body = serde_json::json!({
            "messages": [{ "role": "user", "content": "Xin chao" }]
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("model"));
    }

    #[test]
    fn missing_messages() {
        let body = serde_json::json!({
            "model": "deepseek-default"
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("messages"));
    }

    #[test]
    fn tool_missing_tool_call_id() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "tool", "content": "result" }
            ]
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("tool_call_id"));
    }

    #[test]
    fn function_missing_name() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "function", "content": "result" }
            ]
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("name"));
    }

    // Lỗi parse model và cờ năng lực
    #[test]
    fn unsupported_model() {
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [{ "role": "user", "content": "hello" }]
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("không được hỗ trợ"));
    }

    #[test]
    fn reasoning_effort_variants() {
        for (effort, expected) in [
            ("minimal", true),
            ("low", true),
            ("medium", true),
            ("high", true),
            ("xhigh", true),
            ("unknown", true),
            ("", true),
        ] {
            let body = serde_json::json!({
                "model": "deepseek-default",
                "messages": [{ "role": "user", "content": "hi" }],
                "reasoning_effort": effort
            });
            let req = parse_json(body).unwrap();
            assert_eq!(
                req.thinking_enabled, expected,
                "reasoning_effort={}",
                effort
            );
        }

        // Mặc định bật reasoning khi chưa truyền reasoning_effort
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let req = parse_json(body).unwrap();
        assert!(
            req.thinking_enabled,
            "reasoning_effort absent should default to high"
        );
    }

    #[test]
    fn search_enabled_by_default() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let req = parse_json(body).unwrap();
        assert!(req.search_enabled);
    }

    // Giá trị mặc định của stop sequence và stream_options

    #[test]
    fn stop_single() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "stop": "EOF"
        });
        let req = parse_json(body).unwrap();
        assert_eq!(req.stop, vec!["EOF"]);
    }

    #[test]
    fn stop_multiple() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "stop": ["STOP", "HALT"]
        });
        let req = parse_json(body).unwrap();
        assert_eq!(req.stop, vec!["STOP", "HALT"]);
    }

    #[test]
    fn stream_options() {
        // Giá trị mặc định
        let req = parse_json(serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .unwrap();
        assert_eq!(req.stream, false);
        assert_eq!(req.include_usage, false);
        assert_eq!(req.include_obfuscation, true);

        // Override rõ ràng
        let req2 = parse_json(serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream_options": { "include_usage": true, "include_obfuscation": false }
        }))
        .unwrap();
        assert_eq!(req2.include_usage, true);
        assert_eq!(req2.include_obfuscation, false);
    }

    // Kiểm tra và inject tools

    #[test]
    fn tool_choice_none_ignores_tools() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "function",
                    "function": { "name": "f", "parameters": {} }
                }
            ],
            "tool_choice": "none"
        });
        let req = parse_json(body).unwrap();
        assert!(!req.prompt.contains("Bạn có thể dùng các công cụ sau"));
    }

    #[test]
    fn tool_choice_required_instruction() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "function",
                    "function": { "name": "f" }
                }
            ],
            "tool_choice": "required"
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("bạn phải gọi một hoặc nhiều công cụ"));
    }

    #[test]
    fn parallel_tool_calls_false_instruction() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "f" } }
            ],
            "parallel_tool_calls": false
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("mỗi lần chỉ được gọi một công cụ"));
    }

    #[test]
    fn tool_choice_named_function() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "get_weather" } }
            ],
            "tool_choice": { "type": "function", "function": { "name": "get_weather" } }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("bạn phải gọi công cụ 'get_weather'"));
    }

    #[test]
    fn tool_choice_allowed_tools() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "get_weather" } },
                { "type": "function", "function": { "name": "get_time" } }
            ],
            "tool_choice": {
                "type": "allowed_tools",
                "allowed_tools": {
                    "mode": "required",
                    "tools": [
                        { "type": "function", "function": { "name": "get_weather" } }
                    ]
                }
            }
        });
        let req = parse_json(body).unwrap();
        assert!(
            req.prompt
                .contains("bạn chỉ được chọn trong các công cụ được phép sau: get_weather")
        );
        assert!(req.prompt.contains("bạn phải gọi một hoặc nhiều công cụ"));
    }

    #[test]
    fn tool_choice_custom() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "custom",
                    "custom": { "name": "my_custom", "format": { "type": "text" } }
                }
            ],
            "tool_choice": { "type": "custom", "custom": { "name": "my_custom" } }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("**my_custom** (custom):"));
        assert!(
            req.prompt
                .contains("bạn phải gọi công cụ tùy chỉnh 'my_custom'")
        );
    }

    #[test]
    fn custom_tool_grammar_format() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "custom",
                    "custom": {
                        "name": "grammar_tool",
                        "description": " grammar based tool",
                        "format": {
                            "type": "grammar",
                            "grammar": {
                                "definition": "start: word+",
                                "syntax": "lark"
                            }
                        }
                    }
                }
            ]
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("grammar(syntax: lark)"));
    }

    #[test]
    fn custom_tool_missing_format() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "custom",
                    "custom": { "name": "no_format" }
                }
            ]
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("Cách gọi:"));
        assert!(req.prompt.contains("Không ràng buộc"));
    }

    #[test]
    fn tool_empty_name() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "" } }
            ]
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn tool_choice_required_without_tools() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tool_choice": "required"
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
    }

    #[test]
    fn allowed_tools_bad_mode() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "f" } }
            ],
            "tool_choice": {
                "type": "allowed_tools",
                "allowed_tools": { "mode": "invalid", "tools": [] }
            }
        });
        let err = parse_json(body).unwrap_err();
        assert!(matches!(err, OpenAIAdapterError::BadRequest(_)));
    }

    // Vị trí inject tools: nhúng vào block <think> sau <｜Assistant｜> cuối cùng

    #[test]
    fn tools_injected_into_think_block() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [
                { "role": "user", "content": "Cau hoi thu nhat" },
                { "role": "assistant", "content": "Tra loi" },
                { "role": "user", "content": "Cau hoi thu hai" }
            ],
            "tools": [
                { "type": "function", "function": { "name": "calc" } }
            ]
        });
        let req = parse_json(body).unwrap();
        let prompt = &req.prompt;
        // Định nghĩa công cụ phải được inject vào block <｜Assistant｜><think> cuối cùng
        assert!(
            prompt.contains(
                "<｜Assistant｜><think>Tôi vừa được hệ thống nhắc cần tuân thủ nội dung sau:"
            ),
            "Dinh nghia cong cu phai duoc inject vao block <think>"
        );
        assert!(prompt.contains("## Gọi công cụ"));
        assert!(prompt.contains("calc"));
        // Block <think> phải nằm cuối, sau user message cuối cùng
        let think_pos = prompt.find("<｜Assistant｜><think>").unwrap();
        let last_user_pos = prompt.rfind("Cau hoi thu hai").unwrap();
        assert!(
            think_pos > last_user_pos,
            "Block <think> phai nam sau user message cuoi"
        );
    }

    // Hạ cấp tương thích functions / function_call

    #[test]
    fn functions_legacy_to_tools() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "Thoi tiet Ha Noi?" }],
            "functions": [
                {
                    "name": "get_weather",
                    "description": "Lay thoi tiet",
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
                }
            ],
            "function_call": "auto"
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("get_weather"));
        assert!(req.prompt.contains("Bạn có thể dùng các công cụ sau"));
    }

    #[test]
    fn function_call_named_legacy() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "Tra thoi tiet" }],
            "functions": [
                { "name": "get_weather", "parameters": {} }
            ],
            "function_call": { "name": "get_weather" }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("bạn phải gọi công cụ 'get_weather'"));
    }

    #[test]
    fn tools_priority_over_functions() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "tool_a", "parameters": {} } }
            ],
            "functions": [
                { "name": "func_b", "parameters": {} }
            ],
            "tool_choice": "auto",
            "function_call": { "name": "func_b" }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("tool_a"));
        assert!(!req.prompt.contains("func_b"));
    }

    // Hạ cấp tương thích response_format

    #[test]
    fn response_format_json_object() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "Xuat JSON" }],
            "response_format": { "type": "json_object" }
        });
        let req = parse_json(body).unwrap();
        assert!(
            req.prompt
                .contains("xuất trực tiếp một đối tượng JSON hợp lệ")
        );
    }

    #[test]
    fn response_format_json_schema() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "Xuat co cau truc" }],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "person",
                    "schema": { "type": "object", "properties": { "name": { "type": "string" } } }
                }
            }
        });
        let req = parse_json(body).unwrap();
        assert!(req.prompt.contains("tuân theo định dạng sau"));
        assert!(req.prompt.contains("person"));
    }

    #[test]
    fn response_format_text_no_injection() {
        let body = serde_json::json!({
            "model": "deepseek-default",
            "messages": [{ "role": "user", "content": "hi" }],
            "response_format": { "type": "text" }
        });
        let req = parse_json(body).unwrap();
        assert!(!req.prompt.contains("Hãy xuất theo"));
        assert!(!req.prompt.contains("JSON"));
    }
}
