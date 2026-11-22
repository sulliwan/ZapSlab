//! JSON and structured-data transforms ported from Boop's script collection.
//!
//! Each `apply` mirrors the corresponding Boop script, including its error
//! messages. `serde_json` is compiled with `preserve_order` (unified in via
//! gpui), so object keys retain document order like `JSON.parse`; only
//! `Sort JSON` reorders them.

use super::{Scope, Transform, TransformResult};
use base64::Engine as _;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use serde_json::Value;

/// Characters escaped in query-string keys and values.
///
/// Brackets are deliberately left alone so the generated array markers
/// (`name[]`) survive encoding and can still be recognized when parsing back.
const QUERY_COMPONENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'=')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Format JSON",
        description: "Pretty-print JSON with two-space indentation; invalid JSON is an error.",
        tags: "json,prettify,clean,indent",
        scope: Scope::SelectionOrFullText,
        apply: format_json,
    },
    Transform {
        name: "Minify JSON",
        description: "Remove insignificant whitespace from JSON; invalid JSON is an error.",
        tags: "json,minify,clean,indent",
        scope: Scope::SelectionOrFullText,
        apply: minify_json,
    },
    Transform {
        name: "Sort JSON",
        description: "Recursively sort object keys and array elements (by their JSON text), then pretty-print; invalid JSON is an error.",
        tags: "json,sort",
        scope: Scope::SelectionOrFullText,
        apply: sort_json,
    },
    Transform {
        name: "JSON to CSV",
        description: "Convert a JSON array of objects to CSV; headers come from the first object and nested values are written as JSON text.",
        tags: "table,convert",
        scope: Scope::SelectionOrFullText,
        apply: json_to_csv,
    },
    Transform {
        name: "CSV to JSON",
        description: "Convert CSV to a JSON array of objects using the header row as keys; every value is a string.",
        tags: "table,convert",
        scope: Scope::SelectionOrFullText,
        apply: csv_to_json,
    },
    Transform {
        name: "JSON to YAML",
        description: "Convert JSON to YAML; invalid JSON is an error.",
        tags: "markup,convert",
        scope: Scope::SelectionOrFullText,
        apply: json_to_yaml,
    },
    Transform {
        name: "YAML to JSON",
        description: "Convert YAML to JSON; invalid YAML is an error.",
        tags: "markup,convert",
        scope: Scope::SelectionOrFullText,
        apply: yaml_to_json,
    },
    Transform {
        name: "JSON to Query String",
        description: "Convert a JSON object to a URL query string; nested keys become repeated 'key[]' parameters.",
        tags: "url,query,params,json,convert,encode",
        scope: Scope::SelectionOrFullText,
        apply: json_to_query,
    },
    Transform {
        name: "Query String to JSON",
        description: "Convert a URL query string to a JSON object; array indices are stripped and repeated keys become arrays.",
        tags: "url,query,params,json,convert,decode",
        scope: Scope::SelectionOrFullText,
        apply: query_to_json,
    },
    Transform {
        name: "JWT Decode",
        description: "Decode a JWT's header and payload to JSON; the signature is not verified.",
        tags: "decode,jwt,token",
        scope: Scope::SelectionOrFullText,
        apply: jwt_decode,
    },
];

fn format_json(text: &str) -> TransformResult {
    match serde_json::from_str::<Value>(text) {
        // `serde_json`'s pretty printer uses two-space indentation, matching
        // `JSON.stringify(value, null, 2)`.
        Ok(value) => pretty(&value, "Invalid JSON"),
        Err(_) => TransformResult::error("Invalid JSON"),
    }
}

fn minify_json(text: &str) -> TransformResult {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => compact(&value, "Invalid JSON"),
        Err(_) => TransformResult::error("Invalid JSON"),
    }
}

fn sort_json(text: &str) -> TransformResult {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => pretty(&sort_value(&value), "Invalid JSON"),
        Err(_) => TransformResult::error("Invalid JSON"),
    }
}

