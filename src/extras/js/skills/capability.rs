//! Runtime intersection of session authority and immutable skill manifests.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::{CapabilityManifest, CapabilityTier, HostCapability, IdentityError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillExecutionAttribution {
    pub skill_id: String,
    pub export_name: String,
    pub manifest: CapabilityManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDenied {
    pub skill_id: String,
    pub export_name: String,
    pub operation: HostCapability,
    pub reason: CapabilityDenialReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityDenialReason {
    Undeclared,
    SessionDenied,
    InvalidManifest,
}

#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error("skill attribution is missing an immutable full ID or export")]
    InvalidAttribution,
    #[error("invalid immutable capability manifest: {0}")]
    InvalidManifest(#[from] IdentityError),
    #[error("skill capability denied")]
    Denied(CapabilityDenied),
}

/// Cloneable execution context shared by the JS wrappers and host globals for
/// one dedicated JS thread. Nested scopes intersect manifests.
#[derive(Clone, Default)]
pub struct CapabilityContext {
    stack: Arc<Mutex<Vec<SkillExecutionAttribution>>>,
    active_invocations: Arc<Mutex<BTreeMap<String, SkillExecutionAttribution>>>,
    denials: Arc<Mutex<Vec<CapabilityDenied>>>,
}

impl std::fmt::Debug for CapabilityContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let depth = self.stack.lock().map(|stack| stack.len()).unwrap_or(0);
        let active_invocations = self
            .active_invocations
            .lock()
            .map(|active| active.len())
            .unwrap_or(0);
        formatter
            .debug_struct("CapabilityContext")
            .field("depth", &depth)
            .field("active_invocations", &active_invocations)
            .finish()
    }
}

impl CapabilityContext {
    pub fn enter(
        &self,
        attribution: SkillExecutionAttribution,
    ) -> Result<CapabilityGuard, CapabilityError> {
        self.push(attribution)?;
        Ok(CapabilityGuard {
            context: self.clone(),
            active: true,
        })
    }

    pub(crate) fn push(
        &self,
        attribution: SkillExecutionAttribution,
    ) -> Result<(), CapabilityError> {
        validate_attribution(&attribution)?;
        self.stack
            .lock()
            .map_err(|_| CapabilityError::InvalidAttribution)?
            .push(attribution);
        Ok(())
    }

    pub(crate) fn pop(&self) -> Result<(), CapabilityError> {
        self.stack
            .lock()
            .map_err(|_| CapabilityError::InvalidAttribution)?
            .pop()
            .map(|_| ())
            .ok_or(CapabilityError::InvalidAttribution)
    }

    /// Keep a skill constraint active for the full synchronous or asynchronous
    /// invocation. This closes escape hatches such as `globalThis.read_file`
    /// and indirect `eval`, which bypass lexical host proxies.
    pub(crate) fn begin_invocation(
        &self,
        invocation_id: String,
        attribution: SkillExecutionAttribution,
    ) -> Result<(), CapabilityError> {
        if invocation_id.is_empty() {
            return Err(CapabilityError::InvalidAttribution);
        }
        validate_attribution(&attribution)?;
        let mut active = self
            .active_invocations
            .lock()
            .map_err(|_| CapabilityError::InvalidAttribution)?;
        match active.entry(invocation_id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(attribution);
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(CapabilityError::InvalidAttribution);
            }
        }
        Ok(())
    }

    pub(crate) fn end_invocation(&self, invocation_id: &str) -> Result<(), CapabilityError> {
        self.active_invocations
            .lock()
            .map_err(|_| CapabilityError::InvalidAttribution)?
            .remove(invocation_id)
            .map(|_| ())
            .ok_or(CapabilityError::InvalidAttribution)
    }

    /// Model-authored code outside a skill wrapper has no skill constraint and
    /// is governed solely by normal session permissions.
    pub fn authorize(
        &self,
        operation: HostCapability,
        session_allowed: bool,
    ) -> Result<(), CapabilityError> {
        let stack = self
            .stack
            .lock()
            .map_err(|_| CapabilityError::InvalidAttribution)?;
        let active = self
            .active_invocations
            .lock()
            .map_err(|_| CapabilityError::InvalidAttribution)?;
        let current = stack.last().or_else(|| active.values().next_back());
        let Some(current) = current else {
            return if session_allowed {
                Ok(())
            } else {
                Err(CapabilityError::Denied(CapabilityDenied {
                    skill_id: String::new(),
                    export_name: String::new(),
                    operation,
                    reason: CapabilityDenialReason::SessionDenied,
                }))
            };
        };
        if !session_allowed {
            return self.deny(CapabilityDenied {
                skill_id: current.skill_id.clone(),
                export_name: current.export_name.clone(),
                operation,
                reason: CapabilityDenialReason::SessionDenied,
            });
        }
        // Every nested frame must allow the operation; callees cannot borrow
        // authority from either callers or the ambient session.
        if stack
            .iter()
            .chain(active.values())
            .all(|attribution| attribution.manifest.allows(operation))
        {
            Ok(())
        } else {
            let denied_attribution = stack
                .iter()
                .rev()
                .chain(active.values().rev())
                .find(|attribution| !attribution.manifest.allows(operation))
                .unwrap_or(current);
            self.deny(CapabilityDenied {
                skill_id: denied_attribution.skill_id.clone(),
                export_name: denied_attribution.export_name.clone(),
                operation,
                reason: CapabilityDenialReason::Undeclared,
            })
        }
    }

    fn deny(&self, denial: CapabilityDenied) -> Result<(), CapabilityError> {
        if let Ok(mut denials) = self.denials.lock() {
            // A single JS step has a bounded event response. Retain the first
            // direct policy faults and drop repeated loop noise.
            if denials.len() < 256 {
                denials.push(denial.clone());
            }
        }
        Err(CapabilityError::Denied(denial))
    }

    pub fn take_denials(&self) -> Vec<CapabilityDenied> {
        self.denials
            .lock()
            .map(|mut denials| std::mem::take(&mut *denials))
            .unwrap_or_default()
    }

    pub fn current(&self) -> Option<SkillExecutionAttribution> {
        if let Some(current) = self.stack.lock().ok()?.last().cloned() {
            return Some(current);
        }
        self.active_invocations
            .lock()
            .ok()?
            .values()
            .next_back()
            .cloned()
    }

    pub fn clear(&self) {
        if let Ok(mut stack) = self.stack.lock() {
            stack.clear();
        }
        if let Ok(mut active) = self.active_invocations.lock() {
            active.clear();
        }
    }
}

fn validate_attribution(attribution: &SkillExecutionAttribution) -> Result<(), CapabilityError> {
    if attribution.skill_id.len() != 64
        || !attribution
            .skill_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || attribution.export_name.trim().is_empty()
    {
        return Err(CapabilityError::InvalidAttribution);
    }
    attribution.manifest.validate()?;
    Ok(())
}

pub struct CapabilityGuard {
    context: CapabilityContext,
    active: bool,
}

impl Drop for CapabilityGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = self.context.pop();
            self.active = false;
        }
    }
}

pub fn tier_may_automate(tier: CapabilityTier) -> bool {
    matches!(tier, CapabilityTier::Pure | CapabilityTier::ReadOnly)
}
