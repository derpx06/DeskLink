//! Links held for the person are counted and named by host.

#[test]
fn held_links_are_counted_and_named_by_host() {
    let links = vec![
        "https://a.example/x?y".to_string(),
        "http://u@b.example:8080/".into(),
    ];
    let (title, body) = super::held_links_words("Pixel", &links);
    assert_eq!(title, "2 more links from Pixel");
    assert!(body.ends_with("b.example:8080, a.example"), "{body}");
    assert_eq!(super::link_host("https:"), "link");
}
