use super::*;
use crate::picker::{
    testing::{drive, BACKSPACE, ENTER},
    Outcome,
};

#[test]
fn expiry_prompt_accepts_duration() {
    let result = drive(expiry_screen(Timestamp::now()), |keys| {
        keys.type_text("1h30m");
        keys.press(ENTER);
    });
    assert_eq!(result, Outcome::Done("1h30m".into()));
}

#[test]
fn expiry_prompt_rejects_invalid_and_short_values() {
    let result = drive(
        expiry_screen(Timestamp::new(1_700_000_000, 500_000_000).unwrap()),
        |keys| {
            keys.type_text("bad");
            keys.press(ENTER);
            keys.wait(|s| s.body.iter().any(|line| line.contains("Use a duration")));
            keys.press(BACKSPACE);
            keys.press(BACKSPACE);
            keys.press(BACKSPACE);
            keys.type_text("10m");
            keys.press(ENTER);
            keys.wait(|s| s.body.iter().any(|line| line.contains("longer than 10m")));
            keys.press(BACKSPACE);
            keys.press(BACKSPACE);
            keys.press(BACKSPACE);
            keys.type_text("30m");
            keys.press(ENTER);
        },
    );
    assert_eq!(result, Outcome::Done("30m".into()));
    let now = Timestamp::new(1_700_000_000, 500_000_000).unwrap();
    for text in ["", "bad", "5m", "10m"] {
        assert!(run_duration(text, now).is_err());
    }
    assert_eq!(run_duration("11m", now).unwrap(), 660);
}
