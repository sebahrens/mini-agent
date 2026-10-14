//! Per-thread synchronous picker drawing scope, owned by a captured Renderer.
use std::cell::RefCell;
use std::io::{self, Write};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Surface {
    bytes: Arc<Mutex<Vec<u8>>>,
    size: (u16, u16),
}
#[derive(Default)]
struct State {
    surface: Option<Surface>,
    fault: bool,
    native_only: bool,
    queries: usize,
    writes: usize,
}
thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }

pub(crate) fn capture<T>(
    bytes: Arc<Mutex<Vec<u8>>>,
    size: (u16, u16),
    draw: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Surface>);
    impl Drop for Restore {
        fn drop(&mut self) {
            STATE.with(|state| state.borrow_mut().surface = self.0.take());
        }
    }
    let previous = STATE.with(|state| state.borrow_mut().surface.replace(Surface { bytes, size }));
    let _restore = Restore(previous);
    draw()
}

pub(crate) struct NativeFaultGuard {
    previous: State,
    _same_thread: PhantomData<Rc<()>>,
}
impl NativeFaultGuard {
    pub(crate) fn new(native_only: bool) -> Self {
        let previous = STATE.with(|state| {
            std::mem::replace(
                &mut *state.borrow_mut(),
                State {
                    fault: true,
                    native_only,
                    ..Default::default()
                },
            )
        });
        Self {
            previous,
            _same_thread: PhantomData,
        }
    }
    pub(crate) fn counts(&self) -> (usize, usize) {
        STATE.with(|state| {
            let state = state.borrow();
            (state.queries, state.writes)
        })
    }
}
impl Drop for NativeFaultGuard {
    fn drop(&mut self) {
        STATE.with(|state| *state.borrow_mut() = std::mem::take(&mut self.previous));
    }
}
pub(crate) fn native_only() -> bool {
    STATE.with(|state| state.borrow().native_only)
}

pub(super) fn size() -> io::Result<(u16, u16)> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if let Some(surface) = &state.surface {
            return Ok(surface.size);
        }
        state.queries += 1;
        // The fault controls only the writer: native geometry succeeds, so a
        // propagated WouldBlock identifies the actual uncaptured write path.
        if state.fault {
            return Ok((180, 50));
        }
        crossterm::terminal::size()
    })
}

// Crossterm may otherwise choose WinAPI even with an owned writer. A captured
// command serializes only ANSI; every uncaptured command delegates unchanged.
pub(super) struct CapturedCommand<C> {
    command: C,
    captured: bool,
}
pub(super) fn command<C>(command: C) -> CapturedCommand<C> {
    CapturedCommand {
        command,
        captured: STATE.with(|state| state.borrow().surface.is_some()),
    }
}
impl<C: crossterm::Command + std::fmt::Display> std::fmt::Display for CapturedCommand<C> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.captured {
            self.command.write_ansi(formatter)
        } else {
            self.command.fmt(formatter)
        }
    }
}
impl<C: crossterm::Command> crossterm::Command for CapturedCommand<C> {
    fn write_ansi(&self, formatter: &mut impl std::fmt::Write) -> std::fmt::Result {
        self.command.write_ansi(formatter)
    }
    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        self.captured || self.command.is_ansi_code_supported()
    }
    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        if self.captured {
            return Err(io::Error::other("captured picker cannot execute WinAPI"));
        }
        self.command.execute_winapi()
    }
}

pub(super) enum PickerWriter {
    Capture(Arc<Mutex<Vec<u8>>>),
    Native(io::Stdout),
    Fault,
}
pub(super) fn writer() -> PickerWriter {
    STATE.with(|state| {
        let state = state.borrow();
        if let Some(surface) = &state.surface {
            return PickerWriter::Capture(surface.bytes.clone());
        }
        if state.fault {
            return PickerWriter::Fault;
        }
        PickerWriter::Native(io::stdout())
    })
}
fn fault() -> io::Error {
    STATE.with(|state| state.borrow_mut().writes += 1);
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "controlled native picker writer would block",
    )
}
impl Write for PickerWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Capture(target) => {
                target
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }
            Self::Native(writer) => writer.write(bytes),
            Self::Fault => Err(fault()),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Capture(_) => Ok(()),
            Self::Native(writer) => writer.flush(),
            Self::Fault => Err(fault()),
        }
    }
}

#[test]
fn captured_picker_messages_lists_and_unwind_restore_the_native_fault() {
    let fault = NativeFaultGuard::new(false);
    let bytes = Arc::new(Mutex::new(Vec::new()));
    struct NoNativeCommand;
    impl std::fmt::Display for NoNativeCommand {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("captured command must not use native Display");
        }
    }
    impl crossterm::Command for NoNativeCommand {
        fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
            f.write_str("owned-command")
        }
        #[cfg(windows)]
        fn is_ansi_code_supported(&self) -> bool {
            panic!("captured command must not query native console support");
        }
        #[cfg(windows)]
        fn execute_winapi(&self) -> io::Result<()> {
            panic!("captured command must not call WinAPI");
        }
    }
    capture(bytes.clone(), (30, 12), || {
        let captured = command(NoNativeCommand);
        assert_eq!(captured.to_string(), "owned-command");
        #[cfg(windows)]
        assert!(crossterm::Command::is_ansi_code_supported(&captured));
        let mut writer = writer();
        crossterm::ExecutableCommand::execute(&mut writer, captured).unwrap();
        super::draw_picker_message("scanning files...", true, 10).unwrap();
        super::draw_picker_list(&[], 0, true, Some("no matches"), 10, 0).unwrap();
        super::draw_picker_list(&["owned 雪".into()], 0, true, None, 10, 0).unwrap();
    });
    assert_eq!(fault.counts(), (0, 0));
    let rendered = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(
        rendered.contains("scanning files...")
            && rendered.contains("no matches")
            && rendered.contains("owned 雪")
    );
    assert!(
        std::panic::catch_unwind(|| capture(bytes, (30, 12), || panic!(
            "controlled picker paint failure"
        )))
        .is_err()
    );
    let error = super::draw_picker_message("uncaptured", true, 10).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(
        error.to_string(),
        "controlled native picker writer would block"
    );
    assert_eq!(fault.counts(), (0, 1));
}
