//! Pre-fetch remote image URLs into base64 before translation.
//!
//! Some targets (Gemini, Kiro, CommandCode) cannot fetch a remote URL
//! themselves and require inline base64. This runs on the *source*-format body
//! and rewrites each remote image in place, before the translators see it.
//!
//! A `Value` tree cannot hand out two mutable borrows at once, so this
//! collects JSON paths instead and resolves them after the fetches complete.

use serde_json::Value;

use crate::translator::concerns::image::{fetch_image_as_base64, parse_data_uri};
use crate::translator::formats;

/// Targets that require inline base64 images.
fn target_needs_base64(target_format: &str) -> bool {
    matches!(
        target_format,
        formats::GEMINI
            | formats::GEMINI_CLI
            | formats::VERTEX
            | formats::KIRO
            | formats::COMMANDCODE
    )
}

/// Where a remote image URL lives, and how to rewrite it.
#[derive(Debug, Clone, PartialEq)]
enum ImageRef {
    /// OpenAI `image_url` as a bare string: replace the string itself.
    OpenAiString(Vec<PathSeg>),
    /// OpenAI `image_url: {url}`: replace `.url`.
    OpenAiObject(Vec<PathSeg>),
    /// Claude `source: {type: "url", url}`: replace the whole `source`.
    ClaudeSource(Vec<PathSeg>),
    /// Gemini `fileData: {fileUri}`: replace `fileData` with `inlineData`.
    GeminiPart(Vec<PathSeg>),
}

/// Owned path segment (the borrowed `Seg` cannot outlive the walk).
#[derive(Debug, Clone, PartialEq)]
enum PathSeg {
    Key(String),
    Index(usize),
}

/// Read-only walk used by the path tests; production only ever writes through
/// `resolve_mut`.
#[cfg(test)]
fn resolve<'a>(root: &'a Value, path: &[PathSeg]) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path {
        cur = match seg {
            PathSeg::Key(k) => cur.get(k)?,
            PathSeg::Index(i) => cur.get(*i)?,
        };
    }
    Some(cur)
}

fn resolve_mut<'a>(root: &'a mut Value, path: &[PathSeg]) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path {
        cur = match seg {
            PathSeg::Key(k) => cur.get_mut(k)?,
            PathSeg::Index(i) => cur.get_mut(*i)?,
        };
    }
    Some(cur)
}

fn is_remote_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// `collectImageRefs(body, sourceFormat)`.
fn collect_image_refs(body: &Value, source_format: &str) -> Vec<(ImageRef, String)> {
    let mut refs: Vec<(ImageRef, String)> = Vec::new();

    let push_openai = |messages: Option<&Value>, refs: &mut Vec<(ImageRef, String)>| {
        let Some(messages) = messages.and_then(Value::as_array) else {
            return;
        };
        for (mi, msg) in messages.iter().enumerate() {
            let Some(content) = msg.get("content").and_then(Value::as_array) else {
                continue;
            };
            for (bi, block) in content.iter().enumerate() {
                if block.get("type").and_then(Value::as_str) != Some("image_url") {
                    continue;
                }
                let base = vec![
                    PathSeg::Key("messages".into()),
                    PathSeg::Index(mi),
                    PathSeg::Key("content".into()),
                    PathSeg::Index(bi),
                    PathSeg::Key("image_url".into()),
                ];
                let image_url = block.get("image_url");
                if let Some(url) = image_url.and_then(Value::as_str) {
                    if is_remote_url(url) {
                        refs.push((ImageRef::OpenAiString(base), url.to_string()));
                    }
                } else if let Some(url) =
                    image_url.and_then(|v| v.get("url")).and_then(Value::as_str)
                    && is_remote_url(url)
                {
                    let mut path = base;
                    path.push(PathSeg::Key("url".into()));
                    refs.push((ImageRef::OpenAiObject(path), url.to_string()));
                }
            }
        }
    };

    let push_gemini =
        |contents: Option<&Value>, prefix: &[PathSeg], refs: &mut Vec<(ImageRef, String)>| {
            let Some(contents) = contents.and_then(Value::as_array) else {
                return;
            };
            for (ci, c) in contents.iter().enumerate() {
                let Some(parts) = c.get("parts").and_then(Value::as_array) else {
                    continue;
                };
                for (pi, p) in parts.iter().enumerate() {
                    if let Some(uri) = p
                        .get("fileData")
                        .and_then(|f| f.get("fileUri"))
                        .and_then(Value::as_str)
                        && is_remote_url(uri)
                    {
                        let mut path = prefix.to_vec();
                        path.push(PathSeg::Key("contents".into()));
                        path.push(PathSeg::Index(ci));
                        path.push(PathSeg::Key("parts".into()));
                        path.push(PathSeg::Index(pi));
                        path.push(PathSeg::Key("fileData".into()));
                        refs.push((ImageRef::GeminiPart(path), uri.to_string()));
                    }
                }
            }
        };

    match source_format {
        formats::OPENAI | formats::KIRO | formats::CURSOR | formats::COMMANDCODE => {
            push_openai(body.get("messages"), &mut refs)
        }
        formats::CLAUDE => {
            if let Some(messages) = body.get("messages").and_then(Value::as_array) {
                for (mi, msg) in messages.iter().enumerate() {
                    let Some(content) = msg.get("content").and_then(Value::as_array) else {
                        continue;
                    };
                    for (bi, block) in content.iter().enumerate() {
                        if block.get("type").and_then(Value::as_str) != Some("image") {
                            continue;
                        }
                        let source = block.get("source");
                        if source.and_then(|s| s.get("type")).and_then(Value::as_str) != Some("url")
                        {
                            continue;
                        }
                        if let Some(url) = source.and_then(|s| s.get("url")).and_then(Value::as_str)
                            && is_remote_url(url)
                        {
                            let path = vec![
                                PathSeg::Key("messages".into()),
                                PathSeg::Index(mi),
                                PathSeg::Key("content".into()),
                                PathSeg::Index(bi),
                                PathSeg::Key("source".into()),
                            ];
                            refs.push((ImageRef::ClaudeSource(path), url.to_string()));
                        }
                    }
                }
            }
        }
        formats::GEMINI | formats::GEMINI_CLI | formats::VERTEX => {
            push_gemini(body.get("contents"), &[], &mut refs)
        }
        _ => push_openai(body.get("messages"), &mut refs),
    }
    refs
}

