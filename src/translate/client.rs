use super::TranslationOptions;
use crate::markup::validate_translation;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub(super) fn chat_completion(
    client: &Client,
    options: &TranslationOptions,
    text: &str,
    formulas: &[String],
) -> Result<String> {
    let url = if options.api_base.ends_with("/chat/completions") {
        options.api_base.clone()
    } else {
        format!(
            "{}/chat/completions",
            options.api_base.trim_end_matches('/')
        )
    };

    let target = language_name(&options.target_lang);
    let system = format!(
        "You are a professional academic translator. Translate the user's {source} academic text into {target}. \
         Preserve citations, numbers, equations, URLs, code identifiers, and technical terms. \
         The input may contain placeholders such as <f1>. Keep every placeholder exactly as-is, \
         in the same order, and do not translate or remove it. \
         The input may also contain <b>, </b>, <i>, and </i> style tags. Keep each style tag \
         exactly as-is around the corresponding translated words; do not translate the tags. \
         Preserve literal escapes &lt;, &gt; and &amp;. Keep the translation concise enough for the original layout. Output only the translation, \
         with no preamble, quotes, or explanation.",
        source = "English",
        target = target
    );
    let user = format!(
        "Target language: {target}\n\
         Translate the following paragraph from an academic paper:\n\n{text}"
    );

    let mut body = ChatRequest {
        model: &options.model,
        messages: vec![
            Message {
                role: "system",
                content: system,
            },
            Message {
                role: "user",
                content: user,
            },
        ],
        temperature: options.temperature,
        max_tokens: options.max_tokens,
    };

    let mut last_error = anyhow!("translation failed");
    for attempt in 0..3 {
        match request_translation(
            client,
            &url,
            &body,
            options.api_key.as_deref(),
            text,
            formulas,
        ) {
            Ok(Reply::Complete(text)) => return Ok(text),
            Ok(Reply::Truncated) => {
                last_error = anyhow!("API truncated the translation (finish_reason=length)");
                body.max_tokens = body.max_tokens.saturating_mul(2);
            }
            Ok(Reply::InvalidMarkup) => {
                last_error = anyhow!("translation did not preserve formula or literal markers");
                let markers = formulas
                    .iter()
                    .map(|id| format!("<{id}>"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let retry_target = if options.target_lang.to_ascii_lowercase().starts_with("zh") {
                    "中文"
                } else {
                    target
                };
                body.messages = vec![Message {
                    role: "user",
                    content: format!(
                        "请将以下英文论文段落翻译成{retry_target}。保留全部公式标记 {markers}，每个恰好出现一次，顺序不变。保留数字、引文以及 <b>、</b>、<i>、</i> 样式标记。保留 &lt;、&gt; 和 &amp; 表示的原文。只输出译文。\n\n{text}"
                    ),
                }];
            }
            Err(error) => last_error = error,
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(500 * (1 << attempt)));
        }
    }

    Err(last_error)
}

enum Reply {
    Complete(String),
    Truncated,
    InvalidMarkup,
}

fn request_translation(
    client: &Client,
    url: &str,
    body: &ChatRequest<'_>,
    key: Option<&str>,
    source: &str,
    formulas: &[String],
) -> Result<Reply> {
    let mut request = client.post(url).json(body);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let response = request.send().context("HTTP request failed")?;
    let status = response.status();
    let body = response.text()?;
    if !status.is_success() {
        bail!(
            "API returned {status}: {}",
            body.chars().take(500).collect::<String>()
        );
    }
    let reply: ChatResponse =
        serde_json::from_str(&body).context("failed to parse API response")?;
    decode_reply(reply, source, formulas)
}

fn decode_reply(reply: ChatResponse, source: &str, formulas: &[String]) -> Result<Reply> {
    let choice = reply
        .choices
        .into_iter()
        .next()
        .context("API response has no choices")?;
    if choice.finish_reason.as_deref() == Some("length") {
        return Ok(Reply::Truncated);
    }
    if let Some(reason) = choice.finish_reason.as_deref()
        && reason != "stop"
    {
        bail!("translation did not complete (finish_reason={reason})");
    }
    let text = clean_translation(choice.message.content.as_deref().unwrap_or_default());
    if text.is_empty() {
        bail!("API returned an empty translation");
    }
    Ok(match validate_translation(source, &text, formulas) {
        Some(text) => Reply::Complete(text),
        None => Reply::InvalidMarkup,
    })
}

fn clean_translation(text: &str) -> String {
    let text = text.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .or_else(|| {
            text.strip_prefix('“')
                .and_then(|inner| inner.strip_suffix('”'))
        })
        .unwrap_or(text);
    text.trim().to_string()
}

fn language_name(code: &str) -> &str {
    let lower = code.to_ascii_lowercase();
    if lower.starts_with("zh") {
        "Simplified Chinese"
    } else if lower.starts_with("ja") {
        "Japanese"
    } else if lower.starts_with("ko") {
        "Korean"
    } else if lower.starts_with("de") {
        "German"
    } else if lower.starts_with("fr") {
        "French"
    } else if lower.starts_with("es") {
        "Spanish"
    } else {
        code
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_or_unbound_responses_do_not_become_completed_translations() {
        let reply = |reason: &str, text: &str| ChatResponse {
            choices: vec![Choice {
                finish_reason: Some(reason.into()),
                message: ResponseMessage {
                    content: Some(text.into()),
                },
            }],
        };
        assert!(matches!(
            decode_reply(reply("length", "Frag"), "Text", &[]).unwrap(),
            Reply::Truncated
        ));
        let formulas = ["f1".into(), "f2".into()];
        for bad in ["<f1>", "<f2><f1>", "<f1><f2><f3>"] {
            assert!(matches!(
                decode_reply(reply("stop", bad), "Using <f1> and <f2>", &formulas).unwrap(),
                Reply::InvalidMarkup
            ));
        }
        assert!(matches!(
            decode_reply(
                reply("stop", "字面 <f1> 和 <f2>"),
                "Literal &lt;f1&gt; with <f2>",
                &["f2".into()]
            )
            .unwrap(),
            Reply::Complete(_)
        ));
    }
}
