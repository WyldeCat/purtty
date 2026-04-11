//! URL detection within a row of terminal text.
//!
//! Scans a string for simple http/https/file URLs and returns byte-index
//! ranges. We don't use a full URL parser — terminal text is messy
//! (trailing punctuation, wrapping, ANSI codes already stripped by the
//! grid). The ranges are good enough for hit-testing on Cmd+click.
//!
//! Detection rules:
//! - URL must start with `http://`, `https://`, or `file://`.
//! - Extends until whitespace or an ASCII control character.
//! - Trailing `.`, `,`, `;`, `:`, `!`, `?`, `)`, `]`, `}`, `'`, `"`, `>`
//!   are stripped (common sentence-ending punctuation).
//! - Balanced parens are allowed inside the URL (wikipedia links etc.).

const SCHEMES: &[&str] = &["https://", "http://", "file://"];
const TRAILING_PUNCT: &[char] = &[
    '.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"', '>',
];

/// Return byte-index ranges of URLs found in `text`.
pub fn find_urls(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Try to match any scheme at position i.
        let Some(scheme) = SCHEMES.iter().find(|s| text[i..].starts_with(*s)) else {
            i += 1;
            continue;
        };
        let start = i;
        let mut end = start + scheme.len();
        let mut paren_depth = 0i32;
        while end < bytes.len() {
            let b = bytes[end];
            if b.is_ascii_whitespace() || b < 0x20 {
                break;
            }
            if b == b'(' {
                paren_depth += 1;
            } else if b == b')' {
                if paren_depth == 0 {
                    break;
                }
                paren_depth -= 1;
            }
            end += 1;
        }
        // Trim trailing punctuation, but stop at the scheme end (can't
        // chew back into the scheme itself). If the URL contains a `(`,
        // don't strip trailing `)` — it's likely balanced and part of
        // the URL (e.g. wikipedia disambiguation links).
        let has_open_paren = text[start..end].contains('(');
        while end > start + scheme.len() {
            let ch = text[..end].chars().last().unwrap();
            if ch == ')' && has_open_paren {
                break;
            }
            if TRAILING_PUNCT.contains(&ch) {
                end -= ch.len_utf8();
            } else {
                break;
            }
        }
        if end > start + scheme.len() {
            out.push(start..end);
        }
        i = end.max(i + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_simple_https_url() {
        let urls = find_urls("Go to https://example.com for info");
        assert_eq!(urls.len(), 1);
        assert_eq!(&"Go to https://example.com for info"[urls[0].clone()], "https://example.com");
    }

    #[test]
    fn finds_multiple_urls_on_one_line() {
        let t = "https://a.com and http://b.org";
        let urls = find_urls(t);
        assert_eq!(urls.len(), 2);
        assert_eq!(&t[urls[0].clone()], "https://a.com");
        assert_eq!(&t[urls[1].clone()], "http://b.org");
    }

    #[test]
    fn strips_trailing_period() {
        let t = "See https://example.com.";
        let urls = find_urls(t);
        assert_eq!(urls.len(), 1);
        assert_eq!(&t[urls[0].clone()], "https://example.com");
    }

    #[test]
    fn strips_trailing_paren() {
        let t = "(link: https://example.com)";
        let urls = find_urls(t);
        assert_eq!(urls.len(), 1);
        assert_eq!(&t[urls[0].clone()], "https://example.com");
    }

    #[test]
    fn keeps_balanced_internal_parens() {
        let t = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        let urls = find_urls(t);
        assert_eq!(urls.len(), 1);
        assert_eq!(&t[urls[0].clone()], t);
    }

    #[test]
    fn handles_query_strings() {
        let t = "visit https://example.com/path?a=1&b=2#frag now";
        let urls = find_urls(t);
        assert_eq!(urls.len(), 1);
        assert_eq!(&t[urls[0].clone()], "https://example.com/path?a=1&b=2#frag");
    }

    #[test]
    fn rejects_bare_scheme() {
        // `https://` by itself has nothing after the scheme — not a URL.
        let urls = find_urls("prefix https:// suffix");
        assert!(urls.is_empty());
    }

    #[test]
    fn no_urls_in_plain_text() {
        let urls = find_urls("hello world, no urls here");
        assert!(urls.is_empty());
    }

    #[test]
    fn file_url_detected() {
        let t = "open file:///etc/hosts please";
        let urls = find_urls(t);
        assert_eq!(urls.len(), 1);
        assert_eq!(&t[urls[0].clone()], "file:///etc/hosts");
    }
}
