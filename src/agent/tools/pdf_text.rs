//! Minimal, bounded PDF text-layer extraction for the `read` tool.
//!
//! This is deliberately not a PDF renderer. It locates indirect objects by
//! scanning (so damaged cross-reference tables do not matter), expands object
//! streams, walks the page tree, inflates `FlateDecode` content streams and
//! collects the strings shown by text operators. Fonts with a `ToUnicode` CMap
//! are decoded through it; simple fonts fall back to WinAnsi/Latin-1. Every
//! stage is bounded: input size, decoded bytes per stream and in total,
//! nesting depth, object and page counts. Nothing is executed or rendered.

use std::collections::{HashMap, HashSet};
use std::io::Read;

use super::pdf_crypt;
use super::pdf_filters;

#[cfg(feature = "multimodal")]
#[path = "pdf_images.rs"]
mod pdf_images;
#[cfg(feature = "multimodal")]
pub(crate) use pdf_images::PdfImage;
#[cfg(feature = "multimodal")]
#[path = "pdf_subset.rs"]
mod pdf_subset;

/// Largest PDF file the read tool loads.
pub(crate) const MAX_PDF_BYTES: u64 = 64 * 1024 * 1024;
/// Pages one read call may return; larger documents require `pages`.
pub(crate) const MAX_PDF_PAGES_PER_READ: usize = 20;
const MAX_STREAM_DECODED: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_DECODED: u64 = 256 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_OBJECTS: usize = 1_000_000;
const MAX_PAGES: usize = 100_000;
/// Parsed nodes (array elements, dictionary entries, scalars) one top-level
/// object may contain.
const MAX_NODES_PER_OBJECT: usize = 1_000_000;
/// Parsed nodes, including discarded ones, a whole document may cost.
const MAX_TOTAL_NODES: usize = 4_000_000;
/// Fonts one page's resources may declare.
const MAX_FONTS_PER_PAGE: usize = 1024;
/// ToUnicode map entries one page may materialise across all its fonts.
const MAX_CMAP_ENTRIES_PER_PAGE: usize = 262_144;
/// Wall-clock bound on locating and expanding objects.
const PARSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// Whether `bytes` (a file prefix) carries the PDF header.
pub(crate) fn is_pdf(prefix: &[u8]) -> bool {
    let window = &prefix[..prefix.len().min(1024)];
    window.windows(5).any(|candidate| candidate == b"%PDF-")
}

