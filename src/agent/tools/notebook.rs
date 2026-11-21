//! Bounded notebook JSON transformations. Opaque fields stay raw; cells never execute.
use std::fmt;

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

use super::ToolError;

/// Hard ceiling for any notebook parse; `[tool_limits] notebook_max_bytes`
/// (default 32 MiB) selects the effective limit below it.
pub(crate) const MAX_NOTEBOOK_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CELLS: usize = 4096;
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_VIEW_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_CELL: usize = 4096;

fn error(message: &'static str) -> ToolError {
    ToolError::Msg(message.into())
}
pub(crate) fn byte_limit(configured: u64) -> u64 {
    configured.min(MAX_NOTEBOOK_BYTES)
}
/// Effective notebook raw-size limit: notebooks get their own limit
/// (`[tool_limits] notebook_max_bytes`) because image outputs routinely make
/// them larger than `max_text_file_size`; a larger text cap still applies.
pub(crate) fn notebook_byte_limit(configured_text_cap: u64) -> u64 {
    byte_limit(configured_text_cap.max(super::limits::current().notebook_max_bytes))
}

// A map visitor rejects duplicate interpreted fields. Opaque nested values are
// validated JSON fragments, not recursively traversed or converted through f64.
/// JSON object members in file order, so an edit never reorders keys the
/// notebook's own writer (Colab, hand-written, converters) laid out.
#[derive(Default)]
struct OrderedMap(Vec<(String, Box<RawValue>)>);
impl OrderedMap {
    fn get(&self, key: &str) -> Option<&RawValue> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| &**v)
    }
    fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
    /// Replaces in place, or appends a new member.
    fn insert(&mut self, key: String, value: Box<RawValue>) {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some((_, slot)) => *slot = value,
            None => self.0.push((key, value)),
        }
    }
    fn remove(&mut self, key: &str) {
        self.0.retain(|(k, _)| k != key);
    }
    fn iter(&self) -> impl ExactSizeIterator<Item = (&String, &Box<RawValue>)> {
        self.0.iter().map(|(k, v)| (k, v))
    }
}
impl std::ops::Index<&str> for OrderedMap {
    type Output = RawValue;
    fn index(&self, key: &str) -> &Self::Output {
        self.get(key).expect("notebook member")
    }
}
struct Fields(OrderedMap);
impl<'de> Deserialize<'de> for Fields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> Visitor<'de> for FieldsVisitor {
            type Value = Fields;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a notebook object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Fields, A::Error> {
                let mut fields = OrderedMap::default();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.0.len() >= 256 || key.len() > 1024 || fields.contains_key(&key) {
                        return Err(serde::de::Error::custom(
                            "duplicate or oversized notebook fields",
                        ));
                    }
                    fields.0.push((key, value));
                }
                Ok(Fields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}
impl Fields {
    fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<T, ToolError> {
        self.0
            .get(key)
            .and_then(|raw| serde_json::from_str(raw.get()).ok())
            .ok_or_else(|| error("Notebook has a missing or invalid required field"))
    }
    /// Sets a cell field, encoded in the notebook's own layout at cell-field
    /// depth so the written file matches what Jupyter would produce.
    fn set<T: Serialize>(
        &mut self,
        key: &str,
        value: &T,
        layout: &Layout,
    ) -> Result<(), ToolError> {
        let encoded = layout.encode(value, CELL_FIELD_DEPTH)?;
        let raw =
            RawValue::from_string(encoded).map_err(|_| error("Cannot encode notebook field"))?;
        self.0.insert(key.into(), raw);
        Ok(())
    }
}

/// Nesting depth of a cell field value: root object (0) -> `cells` array
/// (1) -> cell object (2) -> field value (3).
const CELL_FIELD_DEPTH: usize = 3;

/// Whitespace layout of the original notebook file. nbformat writes
/// `indent=1`, sorted keys and a final newline; reproducing the detected
/// layout keeps unchanged cells byte-identical, so a git diff shows only the
/// edited cell.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    /// Indentation unit, `None` for single-line (compact) JSON.
    indent: Option<String>,
    trailing_newline: bool,
}
impl Layout {
    fn detect(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        let trimmed = text.trim_start();
        let indent = trimmed.find('\n').map(|newline| {
            let next = &trimmed[newline + 1..];
            let unit: String = next
                .chars()
                .take_while(|ch| *ch == ' ' || *ch == '\t')
                .take(16)
                .collect();
            if unit.is_empty() {
                " ".to_string()
            } else {
                unit
            }
        });
        Self {
            indent,
            trailing_newline: bytes.ends_with(b"\n"),
        }
    }
    /// Encodes `value` as it appears at `depth` in this layout.
    fn encode<T: Serialize>(&self, value: &T, depth: usize) -> Result<String, ToolError> {
        let Some(indent) = &self.indent else {
            return serde_json::to_string(value).map_err(|_| error("Cannot encode notebook field"));
        };
        let mut out = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
        let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
        value
            .serialize(&mut serializer)
            .map_err(|_| error("Cannot encode notebook field"))?;
        let pretty = String::from_utf8(out).map_err(|_| error("Cannot encode notebook field"))?;
        Ok(pretty.replace('\n', &format!("\n{}", indent.repeat(depth))))
    }
    /// Writes `{ "key": value, ... }` at `depth`; values are written by
    /// `value` (raw fragments verbatim).
    fn object<'a>(
        &self,
        out: &mut String,
        fields: impl ExactSizeIterator<Item = (&'a String, &'a RawValue)>,
        depth: usize,
        cells: Option<&[Fields]>,
    ) -> Result<(), ToolError> {
        let count = fields.len();
        if count == 0 {
            out.push_str("{}");
            return Ok(());
        }
        out.push('{');
        for (index, (key, raw)) in fields.enumerate() {
            self.break_line(out, depth + 1);
            out.push_str(&serde_json::to_string(key).map_err(|_| error("Cannot encode notebook"))?);
            out.push_str(if self.indent.is_some() { ": " } else { ":" });
            match cells {
                Some(cells) if depth == 0 && key == "cells" => self.cells(out, cells)?,
                _ => out.push_str(raw.get()),
            }
            if index + 1 < count {
                out.push(',');
            }
        }
        self.break_line(out, depth);
        out.push('}');
        Ok(())
    }
    fn cells(&self, out: &mut String, cells: &[Fields]) -> Result<(), ToolError> {
        if cells.is_empty() {
            out.push_str("[]");
            return Ok(());
        }
        out.push('[');
        for (index, cell) in cells.iter().enumerate() {
            self.break_line(out, 2);
            self.object(out, cell.0.iter().map(|(k, v)| (k, &**v)), 2, None)?;
            if index + 1 < cells.len() {
                out.push(',');
            }
        }
        self.break_line(out, 1);
        out.push(']');
        Ok(())
    }
    fn break_line(&self, out: &mut String, depth: usize) {
        if let Some(indent) = &self.indent {
            out.push('\n');
            for _ in 0..depth {
                out.push_str(indent);
            }
        }
    }
}

struct Cells(Vec<Fields>);
impl<'de> Deserialize<'de> for Cells {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CellsVisitor;
        impl<'de> Visitor<'de> for CellsVisitor {
            type Value = Cells;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a bounded list of cells")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Cells, A::Error> {
                let mut cells = Vec::new();
                while let Some(cell) = seq.next_element()? {
                    if cells.len() == MAX_CELLS {
                        return Err(serde::de::Error::custom("too many cells"));
                    }
                    cells.push(cell);
                }
                Ok(Cells(cells))
            }
        }
        deserializer.deserialize_seq(CellsVisitor)
    }
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NotebookMode {
    Replace,
    Insert,
    Delete,
}
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct NotebookArgs {
    pub path: String,
    pub mode: NotebookMode,
    #[serde(
        default,
        deserialize_with = "crate::agent::tools::lenient::cell_index_usize"
    )]
    pub cell_index: Option<usize>,
    pub cell_id: Option<String>,
    pub source: Option<String>,
    pub cell_type: Option<String>,
}
impl NotebookArgs {
    fn validate(&self) -> Result<(), ToolError> {
        if self.cell_index.is_some() == self.cell_id.is_some() {
            return Err(error("Specify exactly one cell_index or cell_id"));
        }
        if self.cell_id.as_ref().is_some_and(|id| !valid_id(id)) {
            return Err(error("Invalid cell_id"));
        }
        match self.mode {
            NotebookMode::Delete if self.source.is_some() || self.cell_type.is_some() => {
                return Err(error("Delete does not accept source or cell_type"));
            }
            NotebookMode::Insert | NotebookMode::Replace if self.source.is_none() => {
                return Err(error("Insert and replace require source"));
            }
            _ => {}
        }
        if self
            .source
            .as_ref()
            .is_some_and(|s| s.len() > MAX_SOURCE_BYTES)
        {
            return Err(error("Notebook source exceeds the 1MiB cap"));
        }
        if self
            .cell_type
            .as_deref()
            .is_some_and(|t| !matches!(t, "code" | "markdown" | "raw"))
        {
            return Err(error("cell_type must be code, markdown, or raw"));
        }
        if !is_notebook(std::path::Path::new(&self.path)) {
            return Err(error("notebook_edit requires an .ipynb path"));
        }
        Ok(())
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub(crate) fn is_notebook(path: &std::path::Path) -> bool {
    path.extension()
        .is_some_and(|s| s.eq_ignore_ascii_case("ipynb"))
}
fn source(cell: &Fields) -> Result<(String, bool), ToolError> {
    let raw = cell
        .0
        .get("source")
        .ok_or_else(|| error("Notebook cell has no source"))?;
    let (text, array) = if raw.get().trim_start().starts_with('[') {
        let lines: Vec<String> =
            serde_json::from_str(raw.get()).map_err(|_| error("Invalid notebook source array"))?;
        let size = lines
            .iter()
            .try_fold(0usize, |n, s| n.checked_add(s.len()))
            .ok_or_else(|| error("Notebook source too large"))?;
        if size > MAX_SOURCE_BYTES {
            return Err(error("Notebook source exceeds the 1MiB cap"));
        }
        (lines.concat(), true)
    } else {
        (
            serde_json::from_str::<String>(raw.get())
                .map_err(|_| error("Invalid notebook source"))?,
            false,
        )
    };
    if text.len() > MAX_SOURCE_BYTES {
        return Err(error("Notebook source exceeds the 1MiB cap"));
    }
    Ok((text, array))
}
/// How a cell's `id` relates to nbformat 4.5's unique-id rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IdState {
    Ok,
    Missing,
    Duplicate,
    Invalid,
}

/// The id of each cell and its state. Cells without ids are only a defect from
/// nbformat 4.5 on; older notebooks have none and are reported as `Ok`.
fn id_states(cells: &[Fields], minor: u64) -> Vec<(Option<String>, IdState)> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let ids: Vec<Option<Result<String, ()>>> = cells
        .iter()
        .map(|cell| {
            cell.0
                .contains_key("id")
                .then(|| cell.get::<String>("id").map_err(|_| ()))
        })
        .collect();
    for id in ids.iter().flatten().flatten() {
        *counts.entry(id.clone()).or_default() += 1;
    }
    ids.into_iter()
        .map(|id| match id {
            None if minor >= 5 => (None, IdState::Missing),
            None => (None, IdState::Ok),
            Some(Err(())) => (None, IdState::Invalid),
            Some(Ok(id)) if !valid_id(&id) => (Some(id), IdState::Invalid),
            Some(Ok(id)) if counts.get(&id).copied().unwrap_or(0) > 1 => {
                (Some(id), IdState::Duplicate)
            }
            Some(Ok(id)) => (Some(id), IdState::Ok),
        })
        .collect()
}

