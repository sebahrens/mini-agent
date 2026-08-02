//! Pure learned-skill realm loading for the Phase 6 worker.
//!
//! A loader invocation creates one private QuickJS context for one immutable identity-v2
//! artifact. Stored source sees no effect, proposal, or module globals. The only model-visible
//! values are frozen wrappers; wrapper arguments and results cross contexts as bounded strict
//! JSON strings. Invocation capability construction is deliberately owned by Phase 6 A17.

use rquickjs::context::EvalOptions;
use rquickjs::object::Property;
use rquickjs::{Context, Function, Object, Persistent, Runtime, Value};
use std::collections::HashSet;
use thiserror::Error;

use super::skills::{SKILL_REALM_HARDENING_JS, SkillArtifact, private_skill_source};
use super::worker::STRICT_CLONE_SOURCE;

const BRIDGE_FACTORY_SOURCE: &str = r#"
((parse, apply) => (original, encode) => encodedArguments => {
    try {
        const values = parse(encodedArguments);
        const result = apply(original, undefined, values);
        return encode(result);
    } catch (_) {
        throw 0;
    }
})(JSON.parse, Reflect.apply)
"#;

const MODEL_WRAPPER_FACTORY_SOURCE: &str = r#"
((freeze, parse) => (invoke, encode) => freeze(function (...values) {
    return parse(invoke(encode(values)));
}))(Object.freeze, JSON.parse)
"#;

/// Closed loader failures. Source text and thrown values never enter the error surface.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum RealmError {
    #[error("artifact identity validation failed")]
    Identity,
    #[error("artifact declares an invalid export name")]
    InvalidExport,
    #[error("artifact declares a duplicate export name")]
    DuplicateExport,
    #[error("artifact export collides with a model global")]
    ExportCollision,
    #[error("artifact initialization failed")]
    Initialization,
    #[error("artifact initialization scheduled pending jobs")]
    PendingInitializationJobs,
    #[error("artifact does not define every declared export as a function")]
    MissingExport,
    #[error("artifact wrapper installation failed")]
    WrapperInstallation,
}

/// Metadata proving which immutable artifact was installed into the model context.
#[derive(Debug)]
pub(crate) struct LoadedArtifact {
    artifact_id: String,
    exports: Vec<String>,
}

impl LoadedArtifact {
    pub(crate) fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    pub(crate) fn exports(&self) -> &[String] {
        &self.exports
    }
}

/// Load one identity-v2 artifact into a new private context and install exact frozen wrappers.
///
/// The caller must invoke this before model source evaluation. Any error rejects the whole
/// request; in particular, a pending initialization job is intentionally not drained because
/// running it would execute stored source after the loader has rejected the artifact.
pub(crate) fn load_artifact(
    runtime: &Runtime,
    model_context: &Context,
    artifact: &SkillArtifact,
) -> Result<LoadedArtifact, RealmError> {
    artifact
        .verify_identity()
        .map_err(|_| RealmError::Identity)?;
    validate_export_names(artifact)?;
    reject_model_collisions(model_context, artifact)?;

    let private_context = Context::full(runtime).map_err(|_| RealmError::Initialization)?;
    let (bridge_factory, private_encoder) = private_context
        .with(|ctx| {
            // Capture every boundary primitive before stored source can replace a global.
            let bridge_factory: Function = ctx.eval(BRIDGE_FACTORY_SOURCE)?;
            let encoder: Function = ctx.eval(STRICT_CLONE_SOURCE)?;
            ctx.eval::<(), _>(SKILL_REALM_HARDENING_JS)?;

            let mut options = EvalOptions::default();
            options.filename = Some(format!("skill-{}.js", artifact.id));
            // Evaluate the artifact itself as a Script. Wrapping it in a generated function would
            // change the accepted grammar (notably top-level return/import handling) and would
            // make source-created namespace objects part of the trusted loader boundary.
            let _: Value =
                ctx.eval_with_options(private_skill_source(artifact).as_bytes(), options)?;
            Ok::<_, rquickjs::Error>((
                Persistent::save(&ctx, bridge_factory),
                Persistent::save(&ctx, encoder),
            ))
        })
        .map_err(|_| RealmError::Initialization)?;

    if runtime.is_job_pending() {
        return Err(RealmError::PendingInitializationJobs);
    }

    // Resolve declared lexical/global bindings into a loader-owned namespace. Its properties are
    // ordinary own data properties, so later bridge construction cannot dispatch a getter or a
    // source-created namespace Proxy.
    let namespace = private_context
        .with(|ctx| {
            let namespace = Object::new_proto(ctx.clone(), None)?;
            for export in &artifact.exports {
                let original: Function = ctx.eval(export.name.as_bytes())?;
                namespace.prop(export.name.as_str(), Property::from(original).enumerable())?;
            }
            Ok::<_, rquickjs::Error>(Persistent::save(&ctx, namespace))
        })
        .map_err(|_| RealmError::MissingExport)?;

    if runtime.is_job_pending() {
        return Err(RealmError::PendingInitializationJobs);
    }

    let bridges = private_context
        .with(|ctx| {
            let namespace = namespace.restore(&ctx)?;
            let bridge_factory = bridge_factory.restore(&ctx)?;
            let private_encoder = private_encoder.restore(&ctx)?;
            artifact
                .exports
                .iter()
                .map(|export| {
                    let original: Function = namespace.get(export.name.as_str())?;
                    let bridge: Function =
                        bridge_factory.call((original, private_encoder.clone()))?;
                    Ok(Persistent::save(&ctx, bridge))
                })
                .collect::<rquickjs::Result<Vec<_>>>()
        })
        .map_err(|_| RealmError::MissingExport)?;

    if runtime.is_job_pending() {
        return Err(RealmError::PendingInitializationJobs);
    }

    let wrappers = build_model_wrappers(model_context, artifact, bridges)?;
    if runtime.is_job_pending() {
        return Err(RealmError::PendingInitializationJobs);
    }
    publish_model_wrappers(model_context, wrappers)?;
    Ok(LoadedArtifact {
        artifact_id: artifact.id.clone(),
        exports: artifact
            .exports
            .iter()
            .map(|export| export.name.clone())
            .collect(),
    })
}