/// Recursively sorts object keys and array elements, like the script's `sort`.
fn sort_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => {
            let mut sorted: Vec<Value> = items.iter().map(sort_value).collect();
            // Boop orders elements by comparing their stringified JSON form.
            sorted.sort_by_cached_key(compact_string);
            Value::Array(sorted)
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                if let Some(value) = map.get(key) {
                    sorted.insert(key.clone(), sort_value(value));
                }
            }
            Value::Object(sorted)
        }
        other => other.clone(),
    }
}

fn json_to_csv(text: &str) -> TransformResult {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => return TransformResult::error("Invalid JSON."),
    };
    let rows = match value.as_array() {
        Some(rows) => rows,
        None => return TransformResult::error("Invalid JSON."),
    };
    // The script reads keys from `data[0]`, so an empty or non-object first
    // row is an error.
    let first = match rows.first().and_then(Value::as_object) {
        Some(object) => object,
        None => return TransformResult::error("Invalid JSON."),
    };
    let header: Vec<&String> = first.keys().collect();

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(
        header
            .iter()
            .map(|key| quote_csv_if_needed(key.as_str()))
            .collect::<Vec<_>>()
            .join(","),
    );

    for row in rows {
        let object = row.as_object();
        let cells: Vec<String> = header
            .iter()
            .map(|key| csv_cell(object.and_then(|object| object.get(*key))))
            .collect();
        lines.push(cells.join(","));
    }

    TransformResult::replace(lines.join("\r\n"))
}

/// Formats one CSV cell with Boop's value stringification, then quotes it for
/// RFC 4180 readers.
fn csv_cell(value: Option<&Value>) -> String {
    match value {
        // Boop's replacer maps `null` to an empty string, which
        // `JSON.stringify` renders as `""`.
        Some(Value::Null) => "\"\"".to_string(),
        Some(Value::String(text)) => quote_csv(text),
        // A missing key stringifies to `undefined`, which `join` renders empty.
        None => String::new(),
        Some(other) => quote_csv_if_needed(&compact_string(other)),
    }
}

/// Always-quoted field with internal quotes doubled.
fn quote_csv(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

fn quote_csv_if_needed(text: &str) -> String {
    let needs_quotes = text
        .chars()
        .any(|ch| matches!(ch, ',' | '"' | '\n' | '\r'));
    if needs_quotes {
        quote_csv(text)
    } else {
        text.to_string()
    }
}

fn csv_to_json(text: &str) -> TransformResult {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(text.as_bytes());
    let headers = match reader.headers() {
        Ok(headers) => headers.clone(),
        Err(_) => return TransformResult::error("Invalid CSV"),
    };

    let mut rows = Vec::new();
    for record in reader.records() {
        let record = match record {
            Ok(record) => record,
            Err(_) => return TransformResult::error("Invalid CSV"),
        };
        let mut object = serde_json::Map::new();
        for (index, header) in headers.iter().enumerate() {
            // Papa.parse produces string values, so cells are never coerced.
            let cell = record.get(index).unwrap_or("");
            object.insert(header.to_string(), Value::String(cell.to_string()));
        }
        rows.push(Value::Object(object));
    }

    pretty(&Value::Array(rows), "Invalid CSV")
}

fn json_to_yaml(text: &str) -> TransformResult {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => match serde_yaml_ng::to_string(&value) {
            Ok(yaml) => TransformResult::replace(yaml),
            Err(_) => TransformResult::error("Invalid JSON"),
        },
        Err(_) => TransformResult::error("Invalid JSON"),
    }
}

fn yaml_to_json(text: &str) -> TransformResult {
    match serde_yaml_ng::from_str::<Value>(text) {
        Ok(value) => pretty(&value, "Invalid YAML"),
        Err(_) => TransformResult::error("Invalid YAML"),
    }
}

fn json_to_query(text: &str) -> TransformResult {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => return TransformResult::error("Unable to convert JSON to URL params"),
    };
    let mut parts = Vec::new();
    append_query(&value, None, &mut parts);
    TransformResult::replace(parts.join("&"))
}

