use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::pickers::list::ListPicker;

/// What the user settled on after driving the rewind picker to completion.
pub enum RewindOutcome {
    /// Rewind the conversation to just before the message at this index.
    Confirmed(usize, RewindMode, RewindBinding),
    /// Back out without changing anything.
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RewindMode {
    Conversation,
    Files,
    Both,
}
#[derive(Clone)]
pub(crate) struct RewindBinding {
    session: String,
    history: [u8; 32],
    checkpoints: [u8; 32],
    source: crate::session::file_checkpoints::FileCheckpoints,
    workspace: [u8; 32],
}
/// SHA-256 of `value`'s JSON serialisation, streamed into the hasher so a
/// long history is never materialised as one buffer. Byte-identical to
/// hashing `serde_json::to_vec(value)`.
fn json_digest<T: serde::Serialize + ?Sized>(value: &T) -> anyhow::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.0.finalize().into())
}
impl RewindBinding {
    pub(crate) fn capture(
        session: &crate::session::Session,
        workspace: &crate::paths::WorkspaceBinding,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            session: session.id.to_string(),
            history: json_digest(&session.messages)?,
            checkpoints: json_digest(&session.file_checkpoints)?,
            source: session.file_checkpoints.clone(),
            workspace: workspace
                .compatibility_fingerprint()
                .map_err(anyhow::Error::msg)?,
        })
    }
    pub(crate) fn valid(
        &self,
        session: &crate::session::Session,
        workspace: &crate::paths::WorkspaceBinding,
    ) -> bool {
        Self::capture(session, workspace).is_ok_and(|current| {
            self.session == current.session
                && self.history == current.history
                && self.checkpoints == current.checkpoints
                && self.workspace == current.workspace
        })
    }
}

enum Stage {
    /// Choosing which earlier user turn to rewind to.
    PickMessage,
    /// Confirming the rewind for the chosen turn.
    Confirm,
}

const CONFIRM_REWIND: &str = "Conversation only";
const CONFIRM_FILES: &str = "Restore files only";
const CONFIRM_BOTH: &str = "Restore files and conversation";
const CONFIRM_CANCEL: &str = "Cancel";

/// A two-level modal picker for `/rewind`: first pick an earlier
/// user turn, then confirm the (destructive) rewind. Reuses [`ListPicker`] for
/// rendering and selection at each level; the outcome is read back by the event
/// loop, which owns the session and performs the actual rewind.
pub struct RewindPicker {
    active: bool,
    binding: RewindBinding,
    stage: Stage,
    list: ListPicker,
    /// Message index of each rewind-able user turn, in order. The list shows
    /// the previews, so `list.selected` indexes straight into this.
    targets: Vec<usize>,
    /// The previews, parked here while the confirm step replaces the list.
    previews: Vec<String>,
    chosen: Option<usize>,
    outcome: Option<RewindOutcome>,
}

impl RewindPicker {
    pub(crate) fn new(targets: Vec<(usize, String)>, binding: RewindBinding) -> Self {
        let mut list = ListPicker::new();
        let (targets, previews): (Vec<usize>, Vec<String>) = targets.into_iter().unzip();
        list.set_items(previews);
        RewindPicker {
            active: false,
            binding,
            stage: Stage::PickMessage,
            list,
            targets,
            previews: Vec::new(),
            chosen: None,
            outcome: None,
        }
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.list.set_monochrome(monochrome);
    }

    pub fn set_accent_color(&mut self, accent: Option<crossterm::style::Color>) {
        self.list.set_accent_color(accent);
    }

