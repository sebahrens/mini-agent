//! Bounded proposal validation and durable queue handoff.
//!
//! The QuickJS thread performs only shape validation, canonical identity
//! construction, and a bounded request/response exchange. SQLite, verification,
//! embeddings, held-out data, and lifecycle transitions remain on worker threads.

use rquickjs::{Array, Object, String as JsString};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use super::store::{EnqueueResult, SkillStore, StoreError, current_timestamp};
use super::{
    CapabilityManifest, CapabilityTier, HostCapability, IdentityError, SkillArtifact, SkillExport,
};

pub(crate) const MAX_SOURCE_BYTES: usize = 32 * 1024;
pub(crate) const MAX_DESCRIPTION_BYTES: usize = 1024;
pub(crate) const MAX_EXPORTS: usize = 32;
pub(crate) const MAX_EXPORT_NAME_BYTES: usize = 128;
pub(crate) const MAX_SIGNATURE_BYTES: usize = 512;
pub(crate) const MAX_TESTS: usize = 20;
pub(crate) const MAX_TEST_BYTES: usize = 4 * 1024;
pub(crate) const MAX_TAGS: usize = 32;
pub(crate) const MAX_TAG_BYTES: usize = 64;
pub(crate) const MAX_CANONICAL_INPUT_BYTES: usize = 64 * 1024;
pub(crate) const DEFAULT_SESSION_ATTEMPTS: usize = 3;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct JsExport {
    pub name: String,
    pub signature: String,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct JsCapability {
    pub tier: String,
    pub allowed_hosts: Vec<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct JsProposal {
    pub source: String,
    pub description: String,
    pub exports: Vec<JsExport>,
    pub tests: Vec<String>,
    pub capability: JsCapability,
    pub tags: Vec<String>,
    pub predecessor_id: Option<String>,
}

impl fmt::Debug for JsProposal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JsProposal")
            .field("source", &"<redacted>")
            .field("source_len", &self.source.len())
            .field("description_len", &self.description.len())
            .field("exports", &self.exports.len())
            .field("tests", &"<redacted>")
            .field("test_count", &self.tests.len())
            .field("capability_tier", &self.capability.tier)
            .field("tag_count", &self.tags.len())
            .field("predecessor_id", &self.predecessor_id)
            .finish()
    }
}

impl JsProposal {
    pub(crate) fn from_object(object: &Object<'_>) -> Result<Self, ProposalError> {
        reject_unknown_keys(
            object,
            &[
                "source",
                "description",
                "exports",
                "tests",
                "capability",
                "tags",
                "predecessor_id",
            ],
            "proposal",
        )?;
        let exports = required_array(object, "exports", 1, MAX_EXPORTS)?
            .iter::<Object<'_>>()
            .map(|export| {
                let export = export.map_err(|_| ProposalError::InvalidField {
                    field: "exports",
                    reason: "must be an array of objects",
                })?;
                reject_unknown_keys(&export, &["name", "signature"], "export")?;
                Ok(JsExport {
                    name: required_string(&export, "name", MAX_EXPORT_NAME_BYTES)?,
                    signature: required_string(&export, "signature", MAX_SIGNATURE_BYTES)?,
                })
            })
            .collect::<Result<Vec<_>, ProposalError>>()?;
        let capability =
            object
                .get::<_, Object<'_>>("capability")
                .map_err(|_| ProposalError::InvalidField {
                    field: "capability",
                    reason: "must be an object",
                })?;
        reject_unknown_keys(&capability, &["tier", "allowed_hosts"], "capability")?;

        Ok(Self {
            source: required_string(object, "source", MAX_SOURCE_BYTES)?,
            description: required_string(object, "description", MAX_DESCRIPTION_BYTES)?,
            exports,
            tests: required_string_array(object, "tests", 1, MAX_TESTS, MAX_TEST_BYTES)?,
            capability: JsCapability {
                tier: required_string(&capability, "tier", MAX_TAG_BYTES)?,
                allowed_hosts: required_string_array(
                    &capability,
                    "allowed_hosts",
                    0,
                    4,
                    MAX_TAG_BYTES,
                )?,
            },
            tags: optional_string_array(object, "tags", MAX_TAGS, MAX_TAG_BYTES)?,
            predecessor_id: object
                .get::<_, Option<JsString<'_>>>("predecessor_id")
                .map_err(|_| ProposalError::InvalidField {
                    field: "predecessor_id",
                    reason: "must be a string",
                })?
                .map(|value| js_string(value, "predecessor_id", 64))
                .transpose()?,
        })
    }

