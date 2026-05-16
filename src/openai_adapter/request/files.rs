//! Trích xuất file - lấy dữ liệu file inline từ ChatCompletionsRequest
//!
//! Hỗ trợ trích xuất file từ các content part sau:
//! - `file`: `file_data` ở format data URL (`data:{mime};base64,{data}`)
//! - `image_url`: `url` ở format data URL (`data:image/*;base64,{data}`)
//!
//! File trích xuất sẽ được upload vào hội thoại DeepSeek qua `FilePayload` của ds_core.
//! Với HTTP URL trong `image_url`, đánh dấu cần bật search mode;
//! content part tương ứng trong prompt hiển thị là `[Hãy truy cập liên kết này: {url}]`.

use crate::ds_core::FilePayload;
use crate::openai_adapter::types::{ChatCompletionsRequest, ContentPart, MessageContent};
use base64::Engine;

/// Kết quả trích xuất file
pub(crate) struct ExtractResult {
    /// File inline cần upload vào hội thoại DeepSeek
    pub files: Vec<FilePayload>,
    /// Có chứa HTTP URL cần model truy cập qua search hay không
    pub has_http_urls: bool,
}

/// Trích xuất thông tin file và cờ HTTP URL từ ChatCompletionsRequest
pub(crate) fn extract(req: &ChatCompletionsRequest) -> ExtractResult {
    let mut files = Vec::new();
    let mut has_http_urls = false;

    for msg in &req.messages {
        let Some(MessageContent::Parts(parts)) = &msg.content else {
            continue;
        };
        for part in parts {
            match part.ty.as_str() {
                "file" => {
                    if let Some(file) = extract_file(part) {
                        files.push(file);
                    }
                }
                "image_url" => {
                    if let Some(file) = extract_image(part) {
                        files.push(file);
                    } else if is_http_url(part) {
                        has_http_urls = true;
                    }
                }
                _ => {}
            }
        }
    }

    ExtractResult {
        files,
        has_http_urls,
    }
}

fn is_http_url(part: &ContentPart) -> bool {
    part.image_url
        .as_ref()
        .is_some_and(|img| img.url.starts_with("http://") || img.url.starts_with("https://"))
}

/// Trích xuất file từ content part `file`
///
/// Format `file_data`: `data:{mime};base64,{data}`
fn extract_file(part: &ContentPart) -> Option<FilePayload> {
    let file = part.file.as_ref()?;
    let data_url = file.file_data.as_ref()?;

    let (mime, b64_data) = parse_data_url(data_url)?;
    let content = base64_decode(b64_data)?;
    let filename = file
        .filename
        .clone()
        .unwrap_or_else(|| infer_filename_from_mime(&mime));

    Some(FilePayload {
        filename,
        content,
        content_type: mime,
    })
}

/// Trích xuất ảnh từ content part `image_url`
///
/// Format `url`: `data:image/{format};base64,{data}`
fn extract_image(part: &ContentPart) -> Option<FilePayload> {
    let url = part.image_url.as_ref()?.url.clone();

    let (mime, b64_data) = parse_data_url(&url)?;
    let content = base64_decode(b64_data)?;
    let filename = format!("image.{}", mime_extension(&mime));

    Some(FilePayload {
        filename,
        content,
        content_type: mime,
    })
}

/// Parse data URL và trả về (mime_type, base64_data)
///
/// Format: `data:[<mediatype>][;base64],<data>`
fn parse_data_url(data_url: &str) -> Option<(String, &str)> {
    let remaining = data_url.strip_prefix("data:")?;
    let (header, data) = remaining.split_once(',')?;
    if !header.ends_with(";base64") {
        return None;
    }
    let mime = header
        .strip_suffix(";base64")
        .unwrap_or("application/octet-stream");
    let mime = if mime.is_empty() {
        "application/octet-stream"
    } else {
        mime
    };
    Some((mime.to_string(), data))
}

fn base64_decode(data: &str) -> Option<Vec<u8>> {
    // percent-decoding không bắt buộc, nhưng một số ký tự trong data URL có thể đã được encode
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

fn mime_extension(mime: &str) -> &str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        _ => "bin",
    }
}

