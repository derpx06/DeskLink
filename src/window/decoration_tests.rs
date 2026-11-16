//! Which header-bar decoration layouts crowd the start of the bar.

#[test]
fn only_a_layout_with_something_at_the_start_crowds_the_sidebar_title() {
    assert!(
        super::crowded_start("icon,menu:minimize,maximize,close"),
        "Xfce"
    );
    assert!(
        super::crowded_start("close,minimize,maximize:"),
        "buttons on the left"
    );
    assert!(!super::crowded_start(":minimize,maximize,close"), "GNOME");
    assert!(
        !super::crowded_start("menu:minimize,maximize,close"),
        "Cinnamon, MATE"
    );
    assert!(
        !super::crowded_start("appmenu:close"),
        "GTK 3's appmenu shows nothing in GTK 4"
    );
    assert!(!super::crowded_start(""));
}
