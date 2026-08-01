use crate::extras::js::skills::capability::{
    CapabilityContext, CapabilityDenialReason, CapabilityError, SkillExecutionAttribution,
};
use crate::extras::js::skills::{
    CapabilityManifest, CapabilityTier, HostCapability, test_manifest,
};

fn id(byte: char) -> String {
    std::iter::repeat_n(byte, 64).collect()
}

#[test]
fn skill_and_session_permissions_are_both_required() {
    let context = CapabilityContext::default();
    let _guard = context
        .enter(SkillExecutionAttribution {
            skill_id: id('a'),
            export_name: "run".into(),
            manifest: test_manifest(CapabilityTier::ReadOnly, vec![HostCapability::ReadFile])
                .unwrap(),
        })
        .unwrap();
    assert!(context.authorize(HostCapability::ReadFile, true).is_ok());
    assert!(matches!(
        context.authorize(HostCapability::ReadFile, false),
        Err(CapabilityError::Denied(denial))
            if denial.reason == CapabilityDenialReason::SessionDenied
    ));
    assert!(matches!(
        context.authorize(HostCapability::Spawn, true),
        Err(CapabilityError::Denied(denial))
            if denial.reason == CapabilityDenialReason::Undeclared
    ));
}

#[test]
fn skill_capability_nested_manifests_intersect_and_guards_clear_context() {
    let context = CapabilityContext::default();
    {
        let _caller = context
            .enter(SkillExecutionAttribution {
                skill_id: id('a'),
                export_name: "caller".into(),
                manifest: test_manifest(
                    CapabilityTier::SideEffecting,
                    vec![HostCapability::ReadFile, HostCapability::Spawn],
                )
                .unwrap(),
            })
            .unwrap();
        {
            let _callee = context
                .enter(SkillExecutionAttribution {
                    skill_id: id('b'),
                    export_name: "callee".into(),
                    manifest: test_manifest(
                        CapabilityTier::ReadOnly,
                        vec![HostCapability::ReadFile],
                    )
                    .unwrap(),
                })
                .unwrap();
            assert!(context.authorize(HostCapability::ReadFile, true).is_ok());
            assert!(context.authorize(HostCapability::Spawn, true).is_err());
        }
        assert!(context.authorize(HostCapability::Spawn, true).is_ok());
    }
    assert!(context.current().is_none());
    // Outside wrappers, normal session permission remains authoritative.
    assert!(context.authorize(HostCapability::Spawn, true).is_ok());
}

#[tokio::test]
async fn skill_capability_async_scope_survives_yield_and_cleans_up() {
    let context = CapabilityContext::default();
    {
        let _guard = context
            .enter(SkillExecutionAttribution {
                skill_id: id('d'),
                export_name: "async_export".into(),
                manifest: test_manifest(CapabilityTier::ReadOnly, vec![HostCapability::ReadFile])
                    .unwrap(),
            })
            .unwrap();
        tokio::task::yield_now().await;
        assert!(context.authorize(HostCapability::ReadFile, true).is_ok());
        assert!(context.authorize(HostCapability::WriteFile, true).is_err());
    }
    assert!(context.current().is_none());
}

#[test]
fn tier_zero_denies_every_host_and_forged_ids_fail_closed() {
    let context = CapabilityContext::default();
    let invalid = context.enter(SkillExecutionAttribution {
        skill_id: "short".into(),
        export_name: "run".into(),
        manifest: CapabilityManifest::pure(),
    });
    assert!(matches!(invalid, Err(CapabilityError::InvalidAttribution)));

    let _guard = context
        .enter(SkillExecutionAttribution {
            skill_id: id('c'),
            export_name: "run".into(),
            manifest: CapabilityManifest::pure(),
        })
        .unwrap();
    for operation in [
        HostCapability::ReadFile,
        HostCapability::WriteFile,
        HostCapability::Spawn,
        HostCapability::Fetch,
    ] {
        assert!(context.authorize(operation, true).is_err());
    }
}
