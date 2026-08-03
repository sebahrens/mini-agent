//! Public product coordinates and intentionally retained compatibility names.
//!
//! Keep these categories separate: changing a public repository or protocol
//! identity must never silently migrate persisted user data or project policy.

pub const PUBLIC_NAME: &str = "mini-agent";
#[allow(dead_code)] // Validated by release tooling even when no runtime URL needs the slug alone.
pub const REPOSITORY_SLUG: &str = "sebahrens/mini-agent";
pub const REPOSITORY_URL: &str = "https://github.com/sebahrens/mini-agent";

pub const LEGACY_APP_COMPONENT: &str = "zerostack";
pub const LEGACY_PROJECT_DIRECTORY: &str = ".zerostack";
#[allow(dead_code)] // Documents the compatibility family checked by release tooling and tests.
pub const LEGACY_ENV_PREFIX: &str = "ZEROSTACK_";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_identity_matrix_keeps_public_and_compatibility_names_distinct() {
        assert_eq!(PUBLIC_NAME, env!("CARGO_PKG_NAME"));
        assert_eq!(REPOSITORY_SLUG, "sebahrens/mini-agent");
        assert_eq!(
            REPOSITORY_URL,
            format!("https://github.com/{REPOSITORY_SLUG}")
        );
        assert_eq!(REPOSITORY_URL, env!("CARGO_PKG_REPOSITORY"));
        assert_eq!(REPOSITORY_URL, env!("CARGO_PKG_HOMEPAGE"));
        assert_eq!(LEGACY_APP_COMPONENT, "zerostack");
        assert_eq!(LEGACY_PROJECT_DIRECTORY, ".zerostack");
        assert_eq!(LEGACY_ENV_PREFIX, "ZEROSTACK_");
        assert_ne!(PUBLIC_NAME, LEGACY_APP_COMPONENT);
    }
}
