//! Non-blocking context compression.

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use futures::{StreamExt, stream};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::config::ContextConfig;
use crate::ds_core::{ChatRequest, DeepSeekCore};
use crate::openai_adapter::response::{self, TagConfig};
use crate::openai_adapter::types::{ChatCompletionsRequest, Message, MessageContent};
use crate::openai_adapter::{OpenAIAdapterError, request};

const MAX_CONTEXT_CACHE_ENTRIES: usize = 512;

struct CacheEntry {
    summary: String,
    created_at: Instant,
    covered_count: usize,
    covered_hash: String,
}

struct ContextPlan {
    cache_key: String,
    in_flight_key: String,
    prefix: Vec<Message>,
    non_system: Vec<Message>,
    systems: Vec<Message>,
    prefix_count: usize,
    prefix_hash: String,
    prefix_chars: usize,
}

pub(crate) struct ContextOptimizer {
    cfg: RwLock<ContextConfig>,
    ds_core: Arc<DeepSeekCore>,
    cache: DashMap<String, CacheEntry>,
    in_flight: DashMap<String, ()>,
}

impl ContextOptimizer {
    pub(crate) fn new(cfg: ContextConfig, ds_core: Arc<DeepSeekCore>) -> Arc<Self> {
        Arc::new(Self {
            cfg: RwLock::new(cfg),
            ds_core,
            cache: DashMap::new(),
            in_flight: DashMap::new(),
        })
    }

    pub(crate) async fn reload(&self, cfg: ContextConfig) {
        *self.cfg.write().await = cfg;
    }

    pub(crate) async fn apply(
        self: &Arc<Self>,
        req: &mut ChatCompletionsRequest,
        request_id: &str,
        tag_config: Arc<TagConfig>,
    ) {
        let cfg = self.cfg.read().await.clone();
        if !cfg.enabled {
            return;
        }

        let Some(plan) = plan_request(req, &cfg) else {
            return;
        };

        let mut used_covered_count = None;
        if plan.prefix_chars >= cfg.trigger_chars
            && let Some((summary, covered_count)) = self.cached_summary(&plan, cfg.cache_ttl_secs)
        {
            let mut messages = plan.systems.clone();
            messages.push(summary_message(summary, cfg.summary_max_chars));
            messages.extend(plan.non_system[covered_count..].iter().cloned());
            req.messages = messages;
            used_covered_count = Some(covered_count);
            log::debug!(
                target: "adapter::context",
                "req={} context cache hit: prefix_chars={}, covered_count={}, keep_last={}",
                request_id,
                plan.prefix_chars,
                covered_count,
                cfg.keep_last_messages
            );
        }

        if plan.prefix_chars >= cfg.prewarm_chars && used_covered_count != Some(plan.prefix_count) {
            self.spawn_summary(plan, cfg, request_id.to_string(), tag_config);
        }
    }

    fn cached_summary(&self, plan: &ContextPlan, ttl_secs: u64) -> Option<(String, usize)> {
        if let Some(entry) = self.cache.get(&plan.cache_key) {
            if entry.created_at.elapsed().as_secs() <= ttl_secs
                && entry.covered_count <= plan.prefix_count
                && hash_messages(&plan.non_system[..entry.covered_count]) == entry.covered_hash
            {
                return Some((entry.summary.clone(), entry.covered_count));
            }
        }
        self.cache.remove(&plan.cache_key);
        None
    }

