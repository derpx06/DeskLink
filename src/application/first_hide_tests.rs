//! The background note is shown on the first close only.

#[test]
fn the_background_note_is_for_the_first_close_only() {
    let dir = std::env::temp_dir().join(format!("desklink-hide-{}", uuid::Uuid::new_v4()));
    assert!(super::take_first_hide(&dir));
    assert!(!super::take_first_hide(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}
