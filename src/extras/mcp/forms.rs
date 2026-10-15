//! Strict raw form-schema validation before rmcp's permissive typed decoder.
//! Unsupported shapes are declined, never interpreted as weaker constraints.
use crate::question::{Answer, Question, QuestionBatch, QuestionOption};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

const MAX_BYTES: usize = 16 * 1024;
const MAX_ENUM: usize = 128;
const MAX_IDS: usize = 128;

#[derive(Clone, Debug, Default)]
pub(crate) struct RawForms(Arc<Mutex<Records>>);
#[derive(Debug, Default)]
struct Records {
    records: BTreeMap<String, Option<Form>>,
    used: BTreeSet<String>,
    closed: bool,
}
impl RawForms {
    pub(crate) fn observe(&self, bytes: &[u8]) {
        // Transport caps apply before this call; non-form messages never become
        // an authorization record, regardless of text in their result bodies.
        #[derive(serde::Deserialize)]
        struct Header {
            #[serde(default)]
            method: Option<String>,
            #[serde(default)]
            id: Option<Value>,
        }
        // A message that cannot spell the method needs no JSON lex at all.
        if !may_name_form_method(bytes) {
            return;
        }
        // Ignored params/result fields are scanned without allocating their
        // values. Oversized forms get a rejected record before typed decoding;
        // ordinary large tool responses need no second materialized JSON tree.
        let Ok(header) = serde_json::from_slice::<Header>(bytes) else {
            return;
        };
        self.record(header.method.as_deref(), header.id.as_ref(), Some(bytes));
    }
    /// Header of a stdout line too large to carry a form. Its id still counts
    /// toward reuse and exhaustion exactly like an oversized `observe` call.
    fn observe_scanned(&self, scan: HeaderScan) {
        if let Some((method, id)) = scan.finish() {
            self.record(method.as_deref(), id.as_ref(), None);
        }
    }
    fn record(&self, method: Option<&str>, id: Option<&Value>, bytes: Option<&[u8]>) {
        if method != Some("elicitation/create") {
            return;
        }
        let Some(id) = id else {
            return;
        };
        if !(id.is_string() || id.as_i64().is_some() || id.as_u64().is_some()) {
            return;
        }
        let id = id.to_string();
        if id.len() > 256 {
            return;
        }
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return;
        }
        if state.used.len() == MAX_IDS && !state.used.contains(&id) {
            state.closed = true;
            state.records.clear();
            return;
        }
        let unique = state.used.insert(id.clone());
        let form = match bytes {
            Some(bytes) if unique && bytes.len() <= MAX_BYTES => {
                serde_json::from_slice::<Value>(bytes)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("params")
                            .and_then(|params| Form::parse(params).ok())
                    })
            }
            _ => None,
        };
        state.records.insert(id, form);
    }
    pub(crate) fn take(&self, id: &rmcp::model::RequestId) -> Option<Form> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return None;
        }
        state
            .records
            .remove(&serde_json::to_value(id).ok()?.to_string())
            .flatten()
    }
}

/// Whether `bytes` can contain a JSON string that decodes to
/// `elicitation/create`: either literally, or through a `\u` or `\/` escape
/// (the only JSON escapes that produce characters of that method name).
fn may_name_form_method(bytes: &[u8]) -> bool {
    static PATTERN: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r"(?-u)elicitation/create|\\u|\\/").expect("static pattern")
    });
    PATTERN.is_match(bytes)
}

