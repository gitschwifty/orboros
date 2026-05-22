//! Shared utilities for phase prompt builders / response parsers
//! (task 60).
//!
//! Each phase produces a `(system, user)` prompt pair and a parser
//! that turns the worker's response into a typed plan. Worker
//! responses are JSON, but workers often wrap them in fenced code
//! blocks or surround with prose — these helpers handle all three
//! cases.

/// Tries to deserialize `text` as `T`, with two fallbacks:
///   1. Strict JSON over the trimmed text.
///   2. Contents of the first fenced ```json``` (or just ```...```) block.
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
    None
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
