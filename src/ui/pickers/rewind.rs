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
    /// Restore only the files whose current bytes still match their recorded
    /// postimage; drifted or uncaptured files are left and named.
    FilesUnchanged,
    BothUnchanged,
    /// Restore every non-ignored workspace file to the snapshot taken before
    /// the range's first shell command, undoing shell changes too.
    Workspace,
}
impl RewindMode {
    pub(crate) fn restores_files(self) -> bool {
        !matches!(self, Self::Conversation | Self::Workspace)
    }
    pub(crate) fn is_workspace(self) -> bool {
        self == Self::Workspace
    }
    pub(crate) fn unchanged_only(self) -> bool {
        matches!(self, Self::FilesUnchanged | Self::BothUnchanged)
    }
    pub(crate) fn rewinds_conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::Both | Self::BothUnchanged)
    }
}
#[derive(Clone)]
pub(crate) struct RewindBinding {
    session: String,
    history: [u8; 32],
    checkpoints: [u8; 32],
    source: crate::session::file_checkpoints::FileCheckpoints,
    workspace: [u8; 32],
    /// The bound workspace root, for the workspace-snapshot preview.
    root: std::path::PathBuf,
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
            root: workspace.root().to_path_buf(),
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
const CONFIRM_FILES_UNCHANGED: &str = "Restore unchanged files only";
const CONFIRM_BOTH_UNCHANGED: &str = "Restore unchanged files and conversation";
const CONFIRM_WORKSPACE: &str = "Restore workspace (undoes shell changes too)";
const CONFIRM_CANCEL: &str = "Cancel";

/// A two-level modal picker for `/rewind`: first pick an earlier
/// user turn, then confirm the (destructive) rewind. Reuses [`ListPicker`] for
/// rendering and selection at each level; the outcome is read back by the event
/// loop, which owns the session and performs the actual rewind.
pub struct RewindPicker {
    active: bool,
    binding: Box<RewindBinding>,
    stage: Stage,
    list: ListPicker,
    /// Message index of each rewind-able user turn, in order. The list shows
    /// the previews, so `list.selected` indexes straight into this.
    targets: Vec<usize>,
    /// Every turn's preview; the list shows the newest matches of `query`.
    previews: Vec<String>,
    /// Indexes into `targets`/`previews` of the rows the list shows, oldest
    /// first, so `list.selected` indexes straight into this.
    visible: Vec<usize>,
    /// Typed filter (case-insensitive substring of the preview).
    query: String,
    chosen: Option<usize>,
    outcome: Option<RewindOutcome>,
}

impl RewindPicker {
    pub(crate) fn new(targets: Vec<(usize, String)>, binding: RewindBinding) -> Self {
        let list = ListPicker::new();
        let (targets, previews): (Vec<usize>, Vec<String>) = targets.into_iter().unzip();
        RewindPicker {
            active: false,
            binding: Box::new(binding),
            stage: Stage::PickMessage,
            list,
            targets,
            previews,
            visible: Vec::new(),
            query: String::new(),
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
        self.query.clear();
        self.refresh_list();
    }

    /// Show the newest `ROWS` turns matching the typed filter, with the
    /// newest highlighted: rewinding is almost always "undo the last turn".
    fn refresh_list(&mut self) {
        const ROWS: usize = 50;
        let query = self.query.to_lowercase();
        let matching: Vec<usize> = (0..self.previews.len())
            .filter(|&index| {
                query.is_empty() || self.previews[index].to_lowercase().contains(&query)
            })
            .collect();
        self.visible = matching[matching.len().saturating_sub(ROWS)..].to_vec();
        self.list.set_items(
            self.visible
                .iter()
                .map(|&index| self.previews[index].clone())
                .collect(),
        );
        self.list.activate();
        self.list.selected = self.visible.len().saturating_sub(1);
    }

    fn move_selection(&mut self, to: impl FnOnce(usize, usize) -> usize) {
        let len = self.list.len();
        if len > 0 {
            self.list.selected = to(self.list.selected, len - 1).min(len - 1);
        }
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
        let empty = matches!(self.stage, Stage::PickMessage).then(|| {
            format!(
                "no turn matches '{}' (Backspace edits the filter)",
                self.query
            )
        });
        self.list.draw(out, cols, empty.as_deref(), floor_row)
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
            KeyCode::PageUp => self.move_selection(|at, _| at.saturating_sub(10)),
            KeyCode::PageDown => self.move_selection(|at, _| at + 10),
            KeyCode::Home => self.move_selection(|_, _| 0),
            KeyCode::End => self.move_selection(|_, last| last),
            KeyCode::Char(c)
                if matches!(self.stage, Stage::PickMessage)
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
                self.refresh_list();
            }
            KeyCode::Backspace if matches!(self.stage, Stage::PickMessage) => {
                self.query.pop();
                self.refresh_list();
            }
            KeyCode::Enter => self.confirm(),
            KeyCode::Esc => self.back_or_cancel(),
            _ => {}
        }
        true
    }

