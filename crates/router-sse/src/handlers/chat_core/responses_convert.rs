//! The Chat Completions → Responses API converter, shared by the non-streaming
//! and forced-SSE-to-JSON handlers.
//!
//! The non-streaming and forced-SSE-to-JSON paths both need this converter. It
//! lives in one shared module both siblings depend on, so there is a single
//! implementation rather than two byte-identical copies.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use crate::translator::concerns::primitives::{js_number, js_string, js_truthy};
use crate::translator::schema::{responses_item, role};

/// `extractCustomToolInput(argumentsValue)`.
pub(crate) fn extract_custom_tool_input(arguments_value: Option<&Value>) -> String {
    let arguments_text = match arguments_value {
        Some(Value::String(s)) => s.clone(),
        Some(value) if js_truthy(value) => {
            serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
        }
        _ => "{}".to_string(),
    };
    if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&arguments_text)
        && let Some(input) = parsed.get("input").and_then(Value::as_str)
    {
        return input.to_string();
    }
    arguments_text
}

/// `resp_${id}`.replace(/^resp_chatcmpl-/, "resp_").
pub(crate) fn responses_id(id: Option<&Value>) -> String {
    let id = format!(
        "resp_{}",
        id.filter(|v| js_truthy(v))
            .map(js_string)
            .unwrap_or_default()
    );
    match id.strip_prefix("resp_chatcmpl-") {
        Some(rest) => format!("resp_{rest}"),
        None => id,
    }
}

/// Convert an OpenAI Chat Completions body into a Responses `response` object.
///
/// `status` is the one thing the two callers disagree on: the forced-SSE path
/// always reports `"completed"`, while the non-streaming path surfaces a
/// `length` (or other) `finish_reason` as the status. `None` means "derive it
/// from `choices[0].finish_reason`".
pub(crate) fn completion_to_responses(
    response_body: &Value,
    custom_tool_names: Option<&HashSet<String>>,
    status: Option<&str>,
) -> Value {
    let Some(choice) = response_body.get("choices").and_then(|c| c.get(0)) else {
        return response_body.clone();
    };
    let message = choice.get("message").cloned().unwrap_or(json!({}));
    let mut output: Vec<Value> = Vec::new();

    let reasoning = message
        .get("reasoning_content")
        .filter(|v| js_truthy(v))
        .or_else(|| message.get("reasoning").filter(|v| js_truthy(v)));
    if let Some(Value::String(reasoning)) = reasoning
        && !reasoning.is_empty()
    {
        output.push(json!({
            "type": responses_item::REASONING,
            "summary": [{ "type": responses_item::SUMMARY_TEXT, "text": reasoning }],
        }));
    }

    let text = message.get("content").and_then(Value::as_str).unwrap_or("");
    if !text.is_empty() {
        output.push(json!({
            "type": responses_item::MESSAGE,
            "role": role::ASSISTANT,
            "content": [{ "type": responses_item::OUTPUT_TEXT, "text": text, "annotations": [] }],
        }));
    }

    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for tool_call in tool_calls {
            let function = tool_call.get("function").cloned().unwrap_or(json!({}));
            let name = function.get("name").and_then(Value::as_str).unwrap_or("");
            let custom = custom_tool_names.is_some_and(|names| names.contains(name));
            let id = tool_call.get("id").and_then(Value::as_str).unwrap_or("");

            let mut item = Map::new();
            item.insert(
                "type".into(),
                json!(if custom {
                    responses_item::CUSTOM_TOOL_CALL
                } else {
                    responses_item::FUNCTION_CALL
                }),
            );
            item.insert(
                "id".into(),
                json!(format!("{}_{id}", if custom { "ctc" } else { "fc" })),
            );
            item.insert("call_id".into(), json!(id));
            item.insert("name".into(), json!(name));
            if custom {
                item.insert(
                    "input".into(),
                    json!(extract_custom_tool_input(function.get("arguments"))),
                );
            } else {
                let arguments = match function.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(value) => {
                        serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
                    }
                    None => "{}".to_string(),
                };
                item.insert("arguments".into(), json!(arguments));
            }
            output.push(Value::Object(item));
        }
    }

    let usage = response_body.get("usage").cloned().unwrap_or(json!({}));
    let prompt = usage
        .get("prompt_tokens")
        .filter(|v| js_truthy(v))
        .or_else(|| usage.get("input_tokens").filter(|v| js_truthy(v)))
        .map(|v| js_number(Some(v)))
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .filter(|v| js_truthy(v))
        .or_else(|| usage.get("output_tokens").filter(|v| js_truthy(v)))
        .map(|v| js_number(Some(v)))
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .filter(|v| js_truthy(v))
        .map(|v| js_number(Some(v)))
        .unwrap_or(prompt + completion);

    let status = status.map(str::to_string).unwrap_or_else(|| {
        match choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some("tool_calls") | Some("stop") => "completed".to_string(),
            Some(other) => other.to_string(),
            None => "completed".to_string(),
        }
    });

    let mut result = Map::new();
    result.insert("id".into(), json!(responses_id(response_body.get("id"))));
    result.insert("object".into(), json!("response"));
    result.insert(
        "created_at".into(),
        json!(
            response_body
                .get("created")
                .filter(|v| js_truthy(v))
                .map(|v| js_number(Some(v)))
                .unwrap_or(router_db::time::now_ms() / 1000)
        ),
    );
    result.insert(
        "model".into(),
        json!(
            response_body
                .get("model")
                .filter(|v| js_truthy(v))
                .map(js_string)
                .unwrap_or_else(|| "unknown".to_string())
        ),
    );
    result.insert("status".into(), json!(status));
    result.insert("background".into(), json!(false));
    result.insert("error".into(), Value::Null);
    result.insert("output".into(), Value::Array(output));
    result.insert(
        "usage".into(),
        json!({
            "input_tokens": prompt,
            "output_tokens": completion,
            "total_tokens": total,
        }),
    );
    Value::Object(result)
}
