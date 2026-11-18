//! HTML to plain-text extraction tuned for schedule pages.
//!
//! Removes script/style/navigation/footer boilerplate, prefers the main
//! content block, decodes entities and collapses whitespace so the regex and
//! LLM extractors see one compact text stream.

use std::sync::LazyLock;

use regex::Regex;

use crate::util::truncate_chars;

/// Maximum characters returned by to_text.
pub const DEFAULT_TEXT_LIMIT: usize = 200_000;

static COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("valid comment regex"));

static JUNK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?is)<(?:script|style|noscript|svg|template|iframe|nav|footer|header|aside)\b[^>]*>",
        r".*?</(?:script|style|noscript|svg|template|iframe|nav|footer|header|aside)\s*>"
    ))
    .expect("valid junk tag regex")
});

static MAIN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<main\b[^>]*>(.*?)</main\s*>").expect("valid main regex"));

static ARTICLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<article\b[^>]*>(.*?)</article\s*>").expect("valid article regex")
});

static BODY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<body\b[^>]*>(.*?)</body\s*>").expect("valid body regex"));

static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<[^>]{0,800}>").expect("valid tag regex"));

static SPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t\r\n\u{00a0}\u{3000}]+").expect("valid space regex"));

/// Block-level closing tags become line breaks so schedule tables stay rows.
static BLOCK_END_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)</(?:p|div|li|tr|h[1-6]|section|article|dd|dt|table|ul|ol|main|header|footer|aside|figure|figcaption)\s*>",
    )
    .expect("valid block regex")
});

/// Table cells become " | " separators on the row line.
static CELL_END_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</(?:td|th)\s*>").expect("valid cell regex"));

static BR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>").expect("valid br regex"));

/// Convert an HTML document to compact plain text.
pub fn to_text(html: &str) -> String {
    to_text_limited(html, DEFAULT_TEXT_LIMIT)
}

/// Convert HTML to text and truncate to max_chars characters.
pub fn to_text_limited(html: &str, max_chars: usize) -> String {
    if html.trim().is_empty() {
        return String::new();
    }
    let without_comments = COMMENT_RE.replace_all(html, " ");
    let cleaned = JUNK_RE.replace_all(&without_comments, " ");

    // Prefer an explicit main/article block; schedules usually live there.
    let main_content = MAIN_RE
        .captures(&cleaned)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .or_else(|| {
            ARTICLE_RE
                .captures(&cleaned)
                .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        });

    let body = main_content
        .or_else(|| {
            BODY_RE
                .captures(&cleaned)
                .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        })
        .unwrap_or_else(|| cleaned.to_string());

    // Preserve line structure: block ends/newlines split lines, table cells
    // become " | " so a schedule row stays on one line.
    let with_breaks = BLOCK_END_RE.replace_all(&body, "\n");
    let with_cells = CELL_END_RE.replace_all(&with_breaks, " | ");
    let with_brs = BR_RE.replace_all(&with_cells, "\n");
    let no_tags = TAG_RE.replace_all(&with_brs, " ");
    let decoded = decode_entities(&no_tags);
    let mut lines: Vec<String> = Vec::new();
    for line in decoded.lines() {
        let collapsed = SPACE_RE.replace_all(line, " ");
        let trimmed = collapsed.trim();
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
    }
    truncate_chars(&lines.join("\n"), max_chars).to_string()
}

/// Decode the common HTML entities plus numeric references.
pub fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'&' {
            let ch = input[index..].chars().next().expect("valid char");
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }
        let rest = &input[index + 1..];
        let end = rest.find(';').filter(|offset| *offset <= 12);
        let Some(end) = end else {
            out.push('&');
            index += 1;
            continue;
        };
        let entity = &rest[..end];
        let decoded = decode_entity(entity);
        match decoded {
            Some(text) => {
                out.push_str(&text);
                index += end + 2;
            }
            None => {
                out.push('&');
                index += 1;
            }
        }
    }
    out
}