fn append_query(value: &Value, prefix: Option<&str>, parts: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (param, child) in map {
                // The script expands every nested key to `prefix[]`, dropping
                // the child's own name; arrays then read back as repeated keys.
                let key = match prefix {
                    Some(prefix) => format!("{prefix}[]"),
                    None => param.clone(),
                };
                append_query_value(child, key, parts);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let key = match prefix {
                    Some(prefix) => format!("{prefix}[]"),
                    None => index.to_string(),
                };
                append_query_value(child, key, parts);
            }
        }
        _ => {}
    }
}

fn append_query_value(value: &Value, key: String, parts: &mut Vec<String>) {
    match value {
        // JS string concatenation turns `null` into "null".
        Value::Null => parts.push(format!("{}=null", encode_key(&key))),
        Value::Object(_) | Value::Array(_) => append_query(value, Some(&key), parts),
        Value::Bool(_) | Value::Number(_) => {
            parts.push(format!("{}={}", encode_key(&key), compact_string(value)));
        }
        Value::String(text) => parts.push(format!(
            "{}={}",
            encode_key(&key),
            utf8_percent_encode(text, QUERY_COMPONENT)
        )),
    }
}

fn encode_key(key: &str) -> String {
    utf8_percent_encode(key, QUERY_COMPONENT).to_string()
}

fn query_to_json(text: &str) -> TransformResult {
    let normalized = strip_array_indices(text);
    let mut object = serde_json::Map::new();

    for param in normalized.split('&') {
        // `var [key, value] = param.split('=')` keeps only the first two
        // segments, discarding anything after a second `=`.
        let mut pieces = param.split('=');
        let key = pieces.next().unwrap_or("");
        let value = pieces.next().unwrap_or("");

        let key = match decode_component(key) {
            Ok(key) => key,
            Err(_) => return TransformResult::error("Unable to parse given string"),
        };
        let value = match decode_component(value) {
            Ok(value) => value,
            Err(_) => return TransformResult::error("Unable to parse given string"),
        };

        // `remove` keeps the borrow short enough to re-insert the merged value.
        match object.remove(&key) {
            None => {
                object.insert(key, Value::String(value));
            }
            Some(Value::Array(mut items)) => {
                items.push(Value::String(value));
                object.insert(key, Value::Array(items));
            }
            Some(first) => {
                object.insert(key, Value::Array(vec![first, Value::String(value)]));
            }
        }
    }

    compact(&Value::Object(object), "Unable to parse given string")
}

