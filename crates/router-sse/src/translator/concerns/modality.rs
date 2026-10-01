//! Strip multimodal blocks a model cannot read.
//!
//! Runs on the source-format body before translation. Removed media is replaced
//! with a placeholder so a message never becomes empty — an empty turn makes
//! some upstreams reject the request outright. The current turn gets an
//! explanatory placeholder; earlier turns get a neutral one, because a combo
//! may route each turn to a different model.

use serde_json::{Value, json};

use crate::catalog::Capabilities;
use crate::translator::formats;

/// The three modality capabilities this strips on, using the catalog's field
/// names (`vision` / `audioInput` / `pdf`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Modality {
    Vision,
    AudioInput,
    Pdf,
}

impl Modality {
    fn current(self) -> &'static str {
        match self {
            Modality::Vision => "[image omitted: model has no vision support]",
            Modality::AudioInput => "[audio omitted: model has no audio support]",
            Modality::Pdf => "[file omitted: model has no document support]",
        }
    }
    fn previous(self) -> &'static str {
        match self {
            Modality::Vision => "[Previous image omitted from context.]",
            Modality::AudioInput => "[Previous audio omitted from context.]",
            Modality::Pdf => "[Previous file omitted from context.]",
        }
    }
    fn placeholder(self, is_last: bool) -> &'static str {
        if is_last {
            self.current()
        } else {
            self.previous()
        }
    }
}

/// `caps[cap] === false`.
fn disallowed(caps: &Capabilities, m: Modality) -> bool {
    match m {
        Modality::Vision => !caps.vision,
        Modality::AudioInput => !caps.audio_input,
        Modality::Pdf => !caps.pdf,
    }
}

/// `capForMime`: gemini inlineData/fileData mime → required modality.
fn cap_for_mime(mime: Option<&str>) -> Option<Modality> {
    let mime = mime?;
    if mime.starts_with("image/") {
        Some(Modality::Vision)
    } else if mime.starts_with("audio/") {
        Some(Modality::AudioInput)
    } else if mime == "application/pdf" {
        Some(Modality::Pdf)
    } else {
        None
    }
}

/// `capForOpenAIBlock`.
fn cap_for_openai_block(block: &Value) -> Option<Modality> {
    match block.get("type").and_then(Value::as_str) {
        Some("image_url") | Some("image") => Some(Modality::Vision),
        Some("input_audio") | Some("audio_url") => Some(Modality::AudioInput),
        Some("file") => Some(Modality::Pdf),
        _ => None,
    }
}

/// `capForClaudeBlock`.
fn cap_for_claude_block(block: &Value) -> Option<Modality> {
    match block.get("type").and_then(Value::as_str) {
        Some("image") => Some(Modality::Vision),
        Some("document") => Some(Modality::Pdf),
        _ => None,
    }
}

/// `filterBlocks`: drop disallowed blocks, then append one placeholder per
/// removed modality. A `BTreeSet` keeps the placeholder order deterministic
/// where JS used a `Set` insertion order — the removal order is source order in
/// both, so the emitted placeholders match.
fn filter_blocks<F>(blocks: &[Value], cap_of: F, caps: &Capabilities, is_last: bool) -> Vec<Value>
where
    F: Fn(&Value) -> Option<Modality>,
{
    let mut out = Vec::with_capacity(blocks.len());
    let mut removed: Vec<Modality> = Vec::new();
    for block in blocks {
        if let Some(m) = cap_of(block)
            && disallowed(caps, m)
        {
            if !removed.contains(&m) {
                removed.push(m);
            }
            continue;
        }
        out.push(block.clone());
    }
    for m in removed {
        out.push(json!({"type": "text", "text": m.placeholder(is_last)}));
    }
    out
}

/// `stripOpenAI`.
fn strip_openai(body: &mut Value, caps: &Capabilities) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let last = messages.len().saturating_sub(1);
    for (i, msg) in messages.iter_mut().enumerate() {
        if !caps.vision
            && let Some(obj) = msg.as_object_mut()
        {
            obj.remove("images");
            for key in ["experimental_attachments", "attachments"] {
                if let Some(arr) = obj.get_mut(key).and_then(Value::as_array_mut) {
                    arr.retain(|a| {
                        let is_image = a
                            .get("contentType")
                            .and_then(Value::as_str)
                            .is_some_and(|c| c.starts_with("image/"))
                            || a.get("url")
                                .and_then(Value::as_str)
                                .is_some_and(|u| u.starts_with("data:image/"));
                        !is_image
                    });
                }
            }
        }
        let Some(content) = msg.get("content").and_then(Value::as_array).cloned() else {
            continue;
        };
        msg["content"] = Value::Array(filter_blocks(
            &content,
            cap_for_openai_block,
            caps,
            i == last,
        ));
    }
}

/// `stripClaude`.
fn strip_claude(body: &mut Value, caps: &Capabilities) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let last = messages.len().saturating_sub(1);
    for (i, msg) in messages.iter_mut().enumerate() {
        let Some(content) = msg.get("content").and_then(Value::as_array).cloned() else {
            continue;
        };
        msg["content"] = Value::Array(filter_blocks(
            &content,
            cap_for_claude_block,
            caps,
            i == last,
        ));
    }
}

/// `stripResponses`.
fn strip_responses(body: &mut Value, caps: &Capabilities) {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let last = input.len().saturating_sub(1);
    for (i, item) in input.iter_mut().enumerate() {
        let Some(content) = item.get("content").and_then(Value::as_array).cloned() else {
            continue;
        };
        let mut out = Vec::with_capacity(content.len());
        let mut removed: Vec<Modality> = Vec::new();
        for block in &content {
            let m = match block.get("type").and_then(Value::as_str) {
                Some("input_image") => Some(Modality::Vision),
                Some("input_file") => Some(Modality::Pdf),
                _ => None,
            };
            if let Some(m) = m
                && disallowed(caps, m)
            {
                if !removed.contains(&m) {
                    removed.push(m);
                }
                continue;
            }
            out.push(block.clone());
        }
        for m in removed {
            out.push(json!({"type": "input_text", "text": m.placeholder(i == last)}));
        }
        item["content"] = Value::Array(out);
    }
}

