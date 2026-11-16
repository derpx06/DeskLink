//! Turning on a Bucket answers only an ask that was made.

use crate::device_links::folders::Offer;

#[test]
fn turning_on_a_bucket_answers_only_an_ask_that_was_made() {
    let offers = vec![Offer {
        device: "phone-a".into(),
        id: "bucket".into(),
        name: "Bucket".into(),
        bucket: true,
    }];
    assert!(super::bucket_ask_pending(&offers, "phone-a"));
    assert!(!super::bucket_ask_pending(&offers, "phone-b"));
    assert!(!super::bucket_ask_pending(&[], "phone-a"));
}
