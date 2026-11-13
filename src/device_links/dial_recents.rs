//! Numbers dialled from DeskLink itself (not the phone's call log, which
//! DeskLink never reads): the last few, newest first, kept on this computer
//! and clearable.

use std::path::{Path, PathBuf};

const KEEP: usize = 8;

/// `numbers` with `number` first and any earlier copy (same digits) gone.
pub(crate) fn pushed(mut numbers: Vec<String>, number: &str) -> Vec<String> {
    let digits = |text: &str| {
        text.chars()
            .filter(char::is_ascii_digit)
            .collect::<String>()
    };
    let key = digits(number);
    numbers.retain(|old| digits(old) != key);
    numbers.insert(0, number.to_string());
    numbers.truncate(KEEP);
    numbers
}

fn path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("desklink")
        .join("dial_recents.json")
}

fn load_from(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .map(|numbers| {
            numbers
                .into_iter()
                .filter(|n| crate::device_links::calls::dial_number(n).is_some())
                .take(KEEP)
                .collect()
        })
        .unwrap_or_default()
}

fn save_to(path: &Path, numbers: &[String]) {
    if numbers.is_empty() {
        let _ = std::fs::remove_file(path);
    } else if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        let _ = serde_json::to_string(numbers).map(|text| std::fs::write(path, text));
    }
}

pub(crate) fn load() -> Vec<String> {
    load_from(&path())
}

/// Remembers a number the person asked the phone to dial.
pub(crate) fn add(number: &str) {
    save_to(&path(), &pushed(load(), number));
}

pub(crate) fn clear() {
    save_to(&path(), &[]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_without_repeats_and_bounded() {
        let mut list = Vec::new();
        for n in ["111", "222", "1 1 1"] {
            list = pushed(list, n);
        }
        assert_eq!(list, ["1 1 1", "222"]);
        for i in 0..20 {
            list = pushed(list, &format!("55500{i}"));
        }
        assert_eq!(list.len(), KEEP);
        assert_eq!(list[0], "5550019");
    }

    #[test]
    fn it_survives_a_round_trip_and_clearing() {
        let dir = std::env::temp_dir().join(format!("dl-recents-{}", std::process::id()));
        let file = dir.join("r.json");
        save_to(&file, &["+15551234567".to_string(), "bogus*#".to_string()]);
        assert_eq!(load_from(&file), ["+15551234567"]);
        save_to(&file, &[]);
        assert!(load_from(&file).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