/// `stripGeminiParts(contents, caps)`.
fn strip_gemini_parts(contents: Option<&mut Value>, caps: &Capabilities) {
    let Some(contents) = contents.and_then(Value::as_array_mut) else {
        return;
    };
    let last = contents.len().saturating_sub(1);
    for (i, c) in contents.iter_mut().enumerate() {
        let Some(parts) = c.get("parts").and_then(Value::as_array).cloned() else {
            continue;
        };
        let mut out = Vec::with_capacity(parts.len());
        let mut removed: Vec<Modality> = Vec::new();
        for p in &parts {
            let mime = p
                .get("inlineData")
                .and_then(|d| d.get("mimeType"))
                .or_else(|| p.get("fileData").and_then(|d| d.get("mimeType")))
                .and_then(Value::as_str);
            if let Some(m) = cap_for_mime(mime)
                && disallowed(caps, m)
            {
                if !removed.contains(&m) {
                    removed.push(m);
                }
                continue;
            }
            out.push(p.clone());
        }
        for m in removed {
            out.push(json!({"text": m.placeholder(i == last)}));
        }
        c["parts"] = Value::Array(out);
    }
}

/// `stripUnsupportedModalities(body, sourceFormat, caps)`: `true` when the body
/// was eligible for stripping (some modality is disabled), regardless of whether
/// anything was actually removed.
pub fn strip_unsupported_modalities(
    body: &mut Value,
    source_format: &str,
    caps: &Capabilities,
) -> bool {
    // Fast exit: the model supports everything we would strip.
    if caps.vision && caps.audio_input && caps.pdf {
        return false;
    }

    match source_format {
        formats::OPENAI | formats::KIRO | formats::CURSOR | formats::COMMANDCODE => {
            strip_openai(body, caps)
        }
        formats::CLAUDE => strip_claude(body, caps),
        formats::OPENAI_RESPONSES | formats::OPENAI_RESPONSE | formats::CODEX => {
            strip_responses(body, caps)
        }
        formats::GEMINI | formats::GEMINI_CLI | formats::VERTEX => {
            strip_gemini_parts(body.get_mut("contents"), caps)
        }
        _ => strip_openai(body, caps),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::get_capabilities_for_model;

    #[test]
    fn a_capable_model_is_left_untouched() {
        let caps = get_capabilities_for_model(None, "claude-opus-4.8");
        let mut body = json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {}}]}]});
        let original = body.clone();
        // The fast exit needs all three modalities; this model leaves `pdf`
        // and `audioInput` at the default, so the pass runs and reports true
        // even though nothing was stripped.
        assert!(strip_unsupported_modalities(
            &mut body,
            formats::OPENAI,
            &caps
        ));
        assert_eq!(body, original);
    }

    #[test]
    fn openai_image_blocks_become_a_current_turn_placeholder() {
        // llama-3.1-8b has no vision.
        let caps = get_capabilities_for_model(None, "llama-3.1-8b");
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image_url", "image_url": {}}]},
        ]});
        assert!(strip_unsupported_modalities(
            &mut body,
            formats::OPENAI,
            &caps
        ));
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[1]["type"], json!("text"));
        assert_eq!(
            content[1]["text"],
            json!("[image omitted: model has no vision support]")
        );
    }

    #[test]
    fn earlier_turns_get_the_neutral_placeholder() {
        let caps = get_capabilities_for_model(None, "llama-3.1-8b");
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "image_url", "image_url": {}}]},
            {"role": "user", "content": "now"},
        ]});
        strip_unsupported_modalities(&mut body, formats::OPENAI, &caps);
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            json!("[Previous image omitted from context.]")
        );
    }

    #[test]
    fn claude_documents_are_stripped_for_a_text_only_model() {
        let caps = get_capabilities_for_model(None, "llama-3.1-8b");
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "document", "source": {}}, {"type": "text", "text": "read"}]},
        ]});
        strip_unsupported_modalities(&mut body, formats::CLAUDE, &caps);
        let content = body["messages"][0]["content"].as_array().unwrap();
        // The surviving text block keeps its index; the placeholder is appended.
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], json!("read"));
        assert_eq!(content[1]["type"], json!("text"));
        assert_eq!(
            content[1]["text"],
            json!("[file omitted: model has no document support]")
        );
    }

    #[test]
    fn gemini_parts_are_filtered_by_mime() {
        let caps = get_capabilities_for_model(None, "llama-3.1-8b");
        let mut body = json!({"contents": [
            {"parts": [
                {"inlineData": {"mimeType": "image/png", "data": "x"}},
                {"text": "hi"},
            ]},
        ]});
        strip_unsupported_modalities(&mut body, formats::GEMINI, &caps);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[1]["text"],
            json!("[image omitted: model has no vision support]")
        );
    }

    #[test]
    fn openai_attachment_arrays_lose_their_images() {
        let caps = get_capabilities_for_model(None, "llama-3.1-8b");
        let mut body = json!({"messages": [
            {"role": "user", "content": "x",
             "attachments": [
                 {"contentType": "image/png", "url": "u"},
                 {"contentType": "text/plain", "url": "v"},
             ]},
        ]});
        strip_unsupported_modalities(&mut body, formats::OPENAI, &caps);
        let atts = body["messages"][0]["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["contentType"], json!("text/plain"));
    }
}
