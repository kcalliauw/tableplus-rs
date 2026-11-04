//! Reading `TabSeparatedWithNamesAndTypes` output: a line of column names, a line of type names,
//! then one line per row. Values are the server's own text, escaped so that a raw tab or newline
//! only ever separates fields and rows.

use crate::model::{ColumnMeta, Row};
use crate::value::Value;

/// How a column's text becomes a [`Value`], read from its ClickHouse type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Int,
    UInt,
    Float,
    /// Exact numbers kept as text: decimals and the 128- and 256-bit integers.
    Decimal,
    Bool,
    Text,
}

impl ValueKind {
    /// The kind for a type as the server spells it, such as `Nullable(UInt64)` or
    /// `LowCardinality(Nullable(String))`.
    pub fn of(type_name: &str) -> Self {
        let mut t = type_name.trim();
        while let Some(inner) =
            t.strip_prefix("Nullable(").or_else(|| t.strip_prefix("LowCardinality(")).and_then(|r| r.strip_suffix(')'))
        {
            t = inner.trim();
        }
        match t.split('(').next().unwrap_or_default() {
            "UInt8" | "UInt16" | "UInt32" | "UInt64" => ValueKind::UInt,
            "Int8" | "Int16" | "Int32" | "Int64" => ValueKind::Int,
            "UInt128" | "UInt256" | "Int128" | "Int256" => ValueKind::Decimal,
            "Decimal" | "Decimal32" | "Decimal64" | "Decimal128" | "Decimal256" => ValueKind::Decimal,
            "Float32" | "Float64" | "BFloat16" => ValueKind::Float,
            "Bool" => ValueKind::Bool,
            _ => ValueKind::Text,
        }
    }

    /// The value of one unescaped field; `None` is NULL. Text that isn't UTF-8 becomes bytes.
    pub fn value(self, field: Option<Vec<u8>>) -> Value {
        let Some(bytes) = field else { return Value::Null };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(e) => return Value::Bytes(e.into_bytes().into()),
        };
        let parsed = match self {
            ValueKind::Int => text.parse().ok().map(Value::Int),
            ValueKind::UInt => text.parse().ok().map(Value::UInt),
            ValueKind::Float => text.parse().ok().map(Value::Float),
            ValueKind::Bool => match text.as_str() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            ValueKind::Decimal => return Value::Decimal(text.into()),
            ValueKind::Text => None,
        };
        parsed.unwrap_or_else(|| Value::Text(text.into()))
    }
}

/// Decodes one field's escapes. `None` for `\N`, which is NULL.
pub fn unescape(field: &[u8]) -> Option<Vec<u8>> {
    if field == b"\\N" {
        return None;
    }
    let mut out = Vec::with_capacity(field.len());
    let mut bytes = field.iter().copied();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match bytes.next() {
            Some(b'a') => out.push(0x07),
            Some(b'b') => out.push(0x08),
            Some(b'f') => out.push(0x0c),
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b't') => out.push(b'\t'),
            Some(b'v') => out.push(0x0b),
            Some(b'0') => out.push(0),
            Some(b'x') => {
                let hex: Vec<u8> = bytes.by_ref().take(2).collect();
                match std::str::from_utf8(&hex).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) if hex.len() == 2 => out.push(byte),
                    _ => {
                        out.extend_from_slice(b"\\x");
                        out.extend_from_slice(&hex);
                    }
                }
            }
            Some(other) => out.push(other),
            None => out.push(b'\\'),
        }
    }
    Some(out)
}

/// The decoded fields of one line, without its newline.
pub fn fields(line: &[u8]) -> Vec<Option<Vec<u8>>> {
    line.split(|b| *b == b'\t').map(unescape).collect()
}

/// Turns a response body, fed in chunks as it arrives, into column metadata and rows.
#[derive(Debug, Default)]
pub struct TsvReader {
    /// Bytes after the last complete line.
    partial: Vec<u8>,
    columns: Vec<ColumnMeta>,
    kinds: Vec<ValueKind>,
    lines_read: usize,
    /// Everything from the first line that isn't a row: the server's exception, written after the
    /// rows it had already sent.
    exception: Option<Vec<u8>>,
}

impl TsvReader {
    /// The columns, once both header lines have arrived.
    pub fn columns(&self) -> Option<&[ColumnMeta]> {
        (self.lines_read >= 2).then_some(self.columns.as_slice())
    }

