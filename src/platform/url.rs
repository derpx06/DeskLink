use gtk::gio;

pub fn open_http_url(value: &str) -> Result<(), String> {
    let value = validate_http_url(value)?;
    gio::AppInfo::launch_default_for_uri(value, gio::AppLaunchContext::NONE)
        .map_err(|error| error.to_string())
}

pub fn validate_http_url(value: &str) -> Result<&str, String> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://"))
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value[lower.find("://").unwrap_or(0) + 3..].is_empty()
    {
        return Err("Only non-empty HTTP(S) URLs are allowed".to_string());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::open_http_url;

    #[test]
    fn rejects_non_http_urls() {
        assert!(open_http_url("file:///etc/passwd").is_err());
        assert!(open_http_url("javascript:alert(1)").is_err());
    }
}