    pub fn activate(&mut self) {
        self.active = true;
        self.stage = Stage::PickMessage;
        self.list.activate();
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn take_outcome(&mut self) -> Option<RewindOutcome> {
        self.outcome.take()
    }

    pub fn draw(
        &self,
        out: &mut dyn std::io::Write,
        cols: u16,
        floor_row: u16,
    ) -> std::io::Result<()> {
        self.list.draw(out, cols, None, floor_row)
    }

    /// Handle a key while the picker is open. Always returns `true`: the picker
    /// is modal, so it swallows every keystroke until it resolves.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up => self.list.select_prev(),
            KeyCode::Down => self.list.select_next(),
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.list.select_prev();
                } else {
                    self.list.select_next();
                }
            }
            KeyCode::BackTab => self.list.select_prev(),
            KeyCode::Enter => self.confirm(),
            KeyCode::Esc => self.back_or_cancel(),
            _ => {}
        }
        true
    }

    fn confirm(&mut self) {
        match self.stage {
            Stage::PickMessage => {
                if let Some(&idx) = self.targets.get(self.list.selected) {
                    self.chosen = Some(idx);
                    self.stage = Stage::Confirm;
                    self.previews = self.list.take_items();
                    self.list.set_items(vec![
                        CONFIRM_REWIND.to_string(),
                        format!(
                            "{CONFIRM_FILES} — {}",
                            self.binding.source.summary_from(&self.binding.session, idx)
                        ),
                        format!(
                            "{CONFIRM_BOTH} — {}",
                            self.binding.source.summary_from(&self.binding.session, idx)
                        ),
                        CONFIRM_CANCEL.to_string(),
                    ]);
                    self.list.activate();
                }
            }
            Stage::Confirm => {
                self.outcome = Some(if self.list.selected < 3 {
                    RewindOutcome::Confirmed(
                        self.chosen.unwrap_or(0),
                        match self.list.selected {
                            1 => RewindMode::Files,
                            2 => RewindMode::Both,
                            _ => RewindMode::Conversation,
                        },
                        self.binding.clone(),
                    )
                } else {
                    RewindOutcome::Cancelled
                });
                self.finish();
            }
        }
    }

    fn back_or_cancel(&mut self) {
        match self.stage {
            // Esc at the confirm step backs up to the message list rather than
            // bailing out entirely, so a mis-step is cheap to correct.
            Stage::Confirm => {
                self.stage = Stage::PickMessage;
                self.list.set_items(std::mem::take(&mut self.previews));
                self.list.activate();
                if let Some(pos) = self
                    .chosen
                    .and_then(|idx| self.targets.iter().position(|i| *i == idx))
                {
                    self.list.selected = pos;
                }
            }
            Stage::PickMessage => {
                self.outcome = Some(RewindOutcome::Cancelled);
                self.finish();
            }
        }
    }

    fn finish(&mut self) {
        self.active = false;
        self.list.deactivate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn streamed_digest_equals_hashing_the_serialised_vec() {
        use crate::session::{MessageRole, Session};
        use sha2::{Digest, Sha256};
        let mut session = Session::new("openrouter", "test-model", 128_000, "");
        session.add_message(MessageRole::User, "earlier prompt é雪");
        session.add_message(MessageRole::Assistant, &"answer \"quoted\"\n".repeat(5000));
        session.add_shell_interaction("!printf héllo", "OUT\nline");
        let root = crate::tests::OwnedTestDirectory::new("rewind-digest");
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let binding = RewindBinding::capture(&session, &workspace).unwrap();
        let expected_history: [u8; 32] =
            Sha256::digest(serde_json::to_vec(&session.messages).unwrap()).into();
        let expected_checkpoints: [u8; 32] =
            Sha256::digest(serde_json::to_vec(&session.file_checkpoints).unwrap()).into();
        assert_eq!(binding.history, expected_history);
        assert_eq!(binding.checkpoints, expected_checkpoints);
        assert!(binding.valid(&session, &workspace));
    }

    fn picker() -> RewindPicker {
        let mut p = RewindPicker::new(
            vec![(1, "first".to_string()), (3, "second".to_string())],
            RewindBinding {
                session: "test".into(),
                history: [0; 32],
                checkpoints: [0; 32],
                source: Default::default(),
                workspace: [0; 32],
            },
        );
        p.activate();
        p
    }

    #[test]
    fn enter_then_confirm_yields_the_chosen_message_index() {
        let mut p = picker();
        p.handle(key(KeyCode::Down)); // select second target (message index 3)
        p.handle(key(KeyCode::Enter)); // open confirm step
        assert!(matches!(p.stage, Stage::Confirm));
        p.handle(key(KeyCode::Enter)); // confirm "Rewind and start from here"
        assert!(matches!(
            p.take_outcome(),
            Some(RewindOutcome::Confirmed(3, RewindMode::Conversation, _))
        ));
        assert!(!p.active());
    }

    #[test]
    fn confirm_step_cancel_option_backs_out() {
        let mut p = picker();
        p.handle(key(KeyCode::Enter)); // confirm step for first target
        for _ in 0..3 {
            p.handle(key(KeyCode::Down));
        } // move to "Cancel"
        p.handle(key(KeyCode::Enter));
        assert!(matches!(p.take_outcome(), Some(RewindOutcome::Cancelled)));
        assert!(!p.active());
    }

    #[test]
    fn esc_at_confirm_returns_to_message_list_without_resolving() {
        let mut p = picker();
        p.handle(key(KeyCode::Enter)); // into confirm
        p.handle(key(KeyCode::Esc)); // back to message list
        assert!(matches!(p.stage, Stage::PickMessage));
        assert!(p.take_outcome().is_none());
        assert!(p.active());
    }

    #[test]
    fn esc_at_message_list_cancels() {
        let mut p = picker();
        p.handle(key(KeyCode::Esc));
        assert!(matches!(p.take_outcome(), Some(RewindOutcome::Cancelled)));
        assert!(!p.active());
    }
}
