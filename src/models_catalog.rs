//! Static, embedded model catalog.
//!
//! Model ids change rarely between releases, so instead of hitting each
//! provider's `/models` endpoint at startup (slow — OpenRouter alone returns
//! hundreds of entries and used to block the first frame), we bake a snapshot
//! into the binary. The picker is seeded from this synchronously, with zero
//! network. The live listing is still available on demand via `/models refresh`
//! (see [`crate::ui::slash`]) and for providers not baked here (custom gateways,
//! ollama).
//!
//! The data lives in `data/models.json`, keyed by *zerostack* provider name
//! (so `gemini`, not models.dev's `google`). Refresh it with
//! `scripts/gen-models-catalog.sh`.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::provider::ModelEntry;

const CATALOG_JSON: &str = include_str!("../data/models.json");

#[derive(serde::Deserialize)]
struct RawModel {
    #[serde(default)]
    image_input: Option<bool>,
    id: String,
    name: String,
    context: Option<u32>,
    #[serde(default)]
    input_price: Option<f64>,
    #[serde(default)]
    output_price: Option<f64>,
    #[serde(default)]
    cache_read_price: Option<f64>,
    #[serde(default)]
    cache_write_price: Option<f64>,
}

struct Catalog {
    entries: HashMap<String, Vec<ModelEntry>>,
    #[cfg_attr(not(feature = "multimodal"), allow(dead_code))]
    image_input: HashMap<String, HashMap<String, bool>>,
}
static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    let raw: HashMap<String, Vec<RawModel>> =
        serde_json::from_str(CATALOG_JSON).unwrap_or_default();
    let image_input = raw
        .into_iter()
        .map(|(provider, models)| {
            (
                provider,
                models
                    .into_iter()
                    .filter_map(|model| model.image_input.map(|image| (model.id, image)))
                    .collect(),
            )
        })
        .collect();
    Catalog {
        entries: parse_catalog(CATALOG_JSON),
        image_input,
    }
});

/// Explicit immutable input modality for an exact embedded provider/model ID.
/// Unknown gateways remain unknown; this lookup does not guess from model names.
#[cfg(feature = "multimodal")]
pub(crate) fn model_image_input(provider: &str, model: &str) -> Option<bool> {
    CATALOG.image_input.get(provider)?.get(model).copied()
}

fn parse_catalog(json: &str) -> HashMap<String, Vec<ModelEntry>> {
    let raw: HashMap<String, Vec<RawModel>> = match serde_json::from_str(json) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(
                "embedded data/models.json is malformed; using an empty model catalog: {error}"
            );
            return HashMap::new();
        }
    };

    raw.into_iter()
        .map(|(provider, models)| {
            let entries = models
                .into_iter()
                .map(|m| ModelEntry {
                    id: m.id,
                    display: m.name,
                    context_length: m.context,
                    kind: None,
                    input_price: m.input_price,
                    output_price: m.output_price,
                    cache_read_price: m.cache_read_price,
                    cache_write_price: m.cache_write_price,
                })
                .collect();
            (provider, entries)
        })
        .collect()
}

/// Baked model entries for a provider, or `None` when the provider is not in the
/// catalog (custom gateways, ollama — those resolve live).
pub fn catalog_entries(provider: &str) -> Option<&'static [ModelEntry]> {
    CATALOG.entries.get(provider).map(|v| v.as_slice())
}

#[cfg(test)]
mod tests {
    use super::parse_catalog;

    #[test]
    fn malformed_catalog_falls_back_to_empty() {
        let catalog = parse_catalog("{not valid json");

        assert!(catalog.is_empty());
    }

    #[test]
    fn catalog_cache_pricing_accepts_legacy_missing_and_explicit_zero_tiers() {
        let parsed = parse_catalog(
            r#"{"anthropic":[{"id":"legacy","name":"legacy","context":1000,"input_price":4,"output_price":20},{"id":"priced","name":"priced","context":1000,"input_price":4,"output_price":20,"cache_read_price":0,"cache_write_price":5}]}"#,
        );
        assert_eq!(parsed["anthropic"][0].cache_read_price, None);
        assert_eq!(parsed["anthropic"][0].cache_write_price, None);
        assert_eq!(parsed["anthropic"][1].cache_read_price, Some(0.0));
        assert_eq!(parsed["anthropic"][1].cache_write_price, Some(5.0));
    }
}
