//! Builds a small, self-contained PDF holding only chosen pages of a parsed
//! document, so `read` can hand a provider-native document block for just the
//! pages whose text layer is empty (scans, vector figures).
//!
//! The copy is deliberately conservative: it carries each page's content
//! streams and the resources they reach (fonts, images, form XObjects) and
//! nothing else. The catalog, outlines, named destinations, form fields,
//! annotations (links, JavaScript, URI actions), `OpenAction`, metadata and
//! the original page tree are dropped, and any reference back to a page, the
//! page tree or the catalog is replaced by `null`, so neither the rest of the
//! document nor active content travels with the pages. Encrypted documents
//! are refused: their strings and streams were decrypted in memory and are
//! never re-emitted.

use std::collections::{HashMap, HashSet};

use super::{Dict, Document, MAX_DEPTH, Obj};

/// Largest subset one result may carry (raw bytes, before base64).
pub(crate) const MAX_NATIVE_PDF_BYTES: usize = 6 * 1024 * 1024;
/// Objects one subset may copy.
const MAX_SUBSET_OBJECTS: usize = 100_000;

/// Page keys that would reach outside the page's own rendering.
const DROPPED_PAGE_KEYS: &[&[u8]] = &[
    b"Parent",
    b"Annots",
    b"B",
    b"Thumb",
    b"AA",
    b"PieceInfo",
    b"StructParents",
    b"Metadata",
    b"Dur",
    b"Trans",
    b"Tabs",
    b"ID",
    b"PZ",
    b"SeparationInfo",
    b"VP",
];
/// Page attributes a page may inherit from its ancestors.
const INHERITED_KEYS: &[&[u8]] = &[b"MediaBox", b"CropBox", b"Rotate"];

struct Copier<'a> {
    document: &'a Document,
    numbers: HashMap<u32, u32>,
    queue: Vec<u32>,
    next: u32,
    bytes: usize,
}

impl Copier<'_> {
    fn budget(&mut self, cost: usize) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(cost);
        if self.bytes > MAX_NATIVE_PDF_BYTES {
            return Err(format!(
                "the selected pages exceed the {} MiB limit for a native PDF block",
                MAX_NATIVE_PDF_BYTES / (1024 * 1024)
            ));
        }
        Ok(())
    }

    /// `object` with every reference renumbered (and queued for copying).
    fn remap(&mut self, object: &Obj, depth: usize) -> Result<Obj, String> {
        if depth > MAX_DEPTH {
            return Ok(Obj::Null);
        }
        Ok(match object {
            Obj::Ref(number) => match self.document.objects.get(number) {
                None => Obj::Null,
                Some(target) => {
                    let kind = target
                        .as_dict()
                        .and_then(|dict| dict.get(b"Type"))
                        .and_then(Obj::as_name);
                    if matches!(kind, Some(b"Page" | b"Pages" | b"Catalog")) {
                        Obj::Null
                    } else if let Some(&new) = self.numbers.get(number) {
                        Obj::Ref(new)
                    } else {
                        if self.numbers.len() >= MAX_SUBSET_OBJECTS {
                            return Err("the selected pages reach too many objects".into());
                        }
                        let new = self.next;
                        self.next += 1;
                        self.numbers.insert(*number, new);
                        self.queue.push(*number);
                        Obj::Ref(new)
                    }
                }
            },
            Obj::Array(items) => Obj::Array(
                items
                    .iter()
                    .map(|item| self.remap(item, depth + 1))
                    .collect::<Result<_, _>>()?,
            ),
            Obj::Dict(dict) => Obj::Dict(self.remap_dict(dict, depth)?),
            Obj::Stream(dict, data) => {
                // The length is rewritten on output; an indirect one is not copied.
                let direct = Dict(
                    dict.0
                        .iter()
                        .filter(|(key, _)| key != b"Length")
                        .cloned()
                        .collect(),
                );
                Obj::Stream(self.remap_dict(&direct, depth)?, data.clone())
            }
            other => other.clone(),
        })
    }

    fn remap_dict(&mut self, dict: &Dict, depth: usize) -> Result<Dict, String> {
        let mut out = Vec::with_capacity(dict.0.len());
        for (key, value) in &dict.0 {
            out.push((key.clone(), self.remap(value, depth + 1)?));
        }
        Ok(Dict(out))
    }
}

