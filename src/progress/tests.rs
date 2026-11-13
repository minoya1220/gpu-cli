use super::*;

#[test]
fn elapsed_time_keeps_counting_past_an_hour() {
    assert_eq!(line("Waiting", 0), "\r\x1b[2KWaiting... 0:00");
    assert_eq!(line("Waiting", 62), "\r\x1b[2KWaiting... 1:02");
    assert_eq!(line("Waiting", 3601), "\r\x1b[2KWaiting... 60:01");
}
