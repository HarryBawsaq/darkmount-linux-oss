//! Minimal parser for Valve's text KeyValues format, as used by
//! `libraryfolders.vdf` and `appmanifest_*.acf`.

use anyhow::{bail, Result};

/// Deepest nesting accepted, so a malformed file cannot exhaust the stack.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Obj(Vec<(String, Value)>),
}

impl Value {
    /// Looks up a key in an object. Steam is inconsistent about key case
    /// between versions, so the match is case-insensitive.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries()
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            Value::Obj(_) => None,
        }
    }

    /// The key/value pairs of an object, in file order. Empty for a string.
    pub fn entries(&self) -> &[(String, Value)] {
        match self {
            Value::Obj(entries) => entries,
            Value::Str(_) => &[],
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    Str(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = input.trim_start_matches('\u{feff}').chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '{' => tokens.push(Token::Open),
            '}' => tokens.push(Token::Close),
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => bail!("unterminated quoted string"),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            None => bail!("unterminated quoted string"),
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(c @ ('\\' | '"')) => s.push(c),
                            Some(other) => {
                                s.push('\\');
                                s.push(other);
                            }
                        },
                        Some(c) => s.push(c),
                    }
                }
                tokens.push(Token::Str(s));
            }
            first => {
                let mut s = String::from(first);
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || matches!(c, '{' | '}' | '"') {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                // Platform conditionals such as [$WIN32] carry no data.
                if !(s.starts_with('[') && s.ends_with(']')) {
                    tokens.push(Token::Str(s));
                }
            }
        }
    }
    Ok(tokens)
}

fn parse_object(
    tokens: &mut std::vec::IntoIter<Token>,
    depth: usize,
) -> Result<Vec<(String, Value)>> {
    if depth > MAX_DEPTH {
        bail!("nesting deeper than {MAX_DEPTH} levels");
    }
    let mut entries = Vec::new();
    loop {
        let key = match tokens.next() {
            None if depth == 0 => return Ok(entries),
            None => bail!("unexpected end of input: missing '}}'"),
            Some(Token::Close) if depth > 0 => return Ok(entries),
            Some(Token::Close) => bail!("unexpected '}}'"),
            Some(Token::Open) => bail!("unexpected '{{': expected a key"),
            Some(Token::Str(key)) => key,
        };
        let value = match tokens.next() {
            Some(Token::Str(s)) => Value::Str(s),
            Some(Token::Open) => Value::Obj(parse_object(tokens, depth + 1)?),
            Some(Token::Close) | None => bail!("key {key:?} has no value"),
        };
        entries.push((key, value));
    }
}

/// Parses a document and returns its top-level pairs as an object.
pub fn parse(input: &str) -> Result<Value> {
    let mut tokens = tokenize(input)?.into_iter();
    Ok(Value::Obj(parse_object(&mut tokens, 0)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_objects_and_strings() {
        let doc = parse(
            r#"
            "root"
            {
                "name"      "value"
                "child"
                {
                    "a"     "1"
                    "b"     "2"
                }
            }
            "#,
        )
        .unwrap();
        let root = doc.get("root").unwrap();
        assert_eq!(root.get("name").and_then(Value::as_str), Some("value"));
        let child = root.get("child").unwrap();
        assert_eq!(child.entries().len(), 2);
        assert_eq!(child.get("b").and_then(Value::as_str), Some("2"));
        assert_eq!(root.get("missing"), None);
    }

    #[test]
    fn handles_escapes_comments_and_unquoted_tokens() {
        let doc = parse(
            "// header comment\n\
             root {\n\
               \"path\" \"C:\\\\Games\\\\Steam\" // trailing comment\n\
               \"quote\" \"say \\\"hi\\\"\"\n\
               bare value [$WIN32]\n\
             }\n",
        )
        .unwrap();
        let root = doc.get("root").unwrap();
        assert_eq!(
            root.get("path").and_then(Value::as_str),
            Some(r"C:\Games\Steam")
        );
        assert_eq!(
            root.get("quote").and_then(Value::as_str),
            Some(r#"say "hi""#)
        );
        assert_eq!(root.get("bare").and_then(Value::as_str), Some("value"));
        assert_eq!(root.entries().len(), 3);
    }

    #[test]
    fn key_lookup_is_case_insensitive() {
        let doc = parse(r#""AppState" { "Name" "Game" }"#).unwrap();
        let name = doc.get("appstate").and_then(|s| s.get("name"));
        assert_eq!(name.and_then(Value::as_str), Some("Game"));
    }

    #[test]
    fn strips_byte_order_mark_and_accepts_empty_input() {
        assert_eq!(parse("").unwrap(), Value::Obj(vec![]));
        let doc = parse("\u{feff}\"k\" \"v\"").unwrap();
        assert_eq!(doc.get("k").and_then(Value::as_str), Some("v"));
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(parse(r#""root" { "a" "1""#).is_err(), "missing brace");
        assert!(parse(r#""root" { "a" }"#).is_err(), "key without value");
        assert!(parse(r#""a" "unterminated"#).is_err(), "open string");
        assert!(parse("}").is_err(), "stray brace");
        assert!(parse("{ \"a\" \"1\" }").is_err(), "object without key");
        assert!(parse(r#""dangling""#).is_err(), "key at end of input");
    }

    #[test]
    fn rejects_excessive_nesting() {
        let deep = "\"k\" {".repeat(MAX_DEPTH + 2);
        assert!(parse(&deep).is_err());
    }
}