impl Document {
    /// The ancestor-inherited value of `key` for a page dictionary.
    fn inherited_attribute(&self, page: &Dict, key: &[u8]) -> Option<Obj> {
        let mut current = page;
        for _ in 0..MAX_DEPTH {
            if let Some(value) = current.get(key) {
                return Some(value.clone());
            }
            current = self.resolve(current.get(b"Parent")?).as_dict()?;
        }
        None
    }

    /// A standalone PDF holding `pages` (1-based, in the given order), or why
    /// one cannot be made.
    pub(crate) fn subset_pdf(&self, pages: &[usize]) -> Result<Vec<u8>, String> {
        if self.encrypted {
            return Err("the PDF is encrypted".into());
        }
        if pages.is_empty() {
            return Err("no pages selected".into());
        }
        let count = u32::try_from(pages.len()).map_err(|_| "too many pages".to_string())?;
        let mut copier = Copier {
            document: self,
            numbers: HashMap::new(),
            queue: Vec::new(),
            // 1 = catalog, 2 = page tree, 3.. = pages.
            next: 3 + count,
            bytes: 0,
        };
        let mut page_objects: Vec<(u32, Obj)> = Vec::with_capacity(pages.len());
        for (slot, &page) in pages.iter().enumerate() {
            let (source, resources) = self
                .pages
                .get(page.wrapping_sub(1))
                .ok_or_else(|| format!("page {page} does not exist"))?;
            let mut entries: Vec<(Vec<u8>, Obj)> = Vec::new();
            for (key, value) in &source.0 {
                if DROPPED_PAGE_KEYS.contains(&key.as_slice()) || key == b"Type" {
                    continue;
                }
                entries.retain(|(existing, _)| existing != key);
                entries.push((key.clone(), value.clone()));
            }
            for key in INHERITED_KEYS {
                if !entries.iter().any(|(existing, _)| existing == key)
                    && let Some(value) = self.inherited_attribute(source, key)
                {
                    entries.push((key.to_vec(), value));
                }
            }
            if !entries.iter().any(|(existing, _)| existing == b"Resources")
                && let Some(resources) = resources
            {
                entries.push((b"Resources".to_vec(), Obj::Dict(resources.clone())));
            }
            let mut remapped = copier.remap_dict(&Dict(entries), 0)?;
            remapped
                .0
                .push((b"Type".to_vec(), Obj::Name(b"Page".to_vec())));
            remapped.0.push((b"Parent".to_vec(), Obj::Ref(2)));
            page_objects.push((3 + slot as u32, Obj::Dict(remapped)));
        }
        let mut copied: Vec<(u32, Obj)> = Vec::new();
        let mut done = HashSet::new();
        while let Some(old) = copier.queue.pop() {
            if !done.insert(old) {
                continue;
            }
            let new = copier.numbers[&old];
            let Some(object) = self.objects.get(&old) else {
                continue;
            };
            let remapped = copier.remap(object, 0)?;
            if let Obj::Stream(_, data) = &remapped {
                copier.budget(data.len())?;
            }
            copied.push((new, remapped));
        }

        let mut out: Vec<u8> = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
        let mut offsets: Vec<(u32, usize)> = Vec::new();
        let kids = (0..count)
            .map(|slot| Obj::Ref(3 + slot))
            .collect::<Vec<_>>();
        let mut all: Vec<(u32, Obj)> = vec![
            (
                1,
                Obj::Dict(Dict(vec![
                    (b"Type".to_vec(), Obj::Name(b"Catalog".to_vec())),
                    (b"Pages".to_vec(), Obj::Ref(2)),
                ])),
            ),
            (
                2,
                Obj::Dict(Dict(vec![
                    (b"Type".to_vec(), Obj::Name(b"Pages".to_vec())),
                    (b"Kids".to_vec(), Obj::Array(kids)),
                    (b"Count".to_vec(), Obj::Num(f64::from(count))),
                ])),
            ),
        ];
        all.extend(page_objects);
        all.extend(copied);
        all.sort_by_key(|(number, _)| *number);
        for (number, object) in &all {
            offsets.push((*number, out.len()));
            out.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
            write_object(&mut out, object, 0);
            out.extend_from_slice(b"\nendobj\n");
            if out.len() > MAX_NATIVE_PDF_BYTES + MAX_NATIVE_PDF_BYTES / 4 {
                return Err("the selected pages exceed the limit for a native PDF block".into());
            }
        }
        let size = copier.next;
        let xref_at = out.len();
        out.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        let by_number: HashMap<u32, usize> = offsets.into_iter().collect();
        for number in 1..size {
            match by_number.get(&number) {
                Some(offset) => {
                    out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes())
                }
                // A queued reference whose object vanished: a free entry.
                None => out.extend_from_slice(b"0000000000 00000 f \n"),
            }
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n")
                .as_bytes(),
        );
        if out.len() > MAX_NATIVE_PDF_BYTES + MAX_NATIVE_PDF_BYTES / 4 {
            return Err("the selected pages exceed the limit for a native PDF block".into());
        }
        Ok(out)
    }
}

