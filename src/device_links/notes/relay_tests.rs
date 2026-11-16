//! A change from one phone goes to every other ready phone.

#[test]
fn a_change_from_one_phone_goes_to_every_other_ready_phone() {
    let ready = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    assert_eq!(super::relay_targets(&ready, "a"), ["b", "c"]);
    assert!(super::relay_targets(&["a".to_string()], "a").is_empty());
}
