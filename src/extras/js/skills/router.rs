//! Deterministic lineage-aware replacement-canary routing.

use sha2::{Digest, Sha256};

use super::CapabilityTier;
use super::lifecycle::LifecycleStatus;
use crate::hex;

#[derive(Debug, Clone, PartialEq)]
pub struct CanaryCandidate {
    pub candidate_id: String,
    pub lineage_root_id: String,
    pub status: LifecycleStatus,
    pub model_compatible: bool,
    pub identity_valid: bool,
    pub capability_tier: CapabilityTier,
    pub explicitly_idempotent: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteRequest {
    pub active_id: String,
    pub active_lineage_root_id: String,
    pub turn_id: String,
    pub policy_version: String,
    pub canary_share_basis_points: u16,
    pub retrieval_score: f64,
    pub retrieval_rank: u32,
    pub index_generation: u64,
    pub candidate: Option<CanaryCandidate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    Active,
    Canary,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrozenRoute {
    pub chosen_id: String,
    pub active_id: String,
    pub candidate_id: Option<String>,
    pub route_kind: RouteKind,
    pub route_fingerprint: String,
    pub policy_version: String,
    pub canary_share_basis_points: u16,
    pub retrieval_score: f64,
    pub retrieval_rank: u32,
    pub index_generation: u64,
    pub fallback_before_effects: bool,
}

#[derive(Debug, thiserror::Error)]
#[allow(clippy::enum_variant_names)]
pub enum RouterError {
    #[error("canary share exceeds the configured ten-percent ceiling")]
    InvalidShare,
    #[error("active lineage metadata is missing or inconsistent")]
    InvalidActiveLineage,
    #[error("retrieval score must be finite")]
    InvalidScore,
}

pub fn route(secret_key: &[u8], request: &RouteRequest) -> Result<FrozenRoute, RouterError> {
    if request.canary_share_basis_points > 1_000 {
        return Err(RouterError::InvalidShare);
    }
    if secret_key.is_empty()
        || request.active_id.is_empty()
        || request.active_lineage_root_id.is_empty()
        || request.turn_id.is_empty()
        || request.policy_version.is_empty()
    {
        return Err(RouterError::InvalidActiveLineage);
    }
    if !request.retrieval_score.is_finite() {
        return Err(RouterError::InvalidScore);
    }

    let eligible = request.candidate.as_ref().filter(|candidate| {
        candidate.status == LifecycleStatus::Canary
            && candidate.identity_valid
            && candidate.model_compatible
            && candidate.candidate_id != request.active_id
            && candidate.lineage_root_id == request.active_lineage_root_id
            // A lineage-root canary has no active representative and is never
            // provided as a replacement candidate to this router.
            && candidate.candidate_id != candidate.lineage_root_id
    });

    let candidate_id = eligible.map(|candidate| candidate.candidate_id.as_str());
    let (fingerprint, bucket_seed) = route_fingerprint(
        secret_key,
        &request.active_lineage_root_id,
        candidate_id.unwrap_or("no-candidate"),
        &request.turn_id,
        &request.policy_version,
    );
    let bucket = u32::from(bucket_seed) * 10_000 / 65_536;
    let choose_canary = eligible.is_some() && bucket < u32::from(request.canary_share_basis_points);
    let chosen_id = candidate_id
        .filter(|_| choose_canary)
        .unwrap_or(request.active_id.as_str())
        .to_string();
    let fallback_before_effects = eligible.is_some_and(|candidate| {
        candidate.capability_tier == CapabilityTier::Pure
            || (candidate.capability_tier == CapabilityTier::ReadOnly
                && candidate.explicitly_idempotent)
    });
    Ok(FrozenRoute {
        chosen_id,
        active_id: request.active_id.clone(),
        candidate_id: candidate_id.map(str::to_string),
        route_kind: if choose_canary {
            RouteKind::Canary
        } else {
            RouteKind::Active
        },
        route_fingerprint: fingerprint,
        policy_version: request.policy_version.clone(),
        canary_share_basis_points: request.canary_share_basis_points,
        retrieval_score: request.retrieval_score,
        retrieval_rank: request.retrieval_rank,
        index_generation: request.index_generation,
        fallback_before_effects,
    })
}

impl FrozenRoute {
    pub fn may_fallback(&self, effects_started: bool) -> bool {
        self.route_kind == RouteKind::Canary && self.fallback_before_effects && !effects_started
    }
}

fn route_fingerprint(
    secret_key: &[u8],
    lineage_root_id: &str,
    candidate_id: &str,
    turn_id: &str,
    policy_version: &str,
) -> (String, u16) {
    let mut hash = Sha256::new();
    hash.update(b"mini-agent/canary-route/v1");
    for value in [
        secret_key,
        lineage_root_id.as_bytes(),
        candidate_id.as_bytes(),
        turn_id.as_bytes(),
        policy_version.as_bytes(),
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    let digest = hash.finalize();
    let bucket_seed = u16::from_be_bytes([digest[0], digest[1]]);
    (hex::encode_lower(digest.as_slice()), bucket_seed)
}