/// The remainder of a JSON string body: unescaped bytes and complete escapes.
static STRING_BODY: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
    regex::bytes::Regex::new(r#"(?s-u)\A(?:[^"\\]|\\.)*"#).expect("static pattern")
});
/// Structural bytes at the top-level object depth.
static TOP_STRUCTURE: std::sync::LazyLock<regex::bytes::Regex> = std::sync::LazyLock::new(|| {
    regex::bytes::Regex::new(r#"(?-u)[{}\[\]",:]"#).expect("static pattern")
});
/// Structural bytes that change nesting or start a string at any other depth.
static NESTED_STRUCTURE: std::sync::LazyLock<regex::bytes::Regex> =
    std::sync::LazyLock::new(|| {
        regex::bytes::Regex::new(r#"(?-u)[{}\[\]"]"#).expect("static pattern")
    });

const HEADER_TOKEN_MAX: usize = 4 * 1024;
const HEADER_KEY_MAX: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Method,
    Id,
    Other,
}

/// Streaming scanner for the top-level `method` and `id` of a JSON line that
/// is too large to retain. It agrees with the serde header decode on every
/// JSON object; on malformed input it can only report an id serde would have
/// rejected, which records more (declined) ids, never fewer. A top-level array
/// (which serde would decode positionally) yields no header: the transport
/// never dispatches one as an elicitation request.
#[derive(Default)]
struct HeaderScan {
    depth: usize,
    seen_top: bool,
    not_object: bool,
    in_string: bool,
    escape: bool,
    expect_key: bool,
    key: Option<Vec<u8>>,
    pending: Option<Slot>,
    value: Option<(Slot, Vec<u8>, bool)>,
    method: Option<Result<Vec<u8>, ()>>,
    id: Option<Result<Vec<u8>, ()>>,
    duplicate: bool,
}

impl HeaderScan {
    fn push(&mut self, bytes: &[u8]) {
        if let Some(key) = &mut self.key {
            if key.len() <= HEADER_KEY_MAX {
                let room = HEADER_KEY_MAX + 1 - key.len();
                key.extend_from_slice(&bytes[..bytes.len().min(room)]);
            }
        } else if let Some((_, value, overflow)) = &mut self.value
            && !*overflow
        {
            if value.len() + bytes.len() > HEADER_TOKEN_MAX {
                *overflow = true;
            } else {
                value.extend_from_slice(bytes);
            }
        }
    }

    fn push_bare(&mut self, bytes: &[u8]) {
        // Whitespace between tokens is insignificant; numbers and literals are
        // the only other bare bytes and are kept for the value decode.
        if self.value.is_some() && bytes.iter().any(|byte| !byte.is_ascii_whitespace()) {
            let bare: Vec<u8> = bytes
                .iter()
                .copied()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect();
            self.push(&bare);
        }
    }

    fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.in_string {
                if self.escape {
                    self.escape = false;
                    self.push(&bytes[..1]);
                    bytes = &bytes[1..];
                    continue;
                }
                let end = STRING_BODY.find(bytes).map_or(0, |found| found.end());
                self.push(&bytes[..end]);
                if end == bytes.len() {
                    return;
                }
                self.push(&bytes[end..=end]);
                if bytes[end] == b'"' {
                    self.in_string = false;
                    self.end_string();
                } else {
                    self.escape = true;
                }
                bytes = &bytes[end + 1..];
                continue;
            }
            let pattern = if self.depth == 1 {
                &*TOP_STRUCTURE
            } else {
                &*NESTED_STRUCTURE
            };
            let Some(found) = pattern.find(bytes) else {
                self.push_bare(bytes);
                return;
            };
            let at = found.start();
            self.push_bare(&bytes[..at]);
            self.structural(bytes[at]);
            bytes = &bytes[at + 1..];
        }
    }

    fn structural(&mut self, byte: u8) {
        if self.depth == 0 {
            if byte == b'"' {
                self.in_string = true;
            }
            if self.seen_top || byte != b'{' {
                self.not_object = true;
            }
            self.seen_top = true;
            if matches!(byte, b'{' | b'[') {
                self.depth = 1;
                self.expect_key = byte == b'{';
            }
            return;
        }
        match byte {
            b'"' => {
                self.in_string = true;
                if self.depth == 1 && self.expect_key && self.value.is_none() {
                    self.key = Some(vec![b'"']);
                } else {
                    self.push(b"\"");
                }
            }
            b'{' | b'[' => {
                self.push(&[byte]);
                self.depth += 1;
            }
            b'}' | b']' => {
                if self.depth == 1 {
                    self.finish_value();
                } else {
                    self.push(&[byte]);
                }
                self.depth -= 1;
            }
            b',' if self.depth == 1 => {
                self.finish_value();
                self.expect_key = true;
            }
            b':' if self.depth == 1 => {
                // Only `method` and `id` values are captured; any other value
                // is skipped until its closing `,` or `}` at this depth.
                if let Some(slot) = self.pending.take().filter(|slot| *slot != Slot::Other) {
                    self.value = Some((slot, Vec::new(), false));
                }
                self.expect_key = false;
            }
            _ => self.push(&[byte]),
        }
    }

    fn end_string(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let slot = match serde_json::from_slice::<String>(&key).as_deref() {
            Ok("method") if key.len() <= HEADER_KEY_MAX => Slot::Method,
            Ok("id") if key.len() <= HEADER_KEY_MAX => Slot::Id,
            _ => Slot::Other,
        };
        self.pending = Some(slot);
    }

    fn finish_value(&mut self) {
        let Some((slot, value, overflow)) = self.value.take() else {
            return;
        };
        let token = if overflow { Err(()) } else { Ok(value) };
        let target = match slot {
            Slot::Method => &mut self.method,
            Slot::Id => &mut self.id,
            Slot::Other => return,
        };
        if target.replace(token).is_some() {
            self.duplicate = true;
        }
    }

    fn finish(self) -> Option<(Option<String>, Option<Value>)> {
        if !self.seen_top || self.not_object || self.depth != 0 || self.in_string || self.duplicate
        {
            return None;
        }
        let method = match self.method {
            None => None,
            Some(Ok(raw)) => serde_json::from_slice::<Option<String>>(&raw).ok()?,
            // An over-long method token cannot spell `elicitation/create`.
            Some(Err(())) => Some(String::new()),
        };
        let id = match self.id {
            None => None,
            Some(Ok(raw)) => serde_json::from_slice::<Option<Value>>(&raw).ok()?,
            // An over-long id token can never pass the 256-byte id bound.
            Some(Err(())) => return None,
        };
        Some((method, id))
    }
}

