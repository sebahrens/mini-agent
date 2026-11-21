use super::{SelectionPoint, mouse_up_copies};

#[test]
fn plain_click_does_not_copy_but_a_drag_does() {
    let at = |line, col| Some(SelectionPoint::new(line, col));
    assert!(!mouse_up_copies(false, at(4, 3), at(4, 3)));
    assert!(mouse_up_copies(true, at(4, 3), at(4, 3)));
    assert!(mouse_up_copies(false, at(4, 3), at(6, 0)));
    assert!(mouse_up_copies(false, at(4, 3), at(4, 9)));
    assert!(mouse_up_copies(true, at(6, 0), at(4, 3)));
    assert!(!mouse_up_copies::<SelectionPoint>(true, None, None));
}