/// `prefetchRemoteImages(body, sourceFormat, targetFormat)`: the count of images
/// converted. No-op when the target accepts remote URLs or the body has none.
pub async fn prefetch_remote_images(
    body: &mut Value,
    source_format: &str,
    target_format: &str,
) -> usize {
    if !target_needs_base64(target_format) {
        return 0;
    }
    let refs = collect_image_refs(body, source_format);
    if refs.is_empty() {
        return 0;
    }

    let mut converted = 0;
    for (image_ref, url) in refs {
        if parse_data_uri(&url).is_some() {
            continue; // already inline
        }
        let Some(fetched) = fetch_image_as_base64(&url).await else {
            continue;
        };
        let base64 = fetched.url.split_once(',').map(|(_, b)| b).unwrap_or("");
        match image_ref {
            ImageRef::OpenAiString(path) | ImageRef::OpenAiObject(path) => {
                if let Some(slot) = resolve_mut(body, &path) {
                    *slot = Value::String(fetched.url.clone());
                }
            }
            ImageRef::ClaudeSource(path) => {
                if let Some(slot) = resolve_mut(body, &path) {
                    *slot = serde_json::json!({
                        "type": "base64",
                        "media_type": fetched.mime_type,
                        "data": base64,
                    });
                }
            }
            ImageRef::GeminiPart(path) => {
                if let Some(slot) = resolve_mut(body, &path) {
                    *slot = serde_json::json!({
                        "inlineData": {"mimeType": fetched.mime_type, "data": base64},
                    });
                }
            }
        }
        converted += 1;
    }
    converted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_target_that_accepts_urls_is_not_touched() {
        // openai and claude accept remote URLs, so nothing is collected even
        // though the ref exists.
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
        ]}]});
        let refs = collect_image_refs(&body, formats::OPENAI);
        assert_eq!(refs.len(), 1);
        // The target gate is what stops the fetch.
        assert!(!target_needs_base64(formats::OPENAI));
        assert!(target_needs_base64(formats::GEMINI));
    }

    #[test]
    fn openai_refs_cover_both_image_url_shapes() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": "https://example.com/a.png"},
            {"type": "image_url", "image_url": {"url": "https://example.com/b.png"}},
        ]}]});
        let refs = collect_image_refs(&body, formats::OPENAI);
        assert_eq!(refs.len(), 2);
        assert!(matches!(refs[0].0, ImageRef::OpenAiString(_)));
        assert!(matches!(refs[1].0, ImageRef::OpenAiObject(_)));
        assert_eq!(refs[1].1, "https://example.com/b.png");
    }

    #[test]
    fn a_data_uri_is_not_collected_as_remote() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        ]}]});
        assert!(collect_image_refs(&body, formats::OPENAI).is_empty());
    }

    #[test]
    fn claude_only_collects_url_sources() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}},
        ]}]});
        let refs = collect_image_refs(&body, formats::CLAUDE);
        assert_eq!(refs.len(), 1);
        assert!(matches!(refs[0].0, ImageRef::ClaudeSource(_)));
    }

    #[test]
    fn gemini_file_data_becomes_an_inline_part() {
        let body = json!({"contents": [{"parts": [{"fileData": {"fileUri": "https://example.com/a.png"}}]}]});
        let refs = collect_image_refs(&body, formats::GEMINI);
        assert_eq!(refs.len(), 1);
        assert!(matches!(refs[0].0, ImageRef::GeminiPart(_)));
    }

    #[test]
    fn paths_resolve_back_into_the_tree() {
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
        ]}]});
        let path = vec![
            PathSeg::Key("messages".into()),
            PathSeg::Index(0),
            PathSeg::Key("content".into()),
            PathSeg::Index(0),
            PathSeg::Key("image_url".into()),
            PathSeg::Key("url".into()),
        ];
        assert_eq!(
            resolve(&body, &path).and_then(Value::as_str),
            Some("https://example.com/a.png")
        );
        *resolve_mut(&mut body, &path).unwrap() = json!("data:image/png;base64,AAAA");
        assert_eq!(
            body["messages"][0]["content"][0]["image_url"]["url"],
            json!("data:image/png;base64,AAAA")
        );
    }
}
