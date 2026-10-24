//! Turn an agent's final message into the short text whisrs speaks.
//!
//! The voice instruction asks the model to end with a `<spoken>` reply, but
//! users run different models, some small, so nothing here trusts it:
//!
//! 1. Take the last `<spoken>...</spoken>`. Without one, take the whole reply
//!    and keep its first two sentences.
//! 2. Sanitize: fenced code dropped; inline code of more than three words
//!    dropped, shorter kept as words; `[text](url)` becomes the text; bare
//!    URLs and file paths dropped; markdown markers, list bullets and table
//!    rows stripped; whitespace collapsed.
//! 3. Cap at [`MAX_SPOKEN_CHARS`] on a sentence boundary, so every TTS
//!    backend takes it in one request.
//! 4. Empty after all that: [`EMPTY_REPLY`].
//!
//! Pure functions, no I/O.

use std::sync::LazyLock;

use regex::Regex;

/// The longest reply spoken, in characters.
pub const MAX_SPOKEN_CHARS: usize = 400;

/// What is spoken when nothing speakable is left.
pub const EMPTY_REPLY: &str = "Done.";

/// Sentences kept from a reply without a `<spoken>` block.
const FALLBACK_SENTENCES: usize = 2;

/// Inline code longer than this many words is dropped instead of read.
const INLINE_CODE_MAX_WORDS: usize = 3;

/// The spoken form of an agent's final message.
pub fn spoken_reply(reply: &str) -> String {
    let text = match last_spoken_block(reply) {
        Some(block) => sanitize(block),
        None => first_sentences(&sanitize(&strip_spoken_tags(reply)), FALLBACK_SENTENCES),
    };
    let capped = cap_on_sentence(&text, MAX_SPOKEN_CHARS);
    if capped.trim().is_empty() {
        EMPTY_REPLY.to_string()
    } else {
        capped
    }
}

const OPEN_TAG: &str = "<spoken>";
const CLOSE_TAG: &str = "</spoken>";

/// The content of the last `<spoken>...</spoken>` block (tags matched
/// ASCII-case-insensitively). A last opening tag never closed (a truncated
/// reply) yields everything after it. `None` without an opening tag.
pub fn last_spoken_block(reply: &str) -> Option<&str> {
    // ASCII lowercasing keeps every byte offset valid in `reply`.
    let lower = reply.to_ascii_lowercase();
    let open = lower.rfind(OPEN_TAG)?;
    let start = open + OPEN_TAG.len();
    match lower[start..].find(CLOSE_TAG) {
        Some(len) => Some(&reply[start..start + len]),
        None => {
            // An opening tag after the last close is a truncated block; an
            // opening tag whose close comes later cannot happen (rfind).
            Some(&reply[start..])
        }
    }
}

/// `reply` without any `<spoken>` or `</spoken>` tag.
fn strip_spoken_tags(reply: &str) -> String {
    static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</?spoken>").unwrap());
    TAGS.replace_all(reply, " ").into_owned()
}

/// Strip what a listener cannot use from markdown-ish text. See the module
/// docs for the rules.
pub fn sanitize(text: &str) -> String {
    let mut out = String::new();
    // The paragraph being joined from plain lines.
    let mut paragraph = String::new();
    let mut in_fence = false;

    let flush = |paragraph: &mut String, out: &mut String| {
        let p = finish_clause(paragraph);
        if !p.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&p);
        }
        paragraph.clear();
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            flush(&mut paragraph, &mut out);
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            flush(&mut paragraph, &mut out);
            continue;
        }
        // Table rows and their separators, horizontal rules, setext
        // underlines: nothing to say.
        if trimmed.starts_with('|') || is_rule(trimmed) {
            flush(&mut paragraph, &mut out);
            continue;
        }
        let (body, is_block) = strip_block_marker(trimmed);
        let words = inline(body);
        if words.is_empty() {
            continue;
        }
        if is_block {
            // A heading or a list item is a clause of its own.
            flush(&mut paragraph, &mut out);
            paragraph.push_str(&words);
            flush(&mut paragraph, &mut out);
        } else {
            if !paragraph.is_empty() {
                paragraph.push(' ');
            }
            paragraph.push_str(&words);
        }
    }
    flush(&mut paragraph, &mut out);
    tidy(&out)
}

/// A line of only `-`, `*`, `_` or `=` (three or more): a rule or a setext
/// heading underline.
fn is_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && (compact.chars().all(|c| c == '-')
            || compact.chars().all(|c| c == '*')
            || compact.chars().all(|c| c == '_')
            || compact.chars().all(|c| c == '='))
}

