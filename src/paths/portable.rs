use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// The strictest common component limits used by supported filesystems.
pub const MAX_PORTABLE_COMPONENT_BYTES: usize = 255;
pub const MAX_PORTABLE_COMPONENT_UTF16_UNITS: usize = 255;
/// Linux's common `PATH_MAX` is stricter than the Windows extended-path limit.
pub const MAX_PORTABLE_PATH_BYTES: usize = 4096;
pub const MAX_PORTABLE_PATH_UTF16_UNITS: usize = 32_767;

const OPAQUE_IDENTITY_VERSION: &[u8] = b"zerostack-opaque-name-v1";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortablePathError {
    #[error("a portable path component cannot be empty")]
    EmptyComponent,
    #[error("dot path components are not portable")]
    DotComponent,
    #[error("parent traversal is not allowed")]
    ParentTraversal,
    #[error("absolute, drive-prefixed, and UNC paths are not portable components")]
    AbsolutePath,
    #[error("path component contains NUL")]
    NullCharacter,
    #[error("path component contains control character U+{codepoint:04X}")]
    ControlCharacter { codepoint: u32 },
    #[error("path separators are not allowed in a portable component")]
    Separator,
    #[error("alternate data stream syntax is not allowed in a portable component")]
    AlternateDataStream,
    #[error("character {character:?} is forbidden in a portable component")]
    ForbiddenCharacter { character: char },
    #[error("portable components cannot end in a dot or space")]
    TrailingDotOrSpace,
    #[error("{name:?} is a reserved Windows device name")]
    ReservedWindowsDevice { name: String },
    #[error(
        "component is too long ({utf8_bytes} UTF-8 bytes, {utf16_units} UTF-16 units; maxima are 255 and 255)"
    )]
    ComponentTooLong {
        utf8_bytes: usize,
        utf16_units: usize,
    },
    #[error(
        "path is too long ({utf8_bytes} UTF-8 bytes, {utf16_units} UTF-16 units; maxima are 4096 and 32767)"
    )]
    PathTooLong {
        utf8_bytes: usize,
        utf16_units: usize,
    },
    #[error("path contains a non-UTF-8 component")]
    NonUtf8Component,
    #[error("digest filename extension must be a short ASCII alphanumeric identifier")]
    InvalidExtension,
    #[error("candidate path is outside the requested root")]
    OutsideRoot,
    #[error("path traverses a symbolic link, junction, or reparse point at {path:?}")]
    LinkTraversal { path: PathBuf },
    #[error("could not inspect {path:?}: {message}")]
    Filesystem { path: PathBuf, message: String },
}

/// Validate one human-authored path component against a host-independent policy.
pub fn validate_portable_component(component: &str) -> Result<(), PortablePathError> {
    if component.is_empty() {
        return Err(PortablePathError::EmptyComponent);
    }
    if component == "." || component == ".." {
        return Err(PortablePathError::DotComponent);
    }
    if looks_absolute(component) {
        return Err(PortablePathError::AbsolutePath);
    }

    let utf8_bytes = component.len();
    let utf16_units = component.encode_utf16().count();
    if utf8_bytes > MAX_PORTABLE_COMPONENT_BYTES || utf16_units > MAX_PORTABLE_COMPONENT_UTF16_UNITS
    {
        return Err(PortablePathError::ComponentTooLong {
            utf8_bytes,
            utf16_units,
        });
    }

    for character in component.chars() {
        match character {
            '\0' => return Err(PortablePathError::NullCharacter),
            '/' | '\\' => return Err(PortablePathError::Separator),
            ':' => return Err(PortablePathError::AlternateDataStream),
            '<' | '>' | '"' | '|' | '?' | '*' => {
                return Err(PortablePathError::ForbiddenCharacter { character });
            }
            value if value.is_control() => {
                return Err(PortablePathError::ControlCharacter {
                    codepoint: value as u32,
                });
            }
            _ => {}
        }
    }

    if component.ends_with('.') || component.ends_with(' ') {
        return Err(PortablePathError::TrailingDotOrSpace);
    }
    if is_windows_device(component) {
        return Err(PortablePathError::ReservedWindowsDevice {
            name: component.to_string(),
        });
    }
    Ok(())
}

/// Produce the canonical collision key used on every host.
///
/// NFKC is applied before and after full Unicode case folding so canonically
/// equivalent names and case variants collide even on a case-sensitive host.
pub fn collision_key(component: &str) -> Result<String, PortablePathError> {
    validate_portable_component(component)?;
    let normalized: String = component.nfkc().collect();
    let folded = unicase::UniCase::new(normalized).to_folded_case();
    Ok(folded.nfkc().collect())
}

