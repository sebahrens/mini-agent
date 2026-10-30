//! Dormant compatibility data and validation; ordinary frontend replay remains disabled.
#![allow(
    dead_code,
    reason = "native compatibility API precedes strict frontend publication adoption"
)]
//! Protocol-specific history compatibility. Labels are formatting data, never
//! permission grants, provider-stored item authority, or signature verification.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HistoryProtocol {
    AnthropicMessages,
    OpenRouterChatCompletions,
    OpenAIResponses,
    OpenAIChatCompletions,
    GeminiGenerateContent,
    OllamaChat,
}

/// Static local refusal reasons. No opaque payload, endpoint or credentials are
/// included in a user-visible error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestartReason {
    ChangedDomain,
    ChangedPrefix,
    IncompleteTurn,
    UnavailableSource,
    UnsupportedOverride,
    Limit,
    InFlight,
}

impl std::fmt::Display for RestartReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ChangedDomain => {
                "native thinking context belongs to a different endpoint or model"
            }
            Self::ChangedPrefix => "native thinking context prefix changed",
            Self::IncompleteTurn => "native thinking tool reply is incomplete",
            Self::UnavailableSource => "native thinking context cannot be reproduced",
            Self::UnsupportedOverride => {
                "raw request overrides cannot reproduce native thinking context"
            }
            Self::Limit => "native thinking context exceeds its finite bounds",
            Self::InFlight => {
                "native thinking context is unfinished; start a fresh context to continue"
            }
        })
    }
}

impl std::error::Error for RestartReason {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReplayBoundary {
    CompletedTurn,
    LiveToolReply,
}

/// Supplied only after the exact owning actor and children retire. Visible
/// text, tool counts and imported pending-question data cannot mint this
/// lifecycle witness. No runtime owner is restored by its saved projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetiredNativeOutcome {
    Completed,
    LiveToolReply,
    Incomplete,
}

/// A same-session live clone shares this monotonic cutoff. Forking constructs
/// an independent cell at the inherited cutoff; deserialization never restores
/// a running capture, waiter or grant.
#[derive(Debug, Clone, Default)]
pub(crate) struct NativeFrontier(std::sync::Arc<FrontierState>);

/// Runtime identity only. A deserialized checkpoint has a different cell even
/// when its saved session id and cutoff are identical.
#[derive(Debug, Clone)]
pub(super) struct FrontierIdentity(std::sync::Weak<FrontierState>);

impl FrontierIdentity {
    pub(super) fn matches(&self, frontier: &NativeFrontier) -> bool {
        self.0.ptr_eq(&std::sync::Arc::downgrade(&frontier.0))
    }
}

#[derive(Debug, Default)]
struct FrontierState {
    checkpoint: std::sync::Mutex<FrontierCheckpoint>,
    publication: std::sync::Arc<super::native_publication::PublicationCell>,
}

#[derive(Debug, Clone, Default)]
struct FrontierCheckpoint {
    epoch: u64,
    refusal: Option<RestartReason>,
    retiring: bool,
    activated: bool,
}

pub(crate) struct NativeRetirement(NativeFrontier);
impl Drop for NativeRetirement {
    fn drop(&mut self) {
        self.0.checkpoint().retiring = false;
    }
}

impl NativeFrontier {
    pub(super) fn publication_cell(
        &self,
    ) -> std::sync::Arc<super::native_publication::PublicationCell> {
        std::sync::Arc::clone(&self.0.publication)
    }

    /// Persistence-only denial at the same cutoff. The healthy live binding
    /// remains available to the exact admitted actor; imports cannot restore
    /// that runtime owner.
    pub(super) fn in_flight_projection(&self) -> Self {
        let mut checkpoint = self.checkpoint().clone();
        checkpoint.refusal = Some(RestartReason::InFlight);
        checkpoint.retiring = false;
        Self(std::sync::Arc::new(FrontierState {
            checkpoint: std::sync::Mutex::new(checkpoint),
            publication: Default::default(),
        }))
    }

    pub(super) fn identity(&self) -> FrontierIdentity {
        FrontierIdentity(std::sync::Arc::downgrade(&self.0))
    }

    pub(super) fn activate_publication(&self) {
        self.checkpoint().activated = true;
    }

    fn checkpoint(&self) -> std::sync::MutexGuard<'_, FrontierCheckpoint> {
        self.0
            .checkpoint
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `skip_serializing_if` for `Session::native_frontier`: the in-flight
    /// projection always carries a refusal, so it is never initial.
    pub(super) fn skip_serializing(&self) -> bool {
        !ENCODE_IN_FLIGHT.with(std::cell::Cell::get) && self.is_initial()
    }

    pub(crate) fn is_initial(&self) -> bool {
        let saved = self.checkpoint();
        saved.epoch == 0 && saved.refusal.is_none() && !saved.activated
    }

    pub(crate) fn current(&self) -> u64 {
        self.checkpoint().epoch
    }

    pub(crate) fn ready(&self) -> bool {
        let saved = self.checkpoint();
        !saved.retiring && saved.refusal.is_none()
    }

    pub(crate) fn refusal(&self) -> Option<RestartReason> {
        self.checkpoint().refusal
    }

    /// Exact retired ownership may conservatively fence accepted content that
    /// cannot attach. Serialization observes denial and cutoff coherently;
    /// this tiny in-memory lock never contains filesystem work or suspension.
    pub(crate) fn refuse_expected(
        &self,
        expected: u64,
        reason: RestartReason,
    ) -> Result<(), RestartReason> {
        let mut saved = self.checkpoint();
        if saved.epoch != expected {
            return Err(RestartReason::ChangedPrefix);
        }
        saved.refusal = Some(reason);
        saved.epoch = expected.checked_add(1).ok_or(RestartReason::Limit)?;
        Ok(())
    }

    pub(crate) fn begin_retirement(
        &self,
        expected: u64,
    ) -> Result<NativeRetirement, RestartReason> {
        let mut saved = self.checkpoint();
        if let Some(reason) = saved.refusal {
            return Err(reason);
        }
        if saved.retiring || saved.epoch != expected {
            return Err(RestartReason::ChangedPrefix);
        }
        saved.retiring = true;
        Ok(NativeRetirement(self.clone()))
    }

    #[cfg(test)]
    pub(crate) fn retire(&self) -> Result<u64, RestartReason> {
        let mut saved = self.checkpoint();
        if saved.retiring || saved.refusal.is_some() {
            return Err(RestartReason::ChangedPrefix);
        }
        saved.epoch = saved.epoch.checked_add(1).ok_or(RestartReason::Limit)?;
        Ok(saved.epoch)
    }