/// Removes `[<digit>]=` and `[]=` markers, mirroring `/\[\d?\]=/gi`.
fn strip_array_indices(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let tail = &rest[open + 1..];
        let digits = if tail.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            1
        } else {
            0
        };
        if let Some(remainder) = tail[digits..].strip_prefix("]=") {
            out.push('=');
            rest = remainder;
        } else {
            out.push('[');
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// `decodeURIComponent`, including its rejection of malformed escapes.
fn decode_component(component: &str) -> Result<String, ()> {
    let bytes = component.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let valid = index + 2 < bytes.len()
                && bytes[index + 1].is_ascii_hexdigit()
                && bytes[index + 2].is_ascii_hexdigit();
            if !valid {
                return Err(());
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    percent_decode_str(component)
        .decode_utf8()
        .map(|text| text.into_owned())
        .map_err(|_| ())
}

fn jwt_decode(text: &str) -> TransformResult {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 3 {
        return TransformResult::error("Invalid Token");
    }

    let header = match decode_base64url(parts[0]) {
        Ok(header) => header,
        Err(_) => return TransformResult::error("Invalid Token"),
    };
    let payload = match decode_base64url(parts[1]) {
        Ok(payload) => payload,
        Err(_) => return TransformResult::error("Invalid Token"),
    };

    let header: Value = match serde_json::from_str(&header) {
        Ok(header) => header,
        Err(_) => return TransformResult::error("Error while parsing JSON"),
    };
    let payload: Value = match serde_json::from_str(&payload) {
        Ok(payload) => payload,
        Err(_) => return TransformResult::error("Error while parsing JSON"),
    };

    let mut object = serde_json::Map::new();
    object.insert("header".to_string(), header);
    object.insert("payload".to_string(), payload);
    object.insert("signature".to_string(), Value::String(parts[2].to_string()));

    pretty(&Value::Object(object), "Error while parsing JSON")
}

fn decode_base64url(segment: &str) -> Result<String, ()> {
    let segment = segment.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|_| ())?;
    String::from_utf8(bytes).map_err(|_| ())
}

fn pretty(value: &Value, error: &'static str) -> TransformResult {
    match serde_json::to_string_pretty(value) {
        Ok(text) => TransformResult::replace(text),
        Err(_) => TransformResult::error(error),
    }
}

fn compact(value: &Value, error: &'static str) -> TransformResult {
    match serde_json::to_string(value) {
        Ok(text) => TransformResult::replace(text),
        Err(_) => TransformResult::error(error),
    }
}

/// Compact JSON text for a value. Serializing a `Value` cannot fail.
fn compact_string(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replaced(result: TransformResult) -> String {
        match result {
            TransformResult::Replace(text) => text,
            TransformResult::ReplaceAndReport { text, .. } => text,
            TransformResult::Report(message) => panic!("unexpected report: {message}"),
            TransformResult::Error(message) => panic!("unexpected error: {message}"),
        }
    }

    fn error_message(result: TransformResult) -> String {
        match result {
            TransformResult::Error(message) => message,
            TransformResult::Replace(_)
            | TransformResult::ReplaceAndReport { .. }
            | TransformResult::Report(_) => panic!("expected an error"),
        }
    }

    #[test]
    fn formats_and_minifies_json() {
        assert_eq!(
            replaced(format_json(r#"{"b":2,"a":[1,true]}"#)),
            "{\n  \"b\": 2,\n  \"a\": [\n    1,\n    true\n  ]\n}"
        );
        assert_eq!(replaced(minify_json("{\n  \"a\": 1\n}")), r#"{"a":1}"#);
        assert_eq!(error_message(format_json("{")), "Invalid JSON");
        assert_eq!(error_message(minify_json("{")), "Invalid JSON");
    }

    #[test]
    fn sorts_keys_and_arrays() {
        assert_eq!(
            replaced(sort_json(r#"{"b":{"d":1,"c":2},"a":[3,1,2]}"#)),
            "{\n  \"a\": [\n    1,\n    2,\n    3\n  ],\n  \"b\": {\n    \"c\": 2,\n    \"d\": 1\n  }\n}"
        );
        assert_eq!(
            replaced(sort_json(r#"["b","a"]"#)),
            "[\n  \"a\",\n  \"b\"\n]"
        );
        assert_eq!(error_message(sort_json("{")), "Invalid JSON");
    }

    #[test]
    fn converts_json_to_csv() {
        assert_eq!(
            replaced(json_to_csv(r#"[{"a":1,"b":"x"},{"a":2,"b":"y"}]"#)),
            "a,b\r\n1,\"x\"\r\n2,\"y\""
        );
        assert_eq!(
            replaced(json_to_csv(r#"[{"a":null,"b":true}]"#)),
            "a,b\r\n\"\",true"
        );
        assert_eq!(
            replaced(json_to_csv(r#"[{"a":"x,y"}]"#)),
            "a\r\n\"x,y\""
        );
        assert_eq!(
            replaced(json_to_csv(r#"[{"a":"x\"y"}]"#)),
            "a\r\n\"x\"\"y\""
        );
        assert_eq!(
            replaced(json_to_csv(r#"[{"a":[1,2]}]"#)),
            "a\r\n\"[1,2]\""
        );
        assert_eq!(
            replaced(json_to_csv(r#"[{"a":1,"b":2},{"a":3}]"#)),
            "a,b\r\n1,2\r\n3,"
        );
        assert_eq!(error_message(json_to_csv("{")), "Invalid JSON.");
        assert_eq!(error_message(json_to_csv("[]")), "Invalid JSON.");
    }

    #[test]
    fn converts_csv_to_json() {
        assert_eq!(
            replaced(csv_to_json("a,b\r\n1,2\r\n3,4\r\n")),
            "[\n  {\n    \"a\": \"1\",\n    \"b\": \"2\"\n  },\n  {\n    \"a\": \"3\",\n    \"b\": \"4\"\n  }\n]"
        );
        assert_eq!(
            replaced(csv_to_json("a,b\n\"x,y\",z\n")),
            "[\n  {\n    \"a\": \"x,y\",\n    \"b\": \"z\"\n  }\n]"
        );
        assert_eq!(error_message(csv_to_json("a,b\n1")), "Invalid CSV");
    }

    #[test]
    fn converts_between_json_and_yaml() {
        let json = r#"{"a":1,"b":[1,2]}"#;
        let yaml = replaced(json_to_yaml(json));
        assert_eq!(replaced(yaml_to_json(&yaml)), replaced(format_json(json)));
        assert_eq!(error_message(json_to_yaml("{")), "Invalid JSON");
        assert_eq!(error_message(yaml_to_json("a: [1,")), "Invalid YAML");
    }

    #[test]
    fn converts_json_to_query() {
        assert_eq!(
            replaced(json_to_query(r#"{"a":1,"b":"hello world"}"#)),
            "a=1&b=hello%20world"
        );
        assert_eq!(
            replaced(json_to_query(r#"{"a":[1,2]}"#)),
            "a[]=1&a[]=2"
        );
        assert_eq!(replaced(json_to_query(r#"{"a":{"b":1}}"#)), "a[]=1");
        assert_eq!(
            replaced(json_to_query(r#"{"a":true,"b":null}"#)),
            "a=true&b=null"
        );
        assert_eq!(
            error_message(json_to_query("{")),
            "Unable to convert JSON to URL params"
        );
    }

    #[test]
    fn converts_query_to_json() {
        assert_eq!(replaced(query_to_json("a=1&b=2")), r#"{"a":"1","b":"2"}"#);
        assert_eq!(
            replaced(query_to_json("a[]=1&a[]=2")),
            r#"{"a":["1","2"]}"#
        );
        assert_eq!(
            replaced(query_to_json("a[0]=x&a[1]=y")),
            r#"{"a":["x","y"]}"#
        );
        assert_eq!(
            replaced(query_to_json("a=hello%20world")),
            r#"{"a":"hello world"}"#
        );
        assert_eq!(
            replaced(query_to_json("a=1&a=2")),
            r#"{"a":["1","2"]}"#
        );
        assert_eq!(
            error_message(query_to_json("a=%ZZ")),
            "Unable to parse given string"
        );
    }

    #[test]
    fn query_round_trips() {
        let query = replaced(json_to_query(r#"{"a":"hello world","b":true}"#));
        assert_eq!(query, "a=hello%20world&b=true");
        assert_eq!(
            replaced(query_to_json(&query)),
            r#"{"a":"hello world","b":"true"}"#
        );
    }

    #[test]
    fn decodes_jwt() {
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let expected = concat!(
            "{\n",
            "  \"header\": {\n",
            "    \"alg\": \"HS256\",\n",
            "    \"typ\": \"JWT\"\n",
            "  },\n",
            "  \"payload\": {\n",
            "    \"sub\": \"1234567890\",\n",
            "    \"name\": \"John Doe\",\n",
            "    \"iat\": 1516239022\n",
            "  },\n",
            "  \"signature\": \"SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c\"\n",
            "}"
        );
        assert_eq!(replaced(jwt_decode(token)), expected);
        assert_eq!(error_message(jwt_decode("a.b")), "Invalid Token");
        assert_eq!(error_message(jwt_decode("a.b.c")), "Invalid Token");
        assert_eq!(
            error_message(jwt_decode("e30.b29wcw.sig")),
            "Error while parsing JSON"
        );
    }
}