/// Strip a heading, blockquote or list marker. `true` when the line is a
/// heading or a list item.
fn strip_block_marker(line: &str) -> (&str, bool) {
    let mut line = line;
    // Blockquotes, possibly nested.
    while let Some(rest) = line.strip_prefix('>') {
        line = rest.trim_start();
    }
    if line.starts_with('#') {
        let rest = line.trim_start_matches('#');
        if rest.is_empty() || rest.starts_with(' ') {
            return (rest.trim(), true);
        }
    }
    for bullet in ["- ", "* ", "+ ", "\u{2022} "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            // A task list box.
            let rest = rest
                .strip_prefix("[ ] ")
                .or_else(|| rest.strip_prefix("[x] "))
                .or_else(|| rest.strip_prefix("[X] "))
                .unwrap_or(rest);
            return (rest.trim(), true);
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && digits <= 3 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return (rest.trim(), true);
        }
    }
    (line, false)
}

/// Inline markdown of one line, reduced to words.
fn inline(line: &str) -> String {
    static LINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"!?\[([^\]]*)\]\([^)]*\)").unwrap());
    static AUTOLINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<[a-zA-Z][a-zA-Z0-9+.-]*://[^>\s]*>").unwrap());
    static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`+([^`]*)`+").unwrap());
    static HTML_TAG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"</?[a-zA-Z][a-zA-Z0-9-]*[^>]*>").unwrap());

    let s = AUTOLINK.replace_all(line, " ");
    let s = LINK.replace_all(&s, "$1");
    let s = CODE.replace_all(&s, |caps: &regex::Captures| {
        let inner = caps[1].trim();
        if inner.split_whitespace().count() > INLINE_CODE_MAX_WORDS {
            " ".to_string()
        } else {
            inner.to_string()
        }
    });
    let s = HTML_TAG.replace_all(&s, " ");
    let s = s.replace("**", "").replace("~~", "").replace('*', "");

    let mut words: Vec<String> = Vec::new();
    for token in s.split_whitespace() {
        if let Some(kept) = speakable_token(token) {
            words.push(kept);
        }
    }
    words.join(" ")
}

/// A whitespace-separated token as it should be read: `None` when nothing of
/// it is left. A URL or a path is dropped, but the sentence punctuation after
/// it is kept, so "I edited src/main.rs." still ends its sentence.
fn speakable_token(token: &str) -> Option<String> {
    const LEADING: &[char] = &['(', '[', '{', '"', '\'', '\u{201c}', '\u{2018}'];
    const TRAILING: &[char] = &[
        ')', ']', '}', '"', '\'', ',', '.', ';', ':', '!', '?', '\u{201d}', '\u{2019}',
    ];
    let core_start = token.len() - token.trim_start_matches(LEADING).len();
    let core = token[core_start..].trim_end_matches(TRAILING);
    let trailing = &token[core_start + core.len()..];
    if is_url(core) || is_path(core) {
        let punct: String = trailing
            .chars()
            .filter(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'))
            .collect();
        return (!punct.is_empty()).then_some(punct);
    }
    // `_emphasis_` and stray backticks or hashes left by broken markup.
    let cleaned = token.trim_matches(|c| c == '_' || c == '`');
    let cleaned = cleaned.replace('`', "");
    (!cleaned.is_empty()).then_some(cleaned)
}

/// A URL: a scheme (`https://`, `file://`, ...) or a `www.` host.
fn is_url(core: &str) -> bool {
    let lower = core.to_ascii_lowercase();
    lower.contains("://") || lower.starts_with("www.")
}

/// A file path: absolute or relative (`/x`, `~/x`, `./x`, `../x`), Windows
/// style, two or more slashes, or one slash before a file extension
/// (`src/main.rs`). "and/or", "1/2" and "TCP/IP" are words.
fn is_path(core: &str) -> bool {
    if core.is_empty() {
        return false;
    }
    if core.starts_with('/')
        || core.starts_with("~/")
        || core.starts_with("./")
        || core.starts_with("../")
        || core.contains('\\')
    {
        return true;
    }
    let slashes = core.matches('/').count();
    if slashes >= 2 {
        return true;
    }
    if slashes == 1 {
        let last = core.rsplit('/').next().unwrap_or("");
        if let Some((stem, ext)) = last.rsplit_once('.') {
            return !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 5
                && ext.chars().all(|c| c.is_ascii_alphanumeric());
        }
    }
    false
}