    fn spawn_summary(
        self: &Arc<Self>,
        plan: ContextPlan,
        cfg: ContextConfig,
        request_id: String,
        tag_config: Arc<TagConfig>,
    ) {
        if let Some(entry) = self.cache.get(&plan.cache_key)
            && entry.covered_count >= plan.prefix_count
            && entry.covered_hash == plan.prefix_hash
        {
            return;
        }
        if self
            .in_flight
            .insert(plan.in_flight_key.clone(), ())
            .is_some()
        {
            return;
        }

        let this = self.clone();
        tokio::spawn(async move {
            if cfg.background_delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(cfg.background_delay_ms)).await;
            }
            let cache_key = plan.cache_key.clone();
            let in_flight_key = plan.in_flight_key.clone();
            let prefix_count = plan.prefix_count;
            let prefix_hash = plan.prefix_hash.clone();
            let prefix_chars = plan.prefix_chars;
            match this
                .summarize_prefix(plan.prefix, cfg.clone(), &request_id, tag_config)
                .await
            {
                Ok(summary) => {
                    let summary = truncate_chars(&summary, cfg.summary_max_chars);
                    let should_store = this
                        .cache
                        .get(&cache_key)
                        .is_none_or(|entry| entry.covered_count <= prefix_count);
                    if should_store {
                        this.cache.insert(
                            cache_key.clone(),
                            CacheEntry {
                                summary,
                                created_at: Instant::now(),
                                covered_count: prefix_count,
                                covered_hash: prefix_hash,
                            },
                        );
                        this.prune_cache(cfg.cache_ttl_secs, MAX_CONTEXT_CACHE_ENTRIES);
                        log::debug!(
                            target: "adapter::context",
                            "req={} context summary cached: prefix_chars={}, covered_count={}",
                            request_id,
                            prefix_chars,
                            prefix_count
                        );
                    }
                }
                Err(err) => {
                    log::warn!(
                        target: "adapter::context",
                        "req={} context summary failed: {}",
                        request_id,
                        err
                    );
                }
            }
            this.in_flight.remove(&in_flight_key);
        });
    }

    fn prune_cache(&self, ttl_secs: u64, max_entries: usize) {
        let mut expired = Vec::new();
        let mut entries = Vec::new();

        for entry in self.cache.iter() {
            let age_secs = entry.created_at.elapsed().as_secs();
            if age_secs > ttl_secs {
                expired.push(entry.key().clone());
            } else {
                entries.push((entry.key().clone(), age_secs));
            }
        }

        for key in expired {
            self.cache.remove(&key);
        }

        let len = entries.len();
        if len <= max_entries {
            return;
        }

        entries.sort_by(|a, b| b.1.cmp(&a.1));
        for (key, _) in entries.into_iter().take(len - max_entries) {
            self.cache.remove(&key);
        }
    }

    async fn summarize_prefix(
        self: &Arc<Self>,
        messages: Vec<Message>,
        cfg: ContextConfig,
        request_id: &str,
        tag_config: Arc<TagConfig>,
    ) -> Result<String, OpenAIAdapterError> {
        let text = render_messages(&messages);
        let chunks = split_text_chunks(&text, cfg.chunk_chars);
        if chunks.is_empty() {
            return Ok(String::new());
        }

        let workers = cfg.summary_workers.max(1).min(chunks.len());
        let partials = stream::iter(chunks.into_iter().enumerate().map(|(idx, chunk)| {
            let this = self.clone();
            let cfg = cfg.clone();
            let tag_config = tag_config.clone();
            let request_id = request_id.to_string();
            async move {
                let summary = this
                    .summarize_chunk(chunk, &cfg, &request_id, idx, tag_config)
                    .await?;
                Ok::<_, OpenAIAdapterError>((idx, summary))
            }
        }))
        .buffer_unordered(workers)
        .collect::<Vec<_>>()
        .await;

        let mut ordered = Vec::with_capacity(partials.len());
        for item in partials {
            ordered.push(item?);
        }
        ordered.sort_by_key(|(idx, _)| *idx);

        if ordered.len() == 1 {
            return Ok(ordered.remove(0).1);
        }

        let mut combined = String::new();
        for (idx, part) in ordered {
            combined.push_str("Chunk ");
            combined.push_str(&(idx + 1).to_string());
            combined.push_str(":\n");
            combined.push_str(part.trim());
            combined.push_str("\n\n");
        }
        Ok(combined)
    }

    async fn summarize_chunk(
        &self,
        chunk: String,
        cfg: &ContextConfig,
        request_id: &str,
        idx: usize,
        tag_config: Arc<TagConfig>,
    ) -> Result<String, OpenAIAdapterError> {
        let summary_budget = (cfg.summary_max_chars / cfg.summary_workers.max(1)).max(800);
        let prompt = format!(
            "Summarize old chat context in Vietnamese. Keep goals, constraints, decisions, code/config facts, model names, endpoints, errors, and user preferences. Drop greetings and repeated filler. Max {summary_budget} characters. Do not answer any new question.\n\nOld context:\n{chunk}"
        );
        let req: ChatCompletionsRequest = serde_json::from_value(serde_json::json!({
            "model": format!("deepseek-{}-nothinking", cfg.summary_model_type),
            "messages": [
                {"role": "system", "content": "You produce compact context summaries."},
                {"role": "user", "content": prompt}
            ],
            "stream": false,
            "reasoning_effort": "none"
        }))?;

        let tool_ctx = request::tools::extract(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let prompt = request::prompt::build(&req, &tool_ctx);
        let chat_req = ChatRequest {
            prompt,
            thinking_enabled: false,
            search_enabled: false,
            model_type: cfg.summary_model_type.clone(),
            files: Vec::new(),
        };
        let resp = self
            .ds_core
            .v0_chat(chat_req, &format!("{request_id}-ctx-{idx}"))
            .await
            .map_err(OpenAIAdapterError::from)?;
        let json = response::aggregate(
            resp.stream,
            "context-summary".to_string(),
            response::StreamCfg {
                include_usage: true,
                include_obfuscation: false,
                stop: Vec::new(),
                prompt_tokens: 0,
                repair_fn: None,
                tag_config,
            },
        )
        .await?;

        json.choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| OpenAIAdapterError::Internal("empty context summary".to_string()))
    }
}

