//! Shared utilities for phase prompt builders / response parsers
//! (task 60).
//!
//! Each phase produces a `(system, user)` prompt pair and a parser
//! that turns the worker's response into a typed plan. Worker
//! responses are JSON, but workers often wrap them in fenced code
//! blocks or surround with prose — these helpers handle all three
//! cases.

/// Tries to deserialize `text` as `T`, with these fallbacks:
///   1. Strict JSON over the trimmed text.
///   2. Contents of the first fenced ```json``` (or just ```...```) block.
///   3. A balanced JSON object or array embedded in surrounding prose.
///
/// Returns `None` if neither path produces valid JSON. Callers that
/// need richer error reporting should parse `text` themselves.
#[must_use]
pub fn parse_response_json<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    if let Ok(v) = serde_json::from_str::<T>(text.trim()) {
        return Some(v);
    }
    if let Some(inner) = extract_fenced_json(text) {
        if let Ok(v) = serde_json::from_str::<T>(inner.trim()) {
            return Some(v);
        }
    }

    for candidate in balanced_json_values(text) {
        if let Ok(v) = serde_json::from_str::<T>(candidate) {
            return Some(v);
        }
    }
    None
}

/// Returns balanced JSON object/array candidates embedded in arbitrary text.
/// String contents and escapes are respected so braces inside JSON strings do
/// not terminate the candidate early.
fn balanced_json_values(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut candidates = Vec::new();

    for (start, byte) in bytes.iter().enumerate() {
        if !matches!(*byte, b'{' | b'[') {
            continue;
        }

        let mut expected_closers = Vec::new();
        let mut in_string = false;
        let mut escaped = false;

        for (offset, current) in bytes[start..].iter().enumerate() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if *current == b'\\' {
                    escaped = true;
                } else if *current == b'"' {
                    in_string = false;
                }
                continue;
            }

            match *current {
                b'"' => in_string = true,
                b'{' => expected_closers.push(b'}'),
                b'[' => expected_closers.push(b']'),
                b'}' | b']' => {
                    if expected_closers.pop() != Some(*current) {
                        break;
                    }
                    if expected_closers.is_empty() {
                        candidates.push(&text[start..start + offset + 1]);
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    candidates
}

/// Extracts the contents of the first ```...``` fenced block.
/// Skips the optional language tag on the opening fence line.
#[must_use]
pub fn extract_fenced_json(text: &str) -> Option<String> {
    let start = text.find("```")?;
    let after = &text[start + 3..];
    let body_start = after.find('\n').map_or(0, |i| i + 1);
    let body = &after[body_start..];
    let end = body.find("```")?;
    Some(body[..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Sample {
        name: String,
        count: u32,
    }

    #[test]
    fn parse_strict_json_works() {
        let s: Sample = parse_response_json(r#"{"name":"x","count":3}"#).unwrap();
        assert_eq!(
            s,
            Sample {
                name: "x".into(),
                count: 3
            }
        );
    }

    #[test]
    fn parse_with_surrounding_whitespace_works() {
        let s: Sample = parse_response_json("  \n{\"name\":\"x\",\"count\":3}\n").unwrap();
        assert_eq!(s.name, "x");
    }

    #[test]
    fn parse_fenced_json_works() {
        let text = "Here is the plan:\n```json\n{\"name\":\"y\",\"count\":7}\n```\nDone.";
        let s: Sample = parse_response_json(text).unwrap();
        assert_eq!(s.name, "y");
        assert_eq!(s.count, 7);
    }

    #[test]
    fn parse_fenced_block_no_lang_tag_works() {
        let text = "```\n{\"name\":\"z\",\"count\":1}\n```";
        let s: Sample = parse_response_json(text).unwrap();
        assert_eq!(s.name, "z");
    }

    #[test]
    fn parse_json_embedded_in_prose_with_trailing_confidence_works() {
        let text = "Here is the result:\n\n{\"name\":\"todo\",\"count\":2}\n\nCONFIDENCE: 0.95";
        let s: Sample = parse_response_json(text).unwrap();
        assert_eq!(s.name, "todo");
        assert_eq!(s.count, 2);
    }

    #[test]
    fn parse_embedded_json_handles_braces_inside_strings() {
        let text = "Answer: {\"name\":\"a { brace } and \\\"quote\\\"\",\"count\":2} Thanks.";
        let s: Sample = parse_response_json(text).unwrap();
        assert_eq!(s.name, "a { brace } and \"quote\"");
    }

    #[test]
    fn parse_returns_none_when_no_json() {
        let s: Option<Sample> = parse_response_json("just words, no json");
        assert!(s.is_none());
    }

    #[test]
    fn extract_fenced_returns_inner_text() {
        let text = "prose\n```json\n{\"a\":1}\n```\nmore";
        assert_eq!(extract_fenced_json(text).unwrap(), "{\"a\":1}");
    }
}