fn validate_export_names(artifact: &SkillArtifact) -> Result<(), RealmError> {
    let mut names = HashSet::with_capacity(artifact.exports.len());
    for export in &artifact.exports {
        let mut characters = export.name.chars();
        let valid_start = characters
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
        if !valid_start
            || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
        {
            return Err(RealmError::InvalidExport);
        }
        if !names.insert(export.name.as_str()) {
            return Err(RealmError::DuplicateExport);
        }
    }
    Ok(())
}

fn reject_model_collisions(
    model_context: &Context,
    artifact: &SkillArtifact,
) -> Result<(), RealmError> {
    model_context.with(|ctx| {
        for export in &artifact.exports {
            if ctx
                .globals()
                .contains_key(export.name.as_str())
                .map_err(|_| RealmError::ExportCollision)?
            {
                return Err(RealmError::ExportCollision);
            }
        }
        Ok(())
    })
}

fn build_model_wrappers(
    model_context: &Context,
    artifact: &SkillArtifact,
    bridges: Vec<Persistent<Function<'static>>>,
) -> Result<Vec<(String, Persistent<Function<'static>>)>, RealmError> {
    model_context
        .with(|ctx| {
            // These closures are captured before model source runs, so model prototype/global
            // poisoning cannot change the clone or wrapper contract.
            let model_encoder: Function = ctx.eval(STRICT_CLONE_SOURCE)?;
            let wrapper_factory: Function = ctx.eval(MODEL_WRAPPER_FACTORY_SOURCE)?;
            let model_encoder = Persistent::save(&ctx, model_encoder);

            artifact
                .exports
                .iter()
                .zip(bridges)
                .map(|(export, bridge)| {
                    // Restoring in a sibling context is the A02-proven bridge. The bridge itself
                    // accepts and returns only bounded encoded strings; model arguments and skill
                    // results are never passed to the original function by reference.
                    let invoke = bridge.restore(&ctx)?;
                    let wrapper: Function =
                        wrapper_factory.call((invoke, model_encoder.clone().restore(&ctx)?))?;
                    Ok((export.name.clone(), Persistent::save(&ctx, wrapper)))
                })
                .collect::<rquickjs::Result<Vec<_>>>()
        })
        .map_err(|_| RealmError::WrapperInstallation)
}

fn publish_model_wrappers(
    model_context: &Context,
    wrappers: Vec<(String, Persistent<Function<'static>>)>,
) -> Result<(), RealmError> {
    model_context
        .with(|ctx| {
            let globals = ctx.globals();

            // Recheck immediately before publication. All wrappers have already been built, and
            // duplicate declarations were rejected before source evaluation.
            for (name, _) in &wrappers {
                if globals.contains_key(name.as_str())? {
                    return Err(rquickjs::Error::Unknown);
                }
            }

            let wrappers = wrappers
                .into_iter()
                .map(|(name, wrapper)| Ok((name, wrapper.restore(&ctx)?)))
                .collect::<rquickjs::Result<Vec<_>>>()?;
            // Every semantic failure mode has been checked before the first mutation. Property
            // installation uses final non-configurable descriptors so model code cannot delete or
            // replace a learned-skill binding. An unexpected engine resource failure rejects the
            // whole disposable request/runtime, as required by `load_artifact`'s caller contract.
            for (name, wrapper) in wrappers {
                globals.prop(name.as_str(), Property::from(wrapper).enumerable())?;
            }
            Ok::<_, rquickjs::Error>(())
        })
        .map_err(|_| RealmError::WrapperInstallation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::js::skills::{CapabilityManifest, SkillExport};

    #[test]
    fn invalid_identifier_is_rejected_before_source_generation() {
        let runtime = Runtime::new().unwrap();
        let model = Context::full(&runtime).unwrap();
        let artifact = SkillArtifact::new(
            "throw new Error('must not execute')".to_string(),
            "invalid export fixture".to_string(),
            Vec::new(),
            vec![SkillExport {
                name: "valid};globalThis.escape=1;//".to_string(),
                signature: "()".to_string(),
            }],
            vec!["true".to_string()],
            CapabilityManifest::pure(),
        )
        .unwrap();

        assert!(matches!(
            load_artifact(&runtime, &model, &artifact),
            Err(RealmError::InvalidExport)
        ));
    }
}