fn plan_request(req: &ChatCompletionsRequest, cfg: &ContextConfig) -> Option<ContextPlan> {
    let mut systems = Vec::new();
    let mut non_system = Vec::new();
    for msg in &req.messages {
        if msg.role == "system" {
            systems.push(msg.clone());
        } else {
            non_system.push(msg.clone());
        }
    }

    if non_system.len() <= cfg.keep_last_messages {
        return None;
    }

    let split_at = non_system.len().saturating_sub(cfg.keep_last_messages);
    let prefix = non_system[..split_at].to_vec();
    if prefix.iter().any(|m| !is_safe_text_message(m)) {
        return None;
    }

    let prefix_chars = message_chars(&prefix);
    if prefix_chars < cfg.prewarm_chars {
        return None;
    }

    let prefix_hash = hash_messages(&prefix);
    let cache_key = conversation_key(req, &systems, &non_system);
    let in_flight_key = format!("{cache_key}:{split_at}:{prefix_hash}");

    Some(ContextPlan {
        cache_key,
        in_flight_key,
        prefix,
        non_system,
        systems,
        prefix_count: split_at,
        prefix_hash,
        prefix_chars,
    })
}

fn is_safe_text_message(msg: &Message) -> bool {
    matches!(msg.role.as_str(), "user" | "assistant")
        && msg.tool_calls.is_none()
        && msg.function_call.is_none()
        && msg.audio.is_none()
        && msg.refusal.is_none()
        && msg.content.as_ref().is_none_or(is_text_only_content)
}

fn is_text_only_content(content: &MessageContent) -> bool {
    match content {
        MessageContent::Text(_) => true,
        MessageContent::Parts(parts) => parts.iter().all(|p| {
            p.text.is_some()
                && p.image_url.is_none()
                && p.input_audio.is_none()
                && p.file.is_none()
                && p.refusal.is_none()
        }),
    }
}

fn summary_message(summary: String, max_chars: usize) -> Message {
    Message {
        role: "system".to_string(),
        content: Some(MessageContent::Text(format!(
            "Tom tat ngu canh truoc do (da nen de tang toc):\n{}",
            truncate_chars(&summary, max_chars)
        ))),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        function_call: None,
        audio: None,
        refusal: None,
    }
}

fn message_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|msg| msg.role.len() + msg.content.as_ref().map_or(0, content_text_len))
        .sum()
}

fn content_text_len(content: &MessageContent) -> usize {
    match content {
        MessageContent::Text(text) => text.chars().count(),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|part| part.text.as_deref().unwrap_or_default().chars().count())
            .sum(),
    }
}

fn content_to_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| part.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn render_messages(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        out.push_str(&msg.role);
        out.push_str(": ");
        if let Some(content) = &msg.content {
            out.push_str(&content_to_text(content));
        }
        out.push('\n');
    }
    out
}

fn hash_messages(messages: &[Message]) -> String {
    let mut hasher = Sha256::new();
    for msg in messages {
        hasher.update(msg.role.as_bytes());
        hasher.update([0]);
        if let Some(content) = &msg.content {
            hasher.update(content_to_text(content).as_bytes());
        }
        hasher.update([0xff]);
    }
    hex_digest(hasher.finalize().as_ref())
}

