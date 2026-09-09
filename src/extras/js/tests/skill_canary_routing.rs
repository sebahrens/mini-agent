use crate::extras::js::skills::CapabilityTier;
use crate::extras::js::skills::lifecycle::LifecycleStatus;
use crate::extras::js::skills::router::{CanaryCandidate, RouteKind, RouteRequest, route};

fn request(turn_id: &str) -> RouteRequest {
    RouteRequest {
        active_id: "active".into(),
        active_lineage_root_id: "root".into(),
        turn_id: turn_id.into(),
        policy_version: "v1".into(),
        canary_share_basis_points: 1_000,
        retrieval_score: 0.8,
        retrieval_rank: 2,
        index_generation: 7,
        candidate: Some(CanaryCandidate {
            candidate_id: "candidate".into(),
            lineage_root_id: "root".into(),
            status: LifecycleStatus::Canary,
            model_compatible: true,
            identity_valid: true,
            capability_tier: CapabilityTier::Pure,
            explicitly_idempotent: false,
        }),
    }
}

#[test]
fn same_turn_policy_and_key_route_identically() {
    let first = route(b"local-secret", &request("turn-1")).unwrap();
    let replay = route(b"local-secret", &request("turn-1")).unwrap();
    assert_eq!(first, replay);
    assert!(matches!(
        first.route_kind,
        RouteKind::Active | RouteKind::Canary
    ));
}

#[test]
fn skill_canary_distribution_stays_within_ten_percent() {
    let canaries = (0..10_000)
        .filter(|turn| {
            route(b"local-secret", &request(&format!("turn-{turn}")))
                .unwrap()
                .route_kind
                == RouteKind::Canary
        })
        .count();
    assert!((900..=1_100).contains(&canaries), "canary count {canaries}");
}

#[test]
fn skill_canary_routing_races_ineligible_and_root_canaries_never_replace_active() {
    let eligible = (0..100)
        .map(|turn| request(&format!("turn-{turn}")))
        .find(|input| route(b"key", input).unwrap().route_kind == RouteKind::Canary)
        .expect("fixture must select a canary before eligibility is changed");
    for status in [
        LifecycleStatus::Pending,
        LifecycleStatus::Verified,
        LifecycleStatus::Active,
        LifecycleStatus::Quarantined,
        LifecycleStatus::Superseded,
        LifecycleStatus::Retired,
        LifecycleStatus::Rejected,
    ] {
        let mut input = eligible.clone();
        input.candidate.as_mut().unwrap().status = status;
        let routed = route(b"key", &input).unwrap();
        assert_eq!(routed.route_kind, RouteKind::Active, "{status:?}");
        assert_eq!(routed.candidate_id, None, "{status:?}");
        assert!(!routed.fallback_before_effects, "{status:?}");
    }
    let mut root = eligible;
    let candidate = root.candidate.as_mut().unwrap();
    candidate.candidate_id = "root".into();
    candidate.lineage_root_id = "root".into();
    let routed = route(b"key", &root).unwrap();
    assert_eq!(routed.route_kind, RouteKind::Active);
    assert_eq!(routed.candidate_id, None);
    assert!(!routed.fallback_before_effects);
}

#[test]
fn frozen_fallback_eligibility_matches_capability_and_idempotence() {
    let selected = (0..100)
        .map(|turn| request(&format!("turn-{turn}")))
        .find(|input| route(b"local-secret", input).unwrap().route_kind == RouteKind::Canary)
        .expect("deterministic fixture should select a canary");
    for (tier, idempotent, eligible) in [
        (CapabilityTier::Pure, false, true),
        (CapabilityTier::Pure, true, true),
        (CapabilityTier::ReadOnly, false, false),
        (CapabilityTier::ReadOnly, true, true),
        (CapabilityTier::SideEffecting, false, false),
        (CapabilityTier::SideEffecting, true, false),
    ] {
        let mut input = selected.clone();
        let candidate = input.candidate.as_mut().unwrap();
        candidate.capability_tier = tier;
        candidate.explicitly_idempotent = idempotent;
        let routed = route(b"local-secret", &input).unwrap();
        assert_eq!(routed.route_kind, RouteKind::Canary);
        assert_eq!(
            routed.fallback_before_effects, eligible,
            "{tier:?}, idempotent={idempotent}"
        );
    }
}