    pub(crate) fn validate_and_canonicalize(self) -> Result<SkillArtifact, ProposalError> {
        validate_text("source", &self.source, MAX_SOURCE_BYTES, true)?;
        validate_text(
            "description",
            &self.description,
            MAX_DESCRIPTION_BYTES,
            true,
        )?;
        if self.exports.is_empty() || self.exports.len() > MAX_EXPORTS {
            return Err(ProposalError::InvalidField {
                field: "exports",
                reason: "must contain between 1 and 32 entries",
            });
        }
        if self.tests.is_empty() || self.tests.len() > MAX_TESTS {
            return Err(ProposalError::InvalidField {
                field: "tests",
                reason: "must contain between 1 and 20 entries",
            });
        }
        if self.tags.len() > MAX_TAGS {
            return Err(ProposalError::InvalidField {
                field: "tags",
                reason: "contains too many entries",
            });
        }

        let mut aggregate = self.source.len() + self.description.len();
        let exports = self
            .exports
            .into_iter()
            .map(|export| {
                validate_export_name(&export.name)?;
                validate_text(
                    "exports.signature",
                    &export.signature,
                    MAX_SIGNATURE_BYTES,
                    true,
                )?;
                aggregate = aggregate
                    .saturating_add(export.name.len())
                    .saturating_add(export.signature.len());
                Ok(SkillExport {
                    name: export.name,
                    signature: export.signature,
                })
            })
            .collect::<Result<Vec<_>, ProposalError>>()?;
        for test in &self.tests {
            validate_text("tests", test, MAX_TEST_BYTES, true)?;
            aggregate = aggregate.saturating_add(test.len());
        }
        for tag in &self.tags {
            validate_text("tags", tag, MAX_TAG_BYTES, false)?;
            aggregate = aggregate.saturating_add(tag.len());
        }
        if aggregate > MAX_CANONICAL_INPUT_BYTES {
            return Err(ProposalError::PayloadTooLarge);
        }
        if let Some(predecessor_id) = self.predecessor_id.as_deref() {
            validate_id(predecessor_id)?;
        }

        let tier = CapabilityTier::from_token(&self.capability.tier)
            .ok_or_else(|| ProposalError::InvalidCapability("unknown or forbidden tier".into()))?;
        if self.capability.allowed_hosts.len() > 4 {
            return Err(ProposalError::InvalidCapability(
                "too many allowed hosts".into(),
            ));
        }
        let hosts = self
            .capability
            .allowed_hosts
            .into_iter()
            .map(|host| {
                HostCapability::from_token(&host).ok_or_else(|| {
                    ProposalError::InvalidCapability(format!("unknown or forbidden host {host}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let capability = CapabilityManifest::new(tier, hosts)
            .map_err(|error| ProposalError::InvalidCapability(error.to_string()))?;
        SkillArtifact::new(
            self.source,
            self.description,
            self.tags,
            exports,
            self.tests,
            capability,
        )
        .map_err(ProposalError::Identity)
    }
}

fn required_string(
    object: &Object<'_>,
    key: &'static str,
    max_bytes: usize,
) -> Result<String, ProposalError> {
    let value = object
        .get::<_, JsString<'_>>(key)
        .map_err(|_| ProposalError::InvalidField {
            field: key,
            reason: "must be a string",
        })?;
    js_string(value, key, max_bytes)
}

fn js_string(
    value: JsString<'_>,
    field: &'static str,
    max_bytes: usize,
) -> Result<String, ProposalError> {
    let value = value
        .to_cstring()
        .map_err(|_| ProposalError::InvalidField {
            field,
            reason: "must be valid UTF-8",
        })?;
    if value.len() > max_bytes {
        return Err(ProposalError::InvalidField {
            field,
            reason: "exceeds its byte limit",
        });
    }
    Ok(value.as_str().to_string())
}

fn required_array<'js>(
    object: &Object<'js>,
    key: &'static str,
    min_items: usize,
    max_items: usize,
) -> Result<Array<'js>, ProposalError> {
    let array = object
        .get::<_, Array<'js>>(key)
        .map_err(|_| ProposalError::InvalidField {
            field: key,
            reason: "must be an array",
        })?;
    if array.len() < min_items || array.len() > max_items {
        return Err(ProposalError::InvalidField {
            field: key,
            reason: "contains an invalid number of entries",
        });
    }
    Ok(array)
}

fn required_string_array(
    object: &Object<'_>,
    key: &'static str,
    min_items: usize,
    max_items: usize,
    max_bytes: usize,
) -> Result<Vec<String>, ProposalError> {
    required_array(object, key, min_items, max_items)?
        .iter::<JsString<'_>>()
        .map(|value| {
            value
                .map_err(|_| ProposalError::InvalidField {
                    field: key,
                    reason: "must be an array of strings",
                })
                .and_then(|value| js_string(value, key, max_bytes))
        })
        .collect()
}

fn optional_string_array(
    object: &Object<'_>,
    key: &'static str,
    max_items: usize,
    max_bytes: usize,
) -> Result<Vec<String>, ProposalError> {
    let Some(array) =
        object
            .get::<_, Option<Array<'_>>>(key)
            .map_err(|_| ProposalError::InvalidField {
                field: key,
                reason: "must be an array of strings",
            })?
    else {
        return Ok(Vec::new());
    };
    if array.len() > max_items {
        return Err(ProposalError::InvalidField {
            field: key,
            reason: "contains too many entries",
        });
    }
    array
        .iter::<JsString<'_>>()
        .map(|value| {
            value
                .map_err(|_| ProposalError::InvalidField {
                    field: key,
                    reason: "must be an array of strings",
                })
                .and_then(|value| js_string(value, key, max_bytes))
        })
        .collect()
}

fn reject_unknown_keys(
    object: &Object<'_>,
    allowed: &[&str],
    field: &'static str,
) -> Result<(), ProposalError> {
    for key in object.keys::<String>() {
        let key = key.map_err(|_| ProposalError::InvalidField {
            field,
            reason: "contains an unreadable key",
        })?;
        if !allowed.contains(&key.as_str()) {
            return Err(ProposalError::UnknownField { field, key });
        }
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
    require_nonempty: bool,
) -> Result<(), ProposalError> {
    if (require_nonempty && value.trim().is_empty()) || value.len() > max_bytes {
        return Err(ProposalError::InvalidField {
            field,
            reason: "is empty or exceeds its byte limit",
        });
    }
    if value.contains('\0') {
        return Err(ProposalError::InvalidField {
            field,
            reason: "contains a NUL byte",
        });
    }
    Ok(())
}

fn validate_export_name(value: &str) -> Result<(), ProposalError> {
    validate_text("exports.name", value, MAX_EXPORT_NAME_BYTES, true)?;
    let mut chars = value.chars();
    let valid_start = chars
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
    if !valid_start || !chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(ProposalError::InvalidField {
            field: "exports.name",
            reason: "must be a simple JavaScript identifier",
        });
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), ProposalError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProposalError::InvalidField {
            field: "predecessor_id",
            reason: "must be 64 lowercase hexadecimal characters",
        });
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProposalError {
    #[error("invalid {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    #[error("unknown field {key} in {field}")]
    UnknownField { field: &'static str, key: String },
    #[error("invalid capability: {0}")]
    InvalidCapability(String),
    #[error("proposal payload exceeds the aggregate byte limit")]
    PayloadTooLarge,
    #[error("proposal attempt budget exhausted")]
    BudgetExhausted,
    #[error("proposal queue is full; retry later")]
    QueueFull,
    #[error("proposal queue is closed")]
    QueueClosed,
    #[error("proposal enqueue timed out")]
    QueueTimeout,
    #[error("durable proposal storage is temporarily unavailable; retry later")]
    StoreUnavailable,
    #[error("proposal storage worker could not start")]
    WorkerUnavailable,
    #[error("invalid canonical identity: {0}")]
    Identity(IdentityError),
}

#[derive(Clone)]
pub(crate) struct AttemptBudget {
    remaining: Arc<AtomicUsize>,
}

impl AttemptBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            remaining: Arc::new(AtomicUsize::new(limit)),
        }
    }

    pub(crate) fn consume(&self) -> Result<(), ProposalError> {
        self.remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .map(|_| ())
            .map_err(|_| ProposalError::BudgetExhausted)
    }
}

struct ProposalCommand {
    artifact: SkillArtifact,
    predecessor_id: Option<String>,
    reply: mpsc::Sender<Result<EnqueueResult, ProposalError>>,
}

#[derive(Clone)]
pub(crate) struct ProposalSender {
    sender: mpsc::SyncSender<ProposalCommand>,
    response_timeout: Duration,
}

#[derive(Clone)]
pub(crate) struct ProposalHost {
    pub sender: ProposalSender,
    pub budget: AttemptBudget,
}

impl ProposalHost {
    pub(crate) fn new(sender: ProposalSender, budget: AttemptBudget) -> Self {
        Self { sender, budget }
    }
}

impl ProposalSender {
    pub(crate) fn enqueue(
        &self,
        artifact: SkillArtifact,
        predecessor_id: Option<String>,
    ) -> Result<EnqueueResult, ProposalError> {
        let (reply, response) = mpsc::channel();
        self.sender
            .try_send(ProposalCommand {
                artifact,
                predecessor_id,
                reply,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => ProposalError::QueueFull,
                mpsc::TrySendError::Disconnected(_) => ProposalError::QueueClosed,
            })?;
        response
            .recv_timeout(self.response_timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => ProposalError::QueueTimeout,
                mpsc::RecvTimeoutError::Disconnected => ProposalError::QueueClosed,
            })?
    }
}

pub(crate) struct ProposalReceiver {
    receiver: mpsc::Receiver<ProposalCommand>,
}

pub(crate) struct ProposalWorker {
    sender: Option<ProposalSender>,
    join: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl ProposalWorker {
    pub(crate) fn sender(&self) -> ProposalSender {
        self.sender
            .as_ref()
            .expect("proposal worker sender exists until drop")
            .clone()
    }
}

impl Drop for ProposalWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub(crate) struct ProposalQueue;

impl ProposalQueue {
    pub(crate) fn bounded(
        capacity: usize,
        response_timeout: Duration,
    ) -> (ProposalSender, ProposalReceiver) {
        let (sender, receiver) = mpsc::sync_channel(capacity.max(1));
        (
            ProposalSender {
                sender,
                response_timeout,
            },
            ProposalReceiver { receiver },
        )
    }