fn conversation_key(
    req: &ChatCompletionsRequest,
    systems: &[Message],
    non_system: &[Message],
) -> String {
    if let Some(key) = req.prompt_cache_key.as_deref().filter(|s| !s.is_empty()) {
        return format!("prompt_cache:{key}");
    }
    if let Some(key) = req
        .metadata
        .as_ref()
        .and_then(|m| {
            m.get("conversation_id")
                .or_else(|| m.get("thread_id"))
                .or_else(|| m.get("chat_id"))
                .or_else(|| m.get("id"))
        })
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return format!("metadata:{key}");
    }
    let mut hasher = Sha256::new();
    hasher.update(req.model.as_bytes());
    if let Some(user) = req.user.as_deref().filter(|s| !s.is_empty()) {
        hasher.update(user.as_bytes());
        hasher.update([0xfe]);
    }
    for msg in systems.iter().chain(non_system.iter().take(2)) {
        hasher.update(msg.role.as_bytes());
        hasher.update([0]);
        if let Some(content) = &msg.content {
            hasher.update(content_to_text(content).as_bytes());
        }
        hasher.update([0xff]);
    }
    format!("implicit:{}", hex_digest(hasher.finalize().as_ref()))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn split_text_chunks(text: &str, chunk_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let chunk_chars = chunk_chars.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for ch in text.chars() {
        current.push(ch);
        current_len += 1;
        if current_len >= chunk_chars {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = s.chars().take(max_chars).collect::<String>();
    out.push_str("\n[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_config() -> ContextConfig {
        ContextConfig {
            enabled: true,
            trigger_chars: 40,
            prewarm_chars: 20,
            keep_last_messages: 2,
            chunk_chars: 16,
            summary_workers: 2,
            summary_max_chars: 120,
            cache_ttl_secs: 60,
            background_delay_ms: 0,
            summary_model_type: "default".to_string(),
        }
    }

    fn request(messages: serde_json::Value) -> ChatCompletionsRequest {
        serde_json::from_value(json!({
            "model": "deepseek-default",
            "messages": messages,
            "stream": true,
            "metadata": {"conversation_id": "conv-1"}
        }))
        .unwrap()
    }

    #[test]
    fn short_context_has_no_plan() {
        let req = request(json!([
            {"role": "user", "content": "short"},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "next"}
        ]));

        assert!(plan_request(&req, &test_config()).is_none());
    }

    #[test]
    fn long_context_plans_old_prefix_only() {
        let req = request(json!([
            {"role": "system", "content": "system rule"},
            {"role": "user", "content": "xxxxxxxxxxxxxxxxxxxxxxxxx"},
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user"},
            {"role": "assistant", "content": "recent assistant"}
        ]));
        let plan = plan_request(&req, &test_config()).unwrap();

        assert_eq!(plan.cache_key, "metadata:conv-1");
        assert_eq!(plan.systems.len(), 1);
        assert_eq!(plan.prefix_count, 2);
        assert_eq!(plan.prefix.len(), 2);
        assert_eq!(plan.non_system.len(), 4);
    }

    #[test]
    fn prompt_cache_key_overrides_metadata_key() {
        let mut req = request(json!([
            {"role": "user", "content": "xxxxxxxxxxxxxxxxxxxxxxxxx"},
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user"},
            {"role": "assistant", "content": "recent assistant"}
        ]));
        req.prompt_cache_key = Some("stable-chat".to_string());
        let plan = plan_request(&req, &test_config()).unwrap();

        assert_eq!(plan.cache_key, "prompt_cache:stable-chat");
    }

    #[test]
    fn multimodal_prefix_is_not_summarized() {
        let req = request(json!([
            {
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {"url": "https://example.com/image.png"}
                    }
                ]
            },
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user"},
            {"role": "assistant", "content": "recent assistant"}
        ]));

        assert!(plan_request(&req, &test_config()).is_none());
    }

    #[test]
    fn assistant_tool_calls_in_prefix_disable_summary_plan() {
        let req = request(json!([
            {
                "role": "assistant",
                "content": "calling tool",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{}"}
                    }
                ]
            },
            {"role": "user", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user"},
            {"role": "assistant", "content": "recent assistant"}
        ]));

        assert!(plan_request(&req, &test_config()).is_none());
    }

    #[test]
    fn message_level_audio_in_prefix_disables_summary_plan() {
        let req = request(json!([
            {
                "role": "user",
                "content": "xxxxxxxxxxxxxxxxxxxxxxxxx",
                "audio": {"id": "audio-1"}
            },
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user"},
            {"role": "assistant", "content": "recent assistant"}
        ]));

        assert!(plan_request(&req, &test_config()).is_none());
    }

    #[test]
    fn thread_id_metadata_is_used_for_cache_key() {
        let mut req = request(json!([
            {"role": "user", "content": "xxxxxxxxxxxxxxxxxxxxxxxxx"},
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user"},
            {"role": "assistant", "content": "recent assistant"}
        ]));
        req.metadata = Some(json!({"thread_id": "thread-42"}));

        let plan = plan_request(&req, &test_config()).unwrap();

        assert_eq!(plan.cache_key, "metadata:thread-42");
    }

    #[test]
    fn text_only_parts_prefix_can_be_summarized() {
        let req = request(json!([
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "xxxxxxxxxxxx"},
                    {"type": "text", "text": "yyyyyyyyyyyy"}
                ]
            },
            {"role": "assistant", "content": "zzzzzzzzzzzzzzzzzzzzzzzzz"},
            {"role": "user", "content": "recent user"},
            {"role": "assistant", "content": "recent assistant"}
        ]));

        let plan = plan_request(&req, &test_config()).unwrap();

        assert_eq!(plan.prefix_count, 2);
    }

    #[test]
    fn implicit_cache_key_is_used_without_metadata_or_prompt_cache_key() {
        let mut req = request(json!([
            {"role": "system", "content": "system rule"},
            {"role": "user", "content": "xxxxxxxxxxxxxxxxxxxxxxxxx"},
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user"},
            {"role": "assistant", "content": "recent assistant"}
        ]));
        req.metadata = None;

        let plan = plan_request(&req, &test_config()).unwrap();

        assert!(plan.cache_key.starts_with("implicit:"));
    }

    #[test]
    fn implicit_cache_key_ignores_recent_tail_but_changes_with_user() {
        let mut req_a = request(json!([
            {"role": "system", "content": "system rule"},
            {"role": "user", "content": "xxxxxxxxxxxxxxxxxxxxxxxxx"},
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user a"},
            {"role": "assistant", "content": "recent assistant a"}
        ]));
        req_a.metadata = None;
        req_a.user = Some("user-a".to_string());

        let mut req_b = request(json!([
            {"role": "system", "content": "system rule"},
            {"role": "user", "content": "xxxxxxxxxxxxxxxxxxxxxxxxx"},
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user b"},
            {"role": "assistant", "content": "recent assistant b"}
        ]));
        req_b.metadata = None;
        req_b.user = Some("user-a".to_string());

        let mut req_c = request(json!([
            {"role": "system", "content": "system rule"},
            {"role": "user", "content": "xxxxxxxxxxxxxxxxxxxxxxxxx"},
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user c"},
            {"role": "assistant", "content": "recent assistant c"}
        ]));
        req_c.metadata = None;
        req_c.user = Some("user-c".to_string());

        let key_a = plan_request(&req_a, &test_config()).unwrap().cache_key;
        let key_b = plan_request(&req_b, &test_config()).unwrap().cache_key;
        let key_c = plan_request(&req_c, &test_config()).unwrap().cache_key;

        assert_eq!(key_a, key_b);
        assert_ne!(key_a, key_c);
    }

    #[test]
    fn prefix_chars_below_prewarm_threshold_skips_plan() {
        let mut cfg = test_config();
        cfg.prewarm_chars = 100;
        let req = request(json!([
            {"role": "user", "content": "xxxxxxxxxxxxxxxxxxxxxxxxx"},
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user"},
            {"role": "assistant", "content": "recent assistant"}
        ]));

        assert!(plan_request(&req, &cfg).is_none());
    }

    #[test]
    fn content_to_text_joins_text_parts_in_order() {
        let content = MessageContent::Parts(vec![
            crate::openai_adapter::types::ContentPart {
                ty: "text".to_string(),
                text: Some("first".to_string()),
                image_url: None,
                input_audio: None,
                file: None,
                refusal: None,
            },
            crate::openai_adapter::types::ContentPart {
                ty: "text".to_string(),
                text: Some("second".to_string()),
                image_url: None,
                input_audio: None,
                file: None,
                refusal: None,
            },
        ]);

        assert_eq!(content_to_text(&content), "first\nsecond");
    }

    #[test]
    fn split_text_chunks_keeps_all_text() {
        let chunks = split_text_chunks("abcdef", 2);

        assert_eq!(chunks, vec!["ab", "cd", "ef"]);
    }

    #[test]
    fn keep_last_messages_covering_all_non_system_skips_plan() {
        let mut cfg = test_config();
        cfg.keep_last_messages = 4;
        let req = request(json!([
            {"role": "system", "content": "system rule"},
            {"role": "user", "content": "xxxxxxxxxxxxxxxxxxxxxxxxx"},
            {"role": "assistant", "content": "yyyyyyyyyyyyyyyyyyyyyyyyy"},
            {"role": "user", "content": "recent user"},
            {"role": "assistant", "content": "recent assistant"}
        ]));

        assert!(plan_request(&req, &cfg).is_none());
    }

    #[test]
    fn truncate_chars_appends_marker_when_trimmed() {
        let out = truncate_chars("abcdef", 3);

        assert_eq!(out, "abc\n[truncated]");
    }
}
