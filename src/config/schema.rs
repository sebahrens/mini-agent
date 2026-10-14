//! Side-effect-free editor schema generated from the canonical key catalogue.
//! Top-level and primitive shapes are checked; dynamic integration objects still
//! require the runtime loader's semantic/trust/permission validation.
use serde_json::{Map, Value, json};

use super::keys::{CONFIG_KEYS, ConfigShape};

fn shape(shape: ConfigShape) -> Value {
    match shape {
        ConfigShape::Any => json!({}),
        ConfigShape::Bool => json!({"type":"boolean"}),
        ConfigShape::Integer => json!({"type":"integer", "minimum":0}),
        ConfigShape::Number => json!({"type":"number"}),
        ConfigShape::String => json!({"type":"string"}),
        ConfigShape::Strings => json!({"type":"array","items":{"type":"string"}}),
        ConfigShape::Object => json!({"type":"object"}),
        ConfigShape::StringMap => json!({"type":"object","additionalProperties":{"type":"string"}}),
        ConfigShape::StringArrayMap => {
            json!({"type":"object","additionalProperties":{"type":"array","items":{"type":"string"}}})
        }
        ConfigShape::Enum(values) => json!({"type":"string","enum":values}),
        ConfigShape::BoolOrInteger => {
            json!({"anyOf":[{"type":"boolean"},{"type":"integer","minimum":0}]})
        }
    }
}

pub(crate) fn generate() -> Value {
    let mut properties = Map::new();
    let mut aliases = Vec::new();
    for key in CONFIG_KEYS {
        let mut field = shape(key.shape);
        field["description"] = json!(key.description);
        field["x-mini-agent-restart-required"] = json!(key.restart_required);
        if key.name == "lsp" {
            field["properties"] = json!({"enabled":{"anyOf":[{"type":"boolean"},{"const":"auto"}]}, "servers":{"type":"object"}});
        }
        if let Some(feature) = key.feature {
            field["x-mini-agent-feature"] = json!(feature);
            field["x-mini-agent-feature-enabled"] = json!(super::keys::feature_enabled(feature));
        }
        properties.insert(key.name.to_owned(), field.clone());
        for alias in key.aliases {
            let mut alias_field = field.clone();
            alias_field["deprecated"] = json!(true);
            alias_field["x-mini-agent-canonical-key"] = json!(key.name);
            properties.insert((*alias).to_owned(), alias_field);
            aliases.push(json!({"not":{"required":[key.name,alias]}}));
        }
    }
    json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "title":"mini-agent configuration",
        "description":"Structural editor schema. Dynamic integration objects require runtime validation. Project trust and command permissions remain independent. Keys unavailable in this build are preserved.",
        "type":"object", "properties":properties, "additionalProperties":false, "allOf":aliases
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config69_schema_canonical_catalogue_sample_and_wrong_shapes() {
        let schema = generate();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let sample: toml::Value =
            toml::from_str(include_str!("../../data/config.sample.toml")).unwrap();
        assert!(validator.is_valid(&serde_json::to_value(sample).unwrap()));
        let rich = serde_json::to_value(super::super::load::rich_default_config()).unwrap();
        assert!(
            validator.is_valid(&rich),
            "{:?}",
            validator
                .iter_errors(&rich)
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        );
        for key in CONFIG_KEYS {
            assert!(schema["properties"].get(key.name).is_some());
            for alias in key.aliases {
                assert_eq!(
                    schema["properties"][alias]["x-mini-agent-canonical-key"],
                    key.name
                );
            }
        }
        for bad in [
            json!({"max_tokens":"10"}),
            json!({"no_tools":[]}),
            json!({"cache_ttl":"forever"}),
            json!({"permission-deny":{"read":"*"}}),
            json!({"lsp":{"enabled":"yes"}}),
            json!({"invented":true}),
            json!({"permission_deny":{},"permission-deny":{}}),
        ] {
            assert!(!validator.is_valid(&bad), "{bad}");
        }
        assert!(validator.is_valid(&json!({"lsp":{"enabled":"auto","servers":{"custom":{"command":"x"}}},"permission_deny":{"read":["*"]}})));
    }
}
