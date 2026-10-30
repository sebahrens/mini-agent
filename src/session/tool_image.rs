//! Literal, bounded image tool history. Imported payloads never name files or URLs.
use base64::Engine;
use serde::{Deserialize, Serialize};

const MAX_TOOL_IMAGES: usize = 8;
const MAX_TOOL_IMAGE_ENCODED_BYTES: usize = 7 * 1024 * 1024;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedImage")]
pub struct PersistedToolImage {
    /// Shared so Session clones (several per native turn) do not copy up to
    /// 5 MiB of base64 per image.
    #[serde(serialize_with = "serialize_shared")]
    data: std::sync::Arc<str>,
    mime_type: super::user::ImageMime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<rig::message::ImageDetail>,
}
fn serialize_shared<S: serde::Serializer>(
    data: &std::sync::Arc<str>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(data)
}
impl std::fmt::Debug for PersistedToolImage {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("PersistedToolImage")
            .field("mime_type", &self.mime_type)
            .field("encoded_bytes", &self.data.len())
            .finish()
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedImage {
    data: String,
    mime_type: super::user::ImageMime,
    #[serde(default)]
    detail: Option<rig::message::ImageDetail>,
}
impl TryFrom<UncheckedImage> for PersistedToolImage {
    type Error = &'static str;
    fn try_from(value: UncheckedImage) -> Result<Self, Self::Error> {
        let image = Self {
            data: value.data.into(),
            mime_type: value.mime_type,
            detail: value.detail,
        };
        image.validate()?;
        Ok(image)
    }
}
impl PersistedToolImage {
    pub(crate) fn encoded_len(&self) -> usize {
        self.data.len()
    }
    pub(crate) fn evidence_identity(&self) -> compact_str::CompactString {
        use sha2::Digest;
        let mut digest = sha2::Sha256::new();
        digest.update(b"mini-agent-compacted-image-v1\0");
        digest.update(self.mime_type.as_str().as_bytes());
        digest.update([0]);
        digest.update(serde_json::to_vec(&self.detail).expect("image detail JSON"));
        digest.update([0]);
        digest.update(self.data.as_bytes());
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            .into()
    }
    pub(crate) fn from_rig(image: &rig::message::Image) -> Result<Self, &'static str> {
        use rig::message::{DocumentSourceKind, MimeType};
        let DocumentSourceKind::Base64(data) = &image.data else {
            return Err("Only literal base64 tool images can be retained");
        };
        if image.additional_params.is_some() {
            return Err("Tool image parameters cannot be retained");
        }
        let mime_type = image
            .media_type
            .as_ref()
            .and_then(|mime| super::user::ImageMime::parse(mime.to_mime_type()))
            .ok_or("Unsupported tool image MIME type")?;
        let value = Self {
            data: data.as_str().into(),
            mime_type,
            detail: image.detail.clone(),
        };
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let cap = crate::agent::tools::read_image::MAX_IMAGE_BYTES;
        if self.data.is_empty() || self.data.len() > cap.div_ceil(3) * 4 {
            return Err("Tool image exceeds the 5 MiB byte limit");
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(self.data.as_bytes())
            .map_err(|_| "Invalid tool image base64")?;
        if bytes.len() > cap {
            return Err("Tool image exceeds the 5 MiB byte limit");
        }
        let info = crate::agent::tools::read_image::inspect(&bytes)?
            .ok_or("Unsupported tool image format")?;
        if info.format.mime() != self.mime_type.as_str() {
            return Err("Tool image MIME type does not match its bytes");
        }
        Ok(())
    }
    pub(crate) fn to_rig(&self) -> rig::message::Image {
        rig::message::Image {
            data: rig::message::DocumentSourceKind::Base64(self.data.to_string()),
            media_type: Some(self.mime_type.sdk()),
            detail: self.detail.clone(),
            additional_params: None,
        }
    }
}
pub(crate) fn validate_images(images: &[PersistedToolImage]) -> Result<(), &'static str> {
    if images.len() > MAX_TOOL_IMAGES
        || images
            .iter()
            .try_fold(0usize, |total, image| total.checked_add(image.data.len()))
            .is_none_or(|total| total > MAX_TOOL_IMAGE_ENCODED_BYTES)
    {
        return Err("Tool images exceed their aggregate byte/count limit");
    }
    for image in images {
        image.validate()?;
    }
    Ok(())
}
pub(crate) fn from_result(
    result: &rig::message::ToolResult,
) -> Result<Vec<PersistedToolImage>, &'static str> {
    // Validate the borrowed envelope before cloning or decoding any payload.
    // A non-read tool can return several images, so a per-image bound alone
    // does not constrain the allocation of the complete retained result.
    let mut count = 0usize;
    let mut encoded_bytes = 0usize;
    for part in result.content.iter() {
        if let rig::message::ToolResultContent::Image(image) = part {
            count = count.saturating_add(1);
            if let rig::message::DocumentSourceKind::Base64(data) = &image.data {
                encoded_bytes = encoded_bytes.saturating_add(data.len());
            }
            if count > MAX_TOOL_IMAGES || encoded_bytes > MAX_TOOL_IMAGE_ENCODED_BYTES {
                return Err("Tool images exceed their aggregate byte/count limit");
            }
        }
    }
    let images = result
        .content
        .iter()
        .filter_map(|part| match part {
            rig::message::ToolResultContent::Image(image) => {
                Some(PersistedToolImage::from_rig(image))
            }
            _ => None,
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_images(&images)?;
    Ok(images)
}