/// What an edit changed, for the result line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditSummary {
    index: usize,
    id: Option<String>,
    cells: usize,
}

struct Notebook {
    root: Fields,
    cells: Vec<Fields>,
    minor: u64,
    layout: Layout,
}
impl Notebook {
    /// Serializes in the original layout; unchanged fields are written from
    /// their raw source fragments.
    fn encode(&self) -> Result<Vec<u8>, ToolError> {
        let mut out = String::new();
        self.layout.object(
            &mut out,
            self.root.0.iter().map(|(k, v)| (k, &**v)),
            0,
            Some(&self.cells),
        )?;
        if self.layout.trailing_newline {
            out.push('\n');
        }
        Ok(out.into_bytes())
    }
    fn parse(bytes: &[u8], limit: u64) -> Result<Self, ToolError> {
        if bytes.len() as u64 > byte_limit(limit) {
            return Err(error(
                "Notebook exceeds the bounded raw-file cap; use raw_notebook:true for a line window",
            ));
        }
        let root: Fields = serde_json::from_slice(bytes)
            .map_err(|_| error("Invalid notebook JSON or duplicate/oversized fields"))?;
        if root.get::<u64>("nbformat")? != 4 {
            return Err(error("Only nbformat version 4 is supported"));
        }
        let minor = root.get::<u64>("nbformat_minor")?;
        let cells: Cells = root.get("cells")?;
        if !root
            .0
            .get("metadata")
            .is_some_and(|raw| raw.get().trim_start().starts_with('{'))
        {
            return Err(error("Notebook metadata must be an object"));
        }
        // Missing, duplicate and invalid ids are repairable by Jupyter, so they
        // are marked in the view and refused only as an ambiguous selector.
        for cell in &cells.0 {
            let kind: String = cell.get("cell_type")?;
            if matches!(kind.as_str(), "code" | "markdown" | "raw") {
                source(cell)?;
            }
        }
        Ok(Self {
            root,
            cells: cells.0,
            minor,
            layout: Layout::detect(bytes),
        })
    }
    #[cfg(test)]
    fn edit(self, args: &NotebookArgs, limit: u64) -> Result<Option<Vec<u8>>, ToolError> {
        Ok(self.edit_summary(args, limit)?.0)
    }
    fn edit_summary(
        mut self,
        args: &NotebookArgs,
        limit: u64,
    ) -> Result<(Option<Vec<u8>>, EditSummary), ToolError> {
        args.validate()?;
        let index = match args.cell_index {
            Some(index) => index,
            None => {
                let wanted = args.cell_id.as_deref();
                let matches: Vec<usize> = id_states(&self.cells, self.minor)
                    .iter()
                    .enumerate()
                    .filter(|(_, (id, state))| {
                        id.as_deref() == wanted && matches!(state, IdState::Ok | IdState::Duplicate)
                    })
                    .map(|(index, _)| index)
                    .collect();
                match matches.as_slice() {
                    [] => return Err(error("No cell matches the requested ID")),
                    [index] => *index,
                    several => {
                        return Err(ToolError::Msg(format!(
                            "cell_id '{}' is ambiguous: cells {} share it; address the cell with cell_index",
                            wanted.unwrap_or_default(),
                            several
                                .iter()
                                .map(usize::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )));
                    }
                }
            }
        };
        if index > self.cells.len()
            || (index == self.cells.len() && args.mode != NotebookMode::Insert)
        {
            return Err(error("cell_index is out of range"));
        }
        if args.mode != NotebookMode::Insert {
            let kind: String = self.cells[index].get("cell_type")?;
            if !matches!(kind.as_str(), "code" | "markdown" | "raw") {
                return Err(error("Cannot edit an unsupported cell type"));
            }
        }
        match args.mode {
            NotebookMode::Delete => {
                self.cells.remove(index);
            }
            NotebookMode::Replace => {
                let kind: String = self.cells[index].get("cell_type")?;
                if !matches!(kind.as_str(), "code" | "markdown" | "raw") {
                    return Err(error("Cannot replace an unsupported cell type"));
                }
                let (old, array) = source(&self.cells[index])?;
                let text = args.source.as_deref().expect("validated source");
                let new_kind = args.cell_type.as_deref().unwrap_or(&kind).to_owned();
                if old == text && new_kind == kind {
                    return Ok((None, self.summary(index)));
                }
                let layout = &self.layout;
                let cell = &mut self.cells[index];
                if new_kind != kind {
                    cell.set("cell_type", &new_kind, layout)?;
                    if new_kind == "code" {
                        // Entering code: the fields nbformat requires of one.
                        cell.set("outputs", &Vec::<u8>::new(), layout)?;
                        cell.set("execution_count", &Option::<u64>::None, layout)?;
                    } else if kind == "code" {
                        cell.0.remove("outputs");
                        cell.0.remove("execution_count");
                    }
                }
                if array {
                    cell.set(
                        "source",
                        &text.split_inclusive('\n').collect::<Vec<_>>(),
                        layout,
                    )?;
                } else {
                    cell.set("source", &text, layout)?;
                }
                // Outputs of the old source would look like results of the new one.
                if kind == "code" && new_kind == "code" {
                    cell.set("outputs", &Vec::<u8>::new(), layout)?;
                    cell.set("execution_count", &Option::<u64>::None, layout)?;
                }
            }
            NotebookMode::Insert => {
                if self.cells.len() == MAX_CELLS {
                    return Err(error("Notebook exceeds the 4096-cell cap"));
                }
                let kind = args.cell_type.as_deref().unwrap_or("code");
                let layout = &self.layout;
                let mut cell = Fields(OrderedMap::default());
                cell.set("cell_type", &kind, layout)?;
                cell.set("metadata", &serde_json::json!({}), layout)?;
                cell.set(
                    "source",
                    &args
                        .source
                        .as_deref()
                        .expect("validated source")
                        .split_inclusive('\n')
                        .collect::<Vec<_>>(),
                    layout,
                )?;
                if kind == "code" {
                    cell.set("outputs", &Vec::<u8>::new(), layout)?;
                    cell.set("execution_count", &Option::<u64>::None, layout)?;
                }
                if self.minor >= 5 {
                    cell.set("id", &uuid::Uuid::new_v4().to_string(), layout)?;
                }
                // nbformat writes keys sorted; a new cell follows that order.
                cell.0.0.sort_by(|a, b| a.0.cmp(&b.0));
                self.cells.insert(index, cell);
            }
        }
        let result = self.encode()?;
        if result.len() as u64 > byte_limit(limit) {
            return Err(error("Resulting notebook exceeds the bounded raw-file cap"));
        }
        let summary = self.summary(index);
        Ok((Some(result), summary))
    }
    fn summary(&self, index: usize) -> EditSummary {
        EditSummary {
            index,
            id: self
                .cells
                .get(index)
                .and_then(|cell| cell.get::<String>("id").ok()),
            cells: self.cells.len(),
        }
    }
}

/// Decode a notebook string field stored as a string or a list of strings.
fn multiline_text(raw: &RawValue) -> Option<String> {
    serde_json::from_str::<String>(raw.get()).ok().or_else(|| {
        serde_json::from_str::<Vec<String>>(raw.get())
            .ok()
            .map(|parts| parts.concat())
    })
}

/// Remove ANSI escape sequences (colour codes in IPython tracebacks).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // CSI: parameters/intermediates until a final byte @..~.
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// What the view shows for one output: decoded text (kept from the end when
/// `keep_tail`, so a traceback's last frames survive the budget) plus labels
/// for payloads that are not rendered.
struct RenderedOutput {
    text: Option<String>,
    keep_tail: bool,
    labels: Vec<String>,
    /// PNG/JPEG payloads (mime, whitespace-free base64) the caller may attach.
    images: Vec<(String, String)>,
}

/// Largest base64 image payload one notebook output may attach.
const MAX_OUTPUT_IMAGE_B64: usize = 8 * 1024 * 1024;

fn render_output(kind: &str, output: &Fields, want_images: bool) -> RenderedOutput {
    let mut rendered = RenderedOutput {
        text: None,
        keep_tail: false,
        labels: Vec::new(),
        images: Vec::new(),
    };
    match kind {
        "execute_result" | "display_data" | "update_display_data" => {
            let data = output
                .0
                .get("data")
                .and_then(|raw| serde_json::from_str::<Fields>(raw.get()).ok());
            if let Some(data) = data {
                for (mime, raw) in data.0.iter() {
                    if mime == "text/plain" {
                        rendered.text = multiline_text(raw).map(|text| strip_ansi(&text));
                    } else if want_images && matches!(mime.as_str(), "image/png" | "image/jpeg") {
                        match multiline_text(raw) {
                            Some(text) if text.len() <= MAX_OUTPUT_IMAGE_B64 => {
                                let b64: String =
                                    text.chars().filter(|c| !c.is_whitespace()).collect();
                                rendered.images.push((mime.clone(), b64));
                            }
                            _ => rendered
                                .labels
                                .push(format!("[{mime} output omitted: payload too large]")),
                        }
                    } else {
                        rendered.labels.push(format!("[{mime} output omitted]"));
                    }
                }
            }
        }
        "error" => {
            let ename = output.get::<String>("ename").unwrap_or_default();
            let evalue = output.get::<String>("evalue").unwrap_or_default();
            let traceback = output
                .get::<Vec<String>>("traceback")
                .unwrap_or_default()
                .join("\n");
            let mut text = format!("{ename}: {evalue}");
            if !traceback.is_empty() {
                text.push('\n');
                text.push_str(&traceback);
            }
            rendered.text = Some(strip_ansi(&text));
            rendered.keep_tail = true;
        }
        _ => rendered.text = output.0.get("text").and_then(multiline_text),
    }
    if rendered.text.is_none() && rendered.labels.is_empty() && rendered.images.is_empty() {
        rendered
            .labels
            .push("[opaque output payload omitted]".to_owned());
    }
    rendered
}

/// A virtual line window, deliberately without raw CRC/hashedit/file-line semantics.
pub(crate) struct NotebookView {
    pub text: String,
    pub crc: u32,
    /// PNG/JPEG cell outputs inside the window, decoded; image N of the text
    /// is `images[N - 1]`.
    #[cfg_attr(not(feature = "multimodal"), allow(dead_code))]
    pub images: Vec<NotebookImage>,
}
/// One decoded image output of a notebook cell.
#[cfg_attr(not(feature = "multimodal"), allow(dead_code))]
pub(crate) struct NotebookImage {
    pub bytes: Vec<u8>,
}

/// The selected slice of the notebook's virtual lines.
struct Window {
    offset: usize,
    end: usize,
    selected_cap: usize,
    selected: String,
    total: usize,
    selected_lines: usize,
    truncated_at: Option<usize>,
}
impl Window {
    /// Whether the next line would be inside the window.
    fn visible(&self) -> bool {
        self.total >= self.offset && self.total < self.end
    }
    /// Adds one virtual line; returns whether it was shown.
    fn push(&mut self, line: &str) -> bool {
        use std::fmt::Write;
        let mut shown = false;
        if self.visible() {
            if self
                .selected
                .len()
                .saturating_add(line.len())
                .saturating_add(32)
                > self.selected_cap
            {
                if self.selected_lines == 0 {
                    // A single over-cap virtual line is clipped, not refused.
                    let mut cut = self.selected_cap.saturating_sub(256).min(line.len());
                    while !line.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    writeln!(
                        &mut self.selected,
                        "{}| {} … [virtual line truncated: {} bytes; use raw_notebook:true with character_offset/character_limit for the rest]",
                        self.total + 1,
                        &line[..cut],
                        line.len()
                    )
                    .expect("string writing");
                    self.selected_lines += 1;
                    self.total += 1;
                    self.truncated_at = Some(self.total);
                    self.end = self.total;
                    return true;
                }
                // Stop the window before this line and say where to resume.
                self.truncated_at = Some(self.total);
                self.end = self.total;
                self.total += 1;
                return false;
            }
            writeln!(&mut self.selected, "{}| {}", self.total + 1, line).expect("string writing");
            self.selected_lines += 1;
            shown = true;
        }
        self.total += 1;
        shown
    }
}

#[cfg(test)]
pub(crate) fn view(
    bytes: &[u8],
    path: &str,
    offset: usize,
    limit: usize,
    byte_cap: u64,
) -> Result<NotebookView, ToolError> {
    view_with_images(bytes, path, offset, limit, byte_cap, 0)
}

/// [`view`] that also decodes up to `max_images` PNG/JPEG outputs of the cells
/// inside the window (0 keeps every image as a mime label).
pub(crate) fn view_with_images(
    bytes: &[u8],
    path: &str,
    offset: usize,
    limit: usize,
    byte_cap: u64,
    max_images: usize,
) -> Result<NotebookView, ToolError> {
    use base64::Engine;
    let notebook = Notebook::parse(bytes, byte_cap)?;
    let cap = (byte_cap as usize).min(MAX_VIEW_BYTES);
    let mut win = Window {
        offset,
        end: offset.saturating_add(limit),
        // Room for the header and continuation notice inside the view cap.
        selected_cap: cap.saturating_sub(1024),
        selected: String::new(),
        total: 0,
        selected_lines: 0,
        truncated_at: None,
    };
    let mut images: Vec<NotebookImage> = Vec::new();
    let states = id_states(&notebook.cells, notebook.minor);
    let flagged = states
        .iter()
        .filter(|(_, state)| *state != IdState::Ok)
        .count();
    for (index, cell) in notebook.cells.iter().enumerate() {
        let kind: String = cell.get("cell_type")?;
        let id = match &states[index] {
            (Some(id), IdState::Ok) => format!("; id={id}"),
            (Some(id), IdState::Duplicate) => format!("; id={id} (duplicate)"),
            (Some(id), _) => format!("; id={id} (invalid)"),
            (None, IdState::Missing) => "; id missing".to_owned(),
            (None, IdState::Invalid) => "; id invalid".to_owned(),
            (None, _) => String::new(),
        };
        win.push(&format!("[cell {index}: {kind}{id}]"));
        let text = if matches!(kind.as_str(), "code" | "markdown" | "raw") {
            source(cell)?.0
        } else {
            "[opaque unknown cell preserved]".into()
        };
        for line in text.split('\n') {
            win.push(line);
        }
        if let Some(raw) = cell.0.get("outputs") {
            // Each output stays opaque until its few display fields are decoded.
            let outputs: Cells = serde_json::from_str(raw.get())
                .map_err(|_| error("Invalid or oversized notebook outputs"))?;
            let mut cell_budget = MAX_OUTPUT_CELL;
            for (output_index, output) in outputs.0.iter().enumerate() {
                let kind = output
                    .get::<String>("output_type")
                    .unwrap_or_else(|_| "unknown".into());
                win.push(&format!("[output: {kind}]"));
                let rendered = render_output(&kind, output, max_images > 0);
                if let Some(text) = rendered.text {
                    let permitted = cell_budget.min(text.len());
                    if rendered.keep_tail {
                        let mut start = text.len() - permitted;
                        while !text.is_char_boundary(start) {
                            start += 1;
                        }
                        // The exception line itself always leads.
                        let (first, _) = text.split_once('\n').unwrap_or((&text, ""));
                        if start > 0 {
                            win.push(first);
                            win.push(&format!("[{start} traceback bytes omitted]"));
                        }
                        for line in text[start..].split('\n') {
                            win.push(line);
                        }
                        let kept = text.len() - start;
                        cell_budget -= kept;
                    } else {
                        let mut cutoff = permitted;
                        while !text.is_char_boundary(cutoff) {
                            cutoff -= 1;
                        }
                        for line in text[..cutoff].split('\n') {
                            win.push(line);
                        }
                        cell_budget -= cutoff;
                        if cutoff < text.len() {
                            win.push(&format!("[{} output bytes omitted]", text.len() - cutoff));
                        }
                    }
                }
                for label in &rendered.labels {
                    win.push(label);
                }
                for (mime, b64) in rendered.images {
                    let number = output_index + 1;
                    if !win.visible() {
                        win.push(&format!("[{mime} output omitted]"));
                        continue;
                    }
                    if images.len() >= max_images {
                        win.push(&format!(
                            "[{mime} output omitted: {max_images} image(s) already attached; read a narrower offset/limit window around cell {index} to see it]"
                        ));
                        continue;
                    }
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(&b64)
                        .ok()
                        .filter(|bytes| {
                            super::read_image::ImageFormat::from_magic(bytes).is_some()
                        });
                    let Some(decoded) = decoded else {
                        win.push(&format!("[{mime} output omitted: not a valid image]"));
                        continue;
                    };
                    images.push(NotebookImage { bytes: decoded });
                    let line = format!(
                        "[{mime} output attached as image {} (cell {index} output {number})]",
                        images.len()
                    );
                    if !win.push(&line) {
                        images.pop();
                    }
                }
            }
        }
    }
    let notice = if flagged > 0 {
        format!(
            "Notice: {flagged} cell(s) have a missing, duplicate or invalid id (marked in their headers); address them by cell_index. Ids are not rewritten.\n"
        )
    } else {
        String::new()
    };
    let header = format!(
        "Notebook cell view: {path} ({} cells; {} virtual lines; showing {} line(s) from {})\nNo raw-file CRC or full-overwrite authorization. Use raw_notebook:true for raw JSON.\n{notice}\n",
        notebook.cells.len(),
        win.total,
        win.selected_lines,
        offset.saturating_add(1)
    );
    if header.len().saturating_add(win.selected.len()) > cap {
        return Err(error(
            "Notebook view exceeds its output cap; use a narrower virtual line window",
        ));
    }
    let mut text = header + &win.selected;
    if let Some(next) = win.truncated_at {
        use std::fmt::Write;
        write!(
            &mut text,
            "\n[view truncated after {} line(s) at the {} KiB notebook view cap; continue with offset={}]",
            win.selected_lines,
            cap / 1024,
            next + 1
        )
        .expect("string writing");
    }
    let mut crc = super::crc::Crc32::new();
    crc.update(text.as_bytes());
    Ok(NotebookView {
        text,
        crc: crc.finalize(),
        images,
    })
}

use super::mutation_diff::{DiffOptions, MutationDiff, MutationOutcome};
use super::{AskSender, PermCheck, ReadTracker, check_perm_bound_path, check_perm_path};
use rig::tool::{Tool, ToolCallExtensions, ToolReturn};
use std::path::Path;
use std::sync::Arc;

pub(crate) struct NotebookTool {
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    tracker: ReadTracker,
    limit: u64,
    diff_options: DiffOptions,
    #[cfg(test)]
    publication_cancellation: Option<crate::fs::AtomicWriteCancellation>,
}
struct CancelPublication(crate::fs::AtomicWriteCancellation);
impl Drop for CancelPublication {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
impl NotebookTool {
    pub(crate) fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        workspace: Arc<crate::paths::WorkspaceBinding>,
        tracker: ReadTracker,
        limit: Option<u64>,
        diff_options: DiffOptions,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            workspace,
            tracker,
            limit: notebook_byte_limit(limit.unwrap_or(1024 * 1024)),
            diff_options,
            #[cfg(test)]
            publication_cancellation: None,
        }
    }
    async fn apply(
        &self,
        args: NotebookArgs,
        extensions: &ToolCallExtensions,
    ) -> Result<MutationOutcome, ToolError> {
        args.validate()?;
        super::validate_workspace_binding(Some(&self.workspace))?;
        let target = super::resolve_tool_target(Some(&self.workspace), &args.path);
        let bound = target.bound_relative.clone();
        let (file, identity, expected, parent, captured) = if let Some(relative) = &bound {
            let file = self
                .workspace
                .open_regular_relative(relative)
                .map_err(|err| super::bound_open_error(&self.workspace, relative, err))?;
            let expected = crate::fs::checked_file_metadata(&file)?;
            (
                file,
                self.workspace
                    .logical_relative_path(relative)?
                    .to_string_lossy()
                    .into_owned(),
                Some(expected.clone()),
                None,
                expected,
            )
        } else {
            let canonical = tokio::fs::canonicalize(&target.requested).await?;
            if !is_notebook(&canonical) {
                return Err(error(
                    "Resolved notebook target must have an .ipynb extension",
                ));
            }
            let parent = crate::fs::stable_path_metadata(
                canonical
                    .parent()
                    .ok_or_else(|| error("Notebook target has no parent"))?,
            )
            .await?;
            let file = crate::fs::open_stable_file(&canonical)
                .await?
                .into_std()
                .await;
            let metadata = crate::fs::checked_file_metadata(&file)?;
            if !metadata.is_file() {
                return Err(error("Notebook target is not a regular file"));
            }
            (
                file,
                canonical.to_string_lossy().into_owned(),
                None,
                Some(parent),
                metadata,
            )
        };
        // The prompt gives the exclusive lane back; it is re-acquired before the
        // answer is used. The target was opened and its identity captured before
        // the prompt, so it is re-captured under the re-acquired lane.
        let (coaching, released) = super::concurrency::prompt_unlocked(async {
            if let Some(relative) = &bound {
                check_perm_bound_path(
                    &self.permission,
                    &self.ask_tx,
                    "notebook_edit",
                    &self.workspace,
                    relative,
                )
                .await
            } else {
                check_perm_path(&self.permission, &self.ask_tx, "notebook_edit", &identity).await
            }
        })
        .await;
        let coaching = coaching?;
        let (file, expected) = if released {
            if let Some(relative) = &bound {
                let (file, current) =
                    super::recapture_bound_file(&self.workspace, relative, &captured, true).await?;
                (file, Some(current))
            } else {
                drop(file);
                let fresh = crate::fs::open_stable_file(Path::new(&identity))
                    .await?
                    .into_std()
                    .await;
                let current = crate::fs::checked_file_metadata(&fresh)?;
                crate::fs::ensure_same_file(Path::new(&identity), &captured, &current)?;
                (fresh, expected)
            }
        } else {
            (file, expected)
        };
        if let Some(instructions) = &self.tracker.instructions {
            let instruction_target = bound.as_deref().unwrap_or(Path::new(&identity));
            instructions
                .before_mutation(instruction_target, extensions)
                .await?;
        }
        let limit = self.limit;
        let transform_args = args.clone();
        let tracker = self.tracker.clone();
        let view_key = identity.clone();
        let (before, after, base, summary) = crate::agent::runner::spawn_blocking_scoped(move || {
            use std::io::Read;
            let metadata = file.metadata()?;
            let recorded = metadata.len();
            if recorded > limit {
                return Err(error("Notebook exceeds the bounded raw-file cap"));
            }
            // A cell_index is only meaningful against the cells the model saw:
            // refuse it when the notebook was not viewed or changed since.
            // A cell_id stays valid across unrelated changes.
            if transform_args.cell_index.is_some()
                && !tracker.notebook_view_is_current(&view_key, &metadata)
            {
                return Err(error(
                    "notebook changed since it was read, or was not read: read the notebook (a cell view) again before editing by cell_index, or address the cell with cell_id",
                ));
            }
            let mut bytes = Vec::with_capacity(recorded as usize);
            file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > limit {
                return Err(error("Notebook exceeds the bounded raw-file cap"));
            }
            let base = crate::fs::ContentDigest::of(&bytes);
            let notebook = Notebook::parse(&bytes, limit)?;
            let (after, summary) = notebook.edit_summary(&transform_args, limit)?;
            Ok::<_, ToolError>((bytes, after, base, summary))
        })
        .await
        .map_err(|_| error("Notebook preparation job failed"))??;
        let disclose =
            super::mutation_diff::preimage_allowed(&self.permission, &identity, bound.is_some());
        let mut diff = None;
        if let Some(after) = after {
            crate::spend::run::admit_current_effect()
                .await
                .map_err(|err| ToolError::Msg(err.to_string()))?;
            // Capture the exact preimage for /rewind like native write/edit.
            let before: Arc<[u8]> = Arc::from(before);
            let mut checkpoint = match &bound {
                Some(relative) => {
                    crate::session::file_checkpoints::prepare(
                        &self.workspace,
                        relative,
                        Some(crate::session::file_checkpoints::Preimage::new(
                            Arc::clone(&before),
                            &base,
                        )),
                        disclose,
                    )
                    .await
                }
                None => {
                    crate::session::file_checkpoints::skipped();
                    None
                }
            };
            #[cfg(test)]
            let cancellation = self.publication_cancellation.clone().unwrap_or_default();
            #[cfg(not(test))]
            let cancellation = crate::fs::AtomicWriteCancellation::default();
            let _guard = CancelPublication(cancellation.clone());
            if let Some(relative) = bound {
                let workspace = self.workspace.clone();
                let output = after.clone();
                let expected = expected.expect("bound metadata");
                crate::agent::runner::spawn_blocking_scoped(move || {
                    workspace.replace_relative_atomic_expecting_cancellable(
                        &relative,
                        &output,
                        &expected,
                        &base,
                        &cancellation,
                    )
                })
                .await
                .map_err(|_| error("Notebook publication job failed"))??;
            } else {
                let current = tokio::fs::canonicalize(&target.requested).await?;
                if current != Path::new(&identity) {
                    return Err(error("Notebook path changed after permission check"));
                }
                crate::fs::atomic_write_resolved_expecting_cancellable(
                    Path::new(&identity),
                    &after,
                    parent.expect("external parent"),
                    base,
                    cancellation,
                )
                .await?;
            }
            crate::session::file_checkpoints::published();
            if let Some(checkpoint) = &mut checkpoint {
                checkpoint.committed(&crate::fs::ContentDigest::of(&after), after.len());
            }
            if disclose {
                diff = MutationDiff::replacement(
                    &identity,
                    std::str::from_utf8(&before).ok(),
                    std::str::from_utf8(&after).map_err(|_| error("Invalid encoded notebook"))?,
                    self.diff_options,
                );
            }
            self.tracker.untrack_read_path(&identity);
        }
        // Never disclose whether a guessed preimage was equal when read is not authorized.
        let mut output = format!(
            "Notebook {} request completed for {}",
            match args.mode {
                NotebookMode::Replace => "replace",
                NotebookMode::Insert => "insert",
                NotebookMode::Delete => "delete",
            },
            args.path
        );
        // The index and generated id come from this tool, not from the file's
        // content, but the cell count does reveal it: report only when the
        // preimage may be disclosed.
        if disclose {
            let EditSummary { index, id, cells } = &summary;
            let id = id
                .as_deref()
                .map(|id| format!(" (id={id})"))
                .unwrap_or_default();
            match args.mode {
                NotebookMode::Insert => output.push_str(&format!(
                    ": inserted cell at index {index}{id}; notebook now has {cells} cells"
                )),
                NotebookMode::Replace => output.push_str(&format!(
                    ": replaced cell at index {index}{id}; notebook has {cells} cells"
                )),
                NotebookMode::Delete => output.push_str(&format!(
                    ": deleted cell at index {index}; notebook now has {cells} cells"
                )),
            }
        }
        if let Some(notice) = crate::session::file_checkpoints::take_uncaptured_notice() {
            output.push_str(&notice);
        }
        if let Some(coaching) = coaching {
            output = format!("{coaching}\n\n{output}");
        }
        Ok(MutationOutcome { output, diff })
    }
}
impl Tool for NotebookTool {
    const NAME: &'static str = "notebook_edit";
    type Error = ToolError;
    type Args = NotebookArgs;
    type Output = String;
    fn description(&self) -> String {
        "Edit an .ipynb notebook cell without execution. replace changes source (and the type when cell_type is given) and keeps metadata; a replaced code cell has its stale outputs and execution_count cleared. cell_index edits require a current cell view from read (the call fails when the notebook changed since); cell_id edits do not. The result names the affected index, cell id and cell count. insert adds before a zero-based index or cell ID (index=len appends); delete removes the selected cell. Exactly one cell_index or cell_id is required. Legacy notebooks retain their format version. Use read for cell views, raw_notebook:true for raw JSON.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","additionalProperties":false,"properties":{
        "path":{"type":"string"},"mode":{"type":"string","enum":["replace","insert","delete"]},
        "cell_index":{"type":"integer","minimum":0},"cell_id":{"type":"string","maxLength":64},
        "source":{"type":"string","description":"Literal new source; required for replace/insert"},
        "cell_type":{"type":"string","enum":["code","markdown","raw"],"description":"Insert: type of the new cell (default code). Replace: converts the cell to this type (outputs/execution_count are dropped when leaving code and added when entering it)"}},"required":["path","mode"]})
    }
    async fn call(&self, args: NotebookArgs) -> Result<String, ToolError> {
        Ok(self.apply(args, &ToolCallExtensions::new()).await?.output)
    }
    async fn call_structured(
        &self,
        args: NotebookArgs,
        extensions: &ToolCallExtensions,
    ) -> Result<ToolReturn<String>, ToolError> {
        match self.apply(args, extensions).await {
            Ok(outcome) => Ok(outcome.into_return()),
            Err(ToolError::ContextReview(notice)) => Ok(notice.not_executed()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::checker::{CheckResult, PermissionChecker};
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};
    const FIXTURE: &str = r##"{"nbformat":4,"nbformat_minor":5,"metadata":{"huge":1234567890123456789012345678901234567890},"cells":[{"cell_type":"code","id":"first","source":["old\r\n","last"],"metadata":{"private":"neighbor"},"outputs":[{"output_type":"stream","name":"stdout","text":["kept\n"],"huge":1234567890123456789012345678901234567890}],"execution_count":7},{"cell_type":"markdown","id":"second","source":"# title","metadata":{},"attachments":{"image":{"image/png":"opaque"}}}]}"##;
    fn args(mode: NotebookMode, source: Option<&str>) -> NotebookArgs {
        NotebookArgs {
            path: "book.ipynb".into(),
            mode,
            cell_index: Some(0),
            cell_id: None,
            source: source.map(str::to_owned),
            cell_type: None,
        }
    }
    /// As [`args`], addressing the first fixture cell by its id (no prior view
    /// of the file is needed for an id).
    fn id_args(mode: NotebookMode, source: Option<&str>) -> NotebookArgs {
        NotebookArgs {
            cell_index: None,
            cell_id: Some("first".into()),
            ..args(mode, source)
        }
    }
    fn transformed(input: &str, args: NotebookArgs) -> String {
        String::from_utf8(
            Notebook::parse(input.as_bytes(), MAX_NOTEBOOK_BYTES)
                .unwrap()
                .edit(&args, MAX_NOTEBOOK_BYTES)
                .unwrap()
                .unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn notebook_replace_preserves_raw_outputs_metadata_attachments_and_source_shape() {
        let output = transformed(FIXTURE, args(NotebookMode::Replace, Some("new\r\nlast\n")));
        let notebook = Notebook::parse(output.as_bytes(), MAX_NOTEBOOK_BYTES).unwrap();
        let old = Notebook::parse(FIXTURE.as_bytes(), MAX_NOTEBOOK_BYTES).unwrap();
        assert_eq!(
            notebook.cells[0].0["metadata"].get(),
            old.cells[0].0["metadata"].get()
        );
        // The old outputs belong to the old source.
        assert_eq!(notebook.cells[0].0["outputs"].get(), "[]");
        assert_eq!(notebook.cells[0].0["execution_count"].get(), "null");
        assert_eq!(
            notebook.root.0["metadata"].get(),
            old.root.0["metadata"].get()
        );
        assert_eq!(
            notebook.cells[1].0["attachments"].get(),
            old.cells[1].0["attachments"].get()
        );
        assert_eq!(
            source(&notebook.cells[0]).unwrap(),
            ("new\r\nlast\n".into(), true)
        );
        let mut by_id = args(NotebookMode::Replace, Some("literal $HOME\n!cmd"));
        by_id.cell_index = None;
        by_id.cell_id = Some("second".into());
        let output = transformed(FIXTURE, by_id);
        let notebook = Notebook::parse(output.as_bytes(), MAX_NOTEBOOK_BYTES).unwrap();
        assert_eq!(
            source(&notebook.cells[1]).unwrap(),
            ("literal $HOME\n!cmd".into(), false)
        );
    }
    /// As written by Jupyter/nbformat: indent 1, sorted keys, final newline.
    const JUPYTER: &str = "{\n \"cells\": [\n  {\n   \"cell_type\": \"code\",\n   \"execution_count\": 3,\n   \"id\": \"a1\",\n   \"metadata\": {},\n   \"outputs\": [\n    {\n     \"name\": \"stdout\",\n     \"output_type\": \"stream\",\n     \"text\": [\n      \"1\\n\"\n     ]\n    }\n   ],\n   \"source\": [\n    \"print(1)\\n\",\n    \"x = 1\"\n   ]\n  },\n  {\n   \"cell_type\": \"markdown\",\n   \"id\": \"b2\",\n   \"metadata\": {},\n   \"source\": [\n    \"# Title é\"\n   ]\n  }\n ],\n \"metadata\": {\n  \"kernelspec\": {\n   \"name\": \"python3\"\n  }\n },\n \"nbformat\": 4,\n \"nbformat_minor\": 5\n}\n";

    #[test]
    fn notebook_round_trip_is_byte_identical_and_edits_touch_only_their_cell() {
        let notebook = Notebook::parse(JUPYTER.as_bytes(), MAX_NOTEBOOK_BYTES).unwrap();
        assert_eq!(
            String::from_utf8(notebook.encode().unwrap()).unwrap(),
            JUPYTER
        );
        // The compact fixture keeps its compact layout too.
        let compact = Notebook::parse(FIXTURE.as_bytes(), MAX_NOTEBOOK_BYTES).unwrap();
        let reencoded = String::from_utf8(compact.encode().unwrap()).unwrap();
        assert!(!reencoded.contains('\n'), "{reencoded}");
        // Key order is the file's own, not alphabetical.
        assert_eq!(reencoded, FIXTURE);
        assert_eq!(
            Notebook::parse(reencoded.as_bytes(), MAX_NOTEBOOK_BYTES)
                .unwrap()
                .cells
                .len(),
            2
        );

        let mut replace = args(NotebookMode::Replace, Some("print(2)\n"));
        replace.cell_id = Some("a1".into());
        replace.cell_index = None;
        let output = transformed(JUPYTER, replace);
        let expected = JUPYTER
            .replace(
                "   \"execution_count\": 3,\n",
                "   \"execution_count\": null,\n",
            )
            .replace(
                "   \"outputs\": [\n    {\n     \"name\": \"stdout\",\n     \"output_type\": \"stream\",\n     \"text\": [\n      \"1\\n\"\n     ]\n    }\n   ],\n",
                "   \"outputs\": [],\n",
            )
            .replace(
                "    \"print(1)\\n\",\n    \"x = 1\"\n",
                "    \"print(2)\\n\"\n",
            );
        assert_eq!(output, expected);

        let mut markdown = args(NotebookMode::Replace, Some("# New"));
        markdown.cell_id = Some("b2".into());
        markdown.cell_index = None;
        let output = transformed(JUPYTER, markdown);
        assert_eq!(output, JUPYTER.replace("\"# Title é\"", "\"# New\""));

        let mut insert = args(NotebookMode::Insert, Some("y = 2"));
        insert.cell_index = Some(2);
        let output = transformed(JUPYTER, insert);
        assert!(output.starts_with(&JUPYTER[..JUPYTER.find("\n ],\n").unwrap()]));
        assert!(
            output.contains(",\n  {\n   \"cell_type\": \"code\",\n   \"execution_count\": null,\n"),
            "{output}"
        );
        assert!(output.ends_with("\n \"nbformat_minor\": 5\n}\n"));
    }

    #[test]
    fn notebook_insert_delete_selectors_and_legacy_ids() {
        for kind in ["code", "markdown", "raw"] {
            let mut insert = args(NotebookMode::Insert, Some("α\n"));
            insert.cell_index = Some(2);
            insert.cell_type = Some(kind.into());
            let output = transformed(FIXTURE, insert);
            let notebook = Notebook::parse(output.as_bytes(), MAX_NOTEBOOK_BYTES).unwrap();
            assert_eq!(notebook.cells.len(), 3);
            assert_eq!(notebook.cells[2].get::<String>("cell_type").unwrap(), kind);
            assert!(valid_id(&notebook.cells[2].get::<String>("id").unwrap()));
            assert_eq!(notebook.cells[2].0.contains_key("outputs"), kind == "code");
        }
        let legacy = FIXTURE.replace("\"nbformat_minor\":5", "\"nbformat_minor\":4");
        let output = transformed(&legacy, args(NotebookMode::Insert, Some("x")));
        let notebook = Notebook::parse(output.as_bytes(), MAX_NOTEBOOK_BYTES).unwrap();
        assert_eq!(notebook.minor, 4);
        assert!(!notebook.cells[0].0.contains_key("id"));
        let mut delete = args(NotebookMode::Delete, None);
        delete.cell_index = None;
        delete.cell_id = Some("second".into());
        let output = transformed(FIXTURE, delete);
        assert_eq!(
            Notebook::parse(output.as_bytes(), MAX_NOTEBOOK_BYTES)
                .unwrap()
                .cells
                .len(),
            1
        );
    }
    #[test]
    fn notebook_invalid_schema_ids_selectors_and_caps_fail_without_disclosing_values() {
        for input in [
            FIXTURE.replace("\"nbformat\":4", "\"nbformat\":3"),
            FIXTURE.replace("\"nbformat\":4", "\"nbformat\":4,\"nbformat\":4"),
            FIXTURE.replace("\"source\":\"# title\"", "\"source\":{\"secret\":1}"),
        ] {
            let err = Notebook::parse(input.as_bytes(), MAX_NOTEBOOK_BYTES)
                .err()
                .unwrap()
                .to_string();
            assert!(!err.contains("neighbor"));
            assert!(!err.contains("# title"));
        }
        let mut bad = args(NotebookMode::Replace, Some("x"));
        bad.cell_id = Some("first".into());
        assert!(bad.validate().is_err());
        bad.cell_id = None;
        bad.cell_index = Some(usize::MAX);
        assert!(
            Notebook::parse(FIXTURE.as_bytes(), MAX_NOTEBOOK_BYTES)
                .unwrap()
                .edit(&bad, MAX_NOTEBOOK_BYTES)
                .is_err()
        );
        assert!(Notebook::parse(FIXTURE.as_bytes(), 10).is_err());
        let mut long = args(NotebookMode::Replace, Some("x"));
        long.source = Some("x".repeat(MAX_SOURCE_BYTES + 1));
        assert!(long.validate().is_err());
        let cells = format!(
            "{{\"nbformat\":4,\"nbformat_minor\":4,\"metadata\":{{}},\"cells\":[{}]}}",
            vec!["{}"; MAX_CELLS + 1].join(",")
        );
        assert!(Notebook::parse(cells.as_bytes(), MAX_NOTEBOOK_BYTES).is_err());
        assert!(
            Notebook::parse(FIXTURE.as_bytes(), MAX_NOTEBOOK_BYTES)
                .unwrap()
                .edit(
                    &args(NotebookMode::Replace, Some("old\r\nlast")),
                    MAX_NOTEBOOK_BYTES
                )
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn notebook_opaque_deep_metadata_is_byte_bounded_and_never_recursively_walked() {
        let nested = format!("{}0{}", "[".repeat(20_000), "]".repeat(20_000));
        let fixture = FIXTURE.replace(
            "{\"huge\":1234567890123456789012345678901234567890}",
            &format!("{{\"opaque\":{nested}}}"),
        );
        let output = transformed(&fixture, args(NotebookMode::Replace, Some("new")));
        assert!(output.contains(&nested));
        assert!(Notebook::parse(fixture.as_bytes(), 100).is_err());
    }
    #[test]
    fn notebook_view_reports_virtual_lines_and_bounded_output_without_raw_tags() {
        let rendered = view(FIXTURE.as_bytes(), "book.ipynb", 0, 200, MAX_NOTEBOOK_BYTES)
            .unwrap()
            .text;
        assert!(rendered.contains("[cell 0: code; id=first]"));
        assert!(rendered.contains("kept"));
        assert!(!rendered.contains("1234567890123456789012345678901234567890"));
        assert!(!rendered.contains("[CRC:"));
        let narrow = view(FIXTURE.as_bytes(), "book.ipynb", 1, 1, MAX_NOTEBOOK_BYTES)
            .unwrap()
            .text;
        assert!(narrow.contains("2| old"));
        assert!(!narrow.contains("# title"));
        let huge = FIXTURE.replace(
            "\"kept\\n\"",
            &serde_json::to_string(&"🙂".repeat(30_000)).unwrap(),
        );
        let output = view(huge.as_bytes(), "book.ipynb", 0, 200, MAX_NOTEBOOK_BYTES)
            .unwrap()
            .text;
        assert!(output.contains("output bytes omitted"));
        assert!(output.len() < MAX_VIEW_BYTES);
    }
    /// mini-agent-0eep6: execute_result/display_data text/plain, error
    /// tracebacks (ANSI-stripped) and image mime labels are rendered.
    #[test]
    fn later_cell_outputs_are_visible_in_a_window_that_starts_late() {
        let cells: Vec<String> = (0..30)
            .map(|i| {
                format!(
                    r#"{{"cell_type":"code","id":"c{i}","metadata":{{}},"execution_count":null,"source":["x"],"outputs":[{{"output_type":"stream","name":"stdout","text":["{}"]}}]}}"#,
                    format!("out{i}-").repeat(150)
                )
            })
            .collect();
        let nb = format!(
            r#"{{"cells":[{}],"metadata":{{}},"nbformat":4,"nbformat_minor":5}}"#,
            cells.join(",")
        );
        // Find the first virtual line of cell 25 by viewing everything once.
        let all = view(nb.as_bytes(), "n.ipynb", 0, 100_000, 32 * 1024 * 1024).unwrap();
        let marker = all
            .text
            .lines()
            .find(|l| l.ends_with("[cell 25: code; id=c25]"))
            .unwrap();
        let line_no: usize = marker.split('|').next().unwrap().parse().unwrap();
        let window = view(nb.as_bytes(), "n.ipynb", line_no - 1, 8, 32 * 1024 * 1024).unwrap();
        assert!(window.text.contains("out25-out25-"), "{}", window.text);
        assert!(
            !window.text.contains("output bytes omitted"),
            "{}",
            window.text
        );
    }

    #[test]
    fn notebook_view_renders_results_errors_and_labels_images() {
        let outputs = serde_json::json!([
            {"output_type":"execute_result","execution_count":1,"metadata":{},
             "data":{"text/plain":["   a  b\n","0  1  2"],"text/html":"<table/>"}},
            {"output_type":"display_data","metadata":{},
             "data":{"image/png":"iVBORw0KGgo=","text/plain":"<Figure size 640x480>"}},
            {"output_type":"error","ename":"ZeroDivisionError","evalue":"division by zero",
             "traceback":["\u{1b}[0;31m---------\u{1b}[0m","\u{1b}[0;32mCell In[1], line 1\u{1b}[0m\n----> 1 1/0"]},
            {"output_type":"display_data","metadata":{},"data":{"image/jpeg":"/9j/"}}
        ]);
        let notebook = serde_json::json!({
            "nbformat":4,"nbformat_minor":5,"metadata":{},
            "cells":[{"cell_type":"code","id":"c","source":"df","metadata":{},
                      "execution_count":1,"outputs":outputs}]
        })
        .to_string();
        let text = view(
            notebook.as_bytes(),
            "book.ipynb",
            0,
            200,
            MAX_NOTEBOOK_BYTES,
        )
        .unwrap()
        .text;
        for expected in [
            "[output: execute_result]",
            "| 0  1  2",
            "[text/html output omitted]",
            "| <Figure size 640x480>",
            "[image/png output omitted]",
            "| ZeroDivisionError: division by zero",
            "| Cell In[1], line 1",
            "| ----> 1 1/0",
            "[image/jpeg output omitted]",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text}");
        }
        assert!(!text.contains('\u{1b}'), "{text}");
        assert!(!text.contains("opaque output payload"), "{text}");
        assert!(!text.contains("iVBORw0KGgo"), "{text}");

        // An oversized traceback keeps the exception line and the last frames.
        let frames: Vec<String> = (0..2_000).map(|n| format!("frame {n}")).collect();
        let long = serde_json::json!({
            "nbformat":4,"nbformat_minor":5,"metadata":{},
            "cells":[{"cell_type":"code","id":"c","source":"x","metadata":{},
                      "execution_count":1,"outputs":[{"output_type":"error","ename":"E",
                      "evalue":"boom","traceback":frames}]}]
        })
        .to_string();
        let text = view(long.as_bytes(), "book.ipynb", 0, 5_000, MAX_NOTEBOOK_BYTES)
            .unwrap()
            .text;
        assert!(text.contains("| E: boom"), "{text}");
        assert!(text.contains("traceback bytes omitted]"), "{text}");
        assert!(text.contains("| frame 1999"), "{text}");
        assert!(!text.contains("| frame 5\n"), "{text}");
    }
    /// mini-agent-qsttm: a window over the 64 KiB view cap is truncated with
    /// the exact next offset instead of failing; a lone huge line is clipped.
    #[test]
    fn notebook_view_truncates_oversized_windows_with_a_next_offset() {
        let source: Vec<String> = (0..2_000)
            .map(|n| format!("{n:04} {}\n", "s".repeat(95)))
            .collect();
        let notebook = serde_json::json!({
            "nbformat":4,"nbformat_minor":5,"metadata":{},
            "cells":[{"cell_type":"code","id":"c","source":source,"metadata":{},
                      "execution_count":null,"outputs":[]}]
        })
        .to_string();
        let first = view(
            notebook.as_bytes(),
            "book.ipynb",
            0,
            5_000,
            MAX_NOTEBOOK_BYTES,
        )
        .unwrap()
        .text;
        assert!(first.len() <= MAX_VIEW_BYTES, "{}", first.len());
        let next: usize = first
            .split("continue with offset=")
            .nth(1)
            .and_then(|rest| rest.trim_end_matches(']').parse().ok())
            .expect("next offset");
        assert!(first.contains(&format!("{}| ", next - 1)), "{first:.200}");
        assert!(!first.contains(&format!("\n{next}| ")), "{first:.200}");
        let second = view(
            notebook.as_bytes(),
            "book.ipynb",
            next - 1,
            5,
            MAX_NOTEBOOK_BYTES,
        )
        .unwrap()
        .text;
        assert!(second.contains(&format!("\n{next}| ")), "{second}");

        let wide = serde_json::json!({
            "nbformat":4,"nbformat_minor":5,"metadata":{},
            "cells":[{"cell_type":"markdown","id":"m","source":"w".repeat(200_000),"metadata":{}}]
        })
        .to_string();
        let clipped = view(wide.as_bytes(), "book.ipynb", 1, 1, MAX_NOTEBOOK_BYTES)
            .unwrap()
            .text;
        assert!(
            clipped.contains("[virtual line truncated: 200000 bytes"),
            "{:.200}",
            &clipped[clipped.len() - 300..]
        );
        assert!(clipped.len() <= MAX_VIEW_BYTES);
    }
    fn policy(root: &Path, mode: SecurityMode, config: PermissionConfig) -> PermCheck {
        Arc::new(std::sync::Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(config),
                mode,
                Some(root.into()),
                None,
            )
            .unwrap(),
        ))
    }
    fn tool(
        root: &Path,
        policy: Option<PermCheck>,
        ask: Option<AskSender>,
        tracker: ReadTracker,
    ) -> NotebookTool {
        NotebookTool::new(
            policy,
            ask,
            super::super::capture_workspace_binding(root.into()),
            tracker,
            None,
            DiffOptions::default(),
        )
    }
    #[test]
    fn notebook_permission_precedence_fallback_modes_and_no_inherited_edit_grant() {
        let root = crate::tests::OwnedTestDirectory::new("notebook-policy");
        let default_deny = policy(
            root.path(),
            SecurityMode::Standard,
            PermissionConfig {
                default: Some(Action::Deny),
                ..Default::default()
            },
        );
        assert!(matches!(
            default_deny
                .lock()
                .unwrap()
                .check_path("notebook_edit", "book.ipynb"),
            CheckResult::Denied(_)
        ));
        for mode in [
            SecurityMode::Standard,
            SecurityMode::Guarded,
            SecurityMode::ReadOnly,
            SecurityMode::PlanWrite,
        ] {
            let p = policy(root.path(), mode, PermissionConfig::default());
            let expected = if matches!(mode, SecurityMode::ReadOnly | SecurityMode::PlanWrite) {
                CheckResult::Denied("Blocked by permission rules".into())
            } else {
                CheckResult::Ask
            };
            let result = p.lock().unwrap().check_path("notebook_edit", "book.ipynb");
            if matches!(expected, CheckResult::Denied(_)) {
                assert!(matches!(result, CheckResult::Denied(_)));
            } else {
                assert_eq!(result, expected);
            }
        }
        let p = policy(
            root.path(),
            SecurityMode::Standard,
            PermissionConfig {
                edit: Some(ToolPerm::Simple(Action::Allow)),
                notebook_edit: Some(ToolPerm::Simple(Action::Ask)),
                ..Default::default()
            },
        );
        assert_eq!(
            p.lock().unwrap().check_path("notebook_edit", "book.ipynb"),
            CheckResult::Ask
        );
        let p = policy(
            root.path(),
            SecurityMode::Standard,
            PermissionConfig {
                edit: Some(ToolPerm::Simple(Action::Allow)),
                ..Default::default()
            },
        );
        assert_eq!(
            p.lock().unwrap().check_path("notebook_edit", "book.ipynb"),
            CheckResult::Allowed
        );
        for (own, edit) in [(Action::Deny, Action::Allow), (Action::Allow, Action::Deny)] {
            let p = policy(
                root.path(),
                SecurityMode::Yolo,
                PermissionConfig {
                    edit: Some(ToolPerm::Simple(edit)),
                    notebook_edit: Some(ToolPerm::Simple(own)),
                    ..Default::default()
                },
            );
            assert!(matches!(
                p.lock().unwrap().check_path("notebook_edit", "book.ipynb"),
                CheckResult::Denied(_)
            ));
        }
    }
    #[test]
    fn notebook_edit_session_grants_do_not_become_notebook_grants_and_denies_still_win() {
        let root = crate::tests::OwnedTestDirectory::new("notebook-grants");
        let p = policy(
            root.path(),
            SecurityMode::Standard,
            PermissionConfig::default(),
        );
        p.lock().unwrap().add_session_allowlist("edit".into(), "**");
        assert_eq!(
            p.lock().unwrap().check_path("edit", "book.ipynb"),
            CheckResult::Allowed
        );
        assert_eq!(
            p.lock().unwrap().check_path("notebook_edit", "book.ipynb"),
            CheckResult::Ask
        );
        let p = policy(
            root.path(),
            SecurityMode::Standard,
            PermissionConfig {
                edit: Some(ToolPerm::Simple(Action::Deny)),
                ..Default::default()
            },
        );
        p.lock()
            .unwrap()
            .add_session_allowlist("notebook_edit".into(), "**");
        assert!(matches!(
            p.lock().unwrap().check_path("notebook_edit", "book.ipynb"),
            CheckResult::Denied(_)
        ));
    }

    #[tokio::test]
    async fn notebook_actual_mutation_private_noop_and_change_have_same_shape_and_no_diff() {
        let root = crate::tests::OwnedTestDirectory::new("notebook-private");
        std::fs::write(root.path().join("book.ipynb"), FIXTURE).unwrap();
        for read in [Action::Deny, Action::Ask] {
            let p = policy(
                root.path(),
                SecurityMode::Standard,
                PermissionConfig {
                    notebook_edit: Some(ToolPerm::Simple(Action::Allow)),
                    read: Some(ToolPerm::Simple(read)),
                    ..Default::default()
                },
            );
            let tool = tool(root.path(), Some(p), None, ReadTracker::new(true));
            let noop = tool
                .call_structured(
                    id_args(NotebookMode::Replace, Some("old\r\nlast")),
                    &ToolCallExtensions::new(),
                )
                .await
                .unwrap();
            let changed = tool
                .call_structured(
                    id_args(NotebookMode::Replace, Some("new")),
                    &ToolCallExtensions::new(),
                )
                .await
                .unwrap();
            assert_eq!(noop.output, changed.output);
            assert!(noop.extensions.get::<MutationDiff>().is_none());
            assert!(changed.extensions.get::<MutationDiff>().is_none());
            assert!(!changed.output.contains("neighbor"));
            assert_eq!(
                source(
                    &Notebook::parse(
                        &std::fs::read(root.path().join("book.ipynb")).unwrap(),
                        MAX_NOTEBOOK_BYTES
                    )
                    .unwrap()
                    .cells[0]
                )
                .unwrap()
                .0,
                "new"
            );
            std::fs::write(root.path().join("book.ipynb"), FIXTURE).unwrap();
        }
    }
    #[tokio::test]
    async fn notebook_actual_read_view_never_authorizes_full_raw_overwrite() {
        use super::super::{ReadArgs, ReadTool, WriteArgs, WriteTool};
        let root = crate::tests::OwnedTestDirectory::new("notebook-read");
        std::fs::write(root.path().join("book.ipynb"), FIXTURE).unwrap();
        let tracker = ReadTracker::new(true);
        let read = ReadTool::new_with_tracker(None, None, None, 200, tracker.clone())
            .with_workspace(root.path());
        let write =
            WriteTool::new_with_tracker(None, None, None, tracker).with_workspace(root.path());
        let view = read
            .call(ReadArgs {
                pages: None,
                path: "book.ipynb".into(),
                raw_notebook: false,
                visual: false,
                full_resolution: false,
                character_offset: None,
                character_limit: None,
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        assert!(view.starts_with("Notebook cell view:"));
        let write_args = || WriteArgs {
            path: "book.ipynb".into(),
            content: FIXTURE.into(),
            overwrite: true,
        };
        assert!(write.call(write_args()).await.is_err());
        let raw = read
            .call(ReadArgs {
                pages: None,
                path: "book.ipynb".into(),
                raw_notebook: true,
                visual: false,
                full_resolution: false,
                character_offset: None,
                character_limit: None,
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        assert!(raw.contains("nbformat"));
        write.call(write_args()).await.unwrap();
    }
    #[tokio::test]
    async fn notebook_actual_success_emits_typed_snapshot_and_headless_ask_fails_closed() {
        let root = crate::tests::OwnedTestDirectory::new("notebook-success");
        std::fs::write(root.path().join("book.ipynb"), FIXTURE).unwrap();
        let p = policy(
            root.path(),
            SecurityMode::Standard,
            PermissionConfig::default(),
        );
        assert!(
            tool(root.path(), Some(p), None, ReadTracker::default())
                .call(id_args(NotebookMode::Replace, Some("new")))
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("book.ipynb")).unwrap(),
            FIXTURE
        );
        let result = tool(root.path(), None, None, ReadTracker::default())
            .call_structured(
                id_args(NotebookMode::Replace, Some("new")),
                &ToolCallExtensions::new(),
            )
            .await
            .unwrap();
        assert!(result.extensions.get::<MutationDiff>().unwrap().bounded());
    }
    #[tokio::test]
    async fn notebook_actual_ask_allow_once_deny_and_reply_drop_never_install_grants() {
        use crate::permission::ask::UserDecision;
        for answer in [
            Some(UserDecision::AllowOnce),
            Some(UserDecision::Deny),
            None,
        ] {
            let root = crate::tests::OwnedTestDirectory::new("notebook-ask");
            std::fs::write(root.path().join("book.ipynb"), FIXTURE).unwrap();
            let p = policy(
                root.path(),
                SecurityMode::Standard,
                PermissionConfig::default(),
            );
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            let tool = tool(
                root.path(),
                Some(p.clone()),
                Some(tx),
                ReadTracker::default(),
            );
            let task = tokio::spawn(async move {
                tool.call(id_args(NotebookMode::Replace, Some("new"))).await
            });
            let prompt = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .unwrap()
                .unwrap()
                .into_permission();
            assert_eq!(prompt.tool.as_str(), "notebook_edit");
            assert!(prompt.additional_allow_patterns.is_empty());
            let allowed = matches!(answer, Some(UserDecision::AllowOnce));
            if let Some(answer) = answer {
                prompt.reply.send(answer).unwrap();
            } else {
                drop(prompt.reply);
            }
            let result = task.await.unwrap();
            assert_eq!(result.is_ok(), allowed);
            assert_eq!(
                p.lock().unwrap().check_path("notebook_edit", "book.ipynb"),
                CheckResult::Ask
            );
            let current = std::fs::read_to_string(root.path().join("book.ipynb")).unwrap();
            if allowed {
                assert_eq!(
                    source(
                        &Notebook::parse(current.as_bytes(), MAX_NOTEBOOK_BYTES)
                            .unwrap()
                            .cells[0]
                    )
                    .unwrap()
                    .0,
                    "new"
                );
            } else {
                assert_eq!(current, FIXTURE);
            }
        }
    }
    struct ReleaseProbe(crate::fs::AtomicWritePublicationProbe);
    impl Drop for ReleaseProbe {
        fn drop(&mut self) {
            self.0.resume();
        }
    }
    #[tokio::test]
    async fn notebook_cancel_before_bound_or_external_publication_joins_owned_job_without_write() {
        for external in [false, true] {
            let root = crate::tests::OwnedTestDirectory::new("notebook-cancel");
            let path = root.path().join("book.ipynb");
            std::fs::write(&path, FIXTURE).unwrap();
            let scope = crate::agent::runner::AgentWorkScope::new();
            let (cancel, probe) =
                crate::fs::AtomicWriteCancellation::with_publication_probe_for_test();
            let _release = ReleaseProbe(probe.clone());
            let mut tool = tool(root.path(), None, None, ReadTracker::default());
            tool.publication_cancellation = Some(cancel);
            let mut request = id_args(NotebookMode::Replace, Some("new"));
            if external {
                request.path = path.to_string_lossy().into_owned();
            }
            let work = scope.clone();
            let task = tokio::spawn(async move { work.run(tool.call(request)).await });
            tokio::task::spawn_blocking(move || probe.wait_until_reached())
                .await
                .unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(scope.active_children() > 0);
            _release.0.resume();
            tokio::time::timeout(std::time::Duration::from_secs(3), scope.wait_idle())
                .await
                .unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), FIXTURE);
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        }
    }
    #[tokio::test]
    async fn notebook_cas_refuses_inplace_rewrite_at_exchange_and_restores_newer_bytes() {
        let root = crate::tests::OwnedTestDirectory::new("notebook-cas");
        let path = root.path().join("book.ipynb");
        std::fs::write(&path, FIXTURE).unwrap();
        let (cancel, probe) = crate::fs::AtomicWriteCancellation::with_publication_probe_for_test();
        let _release = ReleaseProbe(probe.clone());
        let mut tool = tool(root.path(), None, None, ReadTracker::default());
        tool.publication_cancellation = Some(cancel);
        let task =
            tokio::spawn(
                async move { tool.call(id_args(NotebookMode::Replace, Some("new"))).await },
            );
        tokio::task::spawn_blocking(move || probe.wait_until_reached())
            .await
            .unwrap();
        let newer = FIXTURE.replace("old\\r\\n", "WIN\\r\\n");
        assert_ne!(newer, FIXTURE);
        std::fs::write(&path, &newer).unwrap();
        _release.0.resume();
        assert!(task.await.unwrap().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
    #[cfg(unix)]
    #[tokio::test]
    #[allow(unsafe_code)] // Test-only mkfifo on an exclusively owned fixture path.
    async fn notebook_bound_fifo_and_symlink_are_refused_without_blocking_or_effects() {
        use std::os::unix::fs::symlink;
        let root = crate::tests::OwnedTestDirectory::new("notebook-nonregular");
        std::fs::write(root.path().join("target.ipynb"), FIXTURE).unwrap();
        symlink("target.ipynb", root.path().join("book.ipynb")).unwrap();
        assert!(
            tool(root.path(), None, None, ReadTracker::default())
                .call(id_args(NotebookMode::Replace, Some("new")))
                .await
                .is_err()
        );
        std::fs::remove_file(root.path().join("book.ipynb")).unwrap();
        let fifo = std::ffi::CString::new(
            root.path()
                .join("book.ipynb")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();
        // SAFETY: a valid owned NUL-terminated fixture pathname, no borrowed kernel state.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        // Keep a broken synchronous FIFO open off the test event loop. A
        // timeout always releases it with a writer/EOF before joining the job.
        let scope = crate::agent::runner::AgentWorkScope::new();
        let worker_scope = scope.clone();
        let tool = tool(root.path(), None, None, ReadTracker::default());
        let mut job = scope
            .run(async move {
                (crate::agent::runner::spawn_blocking_scoped(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    runtime.block_on(
                        worker_scope.run(tool.call(id_args(NotebookMode::Replace, Some("new")))),
                    )
                }),)
            })
            .await
            .0;
        let observed = tokio::time::timeout(std::time::Duration::from_secs(2), &mut job).await;
        let timed_out = observed.is_err();
        let result = match observed {
            Ok(result) => result.unwrap(),
            Err(_) => {
                use std::os::unix::fs::OpenOptionsExt;
                let release = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(root.path().join("book.ipynb"))
                    .unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                drop(release);
                tokio::time::timeout(std::time::Duration::from_secs(3), job)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        scope.wait_idle().await;
        assert!(
            !timed_out,
            "FIFO rejection must return before the controlled writer/EOF release"
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("target.ipynb")).unwrap(),
            FIXTURE
        );
    }
    #[cfg(feature = "hooks")]
    #[test]
    fn notebook_pinned_hook_preimage_cannot_be_mutated_even_with_yolo_allow() {
        let root = crate::tests::OwnedTestDirectory::new("notebook-hook-pin");
        let p = policy(
            root.path(),
            SecurityMode::Yolo,
            PermissionConfig {
                notebook_edit: Some(ToolPerm::Simple(Action::Allow)),
                ..Default::default()
            },
        );
        p.lock()
            .unwrap()
            .set_pinned_hook_files_for_test(vec![Path::new("book.ipynb").to_path_buf()]);
        assert!(matches!(
            p.lock().unwrap().check_path("notebook_edit", "book.ipynb"),
            CheckResult::Denied(_)
        ));
    }
    // mini-agent-nvnkx.238: a notebook whose keys are not alphabetical (Colab
    // writes nbformat first, cells keyed cell_type/source/metadata) keeps its
    // order, so an edit changes only the edited cell's lines.
    const COLAB: &str = "{\n \"nbformat\": 4,\n \"nbformat_minor\": 5,\n \"metadata\": {\n  \"colab\": {}\n },\n \"cells\": [\n  {\n   \"cell_type\": \"markdown\",\n   \"source\": [\n    \"# T\"\n   ],\n   \"metadata\": {},\n   \"id\": \"m1\"\n  },\n  {\n   \"cell_type\": \"code\",\n   \"source\": [\n    \"x = 1\"\n   ],\n   \"metadata\": {},\n   \"id\": \"c1\",\n   \"outputs\": [],\n   \"execution_count\": null\n  }\n ]\n}\n";

    fn by_id(mode: NotebookMode, id: &str, source: Option<&str>) -> NotebookArgs {
        NotebookArgs {
            cell_index: None,
            cell_id: Some(id.into()),
            ..args(mode, source)
        }
    }

    #[test]
    fn notebook_edit_keeps_the_files_own_key_order() {
        let notebook = Notebook::parse(COLAB.as_bytes(), MAX_NOTEBOOK_BYTES).unwrap();
        assert_eq!(
            String::from_utf8(notebook.encode().unwrap()).unwrap(),
            COLAB
        );
        let output = transformed(COLAB, by_id(NotebookMode::Replace, "c1", Some("x = 2")));
        assert_eq!(output, COLAB.replace("\"x = 1\"", "\"x = 2\""));
        // Converting a markdown cell to code appends the fields code needs;
        // the existing keys keep their place.
        let mut convert = by_id(NotebookMode::Replace, "m1", Some("# T"));
        convert.cell_type = Some("code".into());
        let output = transformed(COLAB, convert);
        assert!(output.contains(
            "   \"cell_type\": \"code\",\n   \"source\": [\n    \"# T\"\n   ],\n   \"metadata\": {},\n   \"id\": \"m1\",\n   \"outputs\": [],\n   \"execution_count\": null\n  },"
        ), "{output}");
        // And back: leaving code drops outputs and execution_count.
        let mut back = by_id(NotebookMode::Replace, "c1", Some("x = 1"));
        back.cell_type = Some("markdown".into());
        let output = transformed(COLAB, back);
        assert!(
            !output[output.find("\"c1\"").unwrap()..].contains("outputs"),
            "{output}"
        );
        assert!(output.contains("\"cell_type\": \"markdown\""), "{output}");
    }

    // mini-agent-nvnkx.239
    #[test]
    fn missing_and_duplicate_cell_ids_are_marked_not_refused() {
        let missing = FIXTURE.replace("\"id\":\"first\",", "");
        let shown = view(missing.as_bytes(), "book.ipynb", 0, 100, MAX_NOTEBOOK_BYTES).unwrap();
        assert!(
            shown.text.contains("[cell 0: code; id missing]"),
            "{}",
            shown.text
        );
        assert!(
            shown.text.contains("[cell 1: markdown; id=second]"),
            "{}",
            shown.text
        );
        assert!(
            shown.text.contains("Notice: 1 cell(s) have a missing"),
            "{}",
            shown.text
        );
        let output = transformed(&missing, args(NotebookMode::Replace, Some("new")));
        let notebook = Notebook::parse(output.as_bytes(), MAX_NOTEBOOK_BYTES).unwrap();
        assert_eq!(source(&notebook.cells[0]).unwrap().0, "new");
        // Ids of untouched cells are not rewritten, and none is invented.
        assert!(!notebook.cells[0].0.contains_key("id"));

        let duplicate = FIXTURE.replace("\"second\"", "\"first\"");
        let shown = view(
            duplicate.as_bytes(),
            "book.ipynb",
            0,
            100,
            MAX_NOTEBOOK_BYTES,
        )
        .unwrap();
        assert!(
            shown.text.contains("id=first (duplicate)"),
            "{}",
            shown.text
        );
        let error = Notebook::parse(duplicate.as_bytes(), MAX_NOTEBOOK_BYTES)
            .unwrap()
            .edit(
                &by_id(NotebookMode::Replace, "first", Some("x")),
                MAX_NOTEBOOK_BYTES,
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("ambiguous") && error.contains("cell_index"),
            "{error}"
        );
        let mut second = args(NotebookMode::Replace, Some("x"));
        second.cell_index = Some(1);
        let output = transformed(&duplicate, second);
        let notebook = Notebook::parse(output.as_bytes(), MAX_NOTEBOOK_BYTES).unwrap();
        assert_eq!(source(&notebook.cells[1]).unwrap().0, "x");
    }

    // mini-agent-nvnkx.240
    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[test]
    fn png_outputs_become_images_mapped_to_their_cell() {
        let notebook = format!(
            r#"{{"nbformat":4,"nbformat_minor":5,"metadata":{{}},"cells":[{{"cell_type":"code","id":"a","source":"plot()","metadata":{{}},"execution_count":1,"outputs":[{{"output_type":"display_data","metadata":{{}},"data":{{"text/plain":"<Figure>","image/png":"{PNG_1X1}\n","image/svg+xml":"<svg/>"}}}}]}}]}}"#
        );
        let with = view_with_images(
            notebook.as_bytes(),
            "n.ipynb",
            0,
            100,
            MAX_NOTEBOOK_BYTES,
            4,
        )
        .unwrap();
        assert_eq!(with.images.len(), 1);
        assert!(with.images[0].bytes.starts_with(b"\x89PNG"));
        assert!(
            with.text
                .contains("[image/png output attached as image 1 (cell 0 output 1)]"),
            "{}",
            with.text
        );
        assert!(
            with.text.contains("[image/svg+xml output omitted]"),
            "{}",
            with.text
        );
        // A model without vision keeps the plain label.
        let without = view(notebook.as_bytes(), "n.ipynb", 0, 100, MAX_NOTEBOOK_BYTES).unwrap();
        assert!(without.images.is_empty());
        assert!(
            without.text.contains("[image/png output omitted]"),
            "{}",
            without.text
        );
        // Past the cap the label says how to reach the image.
        let capped = view_with_images(
            notebook.as_bytes(),
            "n.ipynb",
            0,
            100,
            MAX_NOTEBOOK_BYTES,
            0,
        )
        .unwrap();
        assert!(capped.images.is_empty());
        // Outside the selected window nothing is decoded.
        let outside = view_with_images(
            notebook.as_bytes(),
            "n.ipynb",
            50,
            10,
            MAX_NOTEBOOK_BYTES,
            4,
        )
        .unwrap();
        assert!(outside.images.is_empty());
    }

    // mini-agent-nvnkx.237 / .241
    fn record_view(tracker: &ReadTracker, root: &Path, name: &str) {
        let metadata = std::fs::metadata(root.join(name)).unwrap();
        let key = super::super::capture_workspace_binding(root.into())
            .logical_relative_path(Path::new(name))
            .unwrap();
        tracker.record_notebook_view(&key.to_string_lossy(), 0, 100, &metadata, 0);
    }

    #[tokio::test]
    async fn cell_index_edits_require_a_current_view_but_cell_ids_do_not() {
        let root = crate::tests::OwnedTestDirectory::new("notebook-stale-view");
        let path = root.path().join("book.ipynb");
        std::fs::write(&path, FIXTURE).unwrap();
        let tracker = ReadTracker::new(true);
        let mut by_index = args(NotebookMode::Replace, Some("changed"));
        by_index.cell_index = Some(1);
        // Never viewed.
        let error = tool(root.path(), None, None, tracker.clone())
            .call(by_index.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed since it was read"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FIXTURE);
        // Viewed, then a cell is inserted out of band.
        record_view(&tracker, root.path(), "book.ipynb");
        let shifted = FIXTURE.replacen(
            "\"cells\":[",
            "\"cells\":[{\"cell_type\":\"raw\",\"id\":\"zero\",\"source\":\"z\",\"metadata\":{}},",
            1,
        );
        std::fs::write(&path, &shifted).unwrap();
        let error = tool(root.path(), None, None, tracker.clone())
            .call(by_index.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed since it was read"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), shifted);
        // The same sequence by id edits the intended cell.
        tool(root.path(), None, None, tracker.clone())
            .call(by_id(NotebookMode::Replace, "second", Some("changed")))
            .await
            .unwrap();
        let notebook = Notebook::parse(&std::fs::read(&path).unwrap(), MAX_NOTEBOOK_BYTES).unwrap();
        assert_eq!(source(&notebook.cells[0]).unwrap().0, "z");
        assert_eq!(source(&notebook.cells[2]).unwrap().0, "changed");
        // With a fresh view the index is accepted.
        record_view(&tracker, root.path(), "book.ipynb");
        let mut again = args(NotebookMode::Replace, Some("again"));
        again.cell_index = Some(2);
        tool(root.path(), None, None, tracker.clone())
            .call(again)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn insert_result_names_the_new_cell_and_a_follow_up_edit_by_id_needs_no_read() {
        let root = crate::tests::OwnedTestDirectory::new("notebook-result-details");
        let path = root.path().join("book.ipynb");
        std::fs::write(&path, FIXTURE).unwrap();
        let tracker = ReadTracker::new(true);
        record_view(&tracker, root.path(), "book.ipynb");
        let mut insert = args(NotebookMode::Insert, Some("y = 2"));
        insert.cell_index = Some(2);
        let output = tool(root.path(), None, None, tracker.clone())
            .call(insert)
            .await
            .unwrap();
        assert!(output.contains("inserted cell at index 2 (id="), "{output}");
        assert!(output.contains("notebook now has 3 cells"), "{output}");
        let id = output
            .split("(id=")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .unwrap()
            .to_owned();
        let output = tool(root.path(), None, None, tracker.clone())
            .call(by_id(NotebookMode::Replace, &id, Some("y = 3")))
            .await
            .unwrap();
        assert!(output.contains("replaced cell at index 2"), "{output}");
        let notebook = Notebook::parse(&std::fs::read(&path).unwrap(), MAX_NOTEBOOK_BYTES).unwrap();
        assert_eq!(source(&notebook.cells[2]).unwrap().0, "y = 3");
    }
    #[tokio::test]
    async fn a_real_read_of_the_cell_view_authorizes_cell_index_edits() {
        use super::super::{ReadArgs, ReadTool};
        let root = crate::tests::OwnedTestDirectory::new("notebook-read-then-edit");
        std::fs::write(root.path().join("book.ipynb"), FIXTURE).unwrap();
        let tracker = ReadTracker::new(true);
        let read = ReadTool::new_with_tracker(None, None, None, 200, tracker.clone())
            .with_workspace(root.path());
        let mut by_index = args(NotebookMode::Replace, Some("viewed"));
        by_index.cell_index = Some(1);
        assert!(
            tool(root.path(), None, None, tracker.clone())
                .call(by_index.clone())
                .await
                .is_err()
        );
        read.call(ReadArgs {
            path: "book.ipynb".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        tool(root.path(), None, None, tracker)
            .call(by_index)
            .await
            .unwrap();
    }
}