/// Raw-form observation for one stdout line. Bytes are retained only up to the
/// form bound; a longer line can no longer carry a form, so only its header is
/// scanned (for id reuse and exhaustion) instead of copying the whole line.
#[derive(Default)]
pub(crate) struct LineObserver {
    raw: Vec<u8>,
    scan: Option<HeaderScan>,
}

impl LineObserver {
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if let Some(scan) = &mut self.scan {
            scan.feed(bytes);
        } else if self.raw.len() + bytes.len() <= MAX_BYTES {
            self.raw.extend_from_slice(bytes);
        } else {
            let mut scan = HeaderScan::default();
            scan.feed(&self.raw);
            scan.feed(bytes);
            self.raw.clear();
            self.scan = Some(scan);
        }
    }

    pub(crate) fn end(&mut self, forms: &RawForms) {
        match self.scan.take() {
            Some(scan) => forms.observe_scanned(scan),
            None => forms.observe(&self.raw),
        }
        self.raw.clear();
    }
}

/// Position of the next line feed.
pub(crate) fn find_newline(bytes: &[u8]) -> Option<usize> {
    static NEWLINE: std::sync::LazyLock<regex::bytes::Regex> =
        std::sync::LazyLock::new(|| regex::bytes::Regex::new(r"(?-u)\n").expect("static pattern"));
    NEWLINE.find(bytes).map(|found| found.start())
}