fn write_name(out: &mut Vec<u8>, name: &[u8]) {
    out.push(b'/');
    for &byte in name {
        if byte <= b' '
            || byte >= 0x7f
            || byte == b'#'
            || super::is_delimiter(byte)
            || super::is_whitespace(byte)
        {
            out.extend_from_slice(format!("#{byte:02X}").as_bytes());
        } else {
            out.push(byte);
        }
    }
}

fn write_dict(out: &mut Vec<u8>, dict: &Dict, depth: usize) {
    out.extend_from_slice(b"<<");
    // The parser's lookup lets the last duplicate win; emit exactly that one.
    let mut emitted: HashSet<&[u8]> = HashSet::new();
    for (key, value) in dict.0.iter().rev() {
        if !emitted.insert(key) {
            continue;
        }
        write_name(out, key);
        out.push(b' ');
        write_object(out, value, depth + 1);
        out.push(b' ');
    }
    out.extend_from_slice(b">>");
}

fn write_object(out: &mut Vec<u8>, object: &Obj, depth: usize) {
    if depth > MAX_DEPTH + 2 {
        out.extend_from_slice(b"null");
        return;
    }
    match object {
        Obj::Null | Obj::Keyword(_) => out.extend_from_slice(b"null"),
        Obj::Bool(true) => out.extend_from_slice(b"true"),
        Obj::Bool(false) => out.extend_from_slice(b"false"),
        Obj::Num(number) => {
            if !number.is_finite() {
                out.push(b'0');
            } else if number.fract() == 0.0 && number.abs() < 1e15 {
                out.extend_from_slice(format!("{}", *number as i64).as_bytes());
            } else {
                out.extend_from_slice(format!("{number:.6}").as_bytes());
            }
        }
        Obj::Name(name) => write_name(out, name),
        Obj::Str(bytes) => {
            out.push(b'<');
            for byte in bytes {
                out.extend_from_slice(format!("{byte:02X}").as_bytes());
            }
            out.push(b'>');
        }
        Obj::Array(items) => {
            out.push(b'[');
            for item in items {
                write_object(out, item, depth + 1);
                out.push(b' ');
            }
            out.push(b']');
        }
        Obj::Dict(dict) => write_dict(out, dict, depth),
        Obj::Ref(number) => out.extend_from_slice(format!("{number} 0 R").as_bytes()),
        Obj::Stream(dict, data) => {
            let mut dict = dict.clone();
            dict.0.retain(|(key, _)| key != b"Length");
            dict.0
                .push((b"Length".to_vec(), Obj::Num(data.len() as f64)));
            write_dict(out, &dict, depth);
            out.extend_from_slice(b"\nstream\n");
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
        }
    }
}