    fn confirm(&mut self) {
        match self.stage {
            Stage::PickMessage => {
                if let Some(&idx) = self
                    .visible
                    .get(self.list.selected)
                    .and_then(|&row| self.targets.get(row))
                {
                    self.chosen = Some(idx);
                    self.stage = Stage::Confirm;
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
                        format!("{CONFIRM_FILES_UNCHANGED} (leaves files changed since the edit, names them)"),
                        format!("{CONFIRM_BOTH_UNCHANGED} (leaves files changed since the edit, names them)"),
                        format!(
                            "{CONFIRM_WORKSPACE} — {}",
                            self.binding.source.workspace_summary(
                                &self.binding.session,
                                idx,
                                &self.binding.root
                            )
                        ),
                        CONFIRM_CANCEL.to_string(),
                    ]);
                    self.list.activate();
                }
            }
            Stage::Confirm => {
                self.outcome = Some(if self.list.selected < 6 {
                    RewindOutcome::Confirmed(
                        self.chosen.unwrap_or(0),
                        match self.list.selected {
                            1 => RewindMode::Files,
                            2 => RewindMode::Both,
                            3 => RewindMode::FilesUnchanged,
                            4 => RewindMode::BothUnchanged,
                            5 => RewindMode::Workspace,
                            _ => RewindMode::Conversation,
                        },
                        (*self.binding).clone(),
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
                self.refresh_list();
                if let Some(pos) = self.chosen.and_then(|idx| {
                    self.visible
                        .iter()
                        .position(|&row| self.targets.get(row) == Some(&idx))
                }) {
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
                root: std::path::PathBuf::new(),
            },
        );
        p.activate();
        p
    }

    #[test]
    fn enter_then_confirm_yields_the_chosen_message_index() {
        let mut p = picker();
        // The newest turn (message index 3) starts highlighted.
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
        p.handle(key(KeyCode::Enter)); // confirm step for the newest target
        for _ in 0..6 {
            p.handle(key(KeyCode::Down));
        } // move to "Cancel"
        p.handle(key(KeyCode::Enter));
        assert!(matches!(p.take_outcome(), Some(RewindOutcome::Cancelled)));
        assert!(!p.active());
    }

    #[test]
    fn confirm_step_offers_unchanged_only_restore_modes() {
        let mut p = picker();
        p.handle(key(KeyCode::Enter));
        for _ in 0..3 {
            p.handle(key(KeyCode::Down));
        }
        p.handle(key(KeyCode::Enter));
        let Some(RewindOutcome::Confirmed(3, mode, _)) = p.take_outcome() else {
            panic!("expected a confirmed rewind");
        };
        assert_eq!(mode, RewindMode::FilesUnchanged);
        assert!(mode.unchanged_only() && mode.restores_files() && !mode.rewinds_conversation());
        assert!(RewindMode::BothUnchanged.rewinds_conversation());
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

    #[test]
    fn opens_on_the_newest_turn_and_filters_and_pages() {
        let mut p = picker();
        assert_eq!(p.list.selected, 1, "newest turn is highlighted");
        p.handle(key(KeyCode::Home));
        assert_eq!(p.list.selected, 0);
        p.handle(key(KeyCode::End));
        assert_eq!(p.list.selected, 1);
        for c in "fir".chars() {
            p.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(p.list.len(), 1);
        p.handle(key(KeyCode::Enter));
        p.handle(key(KeyCode::Enter));
        assert!(matches!(
            p.take_outcome(),
            Some(RewindOutcome::Confirmed(1, RewindMode::Conversation, _))
        ));
    }

    #[test]
    fn a_long_session_keeps_its_newest_turns_reachable() {
        let targets: Vec<(usize, String)> =
            (0..120).map(|n| (n * 2, format!("turn {n}"))).collect();
        let mut p = RewindPicker::new(
            targets,
            RewindBinding {
                session: "test".into(),
                history: [0; 32],
                checkpoints: [0; 32],
                source: Default::default(),
                workspace: [0; 32],
                root: std::path::PathBuf::new(),
            },
        );
        p.activate();
        assert_eq!(p.list.selected_name(), Some("turn 119"));
        p.handle(key(KeyCode::PageUp));
        assert_eq!(p.list.selected_name(), Some("turn 109"));
    }
}
