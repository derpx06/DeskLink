//! User-facing DeskLink product copy.
//!
//! Keep protocol identifiers in `protocol::legacy_kdeconnect_v8`; they are not
//! public branding.

pub const PRODUCT_NAME: &str = "DeskLink";
#[allow(dead_code)]
pub const PRODUCT_SLUG: &str = "desklink";
pub const PRODUCT_DESCRIPTION: &str = "Connect and control your devices securely.";
pub const LEGACY_PROTOCOL_DISPLAY_NAME: &str = "KDE Connect-compatible protocol v8";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_branding_is_canonical() {
        assert_eq!(PRODUCT_NAME, "DeskLink");
        assert_eq!(PRODUCT_SLUG, "desklink");
        assert_eq!(
            LEGACY_PROTOCOL_DISPLAY_NAME,
            "KDE Connect-compatible protocol v8"
        );
    }
}
