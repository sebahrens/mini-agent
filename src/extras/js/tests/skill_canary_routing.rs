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
    for status in [
        LifecycleStatus::Pending,
        LifecycleStatus::Verified,
        LifecycleStatus::Active,
        LifecycleStatus::Quarantined,
        LifecycleStatus::Superseded,
        LifecycleStatus::Retired,
        LifecycleStatus::Rejected,
    ] {
        let mut input = request("turn");
        input.candidate.as_mut().unwrap().status = status;
        assert_eq!(route(b"key", &input).unwrap().route_kind, RouteKind::Active);
    }
    let mut root = request("turn");
    let candidate = root.candidate.as_mut().unwrap();
    candidate.candidate_id = "root".into();
    candidate.lineage_root_id = "root".into();
    assert_eq!(route(b"key", &root).unwrap().route_kind, RouteKind::Active);
}

#[test]
fn fallback_never_replays_effects() {
    let mut selected = None;
    for turn in 0..100 {
        let routed = route(b"local-secret", &request(&format!("turn-{turn}"))).unwrap();
        if routed.route_kind == RouteKind::Canary {
            selected = Some(routed);
            break;
        }
    }
    let selected = selected.expect("deterministic fixture should select a canary");
    assert!(selected.may_fallback(false));
    assert!(!selected.may_fallback(true));

    let mut tier_two = request("known");
    tier_two.canary_share_basis_points = 1_000;
    tier_two.candidate.as_mut().unwrap().capability_tier = CapabilityTier::SideEffecting;
    for turn in 0..100 {
        tier_two.turn_id = format!("tier2-{turn}");
        let routed = route(b"local-secret", &tier_two).unwrap();
        if routed.route_kind == RouteKind::Canary {
            assert!(!routed.may_fallback(false));
            return;
        }
    }
    panic!("deterministic fixture should select a tier-two canary");
}
