use super::*;

/// cttyn.6: an N-line submission echo is one chat paint, and it is on
/// screen when the call returns (before the caller awaits the turn).
#[test]
fn an_n_line_echo_paints_the_chat_once_and_immediately() {
    let mut renderer = Renderer::for_app_test().unwrap();
    renderer.render_viewport().unwrap();
    let text = (0..50)
        .map(|line| format!("prompt line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let before = renderer.frame_stats();
    echo_submission(&mut renderer, &text).unwrap();
    let paint = renderer.frame_stats().since(before);
    assert_eq!(paint.viewport_paints, 1, "{paint:?}");
    assert_eq!(paint.flushes, 1, "{paint:?}");
    assert!(!renderer.chat_needs_redraw());
    assert!(renderer.captured_output().contains("> prompt line 49"));
    assert_eq!(renderer.feed().block_count(), 51);
}
