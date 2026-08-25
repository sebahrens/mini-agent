use sha2::{Digest, Sha256};

const AJV_BUNDLE: &[u8] = include_bytes!("../vendor/ajv.min.js");
const AJV_8_12_0_SHA256: &str = "2866583ce03b97b6a6c04ffae0cc5399cf54444cc5e2b098449e7a85b372afa1";

#[test]
fn vendored_ajv_bundle_matches_reviewed_upstream_artifact() {
    let digest = Sha256::digest(AJV_BUNDLE);
    assert_eq!(crate::hex::encode_lower(digest), AJV_8_12_0_SHA256);
}