    /// Reads a chunk, appending each row it completes to `rows`.
    pub fn push(&mut self, chunk: &[u8], rows: &mut Vec<Row>) {
        if let Some(exception) = &mut self.exception {
            exception.extend_from_slice(chunk);
            return;
        }
        self.partial.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(end) = self.partial[start..].iter().position(|b| *b == b'\n').map(|p| start + p) {
            let line = self.partial[start..end].to_vec();
            start = end + 1;
            if !self.line(&line, rows) {
                let mut rest = line;
                rest.push(b'\n');
                rest.extend_from_slice(&self.partial[start..]);
                self.exception = Some(rest);
                self.partial.clear();
                return;
            }
        }
        self.partial.drain(..start);
    }

    /// Ends the body: an error when the server reported one after its rows, or when the body
    /// stopped inside a line.
    pub fn finish(self) -> Result<(), String> {
        if let Some(exception) = self.exception {
            return Err(exception_message(&exception));
        }
        if !self.partial.is_empty() {
            return Err(exception_message(&self.partial));
        }
        if self.lines_read < 2 {
            return Err("The server's response ended before the column names and types.".into());
        }
        Ok(())
    }

    /// Reads one complete line; false when it isn't a header or a row.
    fn line(&mut self, line: &[u8], rows: &mut Vec<Row>) -> bool {
        // TabSeparated escapes every carriage return in a value, so a raw one is the marker
        // around an exception.
        if line.contains(&b'\r') {
            return false;
        }
        let fields = fields(line);
        match self.lines_read {
            0 => {
                self.columns = fields
                    .into_iter()
                    .map(|f| ColumnMeta::new(String::from_utf8_lossy(&f.unwrap_or_default()), String::new()))
                    .collect();
            }
            1 => {
                if fields.len() != self.columns.len() {
                    return false;
                }
                for (column, f) in self.columns.iter_mut().zip(fields) {
                    column.type_name = String::from_utf8_lossy(&f.unwrap_or_default()).into_owned();
                }
                self.kinds = self.columns.iter().map(|c| ValueKind::of(&c.type_name)).collect();
            }
            _ => {
                if fields.len() != self.kinds.len() {
                    return false;
                }
                rows.push(self.kinds.iter().zip(fields).map(|(kind, f)| kind.value(f)).collect());
            }
        }
        self.lines_read += 1;
        true
    }
}

