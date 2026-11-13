use super::TranslationOptions;
use crate::markup::{Piece, pieces, validate_translation};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const PLACEHOLDER_RULES: &str = "\
Original PDF objects (formulas, list symbols, URLs, and code) are written as placeholders like <f1>. Keep each placeholder exactly once and unchanged. \
It may move with the words it belongs to, but never add, drop, duplicate, rename, or translate it. \
Each placeholder already represents the complete original object; do not guess its content or add variable names or list symbols beside it. \
Translate every ordinary word around placeholders, including short connecting words and qualifications. \
Only the placeholders are immutable; neighboring prose must not remain in English. \
Keep <b> and <i> tags paired.";

pub(super) fn chat_completion(
    client: &Client,
    options: &TranslationOptions,
    text: &str,
    formulas: &[String],
    context: &str,
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
    let placeholder_rules = if formulas.is_empty() {
        "Keep <b> and <i> tags paired."
    } else {
        PLACEHOLDER_RULES
    };
    let literal_rules = if ["&lt;", "&gt;", "&amp;"]
        .iter()
        .any(|tag| text.contains(tag))
    {
        "Preserve every &lt;, &gt; and &amp; exactly as in the source."
    } else {
        ""
    };
    let emphasis_rules = if text.contains("<b>") || text.contains("<i>") {
        "The <b> and <i> tags mark exactly which source words are bold or italic. \
         Translate all words both inside and outside the tags. Apply each tag only to the \
         translation of its original words, then close it before translating the following plain text. \
         Never extend a short emphasized phrase to a whole sentence or paragraph, remove its emphasis, \
         leave an empty emphasis span, or add emphasis to a heading merely because it is a heading."
    } else {
        ""
    };
    let color_rules = if pieces(text).any(|piece| matches!(piece, Piece::Color(_))) {
        "Keep every <cRRGGBBAA>...</c> color span paired, with its original hexadecimal code. \
         Translate its words and keep the color only on those translated words. \
         Never extend a colored citation to ordinary prose, drop a span, or leave it empty."
    } else {
        ""
    };
    let mut system = format!(
        "You are a professional academic translator. Translate English academic text into {target}. \
         Translate the complete text, including every qualifier in a title or heading. \
         Use only {target} for ordinary prose; retain foreign-script names only when present in the source. \
         Preserve citations, numbers, URLs, code identifiers and technical terms. {placeholder_rules} {literal_rules} {emphasis_rules} {color_rules} \
         Output only the translation.",
    );
    let examples = [
        (
            !formulas.is_empty(),
            "Formula: Given <f1>, use <f2> instead. -> 给定<f1>，改用<f2>。",
        ),
        (
            text.contains("<b>"),
            "Bold: <b>Key claim.</b> Ordinary explanation. -> <b>关键结论。</b>普通说明。",
        ),
        (
            text.contains("<i>"),
            "Italic: The <i>stable</i> solution. -> <i>稳定的</i>解。",
        ),
        (
            !color_rules.is_empty(),
            "Color: <c0000ffff>Smith</c> reports a result. -> <c0000ffff>Smith</c>报告了一项结果。",
        ),
        (
            !literal_rules.is_empty(),
            "Literal escapes: Keep &lt;tag&gt; &amp; code. -> 保留&lt;tag&gt; &amp; 代码。",
        ),
    ];
    let uses_examples = examples.iter().any(|(needed, _)| *needed);
    for (_, example) in examples.into_iter().filter(|(needed, _)| *needed) {
        system.push_str(&format!(
            "\nMarker example (Chinese illustration; use {target} for the actual text): {example}"
        ));
    }
    if uses_examples {
        system.push_str("\nExamples are rules only. Translate only the user message; never add example text or markers absent from it.");
    }
    if !context.is_empty() {
        system.push_str(&format!(
            "\nUse the following paper context only to disambiguate terminology and abbreviations. \
             Do not translate or repeat the context, and do not invent expansions of abbreviations.\n{context}"
        ));
    }
    // Keep instructions out of the user text, including short connective
    // fragments. Some local models otherwise translate the wrapper itself.
    let user = text.to_owned();

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
    let mut invalid_content = false;
    for attempt in 0..3 {
        match request_translation(
            client,
            &url,
            &body,
            options.api_key.as_deref(),
            text,
            formulas,
        ) {
            Ok(Reply::Complete(output)) => {
                if let Some(issue) = language_issue(text, &output, &options.target_lang) {
                    invalid_content = true;
                    last_error = anyhow!("{issue}");
                    body.temperature = 0.0;
                    if formulas.is_empty()
                        && !text.contains("<b>")
                        && !text.contains("<i>")
                        && literal_rules.is_empty()
                    {
                        // A small translator can copy the language names or
                        // error examples in a long correction prompt. Plain
                        // prose needs only a concise, positive instruction.
                        body.messages[0].content = if attempt > 0
                            && options.target_lang.to_ascii_lowercase().starts_with("zh")
                        {
                            "请将用户消息完整翻译为中文，译文全部使用汉字和原文专有名词。".into()
                        } else {
                            format!(
                                "Translate the complete user text into {target}. \
                                 Preserve names, numbers, citations and URLs. Output only the translation."
                            )
                        };
                    } else {
                        body.messages[0].content.push_str(&format!(
                        "\nThe previous response failed validation: {issue}. \
                         Translate all ordinary prose into {target}, including isolated connecting words \
                         next to formulas. Do not introduce words in an unrelated language. \
                         Preserve only source names, technical identifiers, citations and bound markers. \
                         Return the complete corrected translation."
                        ));
                    }
                } else {
                    return Ok(output);
                }
            }
            Ok(Reply::Truncated) => {
                if let Some(chunks) = translation_chunks(text, 600) {
                    // Repeating a long request with a larger token budget can
                    // amplify a local model's repetition loop. Retry smaller
                    // sentence/word-boundary chunks with their own bound IDs.
                    let mut translated = Vec::new();
                    for chunk in chunks {
                        let ids = pieces(&chunk)
                            .filter_map(|piece| match piece {
                                Piece::Formula(id) => Some(id.to_owned()),
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        translated.push(chat_completion(client, options, &chunk, &ids, context)?);
                    }
                    if let Some(output) =
                        validate_translation(text, &translated.join(" "), formulas)
                    {
                        return Ok(output);
                    }
                }
                last_error = anyhow!("API truncated the translation (finish_reason=length)");
                body.max_tokens = body.max_tokens.saturating_mul(2);
            }
            Ok(Reply::InvalidMarkup) => {
                invalid_content = true;
                last_error = anyhow!("translation did not preserve formula or literal markers");
                let markers = formulas
                    .iter()
                    .map(|id| format!("<{id}>"))
                    .collect::<Vec<_>>()
                    .join(" ");
                body.temperature = 0.0;
                body.messages[0].content.push_str(&format!(
                    "\nRequired formula markers: {markers}. Preserve every source emphasis span. \
                     Close each emphasis tag at the end of the corresponding translated source words; \
                     keep all originally plain prose outside it. Do not add emphasis or omit any words. \
                     Return the complete translation without explanations."
                ));
            }
            Err(error) => last_error = error,
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(500 * (1 << attempt)));
        }
    }

    if invalid_content {
        // Retry complete sentences before isolating words between markers.
        // Definitions need their verbs and neighboring nouns to retain meaning.
        if let Some(chunks) = sentence_chunks(text) {
            let translated = chunks
                .iter()
                .map(|chunk| {
                    let ids = pieces(chunk)
                        .filter_map(|piece| match piece {
                            Piece::Formula(id) => Some(id.to_owned()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    chat_completion(client, options, chunk, &ids, context)
                })
                .collect::<Result<Vec<_>>>();
            if let Ok(translated) = translated
                && let Some(output) = validate_translation(text, &translated.join(" "), formulas)
            {
                return Ok(output);
            }
        }
    }
    if invalid_content && pieces(text).any(|piece| !matches!(piece, Piece::Text(_))) {
        // Some local models repeatedly drop or duplicate placeholders. As a
        // bounded fallback, translate the prose between them and reinsert the
        // original objects ourselves; never discard a formula to accept a reply.
        let translated = translate_around_formulas(text, |part| {
            // A paper title can overwhelm a tiny fragment such as "A system of
            // distinct". These fragments need their own text, not title content.
            let fragment_context = if part.chars().count() < 80 {
                context
                    .lines()
                    .filter(|line| !line.starts_with("Paper title:"))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                context.to_owned()
            };
            let term = part.trim();
            if term.chars().count() >= 4 && term.chars().all(char::is_alphabetic) {
                let term_context = format!(
                    "{fragment_context}\nEnclosing source sentence (context only; translate only \
                     the single user term, without adding other words from this sentence): {}",
                    crate::markup::strip_style_tags(text)
                );
                if let Ok(output) = chat_completion(client, options, part, &[], &term_context)
                    && concise_term_translation(term, &output, &options.target_lang)
                {
                    return Ok(output);
                }
            }
            chat_completion(client, options, part, &[], &fragment_context)
        })?;
        if let Some(translated) = validate_translation(text, &translated, formulas) {
            return Ok(translated);
        }
    }

    Err(last_error)
}

fn concise_term_translation(source: &str, output: &str, target: &str) -> bool {
    let letters = output.chars().filter(|ch| ch.is_alphanumeric()).count();
    let limit = source.chars().count().max(6);
    letters > 0
        && letters
            <= if target.to_ascii_lowercase().starts_with("zh") {
                limit
            } else {
                limit * 2
            }
}

fn sentence_chunks(text: &str) -> Option<Vec<String>> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut style = crate::model::TextStyle::default();
    let mut words = text.split_inclusive(char::is_whitespace).peekable();
    while let Some(word) = words.next() {
        current.push_str(word);
        for piece in pieces(word) {
            match piece {
                Piece::Bold(on) => style.bold = on,
                Piece::Italic(on) => style.italic = on,
                Piece::Color(color) => style.color = color,
                _ => {}
            }
        }
        let visible = crate::markup::strip_style_tags(word);
        let starts_sentence = words.peek().is_some_and(|next| {
            let plain = crate::markup::strip_style_tags(next);
            plain.starts_with("<f") || plain.starts_with(char::is_uppercase)
        });
        if !style.bold
            && !style.italic
            && style.color.is_none()
            && current.chars().count() >= 100
            && visible.trim_end().ends_with(['.', '!', '?'])
            && starts_sentence
        {
            chunks.push(std::mem::take(&mut current).trim().to_owned());
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim().to_owned());
    }
    (chunks.len() > 1).then_some(chunks)
}

/// Conservative checks for Chinese prose, not a general semantic quality score.
/// Names, identifiers and citations can legitimately contain other languages.
pub(super) fn language_issue(source: &str, output: &str, target: &str) -> Option<String> {
    if !target.to_ascii_lowercase().starts_with("zh") {
        return None;
    }
    for piece in pieces(output) {
        let Piece::Text(part) = piece else { continue };
        for ch in part.chars().filter(|ch| ch.is_alphabetic()) {
            if ch.is_ascii() || matches!(ch, '\u{3005}'..='\u{3007}') || source.contains(ch) {
                continue;
            }
            // Font coverage includes kana and Hangul; it is not a Chinese
            // language test. Accept Han ideographs, not every CJK script.
            if let Some(block) = unicode_blocks::find_unicode_block(ch)
                && ![
                    "Latin",
                    "Greek",
                    "Mathematical",
                    "Letterlike",
                    "CJK Unified Ideographs",
                    "CJK Compatibility Ideographs",
                    "Bopomofo",
                ]
                .iter()
                .any(|name| block.name().contains(name))
            {
                return Some(format!(
                    "introduced {} letters absent from the source",
                    block.name()
                ));
            }
        }
        // Only flag isolated function words. Adjacent Latin words can be names
        // ("Jacovi and Goldberg"), titles or terms; never rewrite them blindly.
        for word in
            part.split(|ch: char| !(ch.is_ascii_alphanumeric() || "_-./:@+&;#%=?".contains(ch)))
        {
            let word = word.trim_matches(['.', ':', ';']);
            if source.contains("<f")
                && word.len() == 1
                && word.chars().all(|ch| ch.is_ascii_alphabetic())
                && !source
                    .split(|ch: char| !ch.is_ascii_alphanumeric())
                    .any(|original| original == word)
            {
                return Some(format!(
                    "invented variable '{word}' outside the bound formula markers"
                ));
            }
            if !matches!(
                word,
                "and"
                    | "or"
                    | "but"
                    | "the"
                    | "which"
                    | "that"
                    | "with"
                    | "without"
                    | "where"
                    | "then"
                    | "therefore"
                    | "whereas"
                    | "because"
                    | "although"
                    | "unless"
                    | "hence"
                    | "thus"
                    | "moreover"
                    | "however"
                    | "otherwise"
                    | "instead"
                    | "respectively"
                    | "only"
                    | "also"
                    | "both"
                    | "either"
                    | "neither"
            ) {
                continue;
            }
            if ['`', '\'', '"']
                .iter()
                .any(|quote| source.contains(&format!("{quote}{word}{quote}")))
                || source.contains(&format!("{word}("))
            {
                continue;
            }
            for (offset, _) in part.match_indices(word) {
                let punctuation = |ch: char| ch.is_whitespace() || ch.is_ascii_punctuation();
                let before = part[..offset]
                    .trim_end_matches(punctuation)
                    .chars()
                    .next_back();
                let after = part[offset + word.len()..]
                    .trim_start_matches(punctuation)
                    .chars()
                    .next();
                if !before.is_some_and(|ch| ch.is_ascii_alphanumeric())
                    && !after.is_some_and(|ch| ch.is_ascii_alphanumeric())
                {
                    return Some(format!(
                        "ordinary connecting word '{word}' remains untranslated"
                    ));
                }
            }
        }
    }
    None
}

fn translation_chunks(text: &str, limit: usize) -> Option<Vec<String>> {
    if text.chars().count() <= limit {
        return None;
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    let mut bold = false;
    let mut italic = false;
    let mut color = None;
    for word in text.split_inclusive(char::is_whitespace) {
        let length = word.chars().count();
        if count > 0 && count + length > limit && !bold && !italic && color.is_none() {
            chunks.push(std::mem::take(&mut current).trim().to_owned());
            count = 0;
        }
        current.push_str(word);
        count += length;
        for piece in pieces(word) {
            match piece {
                Piece::Bold(on) => bold = on,
                Piece::Italic(on) => italic = on,
                Piece::Color(next) => color = next,
                _ => {}
            }
        }
        if count >= limit / 2
            && !bold
            && !italic
            && color.is_none()
            && word.trim_end().ends_with(['.', '!', '?'])
        {
            chunks.push(std::mem::take(&mut current).trim().to_owned());
            count = 0;
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim().to_owned());
    }
    (chunks.len() > 1).then_some(chunks)
}

fn translate_around_formulas(
    text: &str,
    mut translate: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    let mut output = String::new();
    for piece in pieces(text) {
        match piece {
            Piece::Formula(id) => output.push_str(&format!("<{id}>")),
            Piece::Text(part) => {
                if part.chars().any(char::is_alphabetic) {
                    output.push_str(&translate(part)?);
                } else {
                    output.push_str(part);
                }
            }
            Piece::Bold(on) => output.push_str(if on { "<b>" } else { "</b>" }),
            Piece::Italic(on) => output.push_str(if on { "<i>" } else { "</i>" }),
            Piece::Color(color) => output.push_str(&crate::markup::color_tag(color)),
        }
    }
    Ok(output)
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
    if looks_like_prompt_echo(&text) {
        return Ok(Reply::InvalidMarkup);
    }
    Ok(match validate_translation(source, &text, formulas) {
        Some(text) => Reply::Complete(text),
        None => {
            tracing::debug!(source, output = %text, "translation markup validation failed");
            Reply::InvalidMarkup
        }
    })
}

fn looks_like_prompt_echo(text: &str) -> bool {
    // Small local models occasionally repeat the instruction block instead of
    // translating. Treat that as a failed attempt so the retry prompt can be
    // used instead of writing the instructions into the paper.
    const PHRASES: &[&str] = &[
        "行内公式和受保护对象",
        "每个占位符必须原样出现",
        "行业通用公式",
        "只输出译文",
        "不要重复本说明",
        "Translate only the text between",
        "Keep every placeholder",
        "Output only the translation",
    ];
    PHRASES.iter().any(|phrase| text.contains(phrase))
}

fn clean_translation(text: &str) -> String {
    let text = text.trim();
    // Small models sometimes copy the <<< / >>> delimiters into the answer.
    let text = text
        .strip_prefix("<<<")
        .map(str::trim_start)
        .unwrap_or(text);
    let text = text.strip_suffix(">>>").map(str::trim_end).unwrap_or(text);
    let text = text.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .or_else(|| {
            text.strip_prefix('“')
                .and_then(|inner| inner.strip_suffix('”'))
        })
        .unwrap_or(text);
    text.replace("<<<", "")
        .replace(">>>", "")
        .trim()
        .trim_start_matches("<<")
        .trim_end_matches(">>")
        .trim()
        .to_string()
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
    fn chinese_translation_rejects_unrelated_scripts_and_isolated_connectives() {
        for (source, output) in [
            ("The multiplicity is one.", "多重性为หนึ่ง。"),
            ("Warm-up via policy iteration", "政策迭代による热身"),
            ("Use the model.", "使用モデル。"),
            ("Use the model.", "使用ﾓﾃﾞﾙ。"),
            ("Use the model.", "使用모델。"),
            ("Use <f1> instead.", "使用<f1> instead。"),
            ("Hence <f1> and <f2>.", "因此<f1> and <f2>。"),
            ("Otherwise, use the result.", "otherwise，使用结果。"),
            (
                "Use only <f1> under answeronly supervision.",
                "仅回答only监督下使用<f1>。",
            ),
            ("Use both <f1> and <f2>.", "使用both<f1>和<f2>。"),
            ("Use <f1> also.", "使用<f1> also。"),
            ("<f1> and <f2> are rows.", "X <f1>和<f2>是行。"),
        ] {
            assert!(
                language_issue(source, output, "zh-CN").is_some(),
                "{output}"
            );
        }
        assert!(language_issue("Use <f1> instead.", "改用<f1>。", "zh-CN").is_none());
        assert!(language_issue("Use this instead.", "Use this instead.", "en").is_none());
    }

    #[test]
    fn language_check_keeps_names_citations_identifiers_and_source_scripts() {
        for (source, output) in [
            (
                "Jacovi and Goldberg (2020)",
                "参见 Jacovi and Goldberg (2020)。",
            ),
            ("The TCP/IP and GPT-4 systems.", "TCP/IP 和 GPT-4 系统。"),
            (
                "Use `and` and then instead_of().",
                "使用 `and`，然后调用 instead_of()。",
            ),
            (
                "See https://example.org/where.",
                "参见 https://example.org/where。",
            ),
            ("The &lt;where&gt; tag.", "&lt;where&gt; 标签。"),
            ("The word หนึ่ง means one.", "单词 หนึ่ง 表示一。"),
            ("The name モデル is quoted.", "引用名称 モデル。"),
            ("The name 모델 is quoted.", "引用名称 모델。"),
            ("An ideograph example.", "汉字示例：\u{31350}。"),
            ("The alpha coefficient of Muller.", "Müller 的 α 系数。"),
            ("In case A use <f1>.", "在情况 A 下使用<f1>。"),
            ("The answer-only setting.", "answer-only 设置。"),
        ] {
            assert!(
                language_issue(source, output, "zh-CN").is_none(),
                "{output}"
            );
        }
    }

    #[test]
    fn truncation_chunks_preserve_bound_objects_and_literal_markup() {
        let text = format!(
            "<b>{}</b> Using <f1> and &lt;bot&gt;. {} Finally <f2>.",
            "A sentence. ".repeat(45),
            "Another sentence. ".repeat(25)
        );
        let chunks = translation_chunks(&text, 600).unwrap();
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 600));
        assert!(
            validate_translation(&text, &chunks.join(" "), &["f1".into(), "f2".into()]).is_some()
        );
        assert!(translation_chunks("One short sentence.", 600).is_none());
        assert!(translation_chunks(&"x".repeat(700), 600).is_none());
    }

    #[test]
    fn sentence_retry_preserves_complete_definitions_and_emphasis() {
        let definition = "An <i>assignment</i> maps variables to truth values and satisfies <f1> when the value of every clause is true.";
        let second = "A formula is <i>satisfiable</i> when some assignment satisfies <f2>.";
        let source = format!("{definition} {second}");
        let chunks = sentence_chunks(&source).unwrap();
        assert_eq!(chunks, [definition, second]);
        assert!(
            validate_translation(&source, &chunks.join(" "), &["f1".into(), "f2".into()]).is_some()
        );
        assert!(sentence_chunks(&format!("<b>{source}</b>")).is_none());
        assert!(sentence_chunks("The estimate is 1.25 and uses <f1>.").is_none());
        assert!(concise_term_translation("assignment", "赋值", "zh-CN"));
        assert!(!concise_term_translation(
            "assignment",
            "一个赋值是从变量集合到真值的函数。",
            "zh-CN"
        ));
    }

    #[test]
    fn fallback_preserves_every_formula_without_asking_the_model_to_copy_it() {
        let mut seen = Vec::new();
        let output = translate_around_formulas("<b>Using</b> <f1> and <f2>.", |part| {
            seen.push(part.trim().to_string());
            Ok(if part.contains("Using") {
                "使用"
            } else {
                "和"
            }
            .into())
        })
        .unwrap();
        assert_eq!(output, "<b>使用</b> <f1>和<f2>.");
        assert_eq!(seen, ["Using", "and"]);
        assert!(
            validate_translation(
                "<b>Using</b> <f1> and <f2>.",
                &output,
                &["f1".into(), "f2".into()]
            )
            .is_some()
        );
    }

    #[test]
    fn color_spans_survive_fragment_fallback_and_chunk_boundaries() {
        let source = "<c00008cff><b>Smith</b></c> reports <f1>.";
        let output = translate_around_formulas(source, |part| {
            Ok(if part == "Smith" { "Smith" } else { "报告" }.into())
        })
        .unwrap();
        assert_eq!(output, "<c00008cff><b>Smith</b></c>报告<f1>.");
        assert!(validate_translation(source, &output, &["f1".into()]).is_some());
        let source = format!(
            "<c00008cff>{}</c> {}",
            "A sentence. ".repeat(20),
            "Other prose. ".repeat(20)
        );
        for chunks in [
            translation_chunks(&source, 300).unwrap(),
            sentence_chunks(&source).unwrap(),
        ] {
            assert!(chunks[0].contains("</c>"));
            assert!(validate_translation(&source, &chunks.join(" "), &[]).is_some());
        }
    }

    #[test]
    fn prompt_delimiters_are_removed_without_damaging_formula_tags() {
        assert_eq!(clean_translation("<<<引言 >>>"), "引言");
        assert_eq!(clean_translation("<f1>公式<f2>"), "<f1>公式<f2>");
        assert_eq!(clean_translation("引言 >>"), "引言");
        assert_eq!(clean_translation("其中 >>> 的作用。"), "其中  的作用。");
    }

    #[test]
    fn fallback_translates_single_letter_articles_and_pronouns() {
        let mut seen = Vec::new();
        let output = translate_around_formulas("A <i>proof</i>.", |part| {
            seen.push(part.trim().to_owned());
            Ok(if part.trim() == "A" {
                "一个"
            } else {
                "证明"
            }
            .into())
        })
        .unwrap();
        assert_eq!(seen, ["A", "proof"]);
        assert_eq!(output, "一个<i>证明</i>.");
    }

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
        for bad in ["<f1>", "<f1><f1><f2>", "<f1><f2><f3>"] {
            assert!(matches!(
                decode_reply(reply("stop", bad), "Using <f1> and <f2>", &formulas).unwrap(),
                Reply::InvalidMarkup
            ));
        }
        assert!(matches!(
            decode_reply(
                reply("stop", "先 <f2> 后 <f1>"),
                "Using <f1> and <f2>",
                &formulas
            )
            .unwrap(),
            Reply::Complete(_)
        ));
        assert!(matches!(
            decode_reply(
                reply("stop", "字面 <f1> 和 <f2>"),
                "Literal &lt;f1&gt; with <f2>",
                &["f2".into()]
            )
            .unwrap(),
            Reply::Complete(_)
        ));
        assert_eq!(clean_translation("<<<\n译文\n>>>"), "译文");
        assert_eq!(clean_translation("<<<“译文”>>>"), "译文");
    }
}
