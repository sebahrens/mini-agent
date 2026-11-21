use super::{MidTurnPressureAction, mid_turn_pressure_action};

#[test]
fn partial_reconciliation_preserves_relief_latch_until_complete_snapshot() {
    let first = mid_turn_pressure_action(false, true, true);
    assert_eq!(first, MidTurnPressureAction::Compact);
    let awaiting_relief = true;

    let partial = mid_turn_pressure_action(awaiting_relief, false, false);
    assert_eq!(partial, MidTurnPressureAction::Ignore);

    let still_over = mid_turn_pressure_action(awaiting_relief, true, true);
    assert_eq!(still_over, MidTurnPressureAction::StopContextExhausted);
}