    pub(crate) fn start_store_worker(
        mut store: SkillStore,
        capacity: usize,
        response_timeout: Duration,
    ) -> Result<ProposalWorker, ProposalError> {
        let (sender, receiver) = Self::bounded(capacity, response_timeout);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let join = std::thread::Builder::new()
            .name("skill-proposal-store".to_string())
            .spawn(move || {
                while !worker_shutdown.load(Ordering::Acquire) {
                    match receiver.receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(command) => {
                            let result = current_timestamp()
                                .map_err(|_| ProposalError::StoreUnavailable)
                                .and_then(|now| {
                                    store
                                        .enqueue_proposal(
                                            &command.artifact,
                                            command.predecessor_id.as_deref(),
                                            now,
                                        )
                                        .map_err(map_store_error)
                                });
                            let _ = command.reply.send(result);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|_| ProposalError::WorkerUnavailable)?;
        Ok(ProposalWorker {
            sender: Some(sender),
            join: Some(join),
            shutdown,
        })
    }
}

fn map_store_error(error: StoreError) -> ProposalError {
    match error {
        StoreError::Constraint(message)
            if message.contains("predecessor") || message.contains("identity collision") =>
        {
            ProposalError::InvalidField {
                field: "predecessor_id",
                reason: "does not identify an eligible immutable predecessor",
            }
        }
        _ => ProposalError::StoreUnavailable,
    }
}