    pub(crate) fn fork(&self) -> Self {
        let mut checkpoint = self.checkpoint().clone();
        checkpoint.retiring = false;
        Self(std::sync::Arc::new(FrontierState {
            checkpoint: std::sync::Mutex::new(checkpoint),
            publication: Default::default(),
        }))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RefusedFrontier {
    epoch: u64,
    refusal: RestartReason,
    #[serde(default, skip_serializing_if = "is_false")]
    activated: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivatedFrontier {
    epoch: u64,
    activated: bool,
}

thread_local! {
    /// While set, every frontier on this thread encodes as its
    /// [`NativeFrontier::in_flight_projection`] without building one.
    static ENCODE_IN_FLIGHT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Restores the previous encoding mode when the projected encoding ends.
pub(super) struct InFlightEncoding(bool);
impl InFlightEncoding {
    /// Encode borrowed frontiers as their in-flight projection until dropped,
    /// so a borrowed `Session` serializes exactly as a clone carrying
    /// `in_flight_projection()` would, without deep-cloning the Session.
    pub(super) fn install() -> Self {
        Self(ENCODE_IN_FLIGHT.with(|flag| flag.replace(true)))
    }
}
impl Drop for InFlightEncoding {
    fn drop(&mut self) {
        ENCODE_IN_FLIGHT.with(|flag| flag.set(self.0));
    }
}

impl serde::Serialize for NativeFrontier {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let saved = self.checkpoint();
        let refusal = if ENCODE_IN_FLIGHT.with(std::cell::Cell::get) {
            Some(RestartReason::InFlight)
        } else {
            saved.refusal
        };
        match refusal {
            Some(refusal) => RefusedFrontier {
                epoch: saved.epoch,
                refusal,
                activated: saved.activated,
            }
            .serialize(serializer),
            None if saved.activated => ActivatedFrontier {
                epoch: saved.epoch,
                activated: true,
            }
            .serialize(serializer),
            None => serializer.serialize_u64(saved.epoch),
        }
    }
}

impl<'de> serde::Deserialize<'de> for NativeFrontier {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Saved {
            Healthy(u64),
            Refused(RefusedFrontier),
            Activated(ActivatedFrontier),
        }
        let checkpoint = match Saved::deserialize(deserializer)? {
            Saved::Healthy(epoch) => FrontierCheckpoint {
                epoch,
                refusal: None,
                retiring: false,
                activated: false,
            },
            Saved::Refused(RefusedFrontier {
                epoch,
                refusal,
                activated,
            }) => FrontierCheckpoint {
                epoch,
                refusal: Some(refusal),
                retiring: false,
                activated,
            },
            Saved::Activated(ActivatedFrontier {
                epoch,
                activated: true,
            }) => FrontierCheckpoint {
                epoch,
                refusal: None,
                retiring: false,
                activated: true,
            },
            Saved::Activated(_) => {
                return Err(serde::de::Error::custom(
                    "invalid native publication marker",
                ));
            }
        };
        Ok(Self(std::sync::Arc::new(FrontierState {
            checkpoint: std::sync::Mutex::new(checkpoint),
            publication: Default::default(),
        })))
    }
}

/// Finite compatibility data. Exact native input is retained separately from
/// the display/audit transcript; it carries no imported runtime authority.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeHistory {
    version: u8,
    epoch: u64,
    base: Vec<rig::completion::Message>,
    frames: Vec<crate::agent::native_capture::NativeTurnProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    feed_anchor: Option<FeedAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boundary: Option<ReplayBoundary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) host: Option<crate::provider::NativeHostCompatibility>,
}

pub(crate) const MAX_NATIVE_PAYLOAD: usize = 4 * 1024 * 1024;
const MAX_NATIVE_FRAME: usize = 2 * 1024 * 1024;
pub(crate) const MAX_NATIVE_FRAMES: usize = 128;