#[derive(Clone, Debug, PartialEq)]
enum Obj {
    Null,
    Bool(bool),
    Num(f64),
    Name(Vec<u8>),
    Str(Vec<u8>),
    Array(Vec<Obj>),
    Dict(Dict),
    Ref(u32),
    /// A stream dictionary and its raw (still encoded) data.
    Stream(Dict, Vec<u8>),
    /// A bare keyword such as a content-stream operator.
    Keyword(Vec<u8>),
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Dict(Vec<(Vec<u8>, Obj)>);

impl Dict {
    fn get(&self, key: &[u8]) -> Option<&Obj> {
        self.0.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

impl Obj {
    fn as_dict(&self) -> Option<&Dict> {
        match self {
            Obj::Dict(dict) | Obj::Stream(dict, _) => Some(dict),
            _ => None,
        }
    }
    fn as_name(&self) -> Option<&[u8]> {
        match self {
            Obj::Name(name) => Some(name),
            _ => None,
        }
    }
    fn as_num(&self) -> Option<f64> {
        match self {
            Obj::Num(value) => Some(*value),
            _ => None,
        }
    }
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\0')
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
    /// Nodes parsed since the current top-level `object` call began.
    nodes_object: usize,
    /// Every node this lexer has parsed, kept or discarded.
    nodes_total: usize,
}

impl<'a> Lexer<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos,
            nodes_object: 0,
            nodes_total: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.peek() {
            if is_whitespace(byte) {
                self.pos += 1;
            } else if byte == b'%' {
                while let Some(byte) = self.peek() {
                    if byte == b'\n' || byte == b'\r' {
                        break;
                    }
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn regular_token(&mut self) -> &'a [u8] {
        let start = self.pos;
        while let Some(byte) = self.peek() {
            if is_whitespace(byte) || is_delimiter(byte) {
                break;
            }
            self.pos += 1;
        }
        &self.data[start..self.pos]
    }

    /// Parse one object. `None` at end of input or on a structural error.
    fn object(&mut self, depth: usize) -> Option<Obj> {
        if depth > MAX_DEPTH {
            return None;
        }
        if depth == 0 {
            self.nodes_object = 0;
        }
        self.nodes_object += 1;
        self.nodes_total += 1;
        if self.nodes_object > MAX_NODES_PER_OBJECT {
            return None;
        }
        self.skip_whitespace();
        let byte = self.peek()?;
        match byte {
            b'<' if self.data.get(self.pos + 1) == Some(&b'<') => {
                self.pos += 2;
                let mut entries = Vec::new();
                loop {
                    self.skip_whitespace();
                    match self.peek()? {
                        b'>' => {
                            self.pos += 1;
                            if self.peek() == Some(b'>') {
                                self.pos += 1;
                            }
                            return Some(Obj::Dict(Dict(entries)));
                        }
                        b'/' => {
                            let Obj::Name(key) = self.object(depth + 1)? else {
                                return None;
                            };
                            let value = self.object(depth + 1)?;
                            entries.push((key, value));
                        }
                        _ => {
                            // Skip a malformed token rather than looping.
                            self.object(depth + 1)?;
                        }
                    }
                }
            }
            b'<' => {
                self.pos += 1;
                let mut digits = Vec::new();
                while let Some(byte) = self.peek() {
                    self.pos += 1;
                    if byte == b'>' {
                        break;
                    }
                    if byte.is_ascii_hexdigit() {
                        digits.push(byte);
                    }
                }
                if digits.len() % 2 == 1 {
                    digits.push(b'0');
                }
                let bytes = digits
                    .chunks(2)
                    .map(|pair| {
                        u8::from_str_radix(std::str::from_utf8(pair).unwrap_or("00"), 16)
                            .unwrap_or(0)
                    })
                    .collect();
                Some(Obj::Str(bytes))
            }
            b'(' => {
                self.pos += 1;
                Some(Obj::Str(self.literal_string()))
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_whitespace();
                    if self.peek()? == b']' {
                        self.pos += 1;
                        return Some(Obj::Array(items));
                    }
                    items.push(self.object(depth + 1)?);
                }
            }
            b'/' => {
                self.pos += 1;
                let raw = self.regular_token();
                let mut name = Vec::with_capacity(raw.len());
                let mut index = 0;
                while index < raw.len() {
                    if raw[index] == b'#'
                        && index + 2 < raw.len()
                        && let Ok(value) = u8::from_str_radix(
                            std::str::from_utf8(&raw[index + 1..index + 3]).unwrap_or("zz"),
                            16,
                        )
                    {
                        name.push(value);
                        index += 3;
                    } else {
                        name.push(raw[index]);
                        index += 1;
                    }
                }
                Some(Obj::Name(name))
            }
            b']' | b'>' | b')' | b'{' | b'}' => {
                self.pos += 1;
                Some(Obj::Keyword(vec![byte]))
            }
            _ => {
                let token = self.regular_token();
                if token.is_empty() {
                    self.pos += 1;
                    return Some(Obj::Keyword(vec![byte]));
                }
                match token {
                    b"true" => Some(Obj::Bool(true)),
                    b"false" => Some(Obj::Bool(false)),
                    b"null" => Some(Obj::Null),
                    _ => match parse_number(token) {
                        Some(number) => {
                            // `N G R` is an indirect reference.
                            if token.iter().all(u8::is_ascii_digit) {
                                let save = self.pos;
                                self.skip_whitespace();
                                let generation = self.regular_token();
                                if !generation.is_empty()
                                    && generation.iter().all(u8::is_ascii_digit)
                                {
                                    self.skip_whitespace();
                                    if self.peek() == Some(b'R')
                                        && self
                                            .data
                                            .get(self.pos + 1)
                                            .is_none_or(|b| is_whitespace(*b) || is_delimiter(*b))
                                    {
                                        self.pos += 1;
                                        return Some(Obj::Ref(number as u32));
                                    }
                                }
                                self.pos = save;
                            }
                            Some(Obj::Num(number))
                        }
                        None => Some(Obj::Keyword(token.to_vec())),
                    },
                }
            }
        }
    }

    fn literal_string(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut nesting = 0usize;
        while let Some(byte) = self.peek() {
            self.pos += 1;
            match byte {
                b'(' => {
                    nesting += 1;
                    out.push(byte);
                }
                b')' if nesting == 0 => break,
                b')' => {
                    nesting -= 1;
                    out.push(byte);
                }
                b'\\' => {
                    let Some(escaped) = self.peek() else { break };
                    self.pos += 1;
                    match escaped {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'\r' => {
                            if self.peek() == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut value = u32::from(escaped - b'0');
                            for _ in 0..2 {
                                match self.peek() {
                                    Some(digit @ b'0'..=b'7') => {
                                        value = value * 8 + u32::from(digit - b'0');
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push((value & 0xff) as u8);
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(byte),
            }
        }
        out
    }
}

fn parse_number(token: &[u8]) -> Option<f64> {
    let first = *token.first()?;
    if !(first.is_ascii_digit() || matches!(first, b'+' | b'-' | b'.')) {
        return None;
    }
    std::str::from_utf8(token).ok()?.parse::<f64>().ok()
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| offset + from)
}

/// `needle` starting within the first `slack` bytes at or after `from`.
fn find_near(haystack: &[u8], needle: &[u8], from: usize, slack: usize) -> Option<usize> {
    let end = from
        .saturating_add(slack)
        .saturating_add(needle.len())
        .min(haystack.len());
    let window = haystack.get(from..end)?;
    window
        .windows(needle.len())
        .position(|candidate| candidate == needle)
        .map(|offset| offset + from)
}

/// The parsed object table plus the bounded decoder budget.
pub(crate) struct Document {
    objects: HashMap<u32, Obj>,
    pages: Vec<(Dict, Option<Dict>)>,
    decoded_budget: std::cell::Cell<u64>,
    /// The file declared encryption (its objects were decrypted in memory).
    #[cfg_attr(not(feature = "multimodal"), allow(dead_code))]
    encrypted: bool,
}

/// Why a PDF could not be read as text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PdfError {
    /// Encryption settings that could not be read at all.
    Encrypted,
    /// The standard security handler needs a non-empty user password.
    PasswordRequired,
    /// A security handler or revision this reader does not implement.
    UnsupportedEncryption(String),
    Malformed(&'static str),
    TooLarge,
    TimedOut,
}

/// First string of a trailer `/ID` array.
fn first_id(dict: &Dict) -> Vec<u8> {
    match dict.get(b"ID") {
        Some(Obj::Array(items)) => match items.first() {
            Some(Obj::Str(bytes)) => bytes.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn follow<'a>(objects: &'a HashMap<u32, Obj>, mut object: &'a Obj) -> &'a Obj {
    for _ in 0..8 {
        match object {
            Obj::Ref(number) => match objects.get(number) {
                Some(next) => object = next,
                None => return &Obj::Null,
            },
            _ => break,
        }
    }
    object
}

/// Open the standard security handler with the empty user password and
/// decrypt every stream in place. Strings are left alone: the text this
/// extractor reads lives in streams.
fn decrypt_streams(
    objects: &mut HashMap<u32, Obj>,
    generations: &HashMap<u32, u16>,
    entry: &Obj,
    id0: Vec<u8>,
) -> Result<(), PdfError> {
    let decryptor = {
        let Some(dict) = follow(objects, entry).as_dict() else {
            return Err(PdfError::Encrypted);
        };
        let get = |key: &[u8]| dict.get(key).map(|value| follow(objects, value));
        let integer = |key: &[u8], default: i64| {
            get(key)
                .and_then(Obj::as_num)
                .map_or(default, |value| value as i64)
        };
        let bytes = |key: &[u8]| match get(key) {
            Some(Obj::Str(bytes)) => bytes.clone(),
            _ => Vec::new(),
        };
        let version = integer(b"V", 0);
        let stream_method = if version >= 4 {
            let name = get(b"StmF")
                .and_then(Obj::as_name)
                .unwrap_or(b"Identity")
                .to_vec();
            if name == b"Identity" {
                pdf_crypt::Method::Identity
            } else {
                let cfm = get(b"CF")
                    .and_then(Obj::as_dict)
                    .and_then(|filters| filters.get(&name))
                    .map(|filter| follow(objects, filter))
                    .and_then(Obj::as_dict)
                    .and_then(|filter| filter.get(b"CFM"))
                    .and_then(Obj::as_name);
                match cfm {
                    Some(b"V2") => pdf_crypt::Method::Rc4,
                    Some(b"AESV2") => pdf_crypt::Method::AesV2,
                    Some(b"AESV3") => pdf_crypt::Method::AesV3,
                    Some(b"None") => pdf_crypt::Method::Identity,
                    other => {
                        return Err(PdfError::UnsupportedEncryption(format!(
                            "crypt filter method {}",
                            other.map_or_else(
                                || "(missing)".to_owned(),
                                |name| String::from_utf8_lossy(name).into_owned()
                            )
                        )));
                    }
                }
            }
        } else {
            pdf_crypt::Method::Rc4
        };
        let params = pdf_crypt::EncryptParams {
            filter: get(b"Filter")
                .and_then(Obj::as_name)
                .unwrap_or_default()
                .to_vec(),
            version,
            revision: integer(b"R", 0),
            length_bits: integer(b"Length", 40),
            owner: bytes(b"O"),
            user: bytes(b"U"),
            user_key: bytes(b"UE"),
            permissions: integer(b"P", 0) as u32 as i32,
            id0,
            encrypt_metadata: !matches!(get(b"EncryptMetadata"), Some(Obj::Bool(false))),
            stream_method,
        };
        pdf_crypt::Decryptor::open(&params).map_err(|error| match error {
            pdf_crypt::CryptError::PasswordRequired => PdfError::PasswordRequired,
            pdf_crypt::CryptError::Unsupported(what) => PdfError::UnsupportedEncryption(what),
        })?
    };
    let encrypt_object = match entry {
        Obj::Ref(number) => Some(*number),
        _ => None,
    };
    for (number, object) in objects.iter_mut() {
        if Some(*number) == encrypt_object {
            continue;
        }
        let Obj::Stream(dict, data) = object else {
            continue;
        };
        if decryptor.leaves_clear(dict.get(b"Type").and_then(Obj::as_name))
            || has_identity_crypt_filter(dict)
        {
            continue;
        }
        let generation = generations.get(number).copied().unwrap_or(0);
        *data = decryptor
            .decrypt_stream(*number, generation, data)
            .unwrap_or_default();
    }
    Ok(())
}

/// A stream whose first filter is `/Crypt` with the Identity filter is stored
/// unencrypted even in an encrypted document.
fn has_identity_crypt_filter(dict: &Dict) -> bool {
    let first = match dict.get(b"Filter") {
        Some(Obj::Name(name)) => Some(name.as_slice()),
        Some(Obj::Array(items)) => items.first().and_then(Obj::as_name),
        _ => None,
    };
    if first != Some(b"Crypt") {
        return false;
    }
    let params = match dict.get(b"DecodeParms") {
        Some(Obj::Dict(params)) => Some(params),
        Some(Obj::Array(items)) => items.first().and_then(Obj::as_dict),
        _ => None,
    };
    params
        .and_then(|params| params.get(b"Name"))
        .and_then(Obj::as_name)
        .is_none_or(|name| name == b"Identity")
}

/// What one page's content streams yielded.
#[derive(Debug, Default)]
pub(crate) struct PageContent {
    pub(crate) text: String,
    /// Content streams that were skipped and why, e.g.
    /// `content stream uses JBIG2Decode, not supported`.
    pub(crate) notes: Vec<String>,
    /// The page runs at least one text-showing operator.
    pub(crate) text_operators: bool,
    /// Image XObjects and inline images the page paints.
    pub(crate) images: usize,
    /// Resource names of the image XObjects the page paints, in paint order.
    #[cfg_attr(not(feature = "multimodal"), allow(dead_code))]
    pub(crate) image_names: Vec<Vec<u8>>,
}

/// Why one stream could not be decoded.
#[derive(Debug, PartialEq, Eq)]
enum StreamError {
    Unsupported(String),
    TooLarge,
    Corrupt(&'static str),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Unsupported(filter) => {
                write!(f, "content stream uses {filter}, not supported")
            }
            StreamError::TooLarge => write!(
                f,
                "content stream exceeds the decode budget ({} MiB per stream, {} MiB per call)",
                MAX_STREAM_DECODED / (1024 * 1024),
                MAX_TOTAL_DECODED / (1024 * 1024)
            ),
            StreamError::Corrupt(why) => write!(f, "content stream could not be decoded ({why})"),
        }
    }
}

#[derive(Default)]
struct ContentFlags {
    text_operators: bool,
    images: usize,
    image_names: Vec<Vec<u8>>,
}

impl std::fmt::Display for PdfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PdfError::Encrypted => f.write_str(
                "the PDF is encrypted and its encryption dictionary could not be read",
            ),
            PdfError::PasswordRequired => f.write_str(
                "password required: the PDF is protected by a non-empty user password, which mini-agent cannot supply",
            ),
            PdfError::UnsupportedEncryption(what) => write!(
                f,
                "the PDF is encrypted with an unsupported scheme ({what}); only the standard security handler with an empty user password is supported"
            ),
            PdfError::Malformed(reason) => write!(f, "the PDF could not be parsed ({reason})"),
            PdfError::TooLarge => f.write_str(
                "the PDF exceeds the parsing budget (too many objects, nested nodes, fonts or ToUnicode entries, or too many decoded bytes)",
            ),
            PdfError::TimedOut => {
                f.write_str("parsing the PDF exceeded its time limit; the file is likely malformed")
            }
        }
    }
}

impl Document {
    pub(crate) fn parse(data: &[u8]) -> Result<Self, PdfError> {
        Self::parse_until(data, std::time::Instant::now() + PARSE_DEADLINE)
    }

    fn parse_until(data: &[u8], deadline: std::time::Instant) -> Result<Self, PdfError> {
        let mut objects: HashMap<u32, Obj> = HashMap::new();
        let mut generations: HashMap<u32, u16> = HashMap::new();
        // The last `/Encrypt` entry (object dictionary or trailer) and the
        // first `/ID` string beside it.
        let mut encrypt: Option<(Obj, Vec<u8>)> = None;
        let mut position = 0;
        let mut nodes = 0usize;
        // Once one stream lacks `endstream`, no later one can have it either.
        let mut endstream_missing = false;
        // Bytes the lexer walked for candidates that failed to parse.
        let mut wasted = 0usize;
        let waste_cap = data.len().saturating_mul(8).saturating_add(1 << 20);
        let mut candidates = 0usize;
        while let Some(found) = find(data, b"obj", position) {
            position = found + 3;
            candidates += 1;
            if candidates.is_multiple_of(256) && std::time::Instant::now() >= deadline {
                return Err(PdfError::TimedOut);
            }
            if objects.len() >= MAX_OBJECTS {
                return Err(PdfError::TooLarge);
            }
            // `obj` must follow `<num> <gen> ` and be a whole token.
            if data
                .get(found + 3)
                .is_some_and(|b| !is_whitespace(*b) && !is_delimiter(*b))
            {
                continue;
            }
            let Some((number, generation)) = object_number_before(data, found) else {
                continue;
            };
            let mut lexer = Lexer::new(data, found + 3);
            let parsed = lexer.object(0);
            nodes = nodes.saturating_add(lexer.nodes_total);
            if nodes > MAX_TOTAL_NODES {
                return Err(PdfError::TooLarge);
            }
            let Some(object) = parsed else {
                wasted = wasted.saturating_add(lexer.pos.saturating_sub(found));
                if wasted > waste_cap {
                    // Further candidates would only rescan the same bytes.
                    break;
                }
                continue;
            };
            // Never rescan the bytes this object consumed.
            position = position.max(lexer.pos);
            let object = match object {
                Obj::Dict(dict) => {
                    lexer.skip_whitespace();
                    if data[lexer.pos..].starts_with(b"stream") {
                        let mut start = lexer.pos + 6;
                        if data.get(start) == Some(&b'\r') {
                            start += 1;
                        }
                        if data.get(start) == Some(&b'\n') {
                            start += 1;
                        }
                        let declared = dict
                            .get(b"Length")
                            .and_then(Obj::as_num)
                            .map(|length| length as usize)
                            .and_then(|length| start.checked_add(length));
                        let end = match declared {
                            Some(end)
                                if end <= data.len()
                                    && find_near(data, b"endstream", end, 3).is_some() =>
                            {
                                end
                            }
                            _ => {
                                if endstream_missing {
                                    continue;
                                }
                                let Some(end) = find(data, b"endstream", start) else {
                                    endstream_missing = true;
                                    continue;
                                };
                                let mut end = end;
                                while end > start && matches!(data[end - 1], b'\r' | b'\n') {
                                    end -= 1;
                                }
                                end
                            }
                        };
                        position = end;
                        Obj::Stream(dict, data[start..end].to_vec())
                    } else {
                        Obj::Dict(dict)
                    }
                }
                other => other,
            };
            if let Some(dict) = object.as_dict()
                && let Some(entry) = dict.get(b"Encrypt")
            {
                encrypt = Some((entry.clone(), first_id(dict)));
            }
            generations.insert(number, generation);
            objects.insert(number, object);
        }
        // Classic trailers are not objects; check them separately.
        let mut position = 0;
        let mut trailers = 0usize;
        while let Some(found) = find(data, b"trailer", position) {
            position = found + 7;
            trailers += 1;
            if trailers.is_multiple_of(256) && std::time::Instant::now() >= deadline {
                return Err(PdfError::TimedOut);
            }
            let mut lexer = Lexer::new(data, found + 7);
            let parsed = lexer.object(0);
            nodes = nodes.saturating_add(lexer.nodes_total);
            if nodes > MAX_TOTAL_NODES {
                return Err(PdfError::TooLarge);
            }
            // Never rescan the bytes this trailer consumed.
            position = position.max(lexer.pos);
            if let Some(Obj::Dict(dict)) = parsed
                && let Some(entry) = dict.get(b"Encrypt")
            {
                encrypt = Some((entry.clone(), first_id(&dict)));
            }
        }
        let encrypted = encrypt.is_some();
        if let Some((entry, id0)) = encrypt {
            decrypt_streams(&mut objects, &generations, &entry, id0)?;
        }
        let mut document = Self {
            objects,
            pages: Vec::new(),
            decoded_budget: std::cell::Cell::new(MAX_TOTAL_DECODED),
            encrypted,
        };
        document.expand_object_streams(nodes, deadline)?;
        document.collect_pages()?;
        Ok(document)
    }

    fn expand_object_streams(
        &mut self,
        mut nodes: usize,
        deadline: std::time::Instant,
    ) -> Result<(), PdfError> {
        let streams: Vec<(Dict, Vec<u8>)> = self
            .objects
            .values()
            .filter_map(|object| match object {
                Obj::Stream(dict, data)
                    if dict.get(b"Type").and_then(Obj::as_name) == Some(b"ObjStm") =>
                {
                    Some((dict.clone(), data.clone()))
                }
                _ => None,
            })
            .collect();
        for (dict, raw) in streams {
            let Ok(decoded) = self.decode(&dict, &raw) else {
                continue;
            };
            let count = dict.get(b"N").and_then(Obj::as_num).unwrap_or(0.0) as usize;
            let first = dict.get(b"First").and_then(Obj::as_num).unwrap_or(0.0) as usize;
            let mut header = Lexer::new(&decoded, 0);
            let mut entries = Vec::new();
            for _ in 0..count.min(MAX_OBJECTS) {
                let (Some(Obj::Num(number)), Some(Obj::Num(offset))) =
                    (header.object(0), header.object(0))
                else {
                    break;
                };
                entries.push((number as u32, offset as usize));
            }
            nodes = nodes.saturating_add(header.nodes_total);
            for (index, (number, offset)) in entries.into_iter().enumerate() {
                if self.objects.len() >= MAX_OBJECTS || nodes > MAX_TOTAL_NODES {
                    return Err(PdfError::TooLarge);
                }
                if index.is_multiple_of(256) && std::time::Instant::now() >= deadline {
                    return Err(PdfError::TimedOut);
                }
                if self.objects.contains_key(&number) {
                    continue;
                }
                let mut lexer = Lexer::new(&decoded, first.saturating_add(offset));
                let object = lexer.object(0);
                nodes = nodes.saturating_add(lexer.nodes_total);
                if let Some(object) = object {
                    self.objects.insert(number, object);
                }
            }
            if nodes > MAX_TOTAL_NODES {
                return Err(PdfError::TooLarge);
            }
        }
        Ok(())
    }

    fn resolve<'a>(&'a self, object: &'a Obj) -> &'a Obj {
        let mut current = object;
        for _ in 0..MAX_DEPTH {
            match current {
                Obj::Ref(number) => match self.objects.get(number) {
                    Some(next) => current = next,
                    None => return &Obj::Null,
                },
                _ => return current,
            }
        }
        &Obj::Null
    }

    fn collect_pages(&mut self) -> Result<(), PdfError> {
        let catalog = self
            .objects
            .iter()
            .filter(|(_, object)| {
                object
                    .as_dict()
                    .and_then(|d| d.get(b"Type"))
                    .and_then(Obj::as_name)
                    == Some(b"Catalog")
            })
            .max_by_key(|(number, _)| **number)
            .map(|(_, object)| object.clone());
        let mut pages = Vec::new();
        if let Some(root) = catalog
            .as_ref()
            .and_then(Obj::as_dict)
            .and_then(|catalog| catalog.get(b"Pages"))
        {
            let mut visited = HashSet::new();
            self.walk_pages(root, None, 0, &mut visited, &mut pages)?;
        }
        if pages.is_empty() {
            // Fall back to every page object in object-number order.
            let mut numbers: Vec<&u32> = self
                .objects
                .iter()
                .filter(|(_, object)| {
                    object
                        .as_dict()
                        .and_then(|d| d.get(b"Type"))
                        .and_then(Obj::as_name)
                        == Some(b"Page")
                })
                .map(|(number, _)| number)
                .collect();
            numbers.sort();
            pages = numbers
                .into_iter()
                .filter_map(|number| self.objects[number].as_dict().cloned())
                .map(|page| (page, None))
                .collect();
        }
        if pages.is_empty() {
            return Err(PdfError::Malformed("no pages found"));
        }
        self.pages = pages;
        Ok(())
    }

    fn walk_pages(
        &self,
        node: &Obj,
        inherited: Option<&Dict>,
        depth: usize,
        visited: &mut HashSet<u32>,
        pages: &mut Vec<(Dict, Option<Dict>)>,
    ) -> Result<(), PdfError> {
        if depth > MAX_DEPTH {
            return Ok(());
        }
        if let Obj::Ref(number) = node
            && !visited.insert(*number)
        {
            return Ok(());
        }
        let Some(dict) = self.resolve(node).as_dict() else {
            return Ok(());
        };
        let resources = dict
            .get(b"Resources")
            .and_then(|resources| self.resolve(resources).as_dict())
            .or(inherited);
        match dict.get(b"Type").and_then(Obj::as_name) {
            Some(b"Page") => {
                if pages.len() >= MAX_PAGES {
                    return Err(PdfError::TooLarge);
                }
                pages.push((dict.clone(), resources.cloned()));
            }
            _ => {
                if let Some(Obj::Array(kids)) = dict.get(b"Kids").map(|kids| self.resolve(kids)) {
                    for kid in kids {
                        self.walk_pages(kid, resources, depth + 1, visited, pages)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Decode a stream's data through its filters, within the budgets.
    fn decode(&self, dict: &Dict, raw: &[u8]) -> Result<Vec<u8>, StreamError> {
        self.decode_inner(dict, raw, false).map(|(data, _)| data)
    }

    /// [`Self::decode`], optionally stopping at a trailing `DCTDecode`: the
    /// flag reports that the returned bytes are a JPEG, not decoded pixels.
    fn decode_inner(
        &self,
        dict: &Dict,
        raw: &[u8],
        stop_at_dct: bool,
    ) -> Result<(Vec<u8>, bool), StreamError> {
        let filters: Vec<Vec<u8>> = match dict
            .get(b"Filter")
            .or_else(|| dict.get(b"F"))
            .map(|f| self.resolve(f))
        {
            None | Some(Obj::Null) => Vec::new(),
            Some(Obj::Name(name)) => vec![name.clone()],
            Some(Obj::Array(names)) => names
                .iter()
                .filter_map(|name| self.resolve(name).as_name().map(<[u8]>::to_vec))
                .collect(),
            _ => return Err(StreamError::Corrupt("malformed /Filter entry")),
        };
        let params = dict
            .get(b"DecodeParms")
            .or_else(|| dict.get(b"DP"))
            .map(|p| self.resolve(p));
        let mut data = raw.to_vec();
        let mut jpeg = false;
        for (index, filter) in filters.iter().enumerate() {
            if jpeg {
                return Err(StreamError::Corrupt("filters follow DCTDecode"));
            }
            let parms: Option<&Dict> = match params {
                Some(Obj::Dict(dict)) if index == 0 => Some(dict),
                Some(Obj::Array(items)) => items
                    .get(index)
                    .and_then(|item| self.resolve(item).as_dict()),
                _ => None,
            };
            let limit = usize::try_from(self.decoded_budget.get().min(MAX_STREAM_DECODED))
                .unwrap_or(usize::MAX);
            let name = String::from_utf8_lossy(filter).into_owned();
            let number = |key: &[u8], default: usize| {
                parms
                    .and_then(|parms| parms.get(key))
                    .map(|value| self.resolve(value))
                    .and_then(Obj::as_num)
                    .map_or(default, |value| value.max(0.0) as usize)
            };
            let predictor = || pdf_filters::Predictor {
                predictor: number(b"Predictor", 1).min(u32::MAX as usize) as u32,
                colors: number(b"Colors", 1),
                bits: number(b"BitsPerComponent", 8),
                columns: number(b"Columns", 1),
            };
            let step = match filter.as_slice() {
                b"FlateDecode" | b"Fl" => self
                    .inflate(&data, limit)
                    .and_then(|out| pdf_filters::unpredict(out, predictor())),
                b"LZWDecode" | b"LZW" => {
                    let early = number(b"EarlyChange", 1) != 0;
                    pdf_filters::lzw(&data, early, limit)
                        .and_then(|out| pdf_filters::unpredict(out, predictor()))
                }
                b"ASCII85Decode" | b"A85" => pdf_filters::ascii85(&data, limit),
                b"RunLengthDecode" | b"RL" => pdf_filters::run_length(&data, limit),
                b"ASCIIHexDecode" | b"AHx" => {
                    let mut text = data.clone();
                    text.retain(|byte| !is_whitespace(*byte));
                    text.insert(0, b'<');
                    match Lexer::new(&text, 0).object(0) {
                        Some(Obj::Str(bytes)) => Ok(bytes),
                        _ => Err(pdf_filters::FilterError::Corrupt("invalid ASCIIHex data")),
                    }
                }
                b"DCTDecode" | b"DCT" if stop_at_dct => {
                    jpeg = true;
                    Ok(std::mem::take(&mut data))
                }
                // Encryption is undone before any filter runs.
                b"Crypt" => Ok(std::mem::take(&mut data)),
                _ => return Err(StreamError::Unsupported(name)),
            };
            data = step.map_err(|error| match error {
                pdf_filters::FilterError::TooLarge => StreamError::TooLarge,
                pdf_filters::FilterError::Corrupt(why) => StreamError::Corrupt(why),
                pdf_filters::FilterError::Unsupported(what) => {
                    StreamError::Unsupported(format!("{name} {what}"))
                }
            })?;
            self.decoded_budget
                .set(self.decoded_budget.get().saturating_sub(data.len() as u64));
        }
        Ok((data, jpeg))
    }

    fn inflate(&self, data: &[u8], limit: usize) -> Result<Vec<u8>, pdf_filters::FilterError> {
        let budget = limit as u64;
        let mut out = Vec::new();
        let zlib = flate2::read::ZlibDecoder::new(data)
            .take(budget + 1)
            .read_to_end(&mut out);
        if zlib.is_err() && out.is_empty() {
            out.clear();
            let _ = flate2::read::DeflateDecoder::new(data)
                .take(budget + 1)
                .read_to_end(&mut out);
        }
        if out.len() as u64 > budget {
            return Err(pdf_filters::FilterError::TooLarge);
        }
        if out.is_empty() && zlib.is_err() {
            return Err(pdf_filters::FilterError::Corrupt(
                "FlateDecode data could not be inflated",
            ));
        }
        Ok(out)
    }

    /// Extracted text of page `index` (0-based).
    #[cfg(test)]
    pub(crate) fn page_text(&self, index: usize) -> Result<String, PdfError> {
        Ok(self.page_content(index)?.text)
    }

    /// Extracted text of page `index` (0-based) plus what could not be read.
    pub(crate) fn page_content(&self, index: usize) -> Result<PageContent, PdfError> {
        let Some((page, resources)) = self.pages.get(index) else {
            return Ok(PageContent::default());
        };
        let mut content = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let streams: Vec<&Obj> = match page.get(b"Contents").map(|c| self.resolve(c)) {
            Some(Obj::Array(items)) => items.iter().map(|item| self.resolve(item)).collect(),
            Some(other) => vec![other],
            None => Vec::new(),
        };
        for stream in streams {
            if let Obj::Stream(dict, raw) = stream {
                match self.decode(dict, raw) {
                    Ok(decoded) => {
                        content.extend_from_slice(&decoded);
                        content.push(b'\n');
                    }
                    Err(error) => {
                        let note = error.to_string();
                        if !notes.contains(&note) {
                            notes.push(note);
                        }
                    }
                }
            }
        }
        let fonts = self.fonts(resources.as_ref())?;
        let images = self.image_names(resources.as_ref());
        let mut flags = ContentFlags::default();
        let text = normalize_text(&extract_text(&content, &fonts, &images, &mut flags));
        Ok(PageContent {
            text,
            notes,
            text_operators: flags.text_operators,
            images: flags.images,
            image_names: flags.image_names,
        })
    }

    /// Names in the page's `/XObject` resources that are image XObjects.
    fn image_names(&self, resources: Option<&Dict>) -> HashSet<Vec<u8>> {
        let mut names = HashSet::new();
        if let Some(xobjects) = resources
            .and_then(|resources| resources.get(b"XObject"))
            .and_then(|xobjects| self.resolve(xobjects).as_dict())
        {
            for (name, object) in xobjects.0.iter().take(MAX_FONTS_PER_PAGE) {
                if let Obj::Stream(dict, _) = self.resolve(object)
                    && dict.get(b"Subtype").and_then(Obj::as_name) == Some(b"Image")
                {
                    names.insert(name.clone());
                }
            }
        }
        names
    }

    fn fonts(&self, resources: Option<&Dict>) -> Result<HashMap<Vec<u8>, FontDecoder>, PdfError> {
        let mut fonts = HashMap::new();
        let Some(font_dict) = resources
            .and_then(|resources| resources.get(b"Font"))
            .and_then(|fonts| self.resolve(fonts).as_dict())
        else {
            return Ok(fonts);
        };
        if font_dict.0.len() > MAX_FONTS_PER_PAGE {
            return Err(PdfError::TooLarge);
        }
        // One decoded map per ToUnicode stream object, shared by every font
        // that points at it, with one entry budget for the whole page.
        let mut shared: HashMap<u32, SharedCmap> = HashMap::new();
        let mut entries_left = MAX_CMAP_ENTRIES_PER_PAGE;
        for (name, font) in &font_dict.0 {
            let Some(font) = self.resolve(font).as_dict() else {
                continue;
            };
            let composite = font.get(b"Subtype").and_then(Obj::as_name) == Some(b"Type0");
            let mac_roman = match font.get(b"Encoding").map(|e| self.resolve(e)) {
                Some(Obj::Name(name)) => name == b"MacRomanEncoding",
                Some(Obj::Dict(dict)) => {
                    dict.get(b"BaseEncoding").and_then(Obj::as_name) == Some(b"MacRomanEncoding")
                }
                _ => false,
            };
            let mut decoder = FontDecoder {
                code_bytes: if composite { 2 } else { 1 },
                map: std::rc::Rc::default(),
                composite,
                mac_roman,
            };
            let to_unicode = font.get(b"ToUnicode");
            let key = match to_unicode {
                Some(Obj::Ref(number)) => Some(*number),
                _ => None,
            };
            if let Some((code_bytes, map)) = key.and_then(|key| shared.get(&key)) {
                if let Some(code_bytes) = code_bytes {
                    decoder.code_bytes = *code_bytes;
                }
                decoder.map = std::rc::Rc::clone(map);
            } else if let Some(Obj::Stream(dict, raw)) = to_unicode.map(|t| self.resolve(t))
                && let Ok(cmap) = self.decode(dict, raw)
            {
                let default_bytes = decoder.code_bytes;
                let mut map = HashMap::new();
                let code_bytes = FontDecoder::load_cmap(&cmap, &mut map, &mut entries_left)?;
                if let Some(code_bytes) = code_bytes {
                    decoder.code_bytes = code_bytes;
                }
                decoder.map = std::rc::Rc::new(map);
                if let Some(key) = key {
                    shared.insert(
                        key,
                        (
                            code_bytes.filter(|bytes| *bytes != default_bytes),
                            std::rc::Rc::clone(&decoder.map),
                        ),
                    );
                }
            }
            fonts.insert(name.clone(), decoder);
        }
        Ok(fonts)
    }
}

fn object_number_before(data: &[u8], obj_at: usize) -> Option<(u32, u16)> {
    let mut index = obj_at;
    let skip_ws = |index: &mut usize| {
        while *index > 0 && is_whitespace(data[*index - 1]) {
            *index -= 1;
        }
    };
    let digits = |index: &mut usize| -> Option<(usize, usize)> {
        let end = *index;
        while *index > 0 && data[*index - 1].is_ascii_digit() {
            *index -= 1;
        }
        (end > *index).then_some((*index, end))
    };
    skip_ws(&mut index);
    if index == obj_at {
        return None;
    }
    let (generation_start, generation_end) = digits(&mut index)?;
    let before_generation = index;
    skip_ws(&mut index);
    if index == before_generation {
        return None;
    }
    let (start, end) = digits(&mut index)?;
    if index > 0 && !is_whitespace(data[index - 1]) && !is_delimiter(data[index - 1]) {
        return None;
    }
    let number = std::str::from_utf8(&data[start..end]).ok()?.parse().ok()?;
    let generation = std::str::from_utf8(&data[generation_start..generation_end])
        .ok()?
        .parse()
        .unwrap_or(0);
    Some((number, generation))
}

/// A decoded ToUnicode map shared by every font pointing at its stream, with
/// the code width it declared.
type SharedCmap = (Option<usize>, std::rc::Rc<HashMap<u32, String>>);

struct FontDecoder {
    code_bytes: usize,
    map: std::rc::Rc<HashMap<u32, String>>,
    composite: bool,
    mac_roman: bool,
}

fn utf16be(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|pair| u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]))
        .collect();
    String::from_utf16_lossy(&units)
}

fn code_value(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .take(4)
        .fold(0u32, |value, byte| (value << 8) | u32::from(*byte))
}

impl FontDecoder {
    /// Parse a ToUnicode CMap into `map`, charging `entries_left`. Returns the
    /// declared code width, if any.
    fn load_cmap(
        cmap: &[u8],
        map: &mut HashMap<u32, String>,
        entries_left: &mut usize,
    ) -> Result<Option<usize>, PdfError> {
        let mut code_bytes = None;
        let mut lexer = Lexer::new(cmap, 0);
        let mut operands: Vec<Obj> = Vec::new();
        #[derive(Clone, Copy, PartialEq)]
        enum Mode {
            None,
            Codespace,
            Char,
            Range,
        }
        let mut mode = Mode::None;
        while let Some(token) = lexer.object(0) {
            if lexer.nodes_total > MAX_TOTAL_NODES {
                return Err(PdfError::TooLarge);
            }
            match token {
                Obj::Keyword(keyword) => {
                    match keyword.as_slice() {
                        b"begincodespacerange" => mode = Mode::Codespace,
                        b"beginbfchar" => mode = Mode::Char,
                        b"beginbfrange" => mode = Mode::Range,
                        b"endcodespacerange" | b"endbfchar" | b"endbfrange" => mode = Mode::None,
                        _ => {}
                    }
                    operands.clear();
                }
                other => {
                    operands.push(other);
                    match (mode, operands.as_slice()) {
                        (Mode::Codespace, [Obj::Str(low), Obj::Str(_)]) => {
                            if !low.is_empty() {
                                code_bytes = Some(low.len().min(4));
                            }
                            operands.clear();
                        }
                        (Mode::Char, [Obj::Str(source), Obj::Str(target)]) => {
                            if *entries_left == 0 {
                                return Err(PdfError::TooLarge);
                            }
                            *entries_left -= 1;
                            map.insert(code_value(source), utf16be(target));
                            operands.clear();
                        }
                        (Mode::Range, [Obj::Str(low), Obj::Str(high), target]) => {
                            let (low, high) = (code_value(low), code_value(high));
                            for (step, code) in
                                (low..=high.min(low.saturating_add(65_535))).enumerate()
                            {
                                if *entries_left == 0 {
                                    return Err(PdfError::TooLarge);
                                }
                                *entries_left -= 1;
                                let text = match target {
                                    Obj::Str(base) if !base.is_empty() => {
                                        let mut base = base.clone();
                                        let last = base.len() - 1;
                                        let bumped =
                                            u16::from(base[last]).wrapping_add(step as u16);
                                        base[last] = (bumped & 0xff) as u8;
                                        if last > 0 {
                                            base[last - 1] =
                                                base[last - 1].wrapping_add((bumped >> 8) as u8);
                                        }
                                        utf16be(&base)
                                    }
                                    Obj::Array(items) => match items.get(step) {
                                        Some(Obj::Str(bytes)) => utf16be(bytes),
                                        _ => continue,
                                    },
                                    _ => continue,
                                };
                                map.insert(code, text);
                            }
                            operands.clear();
                        }
                        _ if operands.len() > 3 => operands.clear(),
                        _ => {}
                    }
                }
            }
        }
        Ok(code_bytes)
    }

    fn decode(&self, bytes: &[u8], out: &mut String) {
        for code in bytes.chunks(self.code_bytes.max(1)) {
            let value = code_value(code);
            if let Some(text) = self.map.get(&value) {
                out.push_str(text);
            } else if !self.composite && code.len() == 1 {
                out.push(if self.mac_roman && code[0] >= 0x80 {
                    MAC_ROMAN_HIGH
                        .chars()
                        .nth(usize::from(code[0] - 0x80))
                        .unwrap_or('\u{fffd}')
                } else {
                    win_ansi(code[0])
                });
            }
        }
    }
}

/// MacRoman upper half (0x80..=0xFF), used by fonts declaring
/// `/Encoding /MacRomanEncoding`.
const MAC_ROMAN_HIGH: &str = "ÄÅÇÉÑÖÜáàâäãåçéèêëíìîïñóòôöõúùûü†°¢£§•¶ß®©™´¨≠ÆØ∞±≤≥¥µ∂∑∏π∫ªºΩæø¿¡¬√ƒ≈∆«»… ÀÃÕŒœ–—“”‘’÷◊ÿŸ⁄€‹›ﬁﬂ‡·‚„‰ÂÊÁËÈÍÎÏÌÓÔÒÚÛÙıˆ˜¯˘˙˚¸˝˛ˇ";

/// WinAnsi differs from Latin-1 only in 0x80..=0x9F.
fn win_ansi(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{fffd}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{fffd}', 'Ž',
        '\u{fffd}', '\u{fffd}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ',
        '\u{fffd}', 'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9f => HIGH[usize::from(byte - 0x80)],
        0x09 | 0x0a | 0x0d | 0x20..=0x7e | 0xa0..=0xff => char::from(byte),
        _ => ' ',
    }
}

/// Approximate advance of `ch` as a fraction of the font size; only used to
/// decide whether the next positioned text continues a word.
fn glyph_width(ch: char) -> f64 {
    match ch {
        'i' | 'l' | 'j' | 't' | 'f' | 'r' | 'I' | '.' | ',' | '\'' | '!' | '|' | ':' | ';'
        | ' ' => 0.3,
        'm' | 'w' | 'M' | 'W' | '@' | '%' | '—' => 0.85,
        'A'..='Z' => 0.68,
        _ => 0.5,
    }
}

type Matrix = [f64; 6];
const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn multiply(m: &Matrix, n: &Matrix) -> Matrix {
    [
        m[0] * n[0] + m[1] * n[2],
        m[0] * n[1] + m[1] * n[3],
        m[2] * n[0] + m[3] * n[2],
        m[2] * n[1] + m[3] * n[3],
        m[4] * n[0] + m[5] * n[2] + n[4],
        m[4] * n[1] + m[5] * n[3] + n[5],
    ]
}

fn matrix_operands(operands: &[Obj]) -> Option<Matrix> {
    let values: Vec<f64> = operands.iter().filter_map(Obj::as_num).collect();
    (values.len() == 6).then(|| {
        [
            values[0], values[1], values[2], values[3], values[4], values[5],
        ]
    })
}

/// Text state needed to place separators: the text line matrix, the
/// graphics CTM stack, the font size and the estimated pen position after
/// the last shown string.
struct TextLayout {
    ctm: Matrix,
    ctm_stack: Vec<Matrix>,
    line: Matrix,
    font_size: f64,
    leading: f64,
    /// Device-space pen position and font scale after the last shown text.
    pen: Option<(f64, f64, f64)>,
    /// Text-space advance since the current line position, in font units.
    advance: f64,
}

impl TextLayout {
    fn scale(&self) -> f64 {
        let text = (self.line[0] * self.line[3] - self.line[1] * self.line[2])
            .abs()
            .sqrt();
        let ctm = (self.ctm[0] * self.ctm[3] - self.ctm[1] * self.ctm[2])
            .abs()
            .sqrt();
        (self.font_size * text * ctm).abs().max(0.01)
    }

    fn device(&self, x: f64, y: f64) -> (f64, f64) {
        let m = multiply(&self.line, &self.ctm);
        (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
    }

    fn move_line(&mut self, tx: f64, ty: f64) {
        self.line = multiply(&[1.0, 0.0, 0.0, 1.0, tx, ty], &self.line);
        self.advance = 0.0;
    }

    /// Separator before text shown at the current position.
    fn separator(&self, out: &str) -> Option<char> {
        let (px, py, previous_scale) = self.pen?;
        let (x, y) = self.device(self.advance * self.font_size, 0.0);
        let scale = self.scale().max(previous_scale);
        if (y - py).abs() > 0.6 * scale {
            return Some('\n');
        }
        let gap = (x - px).abs();
        (gap > 0.3 * scale && !out.ends_with([' ', '\n'])).then_some(' ')
    }

    fn shown(&mut self, text: &str) {
        self.advance += text.chars().map(glyph_width).sum::<f64>();
        let (x, y) = self.device(self.advance * self.font_size, 0.0);
        self.pen = Some((x, y, self.scale()));
    }
}

fn extract_text(
    content: &[u8],
    fonts: &HashMap<Vec<u8>, FontDecoder>,
    images: &HashSet<Vec<u8>>,
    flags: &mut ContentFlags,
) -> String {
    let fallback = FontDecoder {
        code_bytes: 1,
        map: std::rc::Rc::default(),
        composite: false,
        mac_roman: false,
    };
    let mut font = &fallback;
    let mut out = String::new();
    let mut operands: Vec<Obj> = Vec::new();
    let mut lexer = Lexer::new(content, 0);
    let mut layout = TextLayout {
        ctm: IDENTITY,
        ctm_stack: Vec::new(),
        line: IDENTITY,
        font_size: 1.0,
        leading: 0.0,
        pen: None,
        advance: 0.0,
    };
    let show = |bytes: &[u8], font: &FontDecoder, layout: &mut TextLayout, out: &mut String| {
        let mut text = String::new();
        font.decode(bytes, &mut text);
        if text.is_empty() {
            return;
        }
        if let Some(separator) = layout.separator(out) {
            out.push(separator);
        }
        out.push_str(&text);
        layout.shown(&text);
    };
    while let Some(token) = lexer.object(0) {
        let Obj::Keyword(operator) = token else {
            if operands.len() < 64 {
                operands.push(token);
            }
            continue;
        };
        let number = |index: usize| operands.get(index).and_then(Obj::as_num).unwrap_or(0.0);
        match operator.as_slice() {
            b"q" if layout.ctm_stack.len() < 256 => layout.ctm_stack.push(layout.ctm),
            b"Q" => layout.ctm = layout.ctm_stack.pop().unwrap_or(IDENTITY),
            b"cm" => {
                if let Some(matrix) = matrix_operands(&operands) {
                    layout.ctm = multiply(&matrix, &layout.ctm);
                }
            }
            b"BT" => {
                layout.line = IDENTITY;
                layout.advance = 0.0;
            }
            b"Tf" => {
                if let Some(Obj::Name(name)) = operands.first() {
                    font = fonts.get(name).unwrap_or(&fallback);
                }
                layout.font_size = number(1);
            }
            b"TL" => layout.leading = number(0),
            b"Td" => layout.move_line(number(0), number(1)),
            b"TD" => {
                layout.leading = -number(1);
                layout.move_line(number(0), number(1));
            }
            b"T*" => layout.move_line(0.0, -layout.leading),
            b"Tm" => {
                if let Some(matrix) = matrix_operands(&operands) {
                    layout.line = matrix;
                    layout.advance = 0.0;
                }
            }
            b"Tj" => {
                flags.text_operators = true;
                if let Some(Obj::Str(bytes)) = operands.last() {
                    show(bytes, font, &mut layout, &mut out);
                }
            }
            b"'" | b"\"" => {
                flags.text_operators = true;
                layout.move_line(0.0, -layout.leading);
                if let Some(Obj::Str(bytes)) = operands.last() {
                    show(bytes, font, &mut layout, &mut out);
                }
            }
            b"TJ" => {
                flags.text_operators = true;
                if let Some(Obj::Array(items)) = operands.last() {
                    for item in items {
                        match item {
                            Obj::Str(bytes) => show(bytes, font, &mut layout, &mut out),
                            Obj::Num(adjust) => {
                                if *adjust < -200.0
                                    && !out.ends_with([' ', '\n'])
                                    && !out.is_empty()
                                {
                                    out.push(' ');
                                }
                                layout.advance -= adjust / 1000.0;
                                if let Some((_, _, scale)) = layout.pen {
                                    let (x, y) =
                                        layout.device(layout.advance * layout.font_size, 0.0);
                                    layout.pen = Some((x, y, scale));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            b"Do" => {
                if let Some(Obj::Name(name)) = operands.first()
                    && images.contains(name)
                {
                    flags.images += 1;
                    if flags.image_names.len() < 64 {
                        flags.image_names.push(name.clone());
                    }
                }
            }
            b"BI" => {
                flags.images += 1;
                // Inline image: skip binary data up to a delimited `EI`.
                let mut index = lexer.pos;
                while let Some(found) = find(content, b"EI", index) {
                    let before = found == 0 || is_whitespace(content[found - 1]);
                    let after = content
                        .get(found + 2)
                        .is_none_or(|byte| is_whitespace(*byte));
                    index = found + 2;
                    if before && after {
                        break;
                    }
                }
                lexer.pos = index.min(content.len());
            }
            _ => {}
        }
        operands.clear();
    }
    out
}

fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let mut collapsed = String::with_capacity(line.len());
        for ch in line.trim_end().chars() {
            if ch == ' ' && collapsed.ends_with(' ') {
                continue;
            }
            collapsed.push(ch);
        }
        let line = collapsed.as_str();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_owned()
}

/// Parse a 1-indexed page selection such as `3`, `1-5` or `2,4,7-9`.
pub(crate) fn parse_page_selection(spec: &str, page_count: usize) -> Result<Vec<usize>, String> {
    let mut pages = Vec::new();
    for part in spec
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let (start, end) = match part.split_once('-') {
            Some((start, end)) => (start.trim(), end.trim()),
            None => (part, part),
        };
        let parse = |value: &str| -> Result<usize, String> {
            value
                .parse::<usize>()
                .ok()
                .filter(|page| *page >= 1)
                .ok_or_else(|| format!("invalid page '{value}' in pages '{spec}'"))
        };
        let start = parse(start)?;
        let end = if end.is_empty() {
            page_count
        } else {
            parse(end)?
        };
        if start > end {
            return Err(format!("page range '{part}' is reversed"));
        }
        if start > page_count {
            return Err(format!(
                "page {start} is past the end of the document ({page_count} pages)"
            ));
        }
        for page in start..=end.min(page_count) {
            if !pages.contains(&page) {
                pages.push(page);
            }
            if pages.len() > MAX_PDF_PAGES_PER_READ {
                return Err(format!(
                    "pages '{spec}' selects more than {MAX_PDF_PAGES_PER_READ} pages; request at most {MAX_PDF_PAGES_PER_READ} per call"
                ));
            }
        }
    }
    if pages.is_empty() {
        return Err("pages must name at least one page, e.g. \"1-5\"".to_owned());
    }
    Ok(pages)
}

/// Compact a sorted page list as `1-3, 5`.
fn page_list(pages: &[usize]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    while index < pages.len() {
        let start = pages[index];
        let mut end = start;
        while index + 1 < pages.len() && pages[index + 1] == end + 1 {
            index += 1;
            end = pages[index];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        index += 1;
    }
    parts.join(", ")
}

/// Explain why none of the selected pages produced text: undecodable streams
/// are named, scanned pages (images but no text operators) get the export
/// hint, and anything else is reported as holding neither text nor images.
fn no_text_report(
    path: &str,
    count: usize,
    pages: &[(usize, PageContent)],
    attached: &Attached,
) -> String {
    let numbers: Vec<usize> = pages.iter().map(|(page, _)| *page).collect();
    let mut out = format!(
        "PDF: {path} ({count} pages): no text could be extracted from page(s) {}.",
        page_list(&numbers)
    );
    let undecoded: Vec<String> = pages
        .iter()
        .filter(|(_, content)| !content.notes.is_empty())
        .map(|(page, content)| format!("page {page}: {}", content.notes.join("; ")))
        .collect();
    if !undecoded.is_empty() {
        out.push_str(&format!(
            " Some content streams were skipped ({}); the text may be present but unreadable by this extractor.",
            undecoded.join(" | ")
        ));
    }
    let scanned: Vec<usize> = pages
        .iter()
        .filter(|(_, c)| c.notes.is_empty() && !c.text_operators && c.images > 0)
        .map(|(page, _)| *page)
        .collect();
    out.push_str(&attached.sentence());
    let scanned: Vec<usize> = scanned
        .into_iter()
        .filter(|page| {
            !attached.images.iter().any(|image| image.page == *page)
                && !attached.document_pages.contains(page)
        })
        .collect();
    if !scanned.is_empty() {
        out.push_str(&format!(
            " Page(s) {} paint images and have no text operators, so they are probably scanned images; export them to images (for example `pdftoppm -png -r 100 -f {first} -l {first} <file> page`) and read the PNG files.",
            page_list(&scanned),
            first = scanned[0],
        ));
    }
    let empty: Vec<usize> = pages
        .iter()
        .filter(|(_, c)| c.notes.is_empty() && c.images == 0 && !c.text_operators)
        .map(|(page, _)| *page)
        .collect();
    if !empty.is_empty() {
        out.push_str(&format!(
            " Page(s) {} contain no text and no images (blank, or drawn only as vector graphics or form XObjects).",
            page_list(&empty)
        ));
    }
    let text_without_chars: Vec<usize> = pages
        .iter()
        .filter(|(_, c)| c.notes.is_empty() && c.text_operators)
        .map(|(page, _)| *page)
        .collect();
    if !text_without_chars.is_empty() {
        out.push_str(&format!(
            " Page(s) {} show text whose characters could not be mapped to Unicode.",
            page_list(&text_without_chars)
        ));
    }
    out
}

/// Images a read may attach for scanned pages.
#[cfg(feature = "multimodal")]
pub(crate) const MAX_PDF_IMAGES_PER_READ: usize = 4;

/// One page image handed to the model beside the text result.
#[cfg_attr(not(feature = "multimodal"), allow(dead_code))]
struct AttachedImage {
    page: usize,
    /// 1-based position among the result's image blocks.
    number: usize,
    width: u32,
    height: u32,
    mime: &'static str,
}

/// Page images attached to a result, and pages that could not be.
#[derive(Default)]
struct Attached {
    images: Vec<AttachedImage>,
    /// Pages carried, in order, by the native PDF document block.
    document_pages: Vec<usize>,
    /// Why other scanned pages have no image.
    problems: Vec<String>,
}

impl Attached {
    /// Model-facing sentence mapping image blocks to pages; empty when none.
    fn sentence(&self) -> String {
        let mut out = String::new();
        if !self.document_pages.is_empty() {
            let items: Vec<String> = self
                .document_pages
                .iter()
                .enumerate()
                .map(|(slot, page)| format!("document page {} = page {page}", slot + 1))
                .collect();
            out.push_str(&format!(
                " The page(s) without a text layer are attached as one PDF document block holding only those pages ({}); read their text and figures from it.",
                items.join(", ")
            ));
        }
        if !self.images.is_empty() {
            let items: Vec<String> = self
                .images
                .iter()
                .map(|image| {
                    format!(
                        "image {} = page {} ({} x {}, {})",
                        image.number, image.page, image.width, image.height, image.mime
                    )
                })
                .collect();
            out.push_str(&format!(
                " The scanned page image(s) are attached as image blocks: {}; read the text from them.",
                items.join(", ")
            ));
        }
        if !self.problems.is_empty() {
            out.push_str(&format!(
                " Page image(s) not attached: {}.",
                self.problems.join("; ")
            ));
        }
        out
    }
}

/// What [`render_with_images`] produced.
pub(crate) struct Rendered {
    pub(crate) text: String,
    /// A standalone PDF of the pages that have no text layer, when native
    /// document input was requested and the pages could be extracted.
    #[cfg(feature = "multimodal")]
    pub(crate) document: Option<Vec<u8>>,
    #[cfg(feature = "multimodal")]
    pub(crate) images: Vec<PdfImage>,
}

/// [`render_with_images`] without page images.
#[cfg(test)]
pub(crate) fn render(
    bytes: &[u8],
    path: &str,
    pages: Option<&str>,
    budget: usize,
) -> Result<String, String> {
    render_with_images(bytes, path, pages, budget, 0, false).map(|rendered| rendered.text)
}

/// Render the selected pages' text layer for the read tool, bounded by
/// `budget` characters. With `max_images > 0`, selected pages that have no
/// text layer but paint images get their page images lifted (at most
/// `max_images` blocks) so a vision model can read them. Errors are
/// model-facing guidance.
pub(crate) fn render_with_images(
    bytes: &[u8],
    path: &str,
    pages: Option<&str>,
    budget: usize,
    max_images: usize,
    native_pdf: bool,
) -> Result<Rendered, String> {
    let document = Document::parse(bytes)
        .map_err(|error| format!("Cannot read '{path}' as PDF text: {error}."))?;
    let count = document.page_count();
    let selected = match pages {
        Some(spec) => parse_page_selection(spec, count)?,
        None if count > MAX_PDF_PAGES_PER_READ => {
            return Err(format!(
                "'{path}' is a PDF with {count} pages; read at most {MAX_PDF_PAGES_PER_READ} pages per call by passing pages, for example pages:\"1-{MAX_PDF_PAGES_PER_READ}\", then continue with pages:\"{}-{}\".",
                MAX_PDF_PAGES_PER_READ + 1,
                count.min(2 * MAX_PDF_PAGES_PER_READ)
            ));
        }
        None => (1..=count).collect(),
    };
    let mut body = String::new();
    let mut body_chars = 0;
    let mut shown = Vec::new();
    let mut without_text: Vec<(usize, PageContent)> = Vec::new();
    let mut stopped_at = None;
    for &page in &selected {
        let content = document.page_content(page - 1).map_err(|error| {
            format!("Cannot read page {page} of '{path}' as PDF text: {error}.")
        })?;
        let notes = content.notes.join("; ");
        let mut pending_without_text = None;
        let mut block = if content.text.is_empty() {
            let block = if notes.is_empty() {
                format!("--- page {page} ---\n[no text layer on this page]\n\n")
            } else {
                format!("--- page {page} ---\n[page {page}: {notes}]\n\n")
            };
            pending_without_text = Some(content);
            block
        } else if notes.is_empty() {
            format!("--- page {page} ---\n{}\n\n", content.text)
        } else {
            format!(
                "--- page {page} ---\n{}\n[page {page}: some content was not extracted: {notes}]\n\n",
                content.text
            )
        };
        let mut block_chars = block.chars().count();
        if !shown.is_empty() && body_chars + block_chars > budget {
            stopped_at = Some(page);
            break;
        }
        if block_chars > budget {
            let cut = block
                .char_indices()
                .nth(budget.saturating_sub(200))
                .map_or(block.len(), |(index, _)| index);
            block.truncate(cut);
            block.push_str(&format!(
                "\n[page {page} truncated at the {budget} character read budget; the rest of this page is not shown]\n\n"
            ));
            block_chars = block.chars().count();
        }
        body_chars += block_chars;
        body.push_str(&block);
        shown.push(page);
        // Only pages actually emitted are candidates for image lifting or the
        // all-scanned report; a page cut by the budget is re-read on continue.
        if let Some(content) = pending_without_text {
            without_text.push((page, content));
        }
    }
    #[cfg_attr(not(feature = "multimodal"), allow(unused_mut))]
    let mut attached = Attached::default();
    #[cfg(feature = "multimodal")]
    let mut images: Vec<PdfImage> = Vec::new();
    #[cfg(feature = "multimodal")]
    let mut native_document: Option<Vec<u8>> = None;
    #[cfg(feature = "multimodal")]
    if native_pdf && !without_text.is_empty() {
        let numbers: Vec<usize> = without_text.iter().map(|(page, _)| *page).collect();
        match document.subset_pdf(&numbers) {
            Ok(subset) => {
                attached.document_pages = numbers;
                native_document = Some(subset);
            }
            // Fall back to lifting page images below.
            Err(reason) => attached
                .problems
                .push(format!("native PDF block unavailable: {reason}")),
        }
    }
    #[cfg(not(feature = "multimodal"))]
    let _ = native_pdf;
    #[cfg(feature = "multimodal")]
    if native_document.is_none() {
        let mut skipped_for_limit = Vec::new();
        for (page, content) in &without_text {
            if !content.notes.is_empty() || content.text_operators || content.images == 0 {
                continue;
            }
            let remaining = max_images.saturating_sub(images.len());
            if max_images == 0 {
                break;
            }
            if remaining == 0 {
                skipped_for_limit.push(*page);
                continue;
            }
            match document.page_images(*page, content, remaining) {
                Ok(lifted) => {
                    for image in lifted {
                        attached.images.push(AttachedImage {
                            page: *page,
                            number: images.len() + 1,
                            width: image.width,
                            height: image.height,
                            mime: image.mime,
                        });
                        images.push(image);
                    }
                }
                Err(reason) => attached.problems.push(format!("page {page}: {reason}")),
            }
        }
        if !skipped_for_limit.is_empty() {
            let list = page_list(&skipped_for_limit);
            attached.problems.push(format!(
                "page(s) {list} (limit of {max_images} images per read; continue with pages:\"{}\")",
                list.replace(", ", ",")
            ));
        }
    }
    #[cfg(not(feature = "multimodal"))]
    let _ = max_images;
    if !shown.is_empty() && without_text.len() == shown.len() {
        return Ok(Rendered {
            text: no_text_report(path, count, &without_text, &attached),
            #[cfg(feature = "multimodal")]
            images,
            #[cfg(feature = "multimodal")]
            document: native_document,
        });
    }
    let mut out = format!(
        "PDF: {path} ({count} pages; extracted text layer of page(s) {}; no line numbers or edit authorization)\n\n{}",
        page_list(&shown),
        body.trim_end()
    );
    if let Some(next) = stopped_at {
        let last = *selected.last().unwrap_or(&next);
        out.push_str(&format!(
            "\n\n[truncated before page {next}: the output reached the {budget} character read budget; continue with pages:\"{next}-{last}\"]"
        ));
    }
    let attached_note = attached.sentence();
    if !attached_note.is_empty() {
        out.push_str(&format!("\n\n[{}]", attached_note.trim()));
    }
    Ok(Rendered {
        text: out,
        #[cfg(feature = "multimodal")]
        images,
        #[cfg(feature = "multimodal")]
        document: native_document,
    })
}

#[cfg(test)]
pub(crate) mod test_pdf {
    //! A tiny PDF writer for tests.
    use std::io::Write;

    pub(crate) enum Content {
        Plain(String),
        Deflated(String),
        /// Extra stream dictionary entries (for example `/Filter /LZWDecode`)
        /// and the already-encoded stream bytes.
        Encoded(String, Vec<u8>),
    }

    /// Number of the first extra object passed to [`build_with`] for `pages`
    /// page/content pairs.
    pub(crate) fn first_extra_object(pages: usize) -> usize {
        5 + pages * 2
    }

    /// Build a PDF whose page `i` shows `pages[i]` with a simple Helvetica
    /// font, or a Type0 font with a ToUnicode CMap when `type0` is set.
    pub(crate) fn build(pages: &[Content], type0: bool) -> Vec<u8> {
        build_with(pages, type0, "", Vec::new())
    }

    /// [`build`] with extra page-tree resources (for example
    /// `/XObject << /Im0 7 0 R >>`) and extra objects numbered from
    /// [`first_extra_object`].
    pub(crate) fn build_with(
        pages: &[Content],
        type0: bool,
        extra_resources: &str,
        extra_objects: Vec<Vec<u8>>,
    ) -> Vec<u8> {
        let mut objects: Vec<Vec<u8>> = Vec::new();
        // 1: catalog, 2: pages, 3: font, 4: cmap; then page/content pairs.
        let first_page = 5;
        let kids: Vec<String> = (0..pages.len())
            .map(|index| format!("{} 0 R", first_page + index * 2))
            .collect();
        objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
        objects.push(
            format!(
                "<< /Type /Pages /Kids [{}] /Count {} /Resources << /Font << /F1 3 0 R >> {extra_resources} >> >>",
                kids.join(" "),
                pages.len()
            )
            .into_bytes(),
        );
        if type0 {
            objects.push(b"<< /Type /Font /Subtype /Type0 /BaseFont /X /Encoding /Identity-H /ToUnicode 4 0 R >>".to_vec());
        } else {
            objects.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
        }
        let cmap = "/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n1 begincodespacerange <0000> <FFFF> endcodespacerange\n2 beginbfchar <0001> <0048> <0002> <00E9> endbfchar\n1 beginbfrange <0010> <0012> <0061> endbfrange\nendcmap end end";
        objects
            .push(format!("<< /Length {} >>\nstream\n{cmap}\nendstream", cmap.len()).into_bytes());
        for (index, content) in pages.iter().enumerate() {
            objects.push(
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R >>",
                    first_page + index * 2 + 1
                )
                .into_bytes(),
            );
            let (dict, data) = match content {
                Content::Plain(text) => (String::new(), text.clone().into_bytes()),
                Content::Encoded(dict, data) => (format!(" {dict}"), data.clone()),
                Content::Deflated(text) => {
                    let mut encoder =
                        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                    encoder.write_all(text.as_bytes()).unwrap();
                    (
                        " /Filter /FlateDecode".to_owned(),
                        encoder.finish().unwrap(),
                    )
                }
            };
            let mut object = format!("<< /Length {}{dict} >>\nstream\n", data.len()).into_bytes();
            object.extend_from_slice(&data);
            object.extend_from_slice(b"\nendstream");
            objects.push(object);
        }
        objects.extend(extra_objects);
        let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            out.extend_from_slice(object);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::pdf_filters::{ascii85_encode, lzw_encode};
    use super::test_pdf::{Content, build, build_with, first_extra_object};
    use super::*;

    const HELLO_PAGE: &str = "BT /F1 12 Tf 72 720 Td (Hello filters) Tj ET";

    fn deflate(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn page_with(dict: &str, data: Vec<u8>) -> Document {
        Document::parse(&build(&[Content::Encoded(dict.into(), data)], false)).unwrap()
    }

    #[cfg(feature = "multimodal")]
    fn scanned_fixture(pages: usize) -> Vec<u8> {
        let jpeg = include_bytes!("fixtures/read-image64/3x2.jpg");
        let mut image = format!(
            "<< /Type /XObject /Subtype /Image /Width 3 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
            jpeg.len()
        )
        .into_bytes();
        image.extend_from_slice(jpeg);
        image.extend_from_slice(b"\nendstream");
        let contents: Vec<Content> = (0..pages)
            .map(|_| Content::Plain("q 612 0 0 792 0 0 cm /Im0 Do Q".into()))
            .collect();
        build_with(
            &contents,
            false,
            &format!("/XObject << /Im0 {} 0 R >>", first_extra_object(pages)),
            vec![image],
        )
    }

    #[cfg(feature = "multimodal")]
    #[test]
    fn subset_keeps_only_the_chosen_pages_with_inherited_resources() {
        let document = Document::parse(&scanned_fixture(3)).unwrap();
        let subset = document.subset_pdf(&[3, 1]).unwrap();
        assert!(subset.starts_with(b"%PDF-1.7"));
        let reparsed = Document::parse(&subset).unwrap();
        assert_eq!(reparsed.page_count(), 2);
        for index in 0..2 {
            let content = reparsed.page_content(index).unwrap();
            assert_eq!(content.images, 1, "page {index} lost its image");
            assert_eq!(content.image_names, vec![b"Im0".to_vec()]);
        }
        // The image bytes travel once per page tree, not once per source page.
        let jpeg = include_bytes!("fixtures/read-image64/3x2.jpg");
        let copies = subset
            .windows(jpeg.len())
            .filter(|window| *window == jpeg.as_slice())
            .count();
        assert_eq!(copies, 1);
        assert!(subset.len() < scanned_fixture(3).len() + 1024);
    }

    #[cfg(feature = "multimodal")]
    #[test]
    fn subset_drops_active_content_and_links_to_other_pages() {
        let mut pdf = scanned_fixture(2);
        let swap = |pdf: &mut Vec<u8>, from: &[u8], to: &[u8]| {
            let at = pdf.windows(from.len()).position(|w| w == from).unwrap();
            pdf.splice(at..at + from.len(), to.iter().copied());
        };
        swap(
            &mut pdf,
            b"<< /Type /Catalog /Pages 2 0 R >>",
            b"<< /Type /Catalog /Pages 2 0 R /OpenAction << /S /JavaScript /JS (app.alert) >> /Names << /JavaScript << >> >> >>",
        );
        let annot = first_extra_object(2) + 1;
        swap(
            &mut pdf,
            b"/MediaBox [0 0 612 792] /Contents 6 0 R",
            format!("/MediaBox [0 0 612 792] /Contents 6 0 R /AA << /O << /S /JavaScript /JS (evil) >> >> /Annots [{annot} 0 R] /Group << /S /Transparency /Back 7 0 R >>").as_bytes(),
        );
        // The annotation links to the other page and runs a URI action.
        pdf.extend_from_slice(
            format!("{annot} 0 obj\n<< /Type /Annot /Subtype /Link /Dest [7 0 R /Fit] /A << /S /URI /URI (http://evil.test/x) >> /P 5 0 R >>\nendobj\n").as_bytes(),
        );
        let document = Document::parse(&pdf).unwrap();
        assert_eq!(document.page_count(), 2);
        let subset = document.subset_pdf(&[1]).unwrap();
        let text = String::from_utf8_lossy(&subset);
        for forbidden in [
            "evil",
            "JavaScript",
            "OpenAction",
            "Annots",
            "/AA",
            "/URI",
            "Names",
            "Outlines",
        ] {
            assert!(!text.contains(forbidden), "{forbidden} survived: {text}");
        }
        let reparsed = Document::parse(&subset).unwrap();
        assert_eq!(reparsed.page_count(), 1);
        // Only the requested page's content stream is in the copy.
        assert_eq!(text.matches("/Type /Page ").count(), 1);
    }

    #[cfg(feature = "multimodal")]
    #[test]
    fn subset_refuses_encrypted_oversized_and_missing_pages() {
        let document = Document::parse(&scanned_fixture(1)).unwrap();
        assert!(
            document
                .subset_pdf(&[2])
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(document.subset_pdf(&[]).is_err());
        let mut encrypted = Document::parse(&scanned_fixture(1)).unwrap();
        encrypted.encrypted = true;
        assert!(
            encrypted
                .subset_pdf(&[1])
                .unwrap_err()
                .contains("encrypted")
        );
        // A page whose single stream is over the cap is refused, not truncated.
        let big = vec![b'x'; pdf_subset::MAX_NATIVE_PDF_BYTES + 1];
        let mut object = format!("<< /Length {} >>\nstream\n", big.len()).into_bytes();
        object.extend_from_slice(&big);
        object.extend_from_slice(b"\nendstream");
        let pdf = build_with(
            &[Content::Plain("q /Big Do Q".into())],
            false,
            &format!("/XObject << /Big {} 0 R >>", first_extra_object(1)),
            vec![object],
        );
        let document = Document::parse(&pdf).unwrap();
        assert!(document.subset_pdf(&[1]).unwrap_err().contains("limit"));
    }

    #[test]
    fn ascii85_then_flate_content_streams_extract_their_text() {
        let data = ascii85_encode(&deflate(HELLO_PAGE.as_bytes()));
        let document = page_with("/Filter [/ASCII85Decode /FlateDecode]", data);
        assert_eq!(document.page_text(0).unwrap(), "Hello filters");
        let short = page_with("/Filter /A85", ascii85_encode(HELLO_PAGE.as_bytes()));
        assert_eq!(short.page_text(0).unwrap(), "Hello filters");
    }

    #[test]
    fn lzw_content_streams_extract_their_text() {
        let document = page_with(
            "/Filter /LZWDecode",
            lzw_encode(HELLO_PAGE.as_bytes(), true),
        );
        assert_eq!(document.page_text(0).unwrap(), "Hello filters");
        let late = page_with(
            "/Filter /LZWDecode /DecodeParms << /EarlyChange 0 >>",
            lzw_encode(HELLO_PAGE.repeat(60).as_bytes(), false),
        );
        assert!(late.page_text(0).unwrap().starts_with("Hello filters"));
    }

    #[test]
    fn run_length_content_streams_extract_their_text() {
        let mut encoded = vec![(HELLO_PAGE.len() - 1) as u8];
        encoded.extend_from_slice(HELLO_PAGE.as_bytes());
        encoded.push(128);
        let document = page_with("/Filter /RunLengthDecode", encoded);
        assert_eq!(document.page_text(0).unwrap(), "Hello filters");
    }

    #[test]
    fn flate_with_a_png_predictor_extracts_its_text() {
        let columns = 8usize;
        let mut padded = HELLO_PAGE.as_bytes().to_vec();
        while !padded.len().is_multiple_of(columns) {
            padded.push(b' ');
        }
        // Encode each row with the Up filter (2).
        let mut rows = Vec::new();
        let mut previous = vec![0u8; columns];
        for row in padded.chunks(columns) {
            rows.push(2);
            rows.extend(row.iter().zip(&previous).map(|(a, b)| a.wrapping_sub(*b)));
            previous = row.to_vec();
        }
        let document = page_with(
            &format!("/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns {columns} >>"),
            deflate(&rows),
        );
        assert_eq!(document.page_text(0).unwrap(), "Hello filters");
    }

    #[test]
    fn decode_parms_arrays_apply_per_filter() {
        let data = ascii85_encode(&deflate(HELLO_PAGE.as_bytes()));
        let document = page_with(
            "/Filter [/ASCII85Decode /FlateDecode] /DecodeParms [null << /Predictor 1 >>]",
            data,
        );
        assert_eq!(document.page_text(0).unwrap(), "Hello filters");
    }

    #[test]
    fn real_reportlab_style_ascii85_flate_fixture_extracts_text() {
        let pdf = include_bytes!("fixtures/pdf/ascii85-flate.pdf");
        assert!(find(pdf, b"ASCII85Decode", 0).is_some());
        let document = Document::parse(pdf).unwrap();
        let text = document.page_text(0).unwrap();
        assert!(text.contains("Hello Encrypted World"), "{text}");
        assert!(text.contains("Second line of text"), "{text}");
    }

    #[test]
    fn unsupported_filters_are_named_instead_of_reported_as_scanned() {
        let pdf = build(
            &[Content::Encoded(
                "/Filter /JBIG2Decode".into(),
                vec![1, 2, 3],
            )],
            false,
        );
        let document = Document::parse(&pdf).unwrap();
        let content = document.page_content(0).unwrap();
        assert!(content.text.is_empty());
        assert_eq!(
            content.notes,
            ["content stream uses JBIG2Decode, not supported"]
        );
        let output = render(&pdf, "doc.pdf", None, 10_000).unwrap();
        assert!(
            output.contains("page 1: content stream uses JBIG2Decode, not supported"),
            "{output}"
        );
        assert!(!output.contains("probably scanned"), "{output}");
        assert!(!output.contains("pdftoppm"), "{output}");
    }

    #[test]
    fn partially_decoded_pages_keep_text_and_name_the_skipped_stream() {
        let pdf = build(&[Content::Plain(HELLO_PAGE.into())], false);
        // Add a second, unsupported stream to the same page.
        let mut text = String::from_utf8_lossy(&pdf).into_owned();
        text = text.replacen("/Contents 6 0 R", "/Contents [6 0 R 7 0 R]", 1);
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice(
            b"\n7 0 obj\n<< /Length 3 /Filter /CCITTFaxDecode >>\nstream\nabc\nendstream\nendobj\n",
        );
        let output = render(&bytes, "doc.pdf", None, 10_000).unwrap();
        assert!(output.contains("Hello filters"), "{output}");
        assert!(
            output.contains("some content was not extracted: content stream uses CCITTFaxDecode"),
            "{output}"
        );
    }

    #[cfg(feature = "multimodal")]
    #[test]
    fn budget_cut_scan_page_is_not_attached_as_an_image() {
        let image = first_extra_object(2);
        let jpeg = include_bytes!("fixtures/read-image64/3x2.jpg");
        let mut object = format!(
            "<< /Type /XObject /Subtype /Image /Width 3 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
            jpeg.len()
        )
        .into_bytes();
        object.extend_from_slice(jpeg);
        object.extend_from_slice(b"\nendstream");
        let pdf = build_with(
            &[
                Content::Plain(HELLO_PAGE.into()),
                Content::Plain("q 612 0 0 792 0 0 cm /Im0 Do Q".into()),
            ],
            false,
            &format!("/XObject << /Im0 {image} 0 R >>"),
            vec![object],
        );
        // Unconstrained: the scan page is lifted as an image.
        let full = render_with_images(&pdf, "doc.pdf", None, 10_000, 4, false).unwrap();
        assert_eq!(full.images.len(), 1, "{}", full.text);
        // Budget fits page 1 only: the scan page is cut, named for
        // continuation, and must not also be attached.
        let cut = render_with_images(&pdf, "doc.pdf", None, 60, 4, false).unwrap();
        assert!(
            cut.text.contains("[truncated before page 2"),
            "{}",
            cut.text
        );
        assert!(cut.text.contains("Hello filters"), "{}", cut.text);
        assert!(cut.images.is_empty(), "{}", cut.text);
        assert!(!cut.text.contains("image"), "{}", cut.text);
    }

    #[test]
    fn scanned_message_needs_images_and_no_text_operators() {
        let image = first_extra_object(3);
        let object = b"<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /Length 1 >>\nstream\n\x00\nendstream".to_vec();
        let pdf = build_with(
            &[
                Content::Plain("q 612 0 0 792 0 0 cm /Im0 Do Q".into()),
                Content::Plain("0 0 100 100 re S".into()),
                Content::Plain("BT /F1 12 Tf 72 720 Td <00> Tj ET".into()),
            ],
            false,
            &format!("/XObject << /Im0 {image} 0 R >>"),
            vec![object],
        );
        let document = Document::parse(&pdf).unwrap();
        let scanned = document.page_content(0).unwrap();
        assert!(!scanned.text_operators);
        assert_eq!(scanned.images, 1);
        assert_eq!(document.page_content(1).unwrap().images, 0);
        assert!(document.page_content(2).unwrap().text_operators);
        let output = render(&pdf, "doc.pdf", None, 10_000).unwrap();
        assert!(
            output.contains(
                "Page(s) 1 paint images and have no text operators, so they are probably scanned"
            ),
            "{output}"
        );
        assert!(
            output.contains("Page(s) 2 contain no text and no images"),
            "{output}"
        );
        assert!(
            output.contains("Page(s) 3 show text whose characters"),
            "{output}"
        );
        // A blank page alone is not called scanned.
        let blank = render(&pdf, "doc.pdf", Some("2"), 10_000).unwrap();
        assert!(!blank.contains("probably scanned"), "{blank}");
    }

    const OWNER_ONLY: [(&str, &[u8]); 5] = [
        (
            "RC4-40 (R2)",
            include_bytes!("fixtures/pdf/encrypted-rc4_40.pdf"),
        ),
        (
            "RC4-128 (R3)",
            include_bytes!("fixtures/pdf/encrypted-rc4_128.pdf"),
        ),
        (
            "AESV2 (R4)",
            include_bytes!("fixtures/pdf/encrypted-aesv2.pdf"),
        ),
        (
            "AES-256 (R5)",
            include_bytes!("fixtures/pdf/encrypted-aes256r5.pdf"),
        ),
        (
            "AES-256 (R6)",
            include_bytes!("fixtures/pdf/encrypted-aes256.pdf"),
        ),
    ];

    /// The fixtures were written by Ghostscript (RC4) and pypdf (AES) with an
    /// owner password only; viewers open them with no password.
    #[test]
    fn owner_password_only_pdfs_decrypt_with_the_empty_user_password() {
        for (label, pdf) in OWNER_ONLY {
            assert!(find(pdf, b"/Encrypt", 0).is_some(), "{label} is encrypted");
            assert!(
                find(pdf, b"Hello Encrypted", 0).is_none(),
                "{label} hides text"
            );
            let document = Document::parse(pdf).unwrap_or_else(|e| panic!("{label}: {e}"));
            let text = document.page_text(0).unwrap();
            assert!(text.contains("Hello Encrypted World"), "{label}: {text}");
            assert!(text.contains("Second line of text"), "{label}: {text}");
            let rendered = render(pdf, "doc.pdf", None, 10_000).unwrap();
            assert!(
                rendered.contains("Hello Encrypted World"),
                "{label}: {rendered}"
            );
        }
    }

    #[cfg(feature = "multimodal")]
    #[test]
    fn decrypted_documents_are_never_re_emitted_as_a_subset() {
        for (label, pdf) in OWNER_ONLY {
            let document = Document::parse(pdf).unwrap();
            assert!(document.encrypted, "{label}");
            assert!(
                document.subset_pdf(&[1]).unwrap_err().contains("encrypted"),
                "{label}"
            );
        }
        assert!(
            !Document::parse(&build(&[Content::Plain("x".into())], false))
                .unwrap()
                .encrypted
        );
    }

    #[test]
    fn pdfs_with_a_real_user_password_report_password_required() {
        let pdf = include_bytes!("fixtures/pdf/encrypted-needpw.pdf");
        assert_eq!(Document::parse(pdf).err(), Some(PdfError::PasswordRequired));
        let message = render(pdf, "doc.pdf", None, 10_000).unwrap_err();
        assert!(message.contains("password required"), "{message}");
    }

    /// Replace `from` (once) with the same-length `to` in a fixture.
    fn patched(pdf: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
        assert_eq!(from.len(), to.len());
        let at = find(pdf, from, 0).unwrap_or_else(|| panic!("{from:?} not found"));
        let mut out = pdf.to_vec();
        out[at..at + from.len()].copy_from_slice(to);
        out
    }

    #[test]
    fn tampered_or_unsupported_encryption_fails_closed_without_panicking() {
        let rc4 = include_bytes!("fixtures/pdf/encrypted-rc4_128.pdf");
        let aes = include_bytes!("fixtures/pdf/encrypted-aes256.pdf");
        // A foreign security handler is named, never "decrypted".
        let foreign = patched(rc4, b"/Standard", b"/Foo.Bar!");
        match Document::parse(&foreign) {
            Err(PdfError::UnsupportedEncryption(what)) => {
                assert!(what.contains("security handler Foo.Bar!"), "{what}")
            }
            other => panic!("unexpected {:?}", other.err()),
        }
        // A different revision is unsupported rather than guessed at.
        let revision = patched(rc4, b"/R 3", b"/R 9");
        assert!(matches!(
            Document::parse(&revision).err(),
            Some(PdfError::UnsupportedEncryption(_))
        ));
        // A damaged user-validation entry is a password failure, not garbage text.
        let mut broken_user = aes.to_vec();
        let at = find(&broken_user, b"/U", 0).unwrap();
        let hex = find(&broken_user, b"<", at).unwrap() + 1;
        broken_user[hex] = if broken_user[hex] == b'0' { b'1' } else { b'0' };
        assert_eq!(
            Document::parse(&broken_user).err(),
            Some(PdfError::PasswordRequired)
        );
        // Every truncation of an encrypted file parses or errors; none panics.
        for len in (0..aes.len()).step_by(37) {
            let _ = Document::parse(&aes[..len]);
            let _ = Document::parse(&rc4[..len.min(rc4.len())]);
        }
        // Garbage in encrypted stream bodies yields a note, not a panic.
        let mut garbled = aes.to_vec();
        let stream = find(&garbled, b"stream", 0).unwrap() + 8;
        for byte in garbled.iter_mut().skip(stream).take(24) {
            *byte ^= 0xA5;
        }
        if let Ok(document) = Document::parse(&garbled) {
            let _ = document.page_content(0);
        }
        // An Encrypt entry that points nowhere stays a clean refusal.
        let mut dangling = build(&[Content::Plain(HELLO_PAGE.into())], false);
        let trailer = find(&dangling, b"/Root", 0).unwrap();
        dangling.splice(trailer..trailer, b"/Encrypt 99 0 R ".iter().copied());
        assert_eq!(Document::parse(&dangling).err(), Some(PdfError::Encrypted));
    }

    #[test]
    fn stream_over_the_decode_budget_is_reported_not_dropped() {
        let big = vec![b' '; (MAX_STREAM_DECODED as usize) + 1024];
        let pdf = build(
            &[Content::Encoded(
                "/Filter /FlateDecode".into(),
                deflate(&big),
            )],
            false,
        );
        let document = Document::parse(&pdf).unwrap();
        let content = document.page_content(0).unwrap();
        assert!(content.text.is_empty());
        assert!(
            content.notes[0].contains("exceeds the decode budget"),
            "{:?}",
            content.notes
        );
    }

    #[test]
    fn extracts_plain_and_deflated_pages_in_page_tree_order() {
        let pdf = build(
            &[
                Content::Plain("BT /F1 12 Tf 72 720 Td (Hello \\(PDF\\) world) Tj 0 -14 Td [(Sec) -300 (ond)] TJ ET".into()),
                Content::Deflated("BT /F1 12 Tf 14 TL 72 720 Td (Page two text) Tj T* (next line) Tj ET".into()),
            ],
            false,
        );
        assert!(is_pdf(&pdf));
        let document = Document::parse(&pdf).unwrap();
        assert_eq!(document.page_count(), 2);
        assert_eq!(document.page_text(0).unwrap(), "Hello (PDF) world\nSec ond");
        assert_eq!(document.page_text(1).unwrap(), "Page two text\nnext line");
    }

    #[test]
    fn decodes_type0_fonts_through_to_unicode_cmaps() {
        let pdf = build(
            &[Content::Plain(
                "BT /F1 12 Tf <000100020010001100120001> Tj ET".into(),
            )],
            true,
        );
        let document = Document::parse(&pdf).unwrap();
        assert_eq!(document.page_text(0).unwrap(), "HéabcH");
    }

    #[test]
    fn rejects_encrypted_and_reports_missing_text_layers() {
        let mut pdf = build(
            &[Content::Plain("q 100 0 0 100 0 0 cm /Im1 Do Q".into())],
            false,
        );
        let document = Document::parse(&pdf).unwrap();
        assert_eq!(document.page_text(0).unwrap(), "");
        let trailer = find(&pdf, b"/Root", 0).unwrap();
        pdf.splice(trailer..trailer, b"/Encrypt 9 0 R ".iter().copied());
        assert_eq!(Document::parse(&pdf).err(), Some(PdfError::Encrypted));
        assert!(Document::parse(b"%PDF-1.4\nnothing").is_err());
    }

    /// PDF 1.5+ files keep catalog, page tree and pages in compressed object
    /// streams, with no classic trailer.
    #[test]
    fn reads_pages_stored_in_compressed_object_streams() {
        use std::io::Write;
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /Contents 5 0 R >>",
        ];
        let mut header = String::new();
        let mut body = String::new();
        for (index, object) in objects.iter().enumerate() {
            header.push_str(&format!("{} {} ", index + 1, body.len()));
            body.push_str(object);
            body.push(' ');
        }
        let stream = format!("{header}{body}");
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(stream.as_bytes()).unwrap();
        let packed = encoder.finish().unwrap();
        let content = "BT (Compressed objects) Tj ET";
        let mut pdf = b"%PDF-1.7\n".to_vec();
        pdf.extend_from_slice(
            format!(
                "4 0 obj\n<< /Type /ObjStm /N 3 /First {} /Length {} /Filter /FlateDecode >>\nstream\n",
                header.len(),
                packed.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&packed);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf.extend_from_slice(
            format!(
                "5 0 obj\n<< /Length {} >>\nstream\n{content}\nendstream\nendobj\n%%EOF\n",
                content.len()
            )
            .as_bytes(),
        );
        let document = Document::parse(&pdf).unwrap();
        assert_eq!(document.page_count(), 1);
        assert_eq!(document.page_text(0).unwrap(), "Compressed objects");
    }

    #[test]
    fn page_selections_are_bounded_and_validated() {
        assert_eq!(parse_page_selection("1-3, 5", 9).unwrap(), vec![1, 2, 3, 5]);
        assert_eq!(parse_page_selection("8-", 9).unwrap(), vec![8, 9]);
        assert_eq!(parse_page_selection("2-50", 4).unwrap(), vec![2, 3, 4]);
        assert!(parse_page_selection("0", 9).is_err());
        assert!(parse_page_selection("4-2", 9).is_err());
        assert!(parse_page_selection("10", 9).is_err());
        assert!(parse_page_selection("1-21", 30).is_err());
        assert!(parse_page_selection("", 3).is_err());
    }

    fn repeated(unit: &str, bytes: usize) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut index = 1;
        while out.len() < bytes {
            out.extend_from_slice(format!("{index} 0 obj{unit}").as_bytes());
            index += 1;
        }
        out
    }

    fn assert_fast(data: &[u8]) {
        let started = std::time::Instant::now();
        let _ = Document::parse(data);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "parse took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn unterminated_streams_and_strings_parse_in_linear_time() {
        assert_fast(&repeated("<</Length 0>>stream\n", 1 << 20));
        assert_fast(&repeated("(", 1 << 20));
        assert_fast(&repeated("<", 1 << 20));
        assert_fast(&repeated("[", 1 << 20));
        assert_fast(&repeated("[(", 1 << 20));
        assert_fast(&repeated("<</A [", 1 << 20));
        let mut trailers = b"%PDF-1.7\n".to_vec();
        while trailers.len() < (1 << 20) {
            trailers.extend_from_slice(b"trailer(");
        }
        assert_fast(&trailers);
    }

    #[test]
    fn huge_declared_stream_length_does_not_overflow() {
        let pdf = b"%PDF-1.7\n1 0 obj<</Length 1e300>>stream\nabc\nendstream endobj\n";
        let _ = Document::parse(pdf);
        let pdf =
            b"%PDF-1.7\n1 0 obj<</Length 18446744073709551615>>stream\nabc\nendstream endobj\n";
        let _ = Document::parse(pdf);
    }

    #[test]
    fn expired_deadline_aborts_parsing() {
        let data = repeated("<</Length 0>>", 1 << 20);
        let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
        assert_eq!(
            Document::parse_until(&data, past).err(),
            Some(PdfError::TimedOut)
        );
    }

    #[test]
    fn compressed_array_bomb_fails_fast_with_too_large() {
        use std::io::Write;
        // 100 header entries all pointing at one 8M-element array.
        let mut header = String::new();
        for number in 1..=100 {
            header.push_str(&format!("{number} 0 "));
        }
        let mut body = header.clone().into_bytes();
        body.push(b'[');
        let chunk = "0 ".repeat(1 << 16);
        for _ in 0..128 {
            body.extend_from_slice(chunk.as_bytes());
        }
        body.push(b']');
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&body).unwrap();
        let packed = encoder.finish().unwrap();
        let mut pdf = format!(
            "%PDF-1.7\n9 0 obj<</Type /ObjStm /N 100 /First {} /Filter /FlateDecode /Length {}>>\nstream\n",
            header.len(),
            packed.len()
        )
        .into_bytes();
        pdf.extend_from_slice(&packed);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        let started = std::time::Instant::now();
        assert_eq!(Document::parse(&pdf).err(), Some(PdfError::TooLarge));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    fn font_page_pdf(fonts: usize, shared_cmap: bool) -> Vec<u8> {
        let mut objects: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        ];
        let mut font_entries = String::new();
        for index in 0..fonts {
            font_entries.push_str(&format!("/F{index} {} 0 R ", 6 + index));
        }
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << {font_entries}>> >> >>"
        ));
        let content = "BT /F0 12 Tf <0001> Tj ET";
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
        let cmap = "begincmap 1 begincodespacerange <0000> <FFFF> endcodespacerange 1 beginbfrange <0000> <FFFF> <0041> endbfrange endcmap";
        objects.push(format!(
            "<< /Length {} >>\nstream\n{cmap}\nendstream",
            cmap.len()
        ));
        for index in 0..fonts {
            let map = if shared_cmap { 5 } else { 6 + fonts + index };
            objects.push(format!(
                "<< /Type /Font /Subtype /Type0 /ToUnicode {map} 0 R >>"
            ));
        }
        if !shared_cmap {
            for _ in 0..fonts {
                objects.push(format!(
                    "<< /Length {} >>\nstream\n{cmap}\nendstream",
                    cmap.len()
                ));
            }
        }
        let mut out = b"%PDF-1.7\n".to_vec();
        for (index, object) in objects.iter().enumerate() {
            out.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        out
    }

    #[test]
    fn two_thousand_font_page_fails_with_too_large() {
        let pdf = font_page_pdf(2000, true);
        let document = Document::parse(&pdf).unwrap();
        assert_eq!(document.page_text(0), Err(PdfError::TooLarge));
    }

    #[test]
    fn per_font_cmaps_share_one_entry_budget_per_page() {
        // 65_536 entries per cmap, one distinct stream per font: the page
        // budget (262_144) is exhausted by the fifth font.
        let pdf = font_page_pdf(8, false);
        let document = Document::parse(&pdf).unwrap();
        assert_eq!(document.page_text(0), Err(PdfError::TooLarge));
    }

    #[test]
    fn fonts_sharing_a_to_unicode_stream_decode_it_once() {
        // Eight fonts, one 65_536-entry stream: fits the budget only because
        // the decoded map is shared.
        let pdf = font_page_pdf(8, true);
        let document = Document::parse(&pdf).unwrap();
        assert_eq!(document.page_text(0).unwrap(), "B");
    }
}
