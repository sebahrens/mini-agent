//! ACP client file access: lets the read, edit and write tools use the
//! connected editor's `fs/read_text_file` and `fs/write_text_file` so unsaved
//! buffers are visible and writes go through the editor.
//!
//! Closed by default. A handle exists only when the operator set
//! `acp_client_file_access = true` (a project-trust-gated key) *and* the client
//! advertised the capability in `initialize`. The tools still run every
//! workspace, symlink and permission check on the on-disk path first; this
//! module only changes where a workspace-bound file's text comes from and goes.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ClientCapabilities, ReadTextFileRequest, SessionId, WriteTextFileRequest,
};
use agent_client_protocol::{Client, ConnectionTo};

use crate::agent::tools::client_fs::{
    ClientFileAccess, ClientFs, ClientFsFuture, MAX_CLIENT_TRANSFER_BYTES,
};

/// How long one client file request may take. A hung editor must not hang the
/// turn; the read falls back to disk and a write reports that it is unconfirmed.
const CLIENT_FS_TIMEOUT: Duration = Duration::from_secs(30);

const CAN_READ: u8 = 0b01;
const CAN_WRITE: u8 = 0b10;

/// Opt-in plus the capabilities the connected client advertised.
#[derive(Debug, Default)]
pub(super) struct ClientFsGate {
    opted_in: bool,
    advertised: AtomicU8,
}

impl ClientFsGate {
    pub(super) fn new(opted_in: bool) -> Self {
        Self {
            opted_in,
            advertised: AtomicU8::new(0),
        }
    }

    /// Remember what the client advertised in `initialize`. Without the
    /// operator opt-in nothing is recorded, so the gate cannot open later.
    pub(super) fn record(&self, capabilities: &ClientCapabilities) {
        if !self.opted_in {
            return;
        }
        let mut bits = 0;
        if capabilities.fs.read_text_file {
            bits |= CAN_READ;
        }
        if capabilities.fs.write_text_file {
            bits |= CAN_WRITE;
        }
        self.advertised.store(bits, Ordering::Release);
    }

    /// The file seam for one session's turn, or `None` while it is closed.
    pub(super) fn handle(
        &self,
        connection: &ConnectionTo<Client>,
        session_id: &SessionId,
    ) -> Option<ClientFs> {
        if !self.opted_in {
            return None;
        }
        let bits = self.advertised.load(Ordering::Acquire);
        (bits != 0).then(|| {
            Arc::new(AcpClientFs {
                connection: connection.clone(),
                session_id: session_id.clone(),
                bits,
            }) as ClientFs
        })
    }

    #[cfg(test)]
    pub(super) fn is_open(&self) -> bool {
        self.opted_in && self.advertised.load(Ordering::Acquire) != 0
    }
}

struct AcpClientFs {
    connection: ConnectionTo<Client>,
    session_id: SessionId,
    bits: u8,
}

impl std::fmt::Debug for AcpClientFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpClientFs")
            .field("session_id", &self.session_id)
            .field("bits", &self.bits)
            .finish_non_exhaustive()
    }
}

impl ClientFileAccess for AcpClientFs {
    fn can_read(&self) -> bool {
        self.bits & CAN_READ != 0
    }

    fn can_write(&self) -> bool {
        self.bits & CAN_WRITE != 0
    }

    fn read_text<'a>(&'a self, path: &'a Path) -> ClientFsFuture<'a, String> {
        Box::pin(async move {
            let request = ReadTextFileRequest::new(self.session_id.clone(), path.to_path_buf());
            let response = tokio::time::timeout(
                CLIENT_FS_TIMEOUT,
                self.connection.send_request(request).block_task(),
            )
            .await
            .map_err(|_| "the client did not answer fs/read_text_file in time".to_string())?
            .map_err(|error| error.message.to_string())?;
            if response.content.len() > MAX_CLIENT_TRANSFER_BYTES {
                return Err(format!(
                    "the client returned more than {MAX_CLIENT_TRANSFER_BYTES} bytes"
                ));
            }
            Ok(response.content)
        })
    }

