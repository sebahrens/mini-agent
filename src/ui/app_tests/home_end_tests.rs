use super::*;

#[test]
fn home_end_edit_a_non_empty_prompt_and_scroll_otherwise() {
    let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
    let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
    let ctrl_home = KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL);
    let ctrl_end = KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL);

    // Non-empty prompt, no picker: the editor gets the key.
    assert_eq!(transcript_scroll_key(home, false, false), None);
    assert_eq!(transcript_scroll_key(end, false, false), None);
    // Empty prompt: plain Home/End scroll the transcript.
    assert_eq!(
        transcript_scroll_key(home, true, false),
        Some(TranscriptScroll::Top)
    );
    assert_eq!(
        transcript_scroll_key(end, true, false),
        Some(TranscriptScroll::Bottom)
    );
    // An open picker keeps its query in sync by not moving the caret.
    assert_eq!(
        transcript_scroll_key(end, false, true),
        Some(TranscriptScroll::Bottom)
    );
    // Ctrl+Home/Ctrl+End always scroll.
    assert_eq!(
        transcript_scroll_key(ctrl_home, false, false),
        Some(TranscriptScroll::Top)
    );
    assert_eq!(
        transcript_scroll_key(ctrl_end, false, false),
        Some(TranscriptScroll::Bottom)
    );
    // Other keys are never transcript scrolls here.
    let page_up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(transcript_scroll_key(page_up, true, false), None);
}
