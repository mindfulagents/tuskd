//! Strict flat frontmatter codec (DECISIONS.md D4).
//!
//! Grammar (line-based, between `---` fences):
//!   doc     := "---\n" (line "\n")* "---\n" body
//!   line    := key ": " value
//!   key     := [A-Za-z_][A-Za-z0-9_]*
//!   value   := scalar | "[" scalar ("," scalar)* "]" | "[]"
//!   scalar  := quoted | bare
//!   quoted  := '"' (escaped chars: \\ \" \n)* '"'
//!   bare    := anything without leading/trailing space that is not ambiguous
//!
//! Rejected with clear errors: nested maps (empty value or `{`-value),
//! duplicate keys, missing fences, malformed lines. Bare scalars parse as
//! i64 then f64 then string; writers quote strings that would be ambiguous.

use crate::error::CoreError;

#[derive(Debug, Clone, PartialEq)]
pub enum FmValue {
    Str(String),
    Int(i64),
    Float(f64),
    List(Vec<String>),
}

impl FmValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FmValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

fn err(msg: impl Into<String>) -> CoreError {
    CoreError::Frontmatter(msg.into())
}

/// Parse a document into ordered (key, value) pairs plus the body.
pub fn parse(text: &str) -> Result<(Vec<(String, FmValue)>, String), CoreError> {
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| err("missing opening '---' fence"))?;
    let mut fields: Vec<(String, FmValue)> = Vec::new();
    let mut pos = 0usize;
    let bytes = rest.as_bytes();
    loop {
        if pos >= bytes.len() {
            return Err(err("missing closing '---' fence"));
        }
        let line_end = rest[pos..]
            .find('\n')
            .map(|i| pos + i)
            .ok_or_else(|| err("missing closing '---' fence"))?;
        let line = &rest[pos..line_end];
        pos = line_end + 1;
        if line == "---" {
            break;
        }
        let (key, raw) = line
            .split_once(':')
            .ok_or_else(|| err(format!("malformed line (no ':'): {line:?}")))?;
        let key = key.trim();
        if key.is_empty()
            || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || key.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(err(format!("invalid key: {key:?}")));
        }
        if fields.iter().any(|(k, _)| k == key) {
            return Err(err(format!("duplicate key: {key}")));
        }
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(err(format!(
                "empty value for {key:?} (nested maps are not supported)"
            )));
        }
        if raw.starts_with('{') {
            return Err(err(format!("inline map for {key:?} is not supported")));
        }
        let value = if raw.starts_with('[') {
            parse_list(raw)?
        } else {
            parse_scalar(raw)?
        };
        fields.push((key.to_string(), value));
    }
    Ok((fields, rest[pos..].to_string()))
}

fn parse_scalar(raw: &str) -> Result<FmValue, CoreError> {
    if raw.starts_with('"') {
        return Ok(FmValue::Str(unquote(raw)?));
    }
    if let Ok(i) = raw.parse::<i64>() {
        return Ok(FmValue::Int(i));
    }
    if let Ok(f) = raw.parse::<f64>() {
        return Ok(FmValue::Float(f));
    }
    Ok(FmValue::Str(raw.to_string()))
}

fn parse_list(raw: &str) -> Result<FmValue, CoreError> {
    let inner = raw
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| err(format!("malformed list: {raw:?}")))?;
    let inner = inner.trim();
    if inner.is_empty() {
        return Ok(FmValue::List(Vec::new()));
    }
    let mut items = Vec::new();
    for part in split_top_level(inner)? {
        let part = part.trim();
        let s = if part.starts_with('"') {
            unquote(part)?
        } else {
            part.to_string()
        };
        items.push(s);
    }
    Ok(FmValue::List(items))
}

/// Split a list body on commas, honoring quoted items.
fn split_top_level(s: &str) -> Result<Vec<String>, CoreError> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_quote {
            cur.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_quote = false;
            }
        } else if c == '"' {
            in_quote = true;
            cur.push(c);
        } else if c == ',' {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if in_quote {
        return Err(err(format!("unterminated quote in list: {s:?}")));
    }
    parts.push(cur);
    Ok(parts)
}

fn unquote(raw: &str) -> Result<String, CoreError> {
    let inner = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| err(format!("malformed quoted string: {raw:?}")))?;
    // Guard against `"a" trailing` style values where strip found the last quote.
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('n') => out.push('\n'),
                other => return Err(err(format!("bad escape \\{other:?} in {raw:?}"))),
            }
        } else if c == '"' {
            return Err(err(format!("stray quote inside quoted string: {raw:?}")));
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn needs_quoting(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    if s.trim() != s {
        return true;
    }
    if s.parse::<i64>().is_ok() || s.parse::<f64>().is_ok() {
        return true;
    }
    s.chars().any(|c| {
        matches!(
            c,
            '"' | '[' | ']' | ',' | ':' | '\\' | '\n' | '\r' | '{' | '#'
        )
    })
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn write_scalar(s: &str) -> String {
    if needs_quoting(s) {
        quote(s)
    } else {
        s.to_string()
    }
}

/// Serialize ordered fields + body into a frontmatter document.
pub fn serialize(fields: &[(String, FmValue)], body: &str) -> String {
    let mut out = String::from("---\n");
    for (k, v) in fields {
        out.push_str(k);
        out.push_str(": ");
        match v {
            FmValue::Str(s) => out.push_str(&write_scalar(s)),
            FmValue::Int(i) => out.push_str(&i.to_string()),
            FmValue::Float(f) => {
                let s = format!("{f}");
                // Ensure floats survive as floats (e.g. 1 -> "1.0").
                if s.parse::<i64>().is_ok() {
                    out.push_str(&format!("{f:.1}"));
                } else {
                    out.push_str(&s);
                }
            }
            FmValue::List(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&write_scalar(item));
                }
                out.push(']');
            }
        }
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_nested_map() {
        let doc = "---\nmeta:\n  inner: 1\n---\nbody";
        let e = parse(doc).unwrap_err();
        assert!(e.to_string().contains("nested maps"));
    }

    #[test]
    fn rejects_duplicate_key() {
        let doc = "---\na: 1\na: 2\n---\n";
        assert!(parse(doc).is_err());
    }

    #[test]
    fn parses_scalars_and_lists() {
        let doc = "---\nid: abc\nn: 7\nf: 0.5\nlist: [a, \"b, c\", 3]\nempty: []\n---\nBODY";
        let (fields, body) = parse(doc).unwrap();
        assert_eq!(body, "BODY");
        assert_eq!(fields[0].1, FmValue::Str("abc".into()));
        assert_eq!(fields[1].1, FmValue::Int(7));
        assert_eq!(fields[2].1, FmValue::Float(0.5));
        assert_eq!(
            fields[3].1,
            FmValue::List(vec!["a".into(), "b, c".into(), "3".into()])
        );
        assert_eq!(fields[4].1, FmValue::List(vec![]));
    }

    #[test]
    fn quoting_roundtrip() {
        for s in [
            "",
            " leading",
            "trailing ",
            "1.0",
            "42",
            "has, comma",
            "has: colon",
            "has \"quotes\" and \\backslash",
            "line\nbreak",
        ] {
            let fields = vec![("k".to_string(), FmValue::Str(s.to_string()))];
            let doc = serialize(&fields, "");
            let (parsed, _) = parse(&doc).unwrap();
            assert_eq!(parsed[0].1, FmValue::Str(s.replace('\r', "\n")), "{s:?}");
        }
    }
}