/// Build a full SHA-256 opaque component from a versioned, length-prefixed identity.
pub fn opaque_name(namespace: &str, identity_parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    update_length_prefixed(&mut hasher, OPAQUE_IDENTITY_VERSION);
    update_length_prefixed(&mut hasher, namespace.as_bytes());
    hasher.update((identity_parts.len() as u64).to_be_bytes());
    for part in identity_parts {
        update_length_prefixed(&mut hasher, part);
    }
    let digest = hasher.finalize();
    let mut output = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// Build `<full-sha256>.<extension>` without including display text in the name.
pub fn digest_filename(
    namespace: &str,
    identity_parts: &[&[u8]],
    extension: &str,
) -> Result<String, PortablePathError> {
    if extension.is_empty()
        || extension.len() > 16
        || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(PortablePathError::InvalidExtension);
    }
    let filename = format!("{}.{}", opaque_name(namespace, identity_parts), extension);
    validate_portable_component(&filename)?;
    Ok(filename)
}

/// Validate a relative path component-by-component, independent of host syntax.
pub fn validate_portable_relative_path(path: &Path) -> Result<(), PortablePathError> {
    let raw = path.to_str().ok_or(PortablePathError::NonUtf8Component)?;
    if raw.is_empty() {
        return Err(PortablePathError::EmptyComponent);
    }
    if looks_absolute(raw) || path.is_absolute() {
        return Err(PortablePathError::AbsolutePath);
    }
    let utf8_bytes = raw.len();
    let utf16_units = raw.encode_utf16().count();
    if utf8_bytes > MAX_PORTABLE_PATH_BYTES || utf16_units > MAX_PORTABLE_PATH_UTF16_UNITS {
        return Err(PortablePathError::PathTooLong {
            utf8_bytes,
            utf16_units,
        });
    }

    for component in raw.split(['/', '\\']) {
        if component.is_empty() {
            return Err(PortablePathError::EmptyComponent);
        }
        if component == "." {
            return Err(PortablePathError::DotComponent);
        }
        if component == ".." {
            return Err(PortablePathError::ParentTraversal);
        }
        validate_portable_component(component)?;
    }
    Ok(())
}

/// Join a validated portable relative path and prove lexical containment.
pub fn contained_join(root: &Path, relative: &Path) -> Result<PathBuf, PortablePathError> {
    validate_portable_relative_path(relative)?;
    let relative = relative
        .to_str()
        .ok_or(PortablePathError::NonUtf8Component)?;
    let mut candidate = root.to_path_buf();
    for component in relative.split(['/', '\\']) {
        candidate.push(component);
    }
    ensure_contained(root, &candidate)?;
    Ok(candidate)
}

/// Prove containment using path components, not string prefixes.
pub fn ensure_contained(root: &Path, candidate: &Path) -> Result<(), PortablePathError> {
    let remainder = candidate
        .strip_prefix(root)
        .map_err(|_| PortablePathError::OutsideRoot)?;
    for component in remainder.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => return Err(PortablePathError::DotComponent),
            Component::ParentDir => return Err(PortablePathError::ParentTraversal),
            Component::RootDir | Component::Prefix(_) => {
                return Err(PortablePathError::OutsideRoot);
            }
        }
    }
    Ok(())
}

/// Reject an existing symlink, Windows junction, or other reparse point in a path.
///
/// Mutation code must combine this policy with the race-resistant secure
/// filesystem contract; this preflight helper alone is not a write primitive.
pub fn ensure_no_link_traversal(root: &Path, candidate: &Path) -> Result<(), PortablePathError> {
    ensure_contained(root, candidate)?;
    let mut current = root.to_path_buf();
    inspect_link(&current)?;
    let remainder = candidate
        .strip_prefix(root)
        .map_err(|_| PortablePathError::OutsideRoot)?;
    for component in remainder.components() {
        let Component::Normal(value) = component else {
            return Err(PortablePathError::ParentTraversal);
        };
        current.push(value);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if is_link_or_reparse(&metadata) => {
                return Err(PortablePathError::LinkTraversal { path: current });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(PortablePathError::Filesystem {
                    path: current,
                    message: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

fn inspect_link(path: &Path) -> Result<(), PortablePathError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_link_or_reparse(&metadata) => Err(PortablePathError::LinkTraversal {
            path: path.to_path_buf(),
        }),
        Ok(_) => Ok(()),
        Err(error) => Err(PortablePathError::Filesystem {
            path: path.to_path_buf(),
            message: error.to_string(),
        }),
    }
}

#[cfg(not(windows))]
pub(crate) fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
pub(crate) fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn update_length_prefixed(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn looks_absolute(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with('\\')
        || value
            .as_bytes()
            .get(1)
            .is_some_and(|byte| *byte == b':' && value.as_bytes()[0].is_ascii_alphabetic())
}

fn is_windows_device(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
        .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}