fn decode_entity(entity: &str) -> Option<String> {
    if let Some(number) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        let code = u32::from_str_radix(number, 16).ok()?;
        return char::from_u32(code).map(|ch| ch.to_string());
    }
    if let Some(number) = entity.strip_prefix('#') {
        let code = number.parse::<u32>().ok()?;
        return char::from_u32(code).map(|ch| ch.to_string());
    }
    let text = match entity {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" | "#39" => "'",
        "nbsp" => " ",
        "hellip" => "...",
        "mdash" => "-",
        "ndash" => "-",
        "lsquo" | "rsquo" => "'",
        "ldquo" | "rdquo" => "\"",
        "times" => "x",
        "middot" => ".",
        "copy" => "(c)",
        "reg" => "(r)",
        "trade" => "(tm)",
        "yen" => "JPY",
        _ => return None,
    };
    Some(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_scripts_and_boilerplate() {
        let html = r#"<html><head>
            <script>var hidden = "毎週月曜 23:30";</script>
            <style>.x { color: red }</style>
            </head><body>
            <nav>home | about</nav>
            <main><h1>放送情報</h1><p>毎週土曜日 23:30 より</p></main>
            <footer>copyright</footer>
            </body></html>"#;
        let text = to_text(html);
        assert!(text.contains("毎週土曜日 23:30"));
        assert!(!text.contains("hidden"));
        assert!(!text.contains("color"));
        assert!(!text.contains("home"));
        assert!(!text.contains("copyright"));
    }

    #[test]
    fn prefers_main_over_sidebar() {
        let html = r#"<html><body>
            <div class="sidebar">OTHER SHOW 毎週月曜 01:00</div>
            <main>MY SHOW 毎週火曜 22:30</main>
            </body></html>"#;
        let text = to_text(html);
        assert!(text.contains("MY SHOW"));
        assert!(!text.contains("OTHER SHOW"));
    }

    #[test]
    fn falls_back_to_article_then_body() {
        let article_html =
            "<html><body><div>sidebar</div><article>article 毎週水曜 21:00</article></body></html>";
        let text = to_text(article_html);
        assert!(text.contains("article 毎週水曜 21:00"));
        assert!(!text.contains("sidebar"));

        let body_html = "<html><body><div>body 毎週木曜 20:00</div></body></html>";
        let text = to_text(body_html);
        assert!(text.contains("body 毎週木曜 20:00"));
    }

    #[test]
    fn decodes_entities() {
        let html = "<p>AT&amp;T &#12354;&#x3042; &quot;x&quot; &lt;y&gt; &hellip; &nbsp; end</p>";
        let text = to_text(html);
        assert!(text.contains("AT&T"));
        assert!(text.contains("ああ"));
        assert!(text.contains("\"x\""));
        assert!(text.contains("<y>"));
        assert!(text.contains("..."));
        assert!(text.ends_with("end"));
    }

    #[test]
    fn keeps_unknown_entities() {
        assert_eq!(decode_entities("a &unknown; b & c"), "a &unknown; b & c");
    }

    #[test]
    fn collapses_whitespace_and_limits() {
        let html = "<p>a   b\t\tc</p>";
        assert_eq!(to_text(html), "a b c");
        let multiline = "<p>a</p><p>b</p>";
        assert_eq!(to_text(multiline), "a\nb");
        let long = format!("<p>{}</p>", "x".repeat(500));
        let limited = to_text_limited(&long, 10);
        assert_eq!(limited.chars().count(), 10);
    }

    #[test]
    fn keeps_schedule_rows_on_one_line() {
        let html = r#"<table>
            <tr><td>作品A</td><td>毎週土曜 23:30</td></tr>
            <tr><td>作品B</td><td>毎週日曜 01:00</td></tr>
        </table>"#;
        let text = to_text(html);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "rows must stay separate: {text:?}");
        assert!(lines[0].contains("作品A"));
        assert!(lines[0].contains("23:30"));
        assert!(lines[1].contains("作品B"));
    }

    #[test]
    fn handles_empty_and_garbage() {
        assert_eq!(to_text(""), "");
        assert_eq!(to_text("   "), "");
        assert_eq!(to_text("plain text only"), "plain text only");
    }
}
