//! Cell values as they come back from any driver.

use std::borrow::Cow;
use std::fmt::Write as _;

/// A single cell value. Drivers convert their native types into this compact enum;
/// anything without a natural Rust representation (timestamps, intervals, arrays, JSON…)
/// is kept as the server's own text rendering, which is exactly what users expect to see.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    /// Exact numerics (NUMERIC / DECIMAL) are kept verbatim to avoid precision loss.
    Decimal(Box<str>),
    Text(Box<str>),
    Bytes(Box<[u8]>),
}

/// Maximum number of characters rendered for a single grid cell.
const DISPLAY_LIMIT: usize = 256;

impl Value {
    pub fn text(s: impl Into<Box<str>>) -> Self {
        Value::Text(s.into())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Numbers are right-aligned in the grid.
    pub fn is_numeric(&self) -> bool {
        matches!(self, Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_))
    }

    /// Single-line text for a grid cell. Long values are truncated and newlines are shown
    /// as `↵` so every row keeps a fixed height.
    pub fn display(&self) -> Cow<'_, str> {
        match self {
            Value::Null => Cow::Borrowed("NULL"),
            Value::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
            Value::Int(i) => Cow::Owned(i.to_string()),
            Value::UInt(u) => Cow::Owned(u.to_string()),
            Value::Float(f) => Cow::Owned(format_float(*f)),
            Value::Decimal(s) => Cow::Borrowed(s),
            Value::Text(s) => single_line(s),
            Value::Bytes(b) => {
                let shown = &b[..b.len().min(DISPLAY_LIMIT / 2)];
                let mut out = hex_string(shown);
                if shown.len() < b.len() {
                    out.push('…');
                }
                Cow::Owned(out)
            }
        }
    }

    /// Full-fidelity text used for editing, copying and CSV export. `None` for NULL.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Text(s) | Value::Decimal(s) => Some(s.to_string()),
            Value::Bytes(b) => Some(hex_string(b)),
            Value::Float(f) => Some(format_float(*f)),
            other => Some(other.display().into_owned()),
        }
    }

    /// Approximate heap + inline size, used for memory accounting in tests.
    pub fn approx_size(&self) -> usize {
        std::mem::size_of::<Value>()
            + match self {
                Value::Text(s) | Value::Decimal(s) => s.len(),
                Value::Bytes(b) => b.len(),
                _ => 0,
            }
    }
}

pub fn format_float(f: f64) -> String {
    if f.is_finite() && f == f.trunc() && f.abs() < 1e15 { format!("{f:.1}") } else { f.to_string() }
}

/// `0xDEADBEEF` style rendering for binary data.
pub fn hex_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for b in bytes {
        let _ = write!(out, "{b:02X}");
    }
    out
}

/// Parses `0xABCD` / `\xabcd` / `X'ABCD'` style hex into bytes.
pub fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let t = text.trim();
    let digits = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .or_else(|| t.strip_prefix("\\x"))
        .or_else(|| t.strip_prefix("X'").or_else(|| t.strip_prefix("x'")).and_then(|r| r.strip_suffix('\'')))?;
    if digits.len() % 2 != 0 {
        return None;
    }
    (0..digits.len()).step_by(2).map(|i| u8::from_str_radix(digits.get(i..i + 2)?, 16).ok()).collect()
}

fn single_line(s: &str) -> Cow<'_, str> {
    let needs_trunc = s.len() > DISPLAY_LIMIT;
    let needs_nl = s.contains(['\n', '\r', '\t']);
    if !needs_trunc && !needs_nl {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len().min(DISPLAY_LIMIT + 4));
    for (i, c) in s.chars().enumerate() {
        if i >= DISPLAY_LIMIT {
            out.push('…');
            break;
        }
        match c {
            '\n' => out.push('↵'),
            '\r' => {}
            '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_text() {
        assert_eq!(Value::Null.display(), "NULL");
        assert_eq!(Value::Null.to_text(), None);
        assert_eq!(Value::Int(-5).display(), "-5");
        assert_eq!(Value::Float(2.0).display(), "2.0");
        assert_eq!(Value::Float(2.5).display(), "2.5");
        assert_eq!(Value::text("a\nb").display(), "a↵b");
        assert_eq!(Value::text("a\nb").to_text().unwrap(), "a\nb");
        assert_eq!(Value::Bytes(vec![0xde, 0xad].into()).display(), "0xDEAD");
        let long = "x".repeat(1000);
        assert!(Value::text(long.as_str()).display().chars().count() <= DISPLAY_LIMIT + 1);
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(parse_hex("0xDEADbeef"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(parse_hex("\\x00ff"), Some(vec![0, 255]));
        assert_eq!(parse_hex("X'0A'"), Some(vec![10]));
        assert_eq!(parse_hex("0x0"), None);
        assert_eq!(parse_hex("hello"), None);
        assert_eq!(hex_string(&[1, 2]), "0x0102");
    }
}