/// End a clause with sentence punctuation unless it has some.
fn finish_clause(clause: &str) -> String {
    let c = tidy(clause);
    if c.is_empty() {
        return c;
    }
    let last = c.chars().last().unwrap_or('.');
    if matches!(last, '.' | '!' | '?' | ':' | ';' | ',') {
        if matches!(last, ':' | ';' | ',') {
            // A clause cut at a colon ("Changes:") before a list reads as a
            // sentence end.
            let mut c = c;
            c.pop();
            c.push('.');
            return c;
        }
        c
    } else {
        format!("{c}.")
    }
}

/// Collapse whitespace, and the gaps and doubled marks that dropping words
/// leaves behind.
fn tidy(text: &str) -> String {
    static SPACE_BEFORE_PUNCT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s+([.,;:!?])").unwrap());
    static EMPTY_BRACKETS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\(\s*\)|\[\s*\]").unwrap());
    static DOUBLED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([.,;:!?])[.,;:]+").unwrap());
    static LEADING_PUNCT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\s.,;:]+").unwrap());

    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = EMPTY_BRACKETS.replace_all(&collapsed, "");
    let s = SPACE_BEFORE_PUNCT.replace_all(&s, "$1");
    let s = DOUBLED.replace_all(&s, "$1");
    let s = LEADING_PUNCT.replace(&s, "");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split into sentences: each ends at `.`, `!` or `?` (and any closing quote
/// or bracket after it) followed by whitespace or the end. Text after the
/// last mark is a final sentence.
pub fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (_, c) = chars[i];
        if matches!(c, '.' | '!' | '?') {
            let mut j = i + 1;
            while j < chars.len() && matches!(chars[j].1, '.' | '!' | '?' | '"' | '\'' | ')') {
                j += 1;
            }
            let at_end = j >= chars.len();
            if at_end || chars[j].1.is_whitespace() {
                let end = if at_end { text.len() } else { chars[j].0 };
                let sentence = text[start..end].trim();
                if !sentence.is_empty() {
                    out.push(sentence);
                }
                start = end;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// The first `n` sentences of `text`.
pub fn first_sentences(text: &str, n: usize) -> String {
    sentences(text)
        .into_iter()
        .take(n)
        .collect::<Vec<_>>()
        .join(" ")
}

/// At most `max` characters of `text`, cut after the last sentence that
/// fits. A first sentence longer than `max` is cut at a word boundary and
/// ended with a period.
pub fn cap_on_sentence(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    for sentence in sentences(text) {
        let extra = if out.is_empty() { 0 } else { 1 };
        if out.chars().count() + extra + sentence.chars().count() > max {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(sentence);
    }
    if !out.is_empty() {
        return out;
    }
    // One sentence longer than the cap: cut it at a word boundary.
    let mut cut = String::new();
    for word in text.split_whitespace() {
        let extra = if cut.is_empty() { 0 } else { 1 };
        // Room for the closing period.
        if cut.chars().count() + extra + word.chars().count() + 1 > max {
            break;
        }
        if !cut.is_empty() {
            cut.push(' ');
        }
        cut.push_str(word);
    }
    if cut.is_empty() {
        // A single "word" longer than the cap.
        cut = text.chars().take(max.saturating_sub(1)).collect();
    }
    let cut = cut.trim_end_matches([',', ';', ':', '-']).to_string();
    format!("{cut}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_spoken_block_wins() {
        let reply = "Working.\n<spoken>First draft.</spoken>\nMore notes.\n\
                     <spoken>I fixed the failing test and pushed it.</spoken>\nDetails below.";
        assert_eq!(
            spoken_reply(reply),
            "I fixed the failing test and pushed it."
        );
    }

    #[test]
    fn tags_match_case_insensitively() {
        assert_eq!(spoken_reply("x <SPOKEN>All done.</Spoken> y"), "All done.");
    }

    #[test]
    fn an_unclosed_block_takes_the_rest() {
        assert_eq!(
            last_spoken_block("notes <spoken>It builds now"),
            Some("It builds now")
        );
        // A clause without an end mark gets one, which TTS reads as a stop.
        assert_eq!(
            spoken_reply("notes <spoken>It builds now"),
            "It builds now."
        );
    }

    #[test]
    fn without_a_block_the_first_two_sentences_are_kept() {
        let reply = "I renamed the function. Tests pass. I also updated the docs. Anything else?";
        assert_eq!(spoken_reply(reply), "I renamed the function. Tests pass.");
    }

    #[test]
    fn markdown_without_a_block_is_read_as_sentences() {
        let reply = "## Summary\n\nFixed the **race** in `claim_session`:\n\n\
                     - moved the check\n- added a test\n\n```rust\nfn x() {}\n```\n";
        assert_eq!(
            spoken_reply(reply),
            "Summary. Fixed the race in claim_session."
        );
    }

    #[test]
    fn fenced_code_is_dropped() {
        let text = "Before.\n```bash\nrm -rf target\necho hi\n```\nAfter.";
        assert_eq!(sanitize(text), "Before. After.");
        // An unterminated fence drops the rest.
        assert_eq!(sanitize("Run this:\n~~~\ncargo build\n"), "Run this.");
    }

    #[test]
    fn short_inline_code_is_kept_long_is_dropped() {
        assert_eq!(sanitize("Use `cargo test` here."), "Use cargo test here.");
        assert_eq!(
            sanitize("Run `cargo test --all-targets -- --nocapture` again."),
            "Run again."
        );
    }

    #[test]
    fn links_become_their_text_and_urls_are_dropped() {
        assert_eq!(
            sanitize("See [the docs](https://example.com/docs) or https://x.io/a?b=c for more."),
            "See the docs or for more."
        );
        assert_eq!(sanitize("Visit <https://example.com>."), "Visit.");
        assert_eq!(sanitize("Go to www.example.com now."), "Go to now.");
    }

    #[test]
    fn file_paths_are_dropped_but_keep_the_sentence_end() {
        assert_eq!(
            sanitize("I edited src/daemon/agent.rs and ~/notes.md."),
            "I edited and."
        );
        assert_eq!(sanitize("Changed src/main.rs."), "Changed.");
        assert_eq!(
            sanitize("Wrote /tmp/out, then stopped."),
            "Wrote, then stopped."
        );
        // Words with one slash and no extension stay.
        assert_eq!(
            sanitize("Use and/or TCP/IP, 1/2 of it."),
            "Use and/or TCP/IP, 1/2 of it."
        );
    }

    #[test]
    fn markers_bullets_and_tables_are_stripped() {
        let text = "# Title\n> quoted *text*\n1. first\n2) second\n* third\n\
                    | a | b |\n|---|---|\n| 1 | 2 |\n---\n__done__";
        assert_eq!(
            sanitize(text),
            "Title. quoted text. first. second. third. done."
        );
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(sanitize("  a\t\tb  \n  c  "), "a b c.");
    }

    #[test]
    fn the_cap_cuts_on_a_sentence_boundary() {
        let sentence = "This sentence is exactly fifty characters long ok.";
        assert_eq!(sentence.chars().count(), 50);
        let text = [sentence; 10].join(" ");
        let capped = cap_on_sentence(&text, 400);
        assert!(capped.chars().count() <= 400);
        assert!(capped.ends_with("long ok."));
        // Eight sentences and seven spaces: 407 > 400, so seven fit.
        assert_eq!(sentences(&capped).len(), 7);
        let reply = format!("<spoken>{text}</spoken>");
        assert_eq!(spoken_reply(&reply), capped);
    }

    #[test]
    fn one_long_sentence_is_cut_at_a_word() {
        let text = "word ".repeat(200);
        let capped = cap_on_sentence(&text, 400);
        assert!(capped.chars().count() <= 400, "{}", capped.len());
        assert!(capped.ends_with("word."));
        let unbroken = "x".repeat(500);
        assert_eq!(cap_on_sentence(&unbroken, 400).chars().count(), 400);
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        let text = "Ünïcödé wörds hëre. ".repeat(30);
        let capped = cap_on_sentence(&text, 400);
        assert!(capped.chars().count() <= 400);
        assert!(capped.ends_with('.'));
    }

    #[test]
    fn nothing_speakable_says_done() {
        assert_eq!(spoken_reply(""), EMPTY_REPLY);
        assert_eq!(spoken_reply("<spoken></spoken>"), EMPTY_REPLY);
        assert_eq!(spoken_reply("```\ncode only\n```"), EMPTY_REPLY);
        assert_eq!(
            spoken_reply("<spoken>https://x.io /a/b</spoken>"),
            EMPTY_REPLY
        );
    }

    #[test]
    fn a_spoken_block_is_sanitized_too() {
        assert_eq!(
            spoken_reply("<spoken>**Done.** I changed `main.rs` in src/main.rs.</spoken>"),
            "Done. I changed main.rs in."
        );
    }

    #[test]
    fn sentences_split_on_marks_followed_by_space() {
        assert_eq!(
            sentences("One. Two! Three? Version 1.5 works. Tail"),
            vec!["One.", "Two!", "Three?", "Version 1.5 works.", "Tail"]
        );
        assert_eq!(
            sentences("He said \"stop.\" Then left."),
            vec!["He said \"stop.\"", "Then left."]
        );
    }

    #[test]
    fn a_reply_with_no_tags_and_stray_closing_tag() {
        assert_eq!(spoken_reply("All good.</spoken> Next."), "All good. Next.");
    }
}