fn infer_filename_from_mime(mime: &str) -> String {
    let ext = match mime {
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/html" => "html",
        "application/json" => "json",
        "application/zip" => "zip",
        "application/xml" => "xml",
        "text/csv" => "csv",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        _ => "file",
    };
    format!("file.{}", ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_req(messages: Vec<serde_json::Value>) -> ChatCompletionsRequest {
        serde_json::from_value(serde_json::json!({
            "model": "deepseek-default",
            "messages": messages,
        }))
        .unwrap()
    }

    fn text_msg(content: &str) -> serde_json::Value {
        serde_json::json!({
            "role": "user",
            "content": content,
        })
    }

    fn file_part(data_url: &str, filename: Option<&str>) -> serde_json::Value {
        let mut part = serde_json::json!({
            "type": "file",
            "file": {
                "file_data": data_url,
            },
        });
        if let Some(name) = filename {
            part["file"]["filename"] = serde_json::json!(name);
        }
        part
    }

    fn image_part(url: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "image_url",
            "image_url": { "url": url },
        })
    }

    fn text_part(content: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "text",
            "text": content,
        })
    }

    fn file_ref_part(file_id: &str, filename: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "file",
            "file": {
                "file_id": file_id,
                "filename": filename,
            },
        })
    }

    fn parts_msg(parts: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "role": "user",
            "content": parts,
        })
    }

    #[test]
    fn no_parts_returns_empty() {
        let result = extract(&make_req(vec![text_msg("hello")]));
        assert!(result.files.is_empty());
        assert!(!result.has_http_urls);
    }

    #[test]
    fn skip_text_part() {
        let result = extract(&make_req(vec![parts_msg(vec![text_part("hello")])]));
        assert!(result.files.is_empty());
        assert!(!result.has_http_urls);
    }

    #[test]
    fn extract_file_with_data_url() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"hello world");
        let data_url = format!("data:text/plain;base64,{}", b64);
        let result = extract(&make_req(vec![parts_msg(vec![file_part(
            &data_url,
            Some("hello.txt"),
        )])]));
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].filename, "hello.txt");
        assert_eq!(result.files[0].content_type, "text/plain");
        assert_eq!(result.files[0].content, b"hello world");
        assert!(!result.has_http_urls);
    }

    #[test]
    fn extract_file_without_filename() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"pdf content");
        let data_url = format!("data:application/pdf;base64,{}", b64);
        let result = extract(&make_req(vec![parts_msg(vec![file_part(&data_url, None)])]));
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].filename, "file.pdf");
        assert_eq!(result.files[0].content_type, "application/pdf");
    }

    #[test]
    fn extract_image_with_data_url() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"image data");
        let data_url = format!("data:image/png;base64,{}", b64);
        let result = extract(&make_req(vec![parts_msg(vec![image_part(&data_url)])]));
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].filename, "image.png");
        assert_eq!(result.files[0].content_type, "image/png");
        assert_eq!(result.files[0].content, b"image data");
        assert!(!result.has_http_urls);
    }

    #[test]
    fn http_image_triggers_search() {
        let result = extract(&make_req(vec![parts_msg(vec![image_part(
            "https://example.com/img.jpg",
        )])]));
        assert!(result.files.is_empty());
        assert!(result.has_http_urls);
    }

    #[test]
    fn skip_file_without_data_url() {
        let result = extract(&make_req(vec![parts_msg(vec![file_ref_part(
            "file-abc", "ref.pdf",
        )])]));
        assert!(result.files.is_empty());
        assert!(!result.has_http_urls);
    }

    #[test]
    fn extract_multiple_files_from_single_message() {
        let b64_1 = base64::engine::general_purpose::STANDARD.encode(b"file1");
        let b64_2 = base64::engine::general_purpose::STANDARD.encode(b"file2");
        let result = extract(&make_req(vec![parts_msg(vec![
            file_part(&format!("data:text/plain;base64,{}", b64_1), Some("a.txt")),
            file_part(
                &format!("data:application/pdf;base64,{}", b64_2),
                Some("b.pdf"),
            ),
        ])]));
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].filename, "a.txt");
        assert_eq!(result.files[1].filename, "b.pdf");
    }

    #[test]
    fn extract_files_from_multiple_messages() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"data");
        let result = extract(&make_req(vec![
            parts_msg(vec![image_part(&format!("data:image/webp;base64,{}", b64))]),
            text_msg("response"),
            parts_msg(vec![file_part(
                &format!("data:application/json;base64,{}", b64),
                Some("data.json"),
            )]),
        ]));
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].content_type, "image/webp");
        assert_eq!(result.files[1].filename, "data.json");
    }

    #[test]
    fn http_url_and_data_url_mixed() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"img");
        let result = extract(&make_req(vec![parts_msg(vec![
            image_part("https://example.com/photo.jpg"),
            image_part(&format!("data:image/png;base64,{}", b64)),
        ])]));
        assert_eq!(result.files.len(), 1);
        assert!(result.has_http_urls);
    }
}