/// The exception text in what followed the rows, from its `Code: …` on.
fn exception_message(text: &[u8]) -> String {
    let text = String::from_utf8_lossy(text);
    match text.find("Code: ") {
        Some(start) => text[start..].lines().next().unwrap_or_default().trim().to_string(),
        None => {
            format!("The server sent a line that isn't a row: {}", text.trim().chars().take(300).collect::<String>())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_follow_the_type() {
        for (t, kind) in [
            ("UInt8", ValueKind::UInt),
            ("UInt64", ValueKind::UInt),
            ("Int32", ValueKind::Int),
            ("Nullable(Int64)", ValueKind::Int),
            ("LowCardinality(Nullable(UInt16))", ValueKind::UInt),
            ("Int128", ValueKind::Decimal),
            ("UInt256", ValueKind::Decimal),
            ("Decimal(18, 4)", ValueKind::Decimal),
            ("Nullable(Decimal64(2))", ValueKind::Decimal),
            ("Float32", ValueKind::Float),
            ("Nullable(Float64)", ValueKind::Float),
            ("Bool", ValueKind::Bool),
            ("String", ValueKind::Text),
            ("LowCardinality(String)", ValueKind::Text),
            ("DateTime64(3, 'UTC')", ValueKind::Text),
            ("Array(UInt8)", ValueKind::Text),
            ("Map(String, UInt64)", ValueKind::Text),
            ("Enum8('a' = 1)", ValueKind::Text),
            ("UUID", ValueKind::Text),
        ] {
            assert_eq!(ValueKind::of(t), kind, "{t}");
        }
    }

    #[test]
    fn values_are_numbers_where_the_type_is() {
        let v = |kind: ValueKind, s: &str| kind.value(Some(s.as_bytes().to_vec()));
        assert_eq!(v(ValueKind::UInt, "18446744073709551615"), Value::UInt(u64::MAX));
        assert_eq!(v(ValueKind::Int, "-8"), Value::Int(-8));
        assert_eq!(v(ValueKind::Float, "2.5"), Value::Float(2.5));
        assert_eq!(v(ValueKind::Float, "inf"), Value::Float(f64::INFINITY));
        assert!(matches!(v(ValueKind::Float, "nan"), Value::Float(f) if f.is_nan()));
        assert_eq!(v(ValueKind::Decimal, "-170141183460469231731687303715884105728"), {
            Value::Decimal("-170141183460469231731687303715884105728".into())
        });
        assert_eq!(v(ValueKind::Bool, "true"), Value::Bool(true));
        assert_eq!(v(ValueKind::Text, "2024-02-29"), Value::text("2024-02-29"));
        assert_eq!(v(ValueKind::Int, "not a number"), Value::text("not a number"));
        assert_eq!(ValueKind::Text.value(None), Value::Null);
        assert_eq!(ValueKind::Text.value(Some(vec![0xff, 0x00, 0xfe])), Value::Bytes(vec![0xff, 0, 0xfe].into()));
        assert!(v(ValueKind::UInt, "7").is_numeric() && v(ValueKind::Decimal, "1.50").is_numeric());
    }

    #[test]
    fn escapes_decode() {
        assert_eq!(unescape(b"\\N"), None);
        assert_eq!(unescape(b"\\\\N"), Some(b"\\N".to_vec()));
        assert_eq!(unescape(b"a\\tb\\nc\\\\d\\'e"), Some(b"a\tb\nc\\d'e".to_vec()));
        assert_eq!(unescape(b"\\0\\r\\b\\f\\a\\v"), Some(vec![0, b'\r', 8, 0x0c, 7, 0x0b]));
        assert_eq!(unescape(b"\\x41\\xzz"), Some(b"A\\xzz".to_vec()));
        assert_eq!(unescape(b"trailing\\"), Some(b"trailing\\".to_vec()));
        assert_eq!(unescape(b""), Some(Vec::new()));
        assert_eq!(fields(b"1\t\\N\t"), vec![Some(b"1".to_vec()), None, Some(Vec::new())]);
    }

    fn read(chunks: &[&[u8]]) -> (TsvReader, Vec<Row>) {
        let mut reader = TsvReader::default();
        let mut rows = Vec::new();
        for c in chunks {
            reader.push(c, &mut rows);
        }
        (reader, rows)
    }

    #[test]
    fn rows_arrive_across_chunk_boundaries() {
        let body = b"id\tvalue\nUInt8\tNullable(String)\n1\ta\\tb\n2\t\\N\n3\tline1\\nline2\n";
        for split in 0..body.len() {
            let (reader, rows) = read(&[&body[..split], &body[split..]]);
            let cols = reader.columns().unwrap();
            assert_eq!((cols[0].name.as_str(), cols[1].type_name.as_str()), ("id", "Nullable(String)"));
            assert_eq!(rows.len(), 3, "split at {split}");
            assert_eq!(rows[0][..], [Value::UInt(1), Value::text("a\tb")]);
            assert_eq!(rows[1][1], Value::Null);
            assert_eq!(rows[2][1], Value::text("line1\nline2"));
            reader.finish().unwrap();
        }
    }

    #[test]
    fn a_result_without_rows_has_columns() {
        let (reader, rows) = read(&[b"a\nUInt8\n"]);
        assert_eq!(reader.columns().unwrap().len(), 1);
        assert!(rows.is_empty());
        reader.finish().unwrap();
        assert!(read(&[b"a\n"]).0.finish().is_err());
    }

    #[test]
    fn an_exception_after_rows_is_an_error() {
        // ClickHouse 25.8 marks it with a line of its own.
        let (reader, rows) = read(&[
            b"n\nUInt64\n1\n2\n__exception__\r\nCode: 395. DB::Exception: boom: while executing ",
            b"'FUNCTION throwIf'. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) (version 25.8.33.6 (official build))\n\n",
        ]);
        assert_eq!(rows.len(), 2);
        let err = reader.finish().unwrap_err();
        assert!(err.starts_with("Code: 395. DB::Exception: boom") && err.ends_with("(official build))"), "{err}");
        // Older servers append the text straight after the rows.
        let (reader, rows) = read(&[b"a\tb\nUInt8\tUInt8\n1\t2\nCode: 241. DB::Exception: Memory limit exceeded\n"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(reader.finish().unwrap_err(), "Code: 241. DB::Exception: Memory limit exceeded");
        assert!(read(&[b"a\nUInt8\n1\n2"]).0.finish().is_err(), "a body cut inside a line");
    }
}