    fn write_text<'a>(&'a self, path: &'a Path, content: &'a str) -> ClientFsFuture<'a, ()> {
        Box::pin(async move {
            let request = WriteTextFileRequest::new(
                self.session_id.clone(),
                path.to_path_buf(),
                content.to_string(),
            );
            tokio::time::timeout(
                CLIENT_FS_TIMEOUT,
                self.connection.send_request(request).block_task(),
            )
            .await
            .map_err(|_| "the client did not confirm fs/write_text_file in time".to_string())?
            .map_err(|error| error.message.to_string())?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::FileSystemCapabilities;

    fn advertising(read: bool, write: bool) -> ClientCapabilities {
        ClientCapabilities::new().fs(FileSystemCapabilities::new()
            .read_text_file(read)
            .write_text_file(write))
    }

    #[test]
    fn closed_without_operator_opt_in_even_when_the_client_advertises() {
        let gate = ClientFsGate::new(false);
        gate.record(&advertising(true, true));
        assert!(!gate.is_open());
        assert_eq!(gate.advertised.load(Ordering::Acquire), 0);
    }

    #[test]
    fn closed_when_opted_in_but_the_client_advertises_nothing() {
        let gate = ClientFsGate::new(true);
        gate.record(&ClientCapabilities::new());
        assert!(!gate.is_open());
        assert!(!ClientFsGate::default().is_open());
    }

    #[test]
    fn opens_only_with_opt_in_and_a_matching_advertisement() {
        let gate = ClientFsGate::new(true);
        gate.record(&advertising(true, false));
        assert!(gate.is_open());
        assert_eq!(gate.advertised.load(Ordering::Acquire), CAN_READ);
        gate.record(&advertising(true, true));
        assert_eq!(
            gate.advertised.load(Ordering::Acquire),
            CAN_READ | CAN_WRITE
        );
    }

    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::schema::v1::{
        InitializeRequest, InitializeResponse, PromptRequest, PromptResponse, ReadTextFileResponse,
        StopReason, WriteTextFileResponse,
    };
    use agent_client_protocol::{Agent, ConnectTo, on_receive_request};
    use std::sync::Mutex;

    /// A minimal agent that opens the gate on `initialize` and, on `prompt`,
    /// exercises the real wire path through the handle it gets.
    struct WireAgent {
        gate: Arc<ClientFsGate>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl ConnectTo<Client> for WireAgent {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> Result<(), agent_client_protocol::Error> {
            let init_gate = self.gate.clone();
            let gate = self.gate.clone();
            let log = self.log.clone();
            Agent
                .builder()
                .on_receive_request(
                    async move |req: InitializeRequest, responder, _cx| {
                        init_gate.record(&req.client_capabilities);
                        responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                    },
                    on_receive_request!(),
                )
                .on_receive_request(
                    async move |req: PromptRequest, responder, cx| {
                        // A turn runs on its own task: awaiting the client from
                        // inside the dispatch handler would block the very loop
                        // that delivers its answers.
                        let handle = gate.handle(&cx, &req.session_id);
                        let log = log.clone();
                        cx.spawn(async move {
                            let line = match handle {
                                None => "closed".to_string(),
                                Some(fs) => {
                                    let read = fs.read_text(Path::new("/ws/a.txt")).await;
                                    let wrote =
                                        fs.write_text(Path::new("/ws/a.txt"), "after").await;
                                    format!("{read:?} {wrote:?}")
                                }
                            };
                            log.lock().unwrap().push(line);
                            responder.respond(PromptResponse::new(StopReason::EndTurn))
                        })
                    },
                    on_receive_request!(),
                )
                .connect_to(client)
                .await
        }
    }

    async fn drive(opted_in: bool, advertise: bool) -> (Vec<String>, Vec<String>) {
        let gate = Arc::new(ClientFsGate::new(opted_in));
        let log = Arc::new(Mutex::new(Vec::new()));
        let client_saw = Arc::new(Mutex::new(Vec::new()));
        let saw_read = client_saw.clone();
        let saw_write = client_saw.clone();
        Client
            .builder()
            .on_receive_request(
                async move |req: ReadTextFileRequest, responder, _cx| {
                    saw_read
                        .lock()
                        .unwrap()
                        .push(format!("read {}", req.path.display()));
                    responder.respond(ReadTextFileResponse::new("buffer text"))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |req: WriteTextFileRequest, responder, _cx| {
                    saw_write.lock().unwrap().push(format!(
                        "write {} {}",
                        req.path.display(),
                        req.content
                    ));
                    responder.respond(WriteTextFileResponse::new())
                },
                on_receive_request!(),
            )
            .connect_with(
                WireAgent {
                    gate,
                    log: log.clone(),
                },
                async move |cx| {
                    let capabilities = if advertise {
                        advertising(true, true)
                    } else {
                        ClientCapabilities::new()
                    };
                    cx.send_request(
                        InitializeRequest::new(ProtocolVersion::V1)
                            .client_capabilities(capabilities),
                    )
                    .block_task()
                    .await?;
                    cx.send_request(PromptRequest::new("s1", vec![]))
                        .block_task()
                        .await?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        let log = log.lock().unwrap().clone();
        let seen = client_saw.lock().unwrap().clone();
        (log, seen)
    }

    #[tokio::test]
    async fn open_gate_round_trips_read_and_write_over_the_protocol() {
        let (log, seen) = drive(true, true).await;
        assert_eq!(seen, ["read /ws/a.txt", "write /ws/a.txt after"]);
        assert_eq!(log, ["Ok(\"buffer text\") Ok(())"]);
    }

    #[tokio::test]
    async fn default_closed_never_sends_a_file_request_even_when_advertised() {
        let (log, seen) = drive(false, true).await;
        assert_eq!(log, ["closed"]);
        assert!(seen.is_empty(), "{seen:?}");
    }

    #[tokio::test]
    async fn opted_in_but_unadvertised_never_sends_a_file_request() {
        let (log, seen) = drive(true, false).await;
        assert_eq!(log, ["closed"]);
        assert!(seen.is_empty(), "{seen:?}");
    }
}