#[derive(Clone, Debug)]
pub(crate) struct Form {
    message: String,
    fields: Vec<Field>,
}
#[derive(Clone, Debug)]
struct Field {
    name: String,
    schema: Value,
    required: bool,
    choices: Vec<String>,
}
fn object<'a>(
    value: &'a Value,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, &'static str> {
    let object = value.as_object().ok_or("form object required")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("unsupported form constraint");
    }
    Ok(object)
}
fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn enum_values(value: &Value) -> Result<Vec<String>, &'static str> {
    let entries = value.as_array().ok_or("enum array required")?;
    if entries.is_empty() || entries.len() > MAX_ENUM {
        return Err("enum bound");
    }
    let mut unique = BTreeSet::new();
    let mut result = Vec::new();
    for entry in entries {
        let text = entry
            .as_str()
            .filter(|text| text.len() <= 128 && !text.chars().any(char::is_control))
            .ok_or("string enum required")?;
        if !unique.insert(text) {
            return Err("duplicate enum");
        }
        result.push(text.to_owned());
    }
    Ok(result)
}
fn titled(value: &Value) -> Result<Vec<String>, &'static str> {
    let entries = value.as_array().ok_or("titled enum array required")?;
    if entries.is_empty() || entries.len() > MAX_ENUM {
        return Err("enum bound");
    }
    let mut values = Vec::new();
    let mut unique = BTreeSet::new();
    for entry in entries {
        object(entry, &["const", "title"])?;
        let constant = entry
            .get("const")
            .and_then(Value::as_str)
            .filter(|text| text.len() <= 128 && !text.chars().any(char::is_control))
            .ok_or("enum const required")?;
        if !entry
            .get("title")
            .and_then(Value::as_str)
            .is_some_and(|text| bounded(text, 128))
            || !unique.insert(constant)
        {
            return Err("enum title or identity invalid");
        }
        values.push(constant.to_owned());
    }
    Ok(values)
}
impl Form {
    pub(crate) fn parse(params: &Value) -> Result<Self, &'static str> {
        object(params, &["mode", "message", "requestedSchema", "_meta"])?;
        if params.get("mode").is_some_and(|mode| mode != "form") {
            return Err("URL elicitation unsupported");
        }
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| bounded(text, 1024))
            .ok_or("form message invalid")?
            .to_owned();
        let schema = params.get("requestedSchema").ok_or("schema required")?;
        object(
            schema,
            &[
                "type",
                "properties",
                "required",
                "additionalProperties",
                "$schema",
            ],
        )?;
        if schema.get("type") != Some(&Value::String("object".into()))
            || schema
                .get("additionalProperties")
                .is_some_and(|flag| flag != &Value::Bool(false))
        {
            return Err("closed flat object required");
        }
        if schema
            .get("$schema")
            .is_some_and(|url| url.as_str() != Some("https://json-schema.org/draft/2020-12/schema"))
        {
            return Err("unsupported schema version");
        }
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .ok_or("properties required")?;
        if properties.is_empty() || properties.len() > crate::question::MAX_QUESTIONS {
            return Err("form supports one to four fields");
        }
        let required = match schema.get("required") {
            None => Vec::new(),
            Some(value) if value.as_array().is_some_and(|values| values.is_empty()) => Vec::new(),
            Some(value) => enum_values(value)?,
        };
        if required.iter().any(|key| !properties.contains_key(key)) {
            return Err("unknown required field");
        }
        let mut fields = Vec::new();
        for (name, schema) in properties.iter().collect::<BTreeMap<_, _>>() {
            if !bounded(name, 128) {
                return Err("field name invalid");
            }
            let kind = schema
                .get("type")
                .and_then(Value::as_str)
                .ok_or("field type required")?;
            let allowed: &[&str] = match kind {
                "string" => &[
                    "type",
                    "title",
                    "description",
                    "minLength",
                    "maxLength",
                    "format",
                    "default",
                    "enum",
                    "enumNames",
                    "oneOf",
                ],
                "number" | "integer" => &[
                    "type",
                    "title",
                    "description",
                    "minimum",
                    "maximum",
                    "default",
                ],
                "boolean" => &["type", "title", "description", "default"],
                "array" => &[
                    "type",
                    "title",
                    "description",
                    "items",
                    "minItems",
                    "maxItems",
                    "default",
                ],
                _ => return Err("flat primitive field required"),
            };
            object(schema, allowed)?;
            for key in ["title", "description"] {
                if schema
                    .get(key)
                    .is_some_and(|value| !value.as_str().is_some_and(|text| bounded(text, 512)))
                {
                    return Err("field description invalid");
                }
            }
            let choices = if kind == "string" {
                if schema.get("enum").is_some() && schema.get("oneOf").is_some() {
                    return Err("ambiguous enum");
                }
                let choices = if let Some(value) = schema.get("enum") {
                    enum_values(value)?
                } else if let Some(value) = schema.get("oneOf") {
                    titled(value)?
                } else {
                    Vec::new()
                };
                if let Some(names) = schema.get("enumNames") {
                    let names = names.as_array().ok_or("enumNames array required")?;
                    if choices.is_empty()
                        || names.len() != choices.len()
                        || names
                            .iter()
                            .any(|name| !name.as_str().is_some_and(|text| bounded(text, 128)))
                    {
                        return Err("enumNames invalid");
                    }
                }
                for key in ["minLength", "maxLength"] {
                    if schema
                        .get(key)
                        .is_some_and(|value| value.as_u64().is_none())
                    {
                        return Err("string length bound invalid");
                    }
                }
                if schema.get("minLength").and_then(Value::as_u64).unwrap_or(0)
                    > schema
                        .get("maxLength")
                        .and_then(Value::as_u64)
                        .unwrap_or(u64::MAX)
                {
                    return Err("string length range invalid");
                }
                if schema.get("format").is_some_and(|value| {
                    !matches!(value.as_str(), Some("email" | "uri" | "date" | "date-time"))
                }) {
                    return Err("string format unsupported");
                }
                if choices.is_empty()
                    && schema.get("enum").is_none()
                    && schema.get("oneOf").is_none()
                    && schema.get("minLength").and_then(Value::as_u64).unwrap_or(0) == 0
                    && schema.get("format").is_none()
                {
                    // The editor deliberately rejects an empty free-text
                    // submission. An explicit choice can carry a valid empty
                    // string without confusing it with optional omission.
                    vec![String::new()]
                } else {
                    choices
                }
            } else if kind == "boolean" {
                vec!["true".into(), "false".into()]
            } else if kind == "array" {
                let items = schema.get("items").ok_or("array enum items required")?;
                object(items, &["type", "enum", "anyOf", "oneOf"])?;
                if items.get("type").is_some_and(|kind| kind != "string") {
                    return Err("array string enum required");
                }
                let alternatives = ["enum", "anyOf", "oneOf"]
                    .into_iter()
                    .filter(|key| items.get(*key).is_some())
                    .collect::<Vec<_>>();
                if alternatives.len() != 1 {
                    return Err("array enum required");
                }
                if alternatives[0] == "enum" {
                    enum_values(&items[alternatives[0]])?;
                } else {
                    titled(&items[alternatives[0]])?;
                }
                for key in ["minItems", "maxItems"] {
                    if schema
                        .get(key)
                        .is_some_and(|value| value.as_u64().is_none())
                    {
                        return Err("array bound invalid");
                    }
                }
                if schema.get("minItems").and_then(Value::as_u64).unwrap_or(0)
                    > schema
                        .get("maxItems")
                        .and_then(Value::as_u64)
                        .unwrap_or(u64::MAX)
                {
                    return Err("array range invalid");
                }
                Vec::new()
            } else {
                for key in ["minimum", "maximum"] {
                    if schema
                        .get(key)
                        .is_some_and(|value| value.as_f64().is_none_or(|value| !value.is_finite()))
                    {
                        return Err("number bound invalid");
                    }
                    if kind == "integer"
                        && schema
                            .get(key)
                            .is_some_and(|value| integer(value).is_none())
                    {
                        return Err("integer bound invalid");
                    }
                }
                if schema
                    .get("minimum")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NEG_INFINITY)
                    > schema
                        .get("maximum")
                        .and_then(Value::as_f64)
                        .unwrap_or(f64::INFINITY)
                {
                    return Err("numeric range invalid");
                }
                if kind == "integer"
                    && let (Some(min), Some(max)) = (
                        schema.get("minimum").and_then(integer),
                        schema.get("maximum").and_then(integer),
                    )
                    && min > max
                {
                    return Err("integer range invalid");
                }
                Vec::new()
            };
            let field = Field {
                name: name.clone(),
                schema: schema.clone(),
                required: required.contains(name),
                choices,
            };
            if let Some(default) = schema.get("default") {
                field.validate(default)?;
            }
            fields.push(field);
        }
        let result = Self { message, fields };
        result.questions("MCP")?.validate()?;
        Ok(result)
    }
    pub(crate) fn questions(&self, server: &str) -> Result<QuestionBatch, &'static str> {
        let mut rows = Vec::new();
        for (index, field) in self.fields.iter().enumerate() {
            let mut options = if field.choices.len() <= 7 {
                field
                    .choices
                    .iter()
                    .map(|choice| QuestionOption {
                        label: if choice.is_empty() {
                            "Empty string".into()
                        } else {
                            choice.clone()
                        },
                        description: String::new(),
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            if !field.required {
                options.push(QuestionOption {
                    label: "Skip optional field".into(),
                    description: "Send no value for this property".into(),
                });
            }
            rows.push(Question {
                question: format!(
                    "{}{}{}: {}",
                    if index == 0 {
                        format!("MCP {server}: {} — ", self.message)
                    } else {
                        String::new()
                    },
                    field.name,
                    if field.required {
                        " (required)"
                    } else {
                        " (optional)"
                    },
                    field.schema
                ),
                options,
                allow_free_text: field.schema["type"] != "boolean",
            });
        }
        let batch = QuestionBatch(rows);
        batch.validate()?;
        Ok(batch)
    }
    pub(crate) fn answer(&self, answers: &[Answer]) -> Result<Value, &'static str> {
        if answers.len() != self.fields.len() {
            return Err("answer count invalid");
        }
        let mut values = serde_json::Map::new();
        for (field, answer) in self.fields.iter().zip(answers) {
            let choices = if field.choices.len() <= 7 {
                field.choices.as_slice()
            } else {
                &[]
            };
            let text = match answer {
                Answer::Option(index) if !field.required && *index == choices.len() => continue,
                Answer::Option(index) => choices.get(*index).ok_or("choice invalid")?.clone(),
                Answer::Text(text) => text.clone(),
            };
            let value = if field.schema["type"] == "string" {
                Value::String(text)
            } else {
                serde_json::from_str(&text).map_err(|_| "typed JSON value required")?
            };
            field.validate(&value)?;
            values.insert(field.name.clone(), value);
        }
        Ok(Value::Object(values))
    }
}
fn integer(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
}
impl Field {
    fn validate(&self, value: &Value) -> Result<(), &'static str> {
        match self.schema["type"].as_str() {
            Some("string") => {
                let text = value.as_str().ok_or("string required")?;
                if text.len() > crate::question::MAX_ANSWER_BYTES {
                    return Err("answer byte bound");
                }
                let len = text.chars().count() as u64;
                if len
                    < self
                        .schema
                        .get("minLength")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                    || len
                        > self
                            .schema
                            .get("maxLength")
                            .and_then(Value::as_u64)
                            .unwrap_or(u64::MAX)
                    || ((self.schema.get("enum").is_some() || self.schema.get("oneOf").is_some())
                        && !self.choices.iter().any(|choice| choice == text))
                {
                    return Err("string constraint failed");
                }
                let valid = match self.schema.get("format").and_then(Value::as_str) {
                    None => true,
                    Some("uri") => reqwest::Url::parse(text).is_ok(),
                    Some("date") => {
                        text.len() == 10
                            && text.as_bytes()[4] == b'-'
                            && text.as_bytes()[7] == b'-'
                            && chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok()
                    }
                    Some("date-time") => chrono::DateTime::parse_from_rfc3339(text).is_ok(),
                    Some("email") => text.split_once('@').is_some_and(|(local, domain)| {
                        !local.is_empty()
                            && !local.chars().any(|c| c.is_whitespace() || c == '@')
                            && domain.split('.').count() >= 2
                            && domain.split('.').all(|label| {
                                !label.is_empty()
                                    && !label.starts_with('-')
                                    && !label.ends_with('-')
                                    && label
                                        .bytes()
                                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                            })
                    }),
                    _ => false,
                };
                if !valid {
                    return Err("string format failed");
                }
            }
            Some("number" | "integer") => {
                if self.schema["type"] == "integer" {
                    let number = integer(value).ok_or("integer required")?;
                    if self
                        .schema
                        .get("minimum")
                        .and_then(integer)
                        .is_some_and(|min| number < min)
                        || self
                            .schema
                            .get("maximum")
                            .and_then(integer)
                            .is_some_and(|max| number > max)
                    {
                        return Err("numeric constraint failed");
                    }
                    return Ok(());
                }
                let number = value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or("finite number required")?;
                if number
                    < self
                        .schema
                        .get("minimum")
                        .and_then(Value::as_f64)
                        .unwrap_or(f64::NEG_INFINITY)
                    || number
                        > self
                            .schema
                            .get("maximum")
                            .and_then(Value::as_f64)
                            .unwrap_or(f64::INFINITY)
                {
                    return Err("numeric constraint failed");
                }
            }
            Some("boolean") => {
                if !value.is_boolean() {
                    return Err("boolean required");
                }
            }
            Some("array") => {
                let array = value.as_array().ok_or("array required")?;
                if array.len() > MAX_ENUM
                    || (array.len() as u64)
                        < self
                            .schema
                            .get("minItems")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                    || (array.len() as u64)
                        > self
                            .schema
                            .get("maxItems")
                            .and_then(Value::as_u64)
                            .unwrap_or(u64::MAX)
                {
                    return Err("array length failed");
                }
                let items = &self.schema["items"];
                let choices = if let Some(value) = items.get("enum") {
                    enum_values(value)?
                } else {
                    titled(
                        items
                            .get("anyOf")
                            .or_else(|| items.get("oneOf"))
                            .ok_or("array enum required")?,
                    )?
                };
                let mut seen = BTreeSet::new();
                for value in array {
                    let text = value.as_str().ok_or("array string required")?;
                    if !choices.iter().any(|choice| choice == text) || !seen.insert(text) {
                        return Err("array selection invalid");
                    }
                }
            }
            _ => return Err("field type unsupported"),
        };
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_request_identity_rejects_forgery_reuse_oversize_and_exhaustion() {
        let records = RawForms::default();
        let valid = serde_json::json!({"jsonrpc":"2.0","id":"owned","method":"elicitation/create","params":{"message":"Public data","requestedSchema":{"type":"object","properties":{"value":{"type":"string"}}}}});
        let id: rmcp::model::RequestId =
            serde_json::from_value(serde_json::json!("owned")).unwrap();
        records.observe(
            &serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","id":"owned","result":valid}))
                .unwrap(),
        );
        assert!(
            records.take(&id).is_none(),
            "ordinary response data cannot mint a form"
        );
        records.observe(&serde_json::to_vec(&valid).unwrap());
        assert!(records.take(&id).is_some());
        records.observe(&serde_json::to_vec(&valid).unwrap());
        assert!(
            records.take(&id).is_none(),
            "one connection cannot reuse a callback ID"
        );
        let mut huge = valid.clone();
        huge["id"] = serde_json::json!("huge");
        huge["params"]["_meta"] = serde_json::json!({"padding":"x".repeat(MAX_BYTES)});
        records.observe(&serde_json::to_vec(&huge).unwrap());
        let huge_id = serde_json::from_value(serde_json::json!("huge")).unwrap();
        assert!(records.take(&huge_id).is_none());
        for n in 0..MAX_IDS {
            let mut request = valid.clone();
            request["id"] = serde_json::json!(n);
            records.observe(&serde_json::to_vec(&request).unwrap());
        }
        let last_id = serde_json::from_value(serde_json::json!(MAX_IDS - 1)).unwrap();
        assert!(
            records.take(&last_id).is_none(),
            "record-budget exhaustion closes the form catalogue"
        );
    }
    fn serde_header(bytes: &[u8]) -> Option<(Option<String>, Option<Value>)> {
        #[derive(serde::Deserialize)]
        struct Header {
            #[serde(default)]
            method: Option<String>,
            #[serde(default)]
            id: Option<Value>,
        }
        serde_json::from_slice::<Header>(bytes)
            .ok()
            .map(|header| (header.method, header.id))
    }
    #[test]
    fn streamed_header_scan_agrees_with_serde_at_every_split() {
        let lines: &[&str] = &[
            r#"{"jsonrpc":"2.0","id":"owned","method":"elicitation/create","params":{"message":"a,b:}{]["}}"#,
            r#"{"params":{"nested":{"id":"inner","method":"elicitation/create"}},"method":"elicitation\/create","id":7}"#,
            r#"{ "method" : "elicitation/create" , "id" : "x\"y\\" , "result":[1,{"id":2}] }"#,
            r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"elicitation/create"}]}}"#,
            r#"{"id":null,"method":null}"#,
            r#"{"id":{"object":true},"method":"elicitation/create"}"#,
            r#"{"id":1,"id":2,"method":"elicitation/create"}"#,
            r#"{"method":7,"id":1}"#,
            r#""elicitation/create""#,
            r#"{"id":1,"method":"elicitation/create""#,
            r#"{"id":18446744073709551615,"method":"elicitation/create","params":{}}"#,
            "{\"id\":\t-4 ,\n\"method\":\"elicitation/create\"}",
        ];
        let mut array = HeaderScan::default();
        array.feed(br#"["elicitation/create",1]"#);
        assert_eq!(array.finish(), None);
        for line in lines {
            let bytes = line.as_bytes();
            let expected = serde_header(bytes);
            for split in 0..=bytes.len() {
                let mut scan = HeaderScan::default();
                scan.feed(&bytes[..split]);
                scan.feed(&bytes[split..]);
                assert_eq!(scan.finish(), expected, "{line} split at {split}");
            }
        }
    }
    #[test]
    fn oversized_stdout_lines_keep_id_reuse_accounting_without_retaining_bytes() {
        let records = RawForms::default();
        let padding = "x\\n\\\"".repeat(MAX_BYTES);
        // The method and id follow a huge params value; nothing of the line
        // beyond the form bound is retained, yet its id is still consumed.
        let huge = format!(
            r#"{{"jsonrpc":"2.0","params":{{"message":"{padding}","requestedSchema":{{"type":"object","properties":{{}}}}}},"method":"elicitation/create","id":"dup"}}"#
        );
        let mut line = LineObserver::default();
        for chunk in huge.as_bytes().chunks(4093) {
            line.feed(chunk);
        }
        assert!(line.raw.len() <= MAX_BYTES && line.scan.is_some());
        line.end(&records);
        assert!(line.raw.is_empty() && line.scan.is_none());
        let id: rmcp::model::RequestId = serde_json::from_value(serde_json::json!("dup")).unwrap();
        assert!(records.take(&id).is_none(), "oversized forms are declined");
        let valid = serde_json::json!({"jsonrpc":"2.0","id":"dup","method":"elicitation/create","params":{"message":"Public data","requestedSchema":{"type":"object","properties":{"value":{"type":"string"}}}}});
        let mut line = LineObserver::default();
        line.feed(&serde_json::to_vec(&valid).unwrap());
        line.end(&records);
        assert!(
            records.take(&id).is_none(),
            "an oversized line still consumes its callback ID"
        );
        // Large responses that merely mention the method never mint or consume.
        let response = format!(
            r#"{{"jsonrpc":"2.0","id":"fresh","result":{{"text":"elicitation/create {padding}"}}}}"#
        );
        let mut line = LineObserver::default();
        line.feed(response.as_bytes());
        line.end(&records);
        let mut fresh = valid.clone();
        fresh["id"] = serde_json::json!("fresh");
        records.observe(&serde_json::to_vec(&fresh).unwrap());
        let fresh_id = serde_json::from_value(serde_json::json!("fresh")).unwrap();
        assert!(records.take(&fresh_id).is_some());
    }
    #[test]
    fn method_prefilter_still_sees_escaped_method_names() {
        assert!(!may_name_form_method(
            br#"{"id":1,"result":{"text":"plain"}}"#
        ));
        let records = RawForms::default();
        for (n, method) in [r#""elicitation\/create""#, r#""elicitation/create""#]
            .into_iter()
            .enumerate()
        {
            let line = format!(
                r#"{{"jsonrpc":"2.0","id":{n},"method":{method},"params":{{"message":"Public","requestedSchema":{{"type":"object","properties":{{"value":{{"type":"string"}}}}}}}}}}"#
            );
            assert!(may_name_form_method(line.as_bytes()));
            records.observe(line.as_bytes());
            let id = serde_json::from_value(serde_json::json!(n)).unwrap();
            assert!(records.take(&id).is_some(), "{method}");
        }
    }
    #[test]
    fn integer_constraints_preserve_exact_large_values_and_empty_string_is_explicit() {
        let params = serde_json::json!({"message":"Choose","requestedSchema":{"type":"object","required":["count","name"],"properties":{"count":{"type":"integer","minimum":9007199254740993u64,"maximum":9007199254740993u64},"name":{"type":"string","maxLength":8}}}});
        let form = Form::parse(&params).unwrap();
        assert_eq!(
            form.questions("server").unwrap().0[1].options[0].label,
            "Empty string"
        );
        assert_eq!(
            form.answer(&[Answer::Text("9007199254740993".into()), Answer::Option(0)])
                .unwrap(),
            serde_json::json!({"count":9007199254740993u64,"name":""})
        );
        assert!(
            form.answer(&[
                Answer::Text("9007199254740992".into()),
                Answer::Text("name".into())
            ])
            .is_err()
        );
        assert_eq!(
            form.answer(&[
                Answer::Text("9007199254740993".into()),
                Answer::Text("name".into())
            ])
            .unwrap()["name"],
            "name"
        );
        let mut reversed = params.clone();
        reversed["requestedSchema"]["properties"]["count"]["maximum"] =
            serde_json::json!(9007199254740992u64);
        assert!(Form::parse(&reversed).is_err());
        let mut fractional = params;
        fractional["requestedSchema"]["properties"]["count"]["minimum"] = serde_json::json!(1.5);
        assert!(Form::parse(&fractional).is_err());
        let date = Form::parse(&serde_json::json!({"message":"Date","requestedSchema":{"type":"object","required":["date"],"properties":{"date":{"type":"string","format":"date"}}}})).unwrap();
        assert!(date.answer(&[Answer::Text("2026-1-02".into())]).is_err());
        assert!(date.answer(&[Answer::Text("2026-01-02".into())]).is_ok());
        let empty_enum = Form::parse(&serde_json::json!({"message":"Empty allowed","requestedSchema":{"type":"object","required":["value"],"properties":{"value":{"type":"string","enum":["","other"]}}}})).unwrap();
        assert_eq!(
            empty_enum.answer(&[Answer::Option(0)]).unwrap()["value"],
            ""
        );
    }
    #[test]
    fn raw_constraints_and_all_flat_primitive_enum_shapes_are_enforced() {
        let params = serde_json::json!({"message":"Choose","requestedSchema":{"type":"object","required":["name","count","ok","tags"],"properties":{"name":{"type":"string","oneOf":[{"const":"雪","title":"Snow"}]},"count":{"type":"integer","minimum":1,"maximum":3},"ok":{"type":"boolean"},"tags":{"type":"array","minItems":1,"maxItems":2,"items":{"anyOf":[{"const":"a","title":"A"},{"const":"b","title":"B"}]}}}}});
        let form = Form::parse(&params).unwrap();
        // BTreeMap property order is count/name/ok/tags.
        assert_eq!(
            form.answer(&[
                Answer::Text("2".into()),
                Answer::Option(0),
                Answer::Option(0),
                Answer::Text("[\"a\",\"b\"]".into())
            ])
            .unwrap(),
            serde_json::json!({"count":2,"name":"雪","ok":true,"tags":["a","b"]})
        );
        for bad in ["0", "2.5", "4"] {
            assert!(
                form.answer(&[
                    Answer::Text(bad.into()),
                    Answer::Option(0),
                    Answer::Option(0),
                    Answer::Text("[\"a\"]".into())
                ])
                .is_err()
            );
        }
        assert!(
            form.answer(&[
                Answer::Text("2".into()),
                Answer::Text("forged".into()),
                Answer::Option(0),
                Answer::Text("[\"a\"]".into())
            ])
            .is_err()
        );
        assert!(
            form.answer(&[
                Answer::Text("2".into()),
                Answer::Option(0),
                Answer::Option(0),
                Answer::Text("[\"a\",\"a\"]".into())
            ])
            .is_err()
        );
        let mut unknown = params.clone();
        unknown["requestedSchema"]["properties"]["name"]["pattern"] =
            Value::String("secret".into());
        assert!(Form::parse(&unknown).is_err());
        let mut malformed = params.clone();
        malformed["requestedSchema"]["properties"]["name"]["enum"] =
            Value::String("not-array".into());
        assert!(Form::parse(&malformed).is_err());
        let mut too_many = params;
        too_many["requestedSchema"]["properties"]["fifth"] = serde_json::json!({"type":"string"});
        assert!(Form::parse(&too_many).is_err());
    }
}
