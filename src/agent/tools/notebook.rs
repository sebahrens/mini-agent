//! Bounded notebook JSON transformations. Opaque fields stay raw; cells never execute.
use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

use super::ToolError;

pub(crate) const MAX_NOTEBOOK_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CELLS: usize = 4096;
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_VIEW_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_CELL: usize = 4096;
const MAX_OUTPUT_TOTAL: usize = 16 * 1024;

fn error(message: &'static str) -> ToolError {
    ToolError::Msg(message.into())
}
pub(crate) fn byte_limit(configured: u64) -> u64 {
    configured.min(MAX_NOTEBOOK_BYTES)
}

// A map visitor rejects duplicate interpreted fields. Opaque nested values are
// validated JSON fragments, not recursively traversed or converted through f64.
#[derive(Serialize)]
#[serde(transparent)]
struct Fields(BTreeMap<String, Box<RawValue>>);
impl<'de> Deserialize<'de> for Fields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> Visitor<'de> for FieldsVisitor {
            type Value = Fields;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a notebook object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Fields, A::Error> {
                let mut fields = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.len() >= 256
                        || key.len() > 1024
                        || fields.insert(key, value).is_some()
                    {
                        return Err(serde::de::Error::custom(
                            "duplicate or oversized notebook fields",
                        ));
                    }
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
    fn set<T: Serialize>(&mut self, key: &str, value: &T) -> Result<(), ToolError> {
        let raw = serde_json::value::to_raw_value(value)
            .map_err(|_| error("Cannot encode notebook field"))?;
        self.0.insert(key.into(), raw);
        Ok(())
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
            NotebookMode::Replace if self.cell_type.is_some() => {
                return Err(error("Replace preserves cell_type; omit cell_type"));
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
struct Notebook {
    root: Fields,
    cells: Vec<Fields>,
    minor: u64,
}
impl Notebook {
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
        let mut ids = HashSet::new();
        for cell in &cells.0 {
            let kind: String = cell.get("cell_type")?;
            if matches!(kind.as_str(), "code" | "markdown" | "raw") {
                source(cell)?;
            }
            if cell.0.contains_key("id") {
                let id: String = cell.get("id")?;
                if !valid_id(&id) || !ids.insert(id) {
                    return Err(error("Notebook contains invalid or duplicate cell IDs"));
                }
            } else if minor >= 5 {
                return Err(error("nbformat 4.5+ requires cell IDs"));
            }
        }
        Ok(Self {
            root,
            cells: cells.0,
            minor,
        })
    }
    fn edit(mut self, args: &NotebookArgs, limit: u64) -> Result<Option<Vec<u8>>, ToolError> {
        args.validate()?;
        let index = match args.cell_index {
            Some(index) => index,
            None => self
                .cells
                .iter()
                .position(|c| c.get::<String>("id").ok().as_deref() == args.cell_id.as_deref())
                .ok_or_else(|| error("No cell matches the requested ID"))?,
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
                if old == text {
                    return Ok(None);
                }
                if array {
                    self.cells[index]
                        .set("source", &text.split_inclusive('\n').collect::<Vec<_>>())?;
                } else {
                    self.cells[index].set("source", &text)?;
                }
            }
            NotebookMode::Insert => {
                if self.cells.len() == MAX_CELLS {
                    return Err(error("Notebook exceeds the 4096-cell cap"));
                }
                let kind = args.cell_type.as_deref().unwrap_or("code");
                let mut cell = Fields(BTreeMap::new());
                cell.set("cell_type", &kind)?;
                cell.set("metadata", &serde_json::json!({}))?;
                cell.set(
                    "source",
                    &args
                        .source
                        .as_deref()
                        .expect("validated source")
                        .split_inclusive('\n')
                        .collect::<Vec<_>>(),
                )?;
                if kind == "code" {
                    cell.set("outputs", &Vec::<u8>::new())?;
                    cell.set("execution_count", &Option::<u64>::None)?;
                }
                if self.minor >= 5 {
                    cell.set("id", &uuid::Uuid::new_v4().to_string())?;
                }
                self.cells.insert(index, cell);
            }
        }
        self.root.set("cells", &self.cells)?;
        let result =
            serde_json::to_vec_pretty(&self.root).map_err(|_| error("Cannot encode notebook"))?;
        if result.len() as u64 > byte_limit(limit) {
            return Err(error("Resulting notebook exceeds the bounded raw-file cap"));
        }
        Ok(Some(result))
    }
}

/// A virtual line window, deliberately without raw CRC/hashedit/file-line semantics.
pub(crate) struct NotebookView {
    pub text: String,
    pub crc: u32,
}
pub(crate) fn view(
    bytes: &[u8],
    path: &str,
    offset: usize,
    limit: usize,
    byte_cap: u64,
) -> Result<NotebookView, ToolError> {
    let notebook = Notebook::parse(bytes, byte_cap)?;
    let cap = (byte_cap as usize).min(MAX_VIEW_BYTES);
    let mut selected = String::new();
    let mut total = 0usize;
    let mut selected_lines = 0usize;
    let end = offset.saturating_add(limit);
    let mut output_budget = MAX_OUTPUT_TOTAL;
    let mut push = |line: &str| -> Result<(), ToolError> {
        if total >= offset && total < end {
            if selected.len().saturating_add(line.len()).saturating_add(32) > cap {
                return Err(error(
                    "Notebook view exceeds its output cap; use a narrower virtual line window",
                ));
            }
            use std::fmt::Write;
            writeln!(&mut selected, "{}| {}", total + 1, line).expect("string writing");
            selected_lines += 1;
        }
        total += 1;
        Ok(())
    };
    for (index, cell) in notebook.cells.iter().enumerate() {
        let kind: String = cell.get("cell_type")?;
        let id = cell.get::<String>("id").ok();
        push(&format!(
            "[cell {index}: {kind}{}]",
            id.map(|id| format!("; id={id}")).unwrap_or_default()
        ))?;
        let text = if matches!(kind.as_str(), "code" | "markdown" | "raw") {
            source(cell)?.0
        } else {
            "[opaque unknown cell preserved]".into()
        };
        for line in text.split('\n') {
            push(line)?;
        }
        if let Some(raw) = cell.0.get("outputs") {
            // Each output stays opaque until its few display fields are decoded.
            let outputs: Cells = serde_json::from_str(raw.get())
                .map_err(|_| error("Invalid or oversized notebook outputs"))?;
            let mut cell_budget = MAX_OUTPUT_CELL;
            for output in &outputs.0 {
                let kind = output
                    .get::<String>("output_type")
                    .unwrap_or_else(|_| "unknown".into());
                push(&format!("[output: {kind}]"))?;
                let text = output.0.get("text").and_then(|r| {
                    serde_json::from_str::<String>(r.get()).ok().or_else(|| {
                        serde_json::from_str::<Vec<String>>(r.get())
                            .ok()
                            .map(|s| s.concat())
                    })
                });
                if let Some(text) = text {
                    let permitted = cell_budget.min(output_budget).min(text.len());
                    let mut cutoff = permitted;
                    while !text.is_char_boundary(cutoff) {
                        cutoff -= 1;
                    }
                    for line in text[..cutoff].split('\n') {
                        push(line)?;
                    }
                    cell_budget -= cutoff;
                    output_budget -= cutoff;
                    if cutoff < text.len() {
                        push(&format!("[{} output bytes omitted]", text.len() - cutoff))?;
                    }
                } else {
                    push("[opaque output payload omitted]")?;
                }
            }
        }
    }
    let header = format!(
        "Notebook cell view: {path} ({} cells; {total} virtual lines; showing {} line(s) from {})\nNo raw-file CRC or full-overwrite authorization. Use raw_notebook:true for raw JSON.\n\n",
        notebook.cells.len(),
        selected_lines,
        offset.saturating_add(1)
    );
    if header.len().saturating_add(selected.len()) > cap {
        return Err(error(
            "Notebook view exceeds its output cap; use a narrower virtual line window",
        ));
    }
    let text = header + &selected;
    let mut crc = super::crc::Crc32::new();
    crc.update(text.as_bytes());
    Ok(NotebookView {
        text,
        crc: crc.finalize(),
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
            limit: byte_limit(limit.unwrap_or(1024 * 1024)),
            diff_options,
            #[cfg(test)]
            publication_cancellation: None,
        }
    }
    async fn apply(&self, args: NotebookArgs) -> Result<MutationOutcome, ToolError> {
        args.validate()?;
        super::validate_workspace_binding(Some(&self.workspace))?;
        let target = super::resolve_tool_target(Some(&self.workspace), &args.path);
        let bound = target.bound_relative.clone();
        let (file, identity, expected, parent) = if let Some(relative) = &bound {
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
                Some(expected),
                None,
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
            )
        };
        let coaching = if let Some(relative) = &bound {
            check_perm_bound_path(
                &self.permission,
                &self.ask_tx,
                "notebook_edit",
                &self.workspace,
                relative,
            )
            .await?
        } else {
            check_perm_path(&self.permission, &self.ask_tx, "notebook_edit", &identity).await?
        };
        let limit = self.limit;
        let transform_args = args.clone();
        let (before, after, base) = crate::agent::runner::spawn_blocking_scoped(move || {
            use std::io::Read;
            let recorded = file.metadata()?.len();
            if recorded > limit {
                return Err(error("Notebook exceeds the bounded raw-file cap"));
            }
            let mut bytes = Vec::with_capacity(recorded as usize);
            file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > limit {
                return Err(error("Notebook exceeds the bounded raw-file cap"));
            }
            let base = crate::fs::ContentDigest::of(&bytes);
            let notebook = Notebook::parse(&bytes, limit)?;
            let after = notebook.edit(&transform_args, limit)?;
            Ok::<_, ToolError>((bytes, after, base))
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
        "Edit an .ipynb notebook cell without execution. replace changes source only, preserving outputs/metadata/type. insert adds before a zero-based index or cell ID (index=len appends); delete removes the selected cell. Exactly one cell_index or cell_id is required. Legacy notebooks retain their format version. Use read for cell views, raw_notebook:true for raw JSON.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","additionalProperties":false,"properties":{
        "path":{"type":"string"},"mode":{"type":"string","enum":["replace","insert","delete"]},
        "cell_index":{"type":"integer","minimum":0},"cell_id":{"type":"string","maxLength":64},
        "source":{"type":"string","description":"Literal new source; required for replace/insert"},
        "cell_type":{"type":"string","enum":["code","markdown","raw"],"description":"Insert only; default code"}},"required":["path","mode"]})
    }
    async fn call(&self, args: NotebookArgs) -> Result<String, ToolError> {
        Ok(self.apply(args).await?.output)
    }
    async fn call_structured(
        &self,
        args: NotebookArgs,
        _extensions: &ToolCallExtensions,
    ) -> Result<ToolReturn<String>, ToolError> {
        Ok(self.apply(args).await?.into_return())
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
        for key in ["outputs", "metadata", "execution_count"] {
            assert_eq!(notebook.cells[0].0[key].get(), old.cells[0].0[key].get());
        }
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
            FIXTURE.replace("\"second\"", "\"first\""),
            FIXTURE.replace("\"id\":\"first\",", ""),
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
                    args(NotebookMode::Replace, Some("old\r\nlast")),
                    &ToolCallExtensions::new(),
                )
                .await
                .unwrap();
            let changed = tool
                .call_structured(
                    args(NotebookMode::Replace, Some("new")),
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
                path: "book.ipynb".into(),
                raw_notebook: false,
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
                path: "book.ipynb".into(),
                raw_notebook: true,
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
                .call(args(NotebookMode::Replace, Some("new")))
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("book.ipynb")).unwrap(),
            FIXTURE
        );
        let result = tool(root.path(), None, None, ReadTracker::default())
            .call_structured(
                args(NotebookMode::Replace, Some("new")),
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
            let task =
                tokio::spawn(
                    async move { tool.call(args(NotebookMode::Replace, Some("new"))).await },
                );
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
            let mut request = args(NotebookMode::Replace, Some("new"));
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
            tokio::spawn(async move { tool.call(args(NotebookMode::Replace, Some("new"))).await });
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
                .call(args(NotebookMode::Replace, Some("new")))
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
                        worker_scope.run(tool.call(args(NotebookMode::Replace, Some("new")))),
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
}
