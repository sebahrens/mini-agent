//! Pure bounds and reserved names for Windows AppContainer control-record publication.
//!
//! Only a complete, synced record is authority. A staging leaf describes no SID, roots or
//! effects; Windows recovery may reclaim it only after exclusive/private object validation.

pub(super) const MAX_CONTROL_RECORD_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecordKind {
    Intent,
    Journal,
}

impl RecordKind {
    pub(super) fn extension(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Journal => "json",
        }
    }

    pub(super) fn leaves(self, id: uuid::Uuid) -> (String, String) {
        let final_leaf = format!("{}.{}", id.simple(), self.extension());
        (format!("{final_leaf}.staged-v1"), final_leaf)
    }
}

pub(super) fn is_staging_leaf(name: &str) -> bool {
    let Some(record) = name.strip_suffix(".staged-v1") else {
        return false;
    };
    let Some((id, extension)) = record.split_once('.') else {
        return false;
    };
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && matches!(extension, "intent" | "json")
}

pub(super) fn validate_payload(payload: &[u8]) -> Result<(), &'static str> {
    if payload.is_empty() || payload.len() > MAX_CONTROL_RECORD_BYTES {
        Err("sandbox: AppContainer control record payload exceeded publication bounds")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_staging_namespace_is_exact_and_bounded() {
        for kind in [RecordKind::Intent, RecordKind::Journal] {
            let (stage, final_leaf) = kind.leaves(uuid::Uuid::nil());
            assert!(is_staging_leaf(&stage));
            assert!(!is_staging_leaf(&final_leaf));
            for invalid in [
                format!("prefix{stage}"),
                format!("{stage}.extra"),
                stage.replace("0000", "AAAA"),
                stage.replace(".staged-v1", ".staged-v2"),
                stage.replace(kind.extension(), "other"),
                format!("../{stage}"),
            ] {
                assert!(!is_staging_leaf(&invalid), "{invalid}");
            }
        }
    }

    #[test]
    fn publication_payload_limit_is_enforced_before_object_creation() {
        assert!(validate_payload(&[]).is_err());
        assert!(validate_payload(&vec![b'x'; MAX_CONTROL_RECORD_BYTES]).is_ok());
        assert!(validate_payload(&vec![b'x'; MAX_CONTROL_RECORD_BYTES + 1]).is_err());
    }
}