#[cfg(test)]
thread_local! {
    /// Bytes written through `bounded_serialized` on this thread, so tests can
    /// assert the serialization work of one event does not grow with history.
    pub(crate) static SERIALIZED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn bounded_serialized<T: serde::Serialize>(
    value: &T,
    limit: usize,
) -> Result<usize, RestartReason> {
    struct Limited {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            #[cfg(test)]
            SERIALIZED_BYTES.with(|total| total.set(total.get() + bytes.len()));
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .filter(|size| *size <= self.limit)
                .ok_or_else(|| std::io::Error::other("native payload exceeds limits"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Limited { bytes: 0, limit };
    serde_json::to_writer(&mut writer, value).map_err(|_| RestartReason::Limit)?;
    Ok(writer.bytes)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedAnchor {
    count: usize,
    digest: [u8; 32],
}

pub(crate) fn feed_digest(
    messages: &[super::SessionMessage],
    compactions: &[super::Compaction],
) -> Result<[u8; 32], RestartReason> {
    hashed_within(&(messages, compactions), super::storage::MAX_SESSION_BYTES)
}

/// SHA-256 of the value's JSON, failing past `limit` bytes. The bound and the
/// hash share one serialization pass; the digest is of the same bytes.
fn hashed_within<T: serde::Serialize>(value: &T, limit: usize) -> Result<[u8; 32], RestartReason> {
    use sha2::Digest;
    let mut hash = HashWriter(sha2::Sha256::new(), 0, limit);
    serde_json::to_writer(&mut hash, value).map_err(|_| RestartReason::Limit)?;
    Ok(hash.0.finalize().into())
}

pub(crate) fn compaction_digest(
    compactions: &[super::Compaction],
) -> Result<[u8; 32], RestartReason> {
    hashed_within(&compactions, super::storage::MAX_SESSION_BYTES)
}

/// Without an owned prior commitment, native opaque history is unavailable.
/// This inspects the typed native conversion, not labels or display text.
fn require_plain_messages(messages: &[rig::completion::Message]) -> Result<(), RestartReason> {
    for message in messages {
        if matches!(message, rig::completion::Message::Assistant { .. }) {
            let native = rig::providers::anthropic::completion::Message::try_from(message.clone())
                .map_err(|_| RestartReason::UnavailableSource)?;
            let native =
                serde_json::to_value(native).map_err(|_| RestartReason::UnavailableSource)?;
            if native["content"]
                .as_array()
                .ok_or(RestartReason::UnavailableSource)?
                .iter()
                .any(opaque)
            {
                return Err(RestartReason::UnavailableSource);
            }
        }
    }
    Ok(())
}

/// Serialized sizes of a growing capture, maintained by its owner so the
/// whole-history byte bounds are arithmetic instead of a re-serialization.
#[derive(Debug, Default, Clone)]
pub(crate) struct CaptureSizes {
    /// Serialized bytes of the base message array.
    pub(crate) base: usize,
    /// The base already passed its immutable (plain-message) check.
    pub(crate) base_checked: bool,
    /// Serialized bytes of each frame, parallel to the frames.
    pub(crate) frames: Vec<usize>,
    /// Leading frames whose immutable checks (delta, commitment) passed.
    pub(crate) checked_frames: usize,
}

/// Serialized bytes of `{"version":1,"epoch":E,"base":[..],"frames":[..]}`
/// (the optional anchor/boundary/host fields are absent for a capture).
pub(crate) fn capture_history_size(epoch: u64, base: usize, frames: &[usize]) -> usize {
    const FRAMING: usize = r#"{"version":1,"epoch":,"base":,"frames":[]}"#.len();
    let digits = epoch.checked_ilog10().map_or(1, |log| log as usize + 1);
    let commas = frames.len().saturating_sub(1);
    frames
        .iter()
        .fold(FRAMING.saturating_add(digits), |total, size| {
            total.saturating_add(*size)
        })
        .saturating_add(base)
        .saturating_add(commas)
}

enum Sizing<'a> {
    Measure(&'a NativeHistory),
    Known { epoch: u64, sizes: &'a CaptureSizes },
}

impl Sizing<'_> {
    fn check_total(&self) -> Result<(), RestartReason> {
        match self {
            Self::Measure(history) => bounded_serialized(*history, MAX_NATIVE_PAYLOAD).map(|_| ()),
            Self::Known { epoch, sizes } => {
                let total = capture_history_size(*epoch, sizes.base, &sizes.frames);
                (total <= MAX_NATIVE_PAYLOAD)
                    .then_some(())
                    .ok_or(RestartReason::Limit)
            }
        }
    }
    fn check_base(&self, base: &[rig::completion::Message]) -> Result<(), RestartReason> {
        match self {
            Self::Measure(_) => bounded_serialized(&base, MAX_NATIVE_FRAME).map(|_| ()),
            Self::Known { sizes, .. } => (sizes.base <= MAX_NATIVE_FRAME)
                .then_some(())
                .ok_or(RestartReason::Limit),
        }
    }
    fn base_checked(&self) -> bool {
        matches!(self, Self::Known { sizes, .. } if sizes.base_checked)
    }
    fn check_frame(
        &self,
        index: usize,
        frame: &crate::agent::native_capture::NativeTurnProjection,
    ) -> Result<(), RestartReason> {
        match self {
            Self::Measure(_) => bounded_serialized(frame, MAX_NATIVE_FRAME).map(|_| ()),
            Self::Known { sizes, .. } => (sizes.frames.get(index).copied().unwrap_or(usize::MAX)
                <= MAX_NATIVE_FRAME)
                .then_some(())
                .ok_or(RestartReason::Limit),
        }
    }
    fn frame_checked(&self, index: usize) -> bool {
        matches!(self, Self::Known { sizes, .. } if index < sizes.checked_frames)
    }
}

fn validate_parts(
    version: u8,
    base: &[rig::completion::Message],
    frames: &[crate::agent::native_capture::NativeTurnProjection],
    sizing: Sizing<'_>,
) -> Result<(), RestartReason> {
    use rig::completion::AssistantContent;
    if version != 1 || frames.is_empty() || frames.len() > MAX_NATIVE_FRAMES {
        return Err(RestartReason::UnavailableSource);
    }
    sizing.check_total()?;
    sizing.check_base(base)?;
    if !sizing.base_checked() {
        require_plain_messages(base)?;
    }
    let mut keys = std::collections::HashSet::new();
    let mut tool_ids = std::collections::HashSet::new();
    let mut internal_ids = std::collections::HashSet::new();
    let mut domain = None;
    for (index, frame) in frames.iter().enumerate() {
        if frame.content.len() > MAX_BINDING_BLOCKS
            || frame.sdk_run_id.is_empty()
            || frame.sdk_run_id.len() > 512
            || frame.configured_model.is_empty()
            || frame.configured_model.len() > 512
            || !keys.insert((&frame.sdk_run_id, frame.turn))
        {
            return Err(RestartReason::UnavailableSource);
        }
        sizing.check_frame(index, frame)?;
        let immutable_checked = sizing.frame_checked(index);
        let delta = frame
            .native_input_delta
            .as_ref()
            .ok_or(RestartReason::UnavailableSource)?;
        if !immutable_checked {
            require_plain_messages(delta)?;
        }
        if index == 0 && !delta.is_empty() {
            return Err(RestartReason::ChangedPrefix);
        }
        let commitment = frame
            .native_commitment
            .as_ref()
            .ok_or(RestartReason::UnavailableSource)?;
        if !immutable_checked {
            commitment.domain.validate_saved()?;
            commitment.validate_completed_content(&frame.content)?;
        }
        let mut positions = std::collections::HashSet::new();
        if commitment
            .blocks
            .iter()
            .any(|block| !positions.insert((block.message, block.block)))
        {
            return Err(RestartReason::UnavailableSource);
        }
        if domain
            .as_ref()
            .is_some_and(|domain| domain != &commitment.domain)
        {
            return Err(RestartReason::ChangedDomain);
        }
        domain = Some(commitment.domain.clone());
        if commitment.blocks.len() > MAX_BINDING_BLOCKS
            || commitment.blocks.iter().any(|block| {
                block.message >= MAX_BINDING_MESSAGES || block.block >= MAX_BINDING_BLOCKS
            })
        {
            return Err(RestartReason::Limit);
        }
        let calls: Vec<_> = frame
            .content
            .iter()
            .filter_map(|item| match item {
                AssistantContent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        // A final, interrupted tool batch may have no advertised mapping.
        // It stays compatibility data and cannot be a completed boundary.
        if frame.advertised_tools.len() > calls.len()
            || (index + 1 != frames.len() && frame.advertised_tools.len() != calls.len())
        {
            return Err(RestartReason::UnavailableSource);
        }
        for call in &calls {
            if call.id.is_empty() || call.id.len() > 512 || !tool_ids.insert(&call.id) {
                return Err(RestartReason::UnavailableSource);
            }
        }
        let mut results = std::collections::HashSet::new();
        for result in &frame.native_tool_results {
            let identity = frame
                .advertised_tools
                .iter()
                .find(|identity| identity.tool_id == result.id)
                .ok_or(RestartReason::UnavailableSource)?;
            if !results.insert(result.id.as_str()) || result.call_id != identity.provider_id {
                return Err(RestartReason::UnavailableSource);
            }
        }
        if frame.native_tool_results.len() > calls.len() {
            return Err(RestartReason::UnavailableSource);
        }
        let mut mapped = std::collections::HashSet::new();
        for identity in &frame.advertised_tools {
            if identity.internal_id.is_empty() || identity.internal_id.len() > 512
                || identity.provider_id.as_ref().is_some_and(|id| id.len() > 512)
                || !internal_ids.insert(&identity.internal_id)
                || !mapped.insert(&identity.tool_id)
                || !frame.content.iter().any(|item| matches!(item, AssistantContent::ToolCall(call) if call.id == identity.tool_id))
            { return Err(RestartReason::UnavailableSource); }
        }
    }
    Ok(())
}

impl NativeHistory {
    pub(crate) fn from_capture(
        epoch: u64,
        base: Vec<rig::completion::Message>,
        frames: Vec<crate::agent::native_capture::NativeTurnProjection>,
    ) -> Result<Self, RestartReason> {
        let history = Self {
            version: 1,
            epoch,
            base,
            frames,
            feed_anchor: None,
            boundary: None,
            host: None,
        };
        history.validate()?;
        Ok(history)
    }

    pub(crate) fn bind_feed(
        &mut self,
        messages: &[super::SessionMessage],
        compactions: &[super::Compaction],
    ) -> Result<(), RestartReason> {
        self.feed_anchor = Some(FeedAnchor {
            count: messages.len(),
            digest: feed_digest(messages, compactions)?,
        });
        Ok(())
    }

    pub(crate) fn validate_feed(
        &self,
        messages: &[super::SessionMessage],
        compactions: &[super::Compaction],
    ) -> Result<(), RestartReason> {
        let anchor = self
            .feed_anchor
            .as_ref()
            .ok_or(RestartReason::UnavailableSource)?;
        // Newly committed history outside this exact retired capture must not
        // silently disappear by replaying its older native-only projection.
        // Likewise a new summary cannot resurrect the prefix it replaced.
        if messages.len() != anchor.count || feed_digest(messages, compactions)? != anchor.digest {
            return Err(RestartReason::ChangedPrefix);
        }
        Ok(())
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn base(&self) -> &[rig::completion::Message] {
        &self.base
    }
    pub(crate) fn frames(&self) -> &[crate::agent::native_capture::NativeTurnProjection] {
        &self.frames
    }

    pub(crate) fn commitment(&self) -> Result<NativeCommitment, RestartReason> {
        self.validate()?;
        let mut combined = self.frames[0].native_commitment.clone().unwrap();
        for frame in self.frames.iter().skip(1) {
            let next = frame.native_commitment.as_ref().unwrap();
            for block in &next.blocks {
                if combined
                    .blocks
                    .iter()
                    .any(|old| old.message == block.message && old.block == block.block)
                {
                    return Err(RestartReason::UnavailableSource);
                }
                combined.blocks.push(block.clone());
            }
        }
        if combined.blocks.len() > MAX_BINDING_OPAQUE_BLOCKS {
            return Err(RestartReason::Limit);
        }
        Ok(combined)
    }

    pub(crate) fn validate(&self) -> Result<(), RestartReason> {
        if let Some(host) = &self.host {
            host.validate()?;
        }
        validate_parts(
            self.version,
            &self.base,
            &self.frames,
            Sizing::Measure(self),
        )
    }

    /// Validates a capture's parts in place with sizes the owner tracked
    /// incrementally: no history clone and no whole-history serialization.
    /// Accepts and rejects exactly what `from_capture(epoch, base, frames)` does.
    pub(crate) fn validate_capture(
        epoch: u64,
        base: &[rig::completion::Message],
        frames: &[crate::agent::native_capture::NativeTurnProjection],
        sizes: &CaptureSizes,
    ) -> Result<(), RestartReason> {
        validate_parts(1, base, frames, Sizing::Known { epoch, sizes })
    }

    fn validate_boundary(&self, boundary: ReplayBoundary) -> Result<(), RestartReason> {
        let frame = self.frames.last().ok_or(RestartReason::IncompleteTurn)?;
        let calls = frame
            .content
            .iter()
            .filter(|item| matches!(item, rig::completion::AssistantContent::ToolCall(_)))
            .count();
        if frame.native_tool_results.len() != calls
            || frame.advertised_tools.len() != calls
            || match boundary {
                ReplayBoundary::CompletedTurn => calls != 0,
                ReplayBoundary::LiveToolReply => calls == 0,
            }
        {
            return Err(RestartReason::IncompleteTurn);
        }
        Ok(())
    }

    pub(crate) fn with_retired_outcome(
        mut self,
        outcome: RetiredNativeOutcome,
    ) -> Result<Self, RestartReason> {
        self.validate()?;
        let boundary = match outcome {
            RetiredNativeOutcome::Completed => ReplayBoundary::CompletedTurn,
            RetiredNativeOutcome::LiveToolReply => ReplayBoundary::LiveToolReply,
            RetiredNativeOutcome::Incomplete => return Err(RestartReason::IncompleteTurn),
        };
        self.validate_boundary(boundary)?;
        self.boundary = Some(boundary);
        Ok(self)
    }

    pub(crate) fn saved_boundary(&self) -> Result<ReplayBoundary, RestartReason> {
        self.validate()?;
        let boundary = self.boundary.ok_or(RestartReason::IncompleteTurn)?;
        self.validate_boundary(boundary)?;
        Ok(boundary)
    }

    pub(crate) fn validate_saved(&self) -> Result<(), RestartReason> {
        self.saved_boundary().map(|_| ())
    }

    pub(crate) fn require_completed_boundary(&self) -> Result<(), RestartReason> {
        if self.saved_boundary()? != ReplayBoundary::CompletedTurn {
            return Err(RestartReason::IncompleteTurn);
        }
        Ok(())
    }

    pub(crate) fn restore_history(
        &self,
        target: HistoryProtocol,
        frontier: &NativeFrontier,
        boundary: ReplayBoundary,
    ) -> Result<Vec<rig::completion::Message>, RestartReason> {
        self.validate()?;
        if target != HistoryProtocol::AnthropicMessages {
            return Err(RestartReason::ChangedDomain);
        }
        if !frontier.ready() || self.epoch != frontier.current() {
            return Err(RestartReason::ChangedPrefix);
        }
        if self.saved_boundary()? != boundary {
            return Err(RestartReason::IncompleteTurn);
        }
        let mut history = self.base.clone();
        for frame in &self.frames {
            history.extend(frame.native_input_delta.as_ref().unwrap().iter().cloned());
            history.push(rig::completion::Message::Assistant {
                id: None,
                content: frame.content.clone(),
            });
            if !frame.native_tool_results.is_empty() {
                // Only a complete surfaced batch attests the SDK grouped User.
                let calls = frame
                    .content
                    .iter()
                    .filter(|item| matches!(item, rig::completion::AssistantContent::ToolCall(_)))
                    .count();
                if frame.native_tool_results.len() != calls || frame.advertised_tools.len() != calls
                {
                    return Err(RestartReason::IncompleteTurn);
                }
                history.push(rig::completion::Message::User {
                    content: rig::OneOrMany::many(
                        frame
                            .native_tool_results
                            .iter()
                            .cloned()
                            .map(rig::message::UserContent::ToolResult),
                    )
                    .map_err(|_| RestartReason::IncompleteTurn)?,
                });
            }
        }
        Ok(history)
    }
}

/// Public-route consistency only. A match does not prove account/model
/// readability or cryptographic signature validity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeDomain {
    endpoint: String,
    request_model: String,
}

impl NativeDomain {
    fn validate_saved(&self) -> Result<(), RestartReason> {
        if self.endpoint.len() > 512 || self.request_model.len() > 512 {
            return Err(RestartReason::Limit);
        }
        let endpoint = self
            .endpoint
            .parse::<http::Uri>()
            .map_err(|_| RestartReason::ChangedDomain)?;
        let probe =
            if rig::providers::anthropic::client::VertexRoute::streaming_model(endpoint.path())
                .is_some()
            {
                serde_json::json!({"anthropic_version":"vertex-2023-10-16"})
            } else {
                serde_json::json!({"model": self.request_model})
            };
        let actual = Self::from_native_view(&endpoint, &probe)?;
        if actual != *self {
            return Err(RestartReason::ChangedDomain);
        }
        Ok(())
    }

    pub(crate) fn from_native_view(
        endpoint: &http::Uri,
        body: &serde_json::Value,
    ) -> Result<Self, RestartReason> {
        let endpoint = endpoint.to_string();
        if endpoint.len() > 512 {
            return Err(RestartReason::Limit);
        }
        let uri = reqwest::Url::parse(&endpoint).map_err(|_| RestartReason::ChangedDomain)?;
        if !matches!(uri.scheme(), "http" | "https")
            || uri.host().is_none()
            || !uri.username().is_empty()
            || uri.password().is_some()
            || uri.query().is_some()
            || uri.fragment().is_some()
        {
            return Err(RestartReason::ChangedDomain);
        }
        let vertex_model =
            rig::providers::anthropic::client::VertexRoute::streaming_model(uri.path());
        let model = if let Some(model) = vertex_model {
            if body.get("model").is_some()
                || body
                    .get("anthropic_version")
                    .and_then(serde_json::Value::as_str)
                    != Some("vertex-2023-10-16")
            {
                return Err(RestartReason::ChangedDomain);
            }
            model
        } else {
            body.get("model")
                .and_then(serde_json::Value::as_str)
                .filter(|model| {
                    !model.is_empty() && model.len() <= 512 && !model.chars().any(char::is_control)
                })
                .ok_or(RestartReason::ChangedDomain)?
        };
        Ok(Self {
            endpoint,
            request_model: model.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BlockCommitment {
    message: usize,
    block: usize,
    digest: [u8; 32],
    content_digest: [u8; 32],
}

/// Commitment to the actual native-composed binding fields, including an
/// assistant's earlier blocks. Cache markers are excluded only at protocol
/// marker positions; a user/tool/schema property named cache_control is data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeCommitment {
    domain: NativeDomain,
    blocks: Vec<BlockCommitment>,
    manual_cache_markers: bool,
    cache_policy_digest: [u8; 32],
}

const MAX_BINDING_BYTES: usize = 4 * 1024 * 1024;
const MAX_BINDING_MESSAGES: usize = 4096;
const MAX_BINDING_BLOCKS: usize = 256;
const MAX_BINDING_OPAQUE_BLOCKS: usize = MAX_NATIVE_FRAMES * MAX_BINDING_BLOCKS;

/// Hashes what is written, refusing bytes past its limit (hash, written, limit).
struct HashWriter(sha2::Sha256, usize, usize);
impl std::io::Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        self.1 = self
            .1
            .checked_add(bytes.len())
            .filter(|size| *size <= self.2)
            .ok_or_else(|| std::io::Error::other("native payload exceeds limits"))?;
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct ByteLimit(usize);
impl std::io::Write for ByteLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|bytes| *bytes <= MAX_BINDING_BYTES)
            .ok_or_else(|| std::io::Error::other("native binding exceeds limits"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn binding_view(
    body: &serde_json::Value,
    manual_cache_markers: bool,
) -> Result<serde_json::Value, RestartReason> {
    serde_json::to_writer(&mut ByteLimit(0), body).map_err(|_| RestartReason::Limit)?;
    let messages = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .filter(|messages| messages.len() <= MAX_BINDING_MESSAGES)
        .ok_or(RestartReason::UnavailableSource)?;
    for message in messages {
        let blocks = message
            .get("content")
            .and_then(serde_json::Value::as_array)
            .filter(|blocks| blocks.len() <= MAX_BINDING_BLOCKS)
            .ok_or(RestartReason::UnavailableSource)?;
        let _ = blocks;
    }
    let mut view = serde_json::json!({
        "system": body.get("system"), "tools": body.get("tools"), "messages": messages
    });
    // Only the actual manual SDK policy regenerates this one moving marker.
    // Keep system/tool markers and every nested/data/explicit marker exact.
    if manual_cache_markers
        && body.get("cache_control").is_none()
        && let Some(last) = view["messages"]
            .as_array_mut()
            .and_then(|messages| messages.last_mut())
        && let Some(block) = last["content"]
            .as_array_mut()
            .and_then(|blocks| blocks.last_mut())
        && matches!(
            block.get("type").and_then(serde_json::Value::as_str),
            Some("text" | "image" | "tool_result" | "document")
        )
        && let Some(map) = block.as_object_mut()
    {
        map.remove("cache_control");
    }
    Ok(view)
}

fn value_digest(value: &serde_json::Value) -> Result<[u8; 32], RestartReason> {
    use sha2::Digest;
    let mut hash = HashWriter(sha2::Sha256::new(), 0, usize::MAX);
    serde_json::to_writer(&mut hash, value).map_err(|_| RestartReason::UnavailableSource)?;
    Ok(hash.0.finalize().into())
}

// Hash each message/block a bounded number of times. Do not reserialize the
// growing whole prefix for every thinking block: large prefixes would turn a
// finite block cap into quadratic work in the synchronous SDK callback.
fn commitments_for_view(view: &serde_json::Value) -> Result<Vec<BlockCommitment>, RestartReason> {
    use sha2::Digest;
    let system = value_digest(&view["system"])?;
    let tools = value_digest(&view["tools"])?;
    let mut history = sha2::Sha256::new();
    let mut commitments = Vec::new();
    for (message, current) in view["messages"].as_array().unwrap().iter().enumerate() {
        let mut metadata = current.clone();
        metadata
            .as_object_mut()
            .ok_or(RestartReason::UnavailableSource)?
            .remove("content");
        let metadata = value_digest(&metadata)?;
        let mut prior_blocks = sha2::Sha256::new();
        for (block, content) in current["content"].as_array().unwrap().iter().enumerate() {
            let content_digest = value_digest(content)?;
            if opaque(content) {
                if commitments.len() >= MAX_BINDING_OPAQUE_BLOCKS {
                    return Err(RestartReason::Limit);
                }
                let mut prefix = sha2::Sha256::new();
                prefix.update(system);
                prefix.update(tools);
                prefix.update(history.clone().finalize());
                prefix.update(metadata);
                prefix.update(prior_blocks.clone().finalize());
                commitments.push(BlockCommitment {
                    message,
                    block,
                    digest: prefix.finalize().into(),
                    content_digest,
                });
            }
            prior_blocks.update(content_digest);
        }
        history.update(value_digest(current)?);
    }
    Ok(commitments)
}

fn opaque(block: &serde_json::Value) -> bool {
    match block.get("type").and_then(serde_json::Value::as_str) {
        Some("thinking") => block
            .get("signature")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|signature| !signature.is_empty()),
        Some("redacted_thinking") => block
            .get("data")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|data| !data.is_empty()),
        _ => false,
    }
}

impl NativeCommitment {
    pub(crate) fn capture(
        endpoint: &http::Uri,
        actual_body: &serde_json::Value,
        manual_cache_markers: bool,
    ) -> Result<Self, RestartReason> {
        let domain = NativeDomain::from_native_view(endpoint, actual_body)?;
        let view = binding_view(actual_body, manual_cache_markers)?;
        let blocks = commitments_for_view(&view)?;
        let cache_policy_digest = value_digest(&serde_json::json!({
            "manual": manual_cache_markers, "automatic": actual_body.get("cache_control")
        }))?;
        Ok(Self {
            domain,
            blocks,
            manual_cache_markers,
            cache_policy_digest,
        })
    }

    pub(crate) fn require_unbound_input(
        endpoint: &http::Uri,
        actual_body: &serde_json::Value,
        manual_cache_markers: bool,
    ) -> Result<(), RestartReason> {
        let observed = Self::capture(endpoint, actual_body, manual_cache_markers)?;
        if observed.blocks.is_empty() {
            Ok(())
        } else {
            Err(RestartReason::UnavailableSource)
        }
    }

    /// A completed native frame must account for every opaque block it
    /// carries, at its exact native block offset. Imported commitments are
    /// structural witnesses, never cryptographic authenticity claims.
    fn validate_completed_content(
        &self,
        content: &rig::OneOrMany<rig::completion::AssistantContent>,
    ) -> Result<(), RestartReason> {
        let native = rig::providers::anthropic::completion::Message::try_from(
            rig::completion::Message::Assistant {
                id: None,
                content: content.clone(),
            },
        )
        .map_err(|_| RestartReason::UnavailableSource)?;
        let native = serde_json::to_value(native).map_err(|_| RestartReason::UnavailableSource)?;
        let blocks = native["content"]
            .as_array()
            .ok_or(RestartReason::UnavailableSource)?;
        let mut expected = blocks.iter().enumerate().filter(|(_, block)| opaque(block));
        let message = self.blocks.first().map(|block| block.message);
        for commitment in &self.blocks {
            let Some((index, block)) = expected.next() else {
                return Err(RestartReason::UnavailableSource);
            };
            if Some(commitment.message) != message
                || commitment.block != index
                || commitment.content_digest != value_digest(block)?
            {
                return Err(RestartReason::UnavailableSource);
            }
        }
        if expected.next().is_some() {
            return Err(RestartReason::UnavailableSource);
        }
        Ok(())
    }

    pub(crate) fn only_message(mut self, index: usize) -> Self {
        self.blocks.retain(|block| block.message == index);
        self
    }

    pub(crate) fn validate_with_policy(
        &self,
        endpoint: &http::Uri,
        actual_body: &serde_json::Value,
        manual_cache_markers: bool,
    ) -> Result<(), RestartReason> {
        if manual_cache_markers != self.manual_cache_markers {
            return Err(RestartReason::ChangedPrefix);
        }
        self.validate(endpoint, actual_body)
    }

    pub(crate) fn validate(
        &self,
        endpoint: &http::Uri,
        actual_body: &serde_json::Value,
    ) -> Result<(), RestartReason> {
        if self.domain != NativeDomain::from_native_view(endpoint, actual_body)? {
            return Err(RestartReason::ChangedDomain);
        }
        if self.blocks.len() > MAX_BINDING_OPAQUE_BLOCKS {
            return Err(RestartReason::Limit);
        }
        if self.cache_policy_digest
            != value_digest(&serde_json::json!({
                "manual": self.manual_cache_markers, "automatic": actual_body.get("cache_control")
            }))?
        {
            return Err(RestartReason::ChangedPrefix);
        }
        let view = binding_view(actual_body, self.manual_cache_markers)?;
        let current = commitments_for_view(&view)?;
        if current != self.blocks {
            return Err(RestartReason::ChangedPrefix);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn body() -> serde_json::Value {
        json!({"model":"claude-opus-5-5","system":[{"type":"text","text":"stable"}],
            "tools":[{"name":"read","input_schema":{"type":"object","properties":{"cache_control":{"type":"string"}}}}],
            "messages":[{"role":"user","content":[{"type":"text","text":"literal user"}]},
            {"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"signed-one"},
              {"type":"tool_use","id":"toolu_1","name":"read","input":{"cache_control":"data"}},
              {"type":"redacted_thinking","data":"opaque-two"}, {"type":"text","text":"done"}]}]})
    }
    fn plain_history() -> NativeHistory {
        use crate::agent::native_capture::NativeTurnProjection;
        let mut source = body();
        source["messages"][1]["content"] = json!([{"type":"text","text":"done"}]);
        let endpoint = "https://example.test/v1/messages".parse().unwrap();
        NativeHistory::from_capture(
            0,
            vec![rig::completion::Message::user("literal user")],
            vec![NativeTurnProjection {
                sdk_run_id: "owned-run".into(),
                turn: 1,
                configured_model: "claude-opus-5-5".into(),
                content: rig::OneOrMany::one(rig::completion::AssistantContent::text("done")),
                advertised_tools: Vec::new(),
                native_tool_results: Vec::new(),
                native_input_delta: Some(Vec::new()),
                native_commitment: Some(
                    NativeCommitment::capture(&endpoint, &source, true).unwrap(),
                ),
            }],
        )
        .unwrap()
        .with_retired_outcome(RetiredNativeOutcome::Completed)
        .unwrap()
    }

    #[test]
    fn native_context_anchor_refuses_unobserved_messages_and_compaction_without_erasing_data() {
        let mut original = super::super::Session::new("anthropic", "claude-opus-5-5", 32_000, "");
        original.add_message(super::super::MessageRole::User, "literal user");
        original.add_message(super::super::MessageRole::Assistant, "done");
        original.store_native_history(plain_history()).unwrap();
        assert!(
            original
                .native_restore_history(
                    HistoryProtocol::AnthropicMessages,
                    ReplayBoundary::CompletedTurn,
                )
                .is_ok()
        );
        let mut appended = original.clone();
        appended.add_message(
            super::super::MessageRole::Assistant,
            "new context outside native capture",
        );
        let mut compacted = original.clone();
        compacted.compactions.push(super::super::Compaction {
            file_requests: Default::default(),
            summary: "current summary replaces old context".into(),
            first_kept_index: 1,
            summarized_count: 1,
            token_savings: 10,
            created_at: "2026-10-14T00:00:00Z".into(),
            memory_ref: None,
            pruned: None,
        });
        let outcomes = [&appended, &compacted].map(|session| {
            let before = serde_json::to_value(session).unwrap();
            let outcome = session.native_restore_history(
                HistoryProtocol::AnthropicMessages,
                ReplayBoundary::CompletedTurn,
            );
            assert_eq!(serde_json::to_value(session).unwrap(), before);
            outcome.map(|_| ())
        });
        assert_eq!(outcomes, [Err(RestartReason::ChangedPrefix); 2]);
    }

    #[test]
    fn native_complete_tool_tail_does_not_attest_completed_actor_boundary() {
        let mut history = plain_history();
        let frame = &mut history.frames[0];
        frame.content = rig::OneOrMany::one(rig::completion::AssistantContent::tool_call(
            "canonical-one",
            "read",
            json!({}),
        ));
        frame
            .advertised_tools
            .push(crate::agent::native_capture::AdvertisedToolIdentity {
                tool_id: "canonical-one".into(),
                provider_id: None,
                internal_id: "internal-one".into(),
            });
        frame.native_tool_results.push(rig::message::ToolResult {
            id: "canonical-one".into(),
            call_id: None,
            content: rig::OneOrMany::one(rig::message::ToolResultContent::text(
                "actual surfaced result",
            )),
        });
        assert_eq!(
            history.require_completed_boundary(),
            Err(RestartReason::IncompleteTurn)
        );
    }

    #[test]
    fn native_history_import_rejects_unknown_fields_partial_mappings_and_live_reply_as_completed() {
        use rig::completion::AssistantContent;
        let original = plain_history();
        let mut unknown_boundary = serde_json::to_value(&original).unwrap();
        unknown_boundary["boundary"] = json!("imported_runtime_authority");
        assert!(serde_json::from_value::<NativeHistory>(unknown_boundary).is_err());
        let mut unsealed = original.clone();
        unsealed.boundary = None;
        assert_eq!(
            unsealed.saved_boundary(),
            Err(RestartReason::IncompleteTurn)
        );
        assert_eq!(
            unsealed.validate_saved(),
            Err(RestartReason::IncompleteTurn)
        );
        assert!(matches!(
            unsealed.with_retired_outcome(RetiredNativeOutcome::Incomplete),
            Err(RestartReason::IncompleteTurn)
        ));
        let mut raw = serde_json::to_value(&original).unwrap();
        raw["unexpected_authority"] = json!({"grant":"all"});
        assert!(serde_json::from_value::<NativeHistory>(raw).is_err());
        let mut encoded_session = serde_json::to_value(super::super::Session::new(
            "anthropic",
            "claude-opus-5-5",
            32_000,
            "",
        ))
        .unwrap();
        let mut malicious = serde_json::to_value(&original).unwrap();
        malicious["opaque-secret-unknown-key"] = true.into();
        encoded_session["native_history"] = malicious;
        let error = super::super::storage::decode_session_json(
            &serde_json::to_string(&encoded_session).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "invalid persisted session JSON");

        let mut credential_domain = original.clone();
        credential_domain.frames[0]
            .native_commitment
            .as_mut()
            .unwrap()
            .domain
            .endpoint = "https://key:opaque-secret@example.test/v1/messages".into();
        assert_eq!(
            credential_domain.validate(),
            Err(RestartReason::ChangedDomain)
        );
        let mut missing = original.clone();
        missing.frames[0].native_input_delta = None;
        assert_eq!(missing.validate(), Err(RestartReason::UnavailableSource));
        let mut incomplete = original.clone();
        incomplete.frames[0].content =
            rig::OneOrMany::one(AssistantContent::tool_call("call-one", "read", json!({})));
        assert_eq!(
            incomplete.restore_history(
                HistoryProtocol::AnthropicMessages,
                &NativeFrontier::default(),
                ReplayBoundary::CompletedTurn
            ),
            Err(RestartReason::IncompleteTurn)
        );
        assert_eq!(
            incomplete.restore_history(
                HistoryProtocol::AnthropicMessages,
                &NativeFrontier::default(),
                ReplayBoundary::LiveToolReply
            ),
            Err(RestartReason::IncompleteTurn),
        );
        let mut next = original.frames[0].clone();
        next.turn = 2;
        next.native_input_delta = Some(vec![rig::completion::Message::user("new")]);
        incomplete.frames.push(next);
        assert_eq!(incomplete.validate(), Err(RestartReason::UnavailableSource));
    }

    #[test]
    fn native_history_base_frame_and_total_bounds_refuse_without_truncation() {
        let original = plain_history();
        let mut large_base = original.clone();
        large_base.base[0] = rig::completion::Message::user("b".repeat(MAX_NATIVE_FRAME));
        assert_eq!(large_base.validate(), Err(RestartReason::Limit));
        let mut large_frame = original.clone();
        large_frame.frames[0].content = rig::OneOrMany::one(
            rig::completion::AssistantContent::text("s".repeat(MAX_NATIVE_FRAME)),
        );
        assert_eq!(large_frame.validate(), Err(RestartReason::Limit));
        let mut total = original.clone();
        total.frames[0].content = rig::OneOrMany::one(rig::completion::AssistantContent::text(
            "t".repeat(1_500_000),
        ));
        for turn in 2..=3 {
            let mut frame = total.frames[0].clone();
            frame.turn = turn;
            total.frames.push(frame);
        }
        assert_eq!(total.validate(), Err(RestartReason::Limit));
        assert_eq!(total.frames.len(), 3);
        assert!(original.validate().is_ok());
    }

    #[test]
    fn native_session_bound_counts_generic_history_and_refuses_candidate_without_mutation() {
        let mut session = super::super::Session::new("anthropic", "claude-opus-5-5", 32_000, "");
        session.add_message(
            super::super::MessageRole::User,
            &"g".repeat(super::super::storage::MAX_SESSION_BYTES),
        );
        assert_eq!(
            session.store_native_history(plain_history()),
            Err(RestartReason::Limit)
        );
        assert!(session.native_history_snapshot().is_none());
        let oversized = serde_json::to_string(&session).unwrap();
        assert!(
            super::super::storage::decode_session_json(&oversized)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn native_frontier_clone_fork_reload_and_exhaustion_never_resurrect_retired_epoch() {
        let frontier = NativeFrontier::default();
        let clone = frontier.clone();
        let fork = frontier.fork();
        assert_eq!(frontier.retire(), Ok(1));
        assert_eq!(clone.current(), 1);
        assert_eq!(fork.current(), 0);
        let loaded: NativeFrontier =
            serde_json::from_str(&serde_json::to_string(&clone).unwrap()).unwrap();
        clone.retire().unwrap();
        assert_eq!(loaded.current(), 1);
        let exhausted: NativeFrontier = serde_json::from_str(&u64::MAX.to_string()).unwrap();
        assert_eq!(exhausted.retire(), Err(RestartReason::Limit));
        assert_eq!(exhausted.current(), u64::MAX);
        assert_ne!(ReplayBoundary::LiveToolReply, ReplayBoundary::CompletedTurn);
    }

    #[test]
    fn native_commitment_missing_opaque_witness_refuses_changed_actual_prefix() {
        let endpoint = "https://example.test/v1/messages".parse().unwrap();
        let mut actual = body();
        let mut missing = NativeCommitment::capture(&endpoint, &actual, true).unwrap();
        missing.blocks.clear();
        actual["system"][0]["text"] = "changed without any witnesses".into();
        assert_eq!(
            missing.validate(&endpoint, &actual),
            Err(RestartReason::ChangedPrefix)
        );
    }

    #[test]
    fn native_commitments_ignore_only_generated_last_marker_and_nonbinding_controls() {
        let endpoint = "https://example.test/v1/messages".parse().unwrap();
        let original = body();
        let commitment = NativeCommitment::capture(&endpoint, &original, true).unwrap();
        assert_eq!(commitment.blocks.len(), 2);
        let mut later = original.clone();
        let marker = json!({"type":"ephemeral"});
        later["messages"][1]["content"][3]["cache_control"] = marker.clone();
        // The SDK would clear the old generated marker before appending.
        later["messages"][1]["content"][3]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        later["output_config"] = json!({"effort":"max"});
        later["max_tokens"] = 1000.into();
        later["messages"].as_array_mut().unwrap().push(
            json!({"role":"user","content":[{"type":"text","text":"next","cache_control":marker}]}),
        );
        assert_eq!(commitment.validate(&endpoint, &later), Ok(()));
        for path in [0, 1, 2, 3, 4, 5] {
            let mut edited = later.clone();
            match path {
                0 => {
                    edited["tools"][0]["input_schema"]["properties"]["cache_control"]["type"] =
                        "number".into()
                }
                1 => {
                    edited["messages"][1]["content"][1]["input"]["cache_control"] =
                        "changed-data".into()
                }
                2 => edited["messages"][0]["content"][0]["text"] = "rewritten-user".into(),
                3 => edited["system"][0]["cache_control"] = json!({"type":"ephemeral","ttl":"1h"}),
                4 => edited["tools"][0]["cache_control"] = json!({"type":"ephemeral"}),
                _ => {
                    edited["messages"][0]["content"][0]["cache_control"] =
                        json!({"type":"ephemeral"})
                }
            }
            assert_eq!(
                commitment.validate(&endpoint, &edited),
                Err(RestartReason::ChangedPrefix)
            );
        }
        let mut single = original;
        single["messages"][1]["content"]
            .as_array_mut()
            .unwrap()
            .truncate(1);
        let single_commitment = NativeCommitment::capture(&endpoint, &single, true).unwrap();
        single["messages"][1]["content"][0]["signature"] = "tampered".into();
        assert_eq!(
            single_commitment.validate(&endpoint, &single),
            Err(RestartReason::ChangedPrefix)
        );
        let mut gap = later;
        gap["messages"][1]["content"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        assert_eq!(
            commitment.validate(&endpoint, &gap),
            Err(RestartReason::ChangedPrefix)
        );
    }
    #[test]
    fn native_commitments_refuse_credential_routes_model_changes_and_payload_limits() {
        let endpoint = "https://example.test/v1/messages".parse().unwrap();
        let mut source = body();
        let commitment = NativeCommitment::capture(&endpoint, &source, true).unwrap();
        for uri in [
            "https://key:secret@example.test/v1/messages",
            "https://example.test/v1/messages?api_key=secret",
        ] {
            assert!(NativeDomain::from_native_view(&uri.parse().unwrap(), &source).is_err());
        }
        assert_eq!(
            commitment.validate(&"https://other.test/v1/messages".parse().unwrap(), &source),
            Err(RestartReason::ChangedDomain)
        );
        source["model"] = "claude-sonnet-5-5".into();
        assert_eq!(
            commitment.validate(&endpoint, &source),
            Err(RestartReason::ChangedDomain)
        );
        source["messages"][0]["content"][0]["text"] = "x".repeat(MAX_BINDING_BYTES).into();
        assert_eq!(
            NativeCommitment::capture(&endpoint, &source, true),
            Err(RestartReason::Limit)
        );
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::{NativeFrontier, RestartReason};

    #[test]
    fn native_refusal_cutoff_is_shared_persisted_and_forked_without_runtime_owners() {
        let frontier = NativeFrontier::default();
        let older = frontier.clone();
        frontier
            .refuse_expected(0, RestartReason::IncompleteTurn)
            .unwrap();
        assert_eq!(older.current(), 1);
        assert!(!older.ready());
        let encoded = serde_json::to_value(&older).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({"epoch": 1, "refusal": "incomplete_turn"})
        );
        let imported: NativeFrontier = serde_json::from_value(encoded).unwrap();
        assert_eq!(imported.refusal(), Some(RestartReason::IncompleteTurn));
        assert!(!imported.ready());
        assert!(imported.begin_retirement(1).is_err());
        let independent = imported.fork();
        assert_eq!(independent.current(), 1);
        assert_eq!(independent.refusal(), imported.refusal());
        let healthy: NativeFrontier = serde_json::from_str("7").unwrap();
        assert!(healthy.ready());
        assert_eq!(serde_json::to_string(&healthy).unwrap(), "7");
        assert!(
            serde_json::from_str::<NativeFrontier>(
                r#"{"epoch":1,"refusal":"incomplete_turn","grant":true}"#
            )
            .is_err()
        );
    }

    #[test]
    fn stale_refusal_cannot_poison_a_newer_cutoff_and_counter_exhaustion_stays_denied() {
        let healthy: NativeFrontier = serde_json::from_str("9").unwrap();
        assert_eq!(
            healthy.refuse_expected(8, RestartReason::UnavailableSource),
            Err(RestartReason::ChangedPrefix)
        );
        assert!(healthy.ready());
        assert_eq!(healthy.refusal(), None);
        let exhausted: NativeFrontier = serde_json::from_str(&u64::MAX.to_string()).unwrap();
        assert_eq!(
            exhausted.refuse_expected(u64::MAX, RestartReason::UnavailableSource),
            Err(RestartReason::Limit)
        );
        assert!(!exhausted.ready());
        let imported: NativeFrontier =
            serde_json::from_str(&serde_json::to_string(&exhausted).unwrap()).unwrap();
        assert!(!imported.ready());
        assert_eq!(imported.refusal(), Some(RestartReason::UnavailableSource));
    }
}

#[cfg(test)]
mod refusal_storage_tests {
    use super::{NativeFrontier, RestartReason};

    #[test]
    fn native_denied_snapshot_survives_failed_save_and_exact_private_reload() {
        let roots = crate::tests::OwnedTestDirectory::new("native-denied-save");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(roots.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(roots.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(roots.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(roots.path().join("cache").into_os_string()),
            ),
        ]);
        let mut session = crate::session::Session::new("anthropic", "claude-opus-5-5", 32_000, "");
        session.add_message(
            crate::session::MessageRole::User,
            "retained literal failed prompt",
        );
        let older = session.clone();
        session
            .native_frontier
            .refuse_expected(0, RestartReason::UnavailableSource)
            .unwrap();
        let sessions = crate::paths::process_paths_ref().unwrap().sessions_dir();
        crate::paths::ensure_private_directory(&sessions).unwrap();
        let target = sessions.join(format!("{}.json", session.id));
        std::fs::create_dir(&target).unwrap();
        assert!(crate::session::storage::save_session(&session).is_err());
        assert!(
            !older.native_frontier.ready(),
            "failed save must not un-fence older clones"
        );
        assert_eq!(
            session.native_frontier.refusal(),
            Some(RestartReason::UnavailableSource)
        );
        std::fs::remove_dir(&target).unwrap();
        // Removing the bad target cannot manufacture native writer authority.
        assert!(crate::session::storage::save_session(&session).is_err());
        // This test owns imported compatibility data, not an accepted actor.
        // Create the bounded private data fixture explicitly; publication of
        // real actor observations is covered by native_publication/frontends.
        let json = crate::session::storage::serialize_bounded_session(&session).unwrap();
        crate::fs::private_atomic_write_sync(&target, json.as_bytes()).unwrap();
        let imported = crate::session::storage::load_session_exact(&session.id)
            .unwrap()
            .unwrap();
        assert_eq!(imported.native_frontier.current(), 1);
        assert_eq!(
            imported.native_frontier.refusal(),
            Some(RestartReason::UnavailableSource)
        );
        assert_eq!(
            imported.messages[0].content,
            "retained literal failed prompt"
        );
        assert!(imported.permission_allowlist.is_empty());
        assert!(!imported.native_frontier.ready());
        let fork = imported.native_frontier.fork();
        assert!(!fork.ready());
        assert_eq!(fork.refusal(), imported.native_frontier.refusal());
        assert!(
            serde_json::from_str::<NativeFrontier>(r#"{"epoch":1,"refusal":"provider_payload"}"#)
                .is_err()
        );
        crate::session::lock::release_session(&session.id);
    }
}

#[cfg(test)]
mod active_refusal_tests {
    use super::{NativeFrontier, RestartReason};

    #[test]
    fn native_accepted_refusal_survives_an_active_retirement_and_its_drop() {
        let frontier = NativeFrontier::default();
        let older = frontier.clone();
        let retirement = frontier.begin_retirement(0).unwrap();
        assert!(!frontier.ready());
        assert_eq!(frontier.retire(), Err(RestartReason::ChangedPrefix));
        assert!(frontier.begin_retirement(0).is_err());
        assert_eq!(
            frontier.refuse_expected(0, RestartReason::UnavailableSource),
            Ok(())
        );
        assert_eq!(older.refusal(), Some(RestartReason::UnavailableSource));
        assert_eq!(older.current(), 1);
        drop(retirement);
        assert!(!older.ready());
        let imported: NativeFrontier =
            serde_json::from_str(&serde_json::to_string(&older).unwrap()).unwrap();
        assert_eq!(imported.refusal(), Some(RestartReason::UnavailableSource));
        assert!(!imported.ready());
    }
}

#[cfg(test)]
mod vertex81_domain_tests {
    use super::*;
    #[test]
    fn vertex81_native_domain_uses_exact_route_without_inventing_wire_model() {
        let endpoint: http::Uri = "https://us-east5-aiplatform.googleapis.com/v1/projects/owned/locations/us-east5/publishers/anthropic/models/claude-owned@20260101:streamRawPredict".parse().unwrap();
        let body = serde_json::json!({"anthropic_version":"vertex-2023-10-16","messages":[]});
        let domain = NativeDomain::from_native_view(&endpoint, &body).unwrap();
        assert_eq!(domain.request_model, "claude-owned@20260101");
        domain.validate_saved().unwrap();
        let mut saved = domain.clone();
        saved.request_model = "other".into();
        assert_eq!(saved.validate_saved(), Err(RestartReason::ChangedDomain));
        for body in [
            serde_json::json!({"model":"claude-owned@20260101","anthropic_version":"vertex-2023-10-16"}),
            serde_json::json!({"anthropic_version":"wrong"}),
            serde_json::json!({}),
        ] {
            assert!(NativeDomain::from_native_view(&endpoint, &body).is_err());
        }
        for bad in [
            endpoint.to_string().replace("/anthropic/", "/google/"),
            endpoint.to_string().replace("claude-owned@20260101", ".."),
            format!("{endpoint}?private=secret"),
        ] {
            assert!(NativeDomain::from_native_view(&bad.parse().unwrap(), &body).is_err());
        }
    }
}
