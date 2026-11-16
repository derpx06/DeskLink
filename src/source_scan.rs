//! Test-only: the production part of a Rust source, for the tests that scan
//! the sources (words a person reads, who may place a call, pinned call sites).
//!
//! A scan must cover everything but test code. Stopping at the first
//! `#[cfg(test)]` is wrong: a module split into files declares
//! `#[cfg(test)] mod tests;` and keeps production code below it, and a file
//! may have test-only items in the middle. So [`production`] blanks out each
//! `#[cfg(test)]` item and keeps the rest.
//!
//! May depend on: std only.

/// Whether `path` is a test module of its own (`tests.rs`, `*_tests.rs`):
/// the `#[cfg(test)]` is on its `mod` line in the parent, so the whole file
/// is test code.
pub(crate) fn is_test_file(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "tests.rs" || name.ends_with("_tests.rs"))
}

/// The production part of every non-test file of a module directory, as one
/// text, for a pin that must cover the whole module. `dir` is relative to
/// `src/`; `files` pairs each file name with its `include_str!`. Panics when
/// the directory holds a non-test file `files` does not name, so a new file
/// cannot escape the pin.
pub(crate) fn module_sources(dir: &str, files: &[(&str, &str)]) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(dir);
    let mut present: Vec<String> = std::fs::read_dir(&path)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs") && !is_test_file(path))
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    present.sort();
    let mut listed: Vec<String> = files.iter().map(|(name, _)| name.to_string()).collect();
    listed.sort();
    assert_eq!(present, listed, "{dir}: a file is missing from this pin");
    files
        .iter()
        .map(|(_, text)| production(text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text` with every `#[cfg(test)]` item blanked out, each of its lines left
/// empty so line numbers stay: an inline module, a function, a static, a
/// `thread_local!`, a block, or a one-line `mod x;`/`use` declaration (with the
/// item's other attributes and docs). Code after a test item, and
/// `#[cfg(not(test))]` code, is kept.
pub(crate) fn production(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut keep = vec![true; lines.len()];
    let mut at = 0;
    while at < lines.len() {
        if lines[at].trim() != "#[cfg(test)]" {
            at += 1;
            continue;
        }
        let indent = lines[at].len() - lines[at].trim_start().len();
        let mut end = at + 1;
        while end < lines.len() && {
            let line = lines[end].trim_start();
            line.starts_with("#[") || line.starts_with("///")
        } {
            end += 1;
        }
        if let Some(first) = lines.get(end).map(|line| line.trim_end()) {
            let one_line = first.ends_with(';') || (first.contains('{') && first.ends_with('}'));
            if first.ends_with('=') {
                // `static X: T =` continued on the next lines, up to its `;`.
                while end + 1 < lines.len() && !lines[end].trim_end().ends_with(';') {
                    end += 1;
                }
            } else if !one_line {
                // Up to its closing brace at the item's own indentation (rustfmt).
                let mut close = end + 1;
                while close < lines.len() {
                    let line = lines[close];
                    if line.len() > indent
                        && line[..indent].trim().is_empty()
                        && line[indent..].starts_with('}')
                    {
                        break;
                    }
                    close += 1;
                }
                end = close.min(lines.len() - 1);
            }
        }
        let end = end.min(lines.len() - 1);
        for line in keep.iter_mut().take(end + 1).skip(at) {
            *line = false;
        }
        at = end + 1;
    }
    let mut out = String::with_capacity(text.len());
    for (line, kept) in lines.iter().zip(keep) {
        if kept {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// A split module: its test module is declared at the top, production
/// code follows, and test-only items sit in the middle.
pub(crate) const SPLIT_FIXTURE: &str = concat!(
    "use super::*;\n",
    "\n",
    "#[cfg(test)]\n",
    "mod tests;\n",
    "#[cfg(test)]\n",
    "mod more_tests;\n",
    "\n",
    "const PAGE: &str = \"Open the WebRTC page\";\n",
    "\n",
    "#[cfg(test)]\n",
    "/// Tests: a fake.\n",
    "pub(crate) fn fake() -> &'static str {\n",
    "    \"test-only words\"\n",
    "}\n",
    "\n",
    "#[cfg(test)]\n",
    "static FAKE: std::sync::Mutex<Vec<String>> =\n",
    "    std::sync::Mutex::new(Vec::new());\n",
    "\n",
    "#[cfg(test)]\n",
    "thread_local! {\n",
    "    static LOCAL: u8 = const { 0 };\n",
    "}\n",
    "\n",
    "fn dial() {\n",
    "    #[cfg(test)]\n",
    "    {\n",
    "        let _ = \"inside a test block\";\n",
    "    }\n",
    "    send(\"dial\", Telephony(1));\n",
    "    #[cfg(not(test))]\n",
    "    real();\n",
    "}\n",
    "\n",
    "#[cfg(test)]\n",
    "mod inline_tests {\n",
    "    fn t() {\n",
    "        let _ = \"inline test\";\n",
    "    }\n",
    "}\n",
    "\n",
    "const AFTER: &str = \"Turn on the relay\";\n",
);

#[cfg(test)]
mod tests {
    use super::{production, SPLIT_FIXTURE as SPLIT};

    #[test]
    fn test_files_are_known_by_name() {
        use std::path::Path;
        assert!(super::is_test_file(Path::new("src/window/tests.rs")));
        assert!(super::is_test_file(Path::new("src/window/jargon_tests.rs")));
        assert!(!super::is_test_file(Path::new("src/window/mod.rs")));
        assert!(!super::is_test_file(Path::new(
            "src/device_links/tests_helper.rs"
        )));
    }

    #[test]
    fn a_test_module_declared_at_the_top_does_not_end_the_scan() {
        let kept = production(SPLIT);
        assert!(kept.contains("Open the WebRTC page"), "{kept}");
        assert!(kept.contains("send(\"dial\", Telephony(1));"), "{kept}");
        assert!(kept.contains("real();"), "cfg(not(test)) code is kept");
        assert!(
            kept.contains("Turn on the relay"),
            "code after an inline test module"
        );
        for gone in [
            "mod tests;",
            "mod more_tests;",
            "test-only words",
            "Tests: a fake",
            "FAKE",
            "Mutex::new(Vec::new())",
            "LOCAL",
            "inside a test block",
            "inline test",
        ] {
            assert!(!kept.contains(gone), "{gone} should be left out:\n{kept}");
        }
        assert_eq!(
            kept.lines().count(),
            SPLIT.lines().count(),
            "line numbers stay"
        );
        let page = SPLIT.lines().position(|line| line.contains("WebRTC page"));
        assert_eq!(
            kept.lines().position(|line| line.contains("WebRTC page")),
            page
        );
    }
}
