//! The words the window shows stay free of the machinery's jargon.

/// A `#[cfg(test)] mod tests;` line near the top does not end the scan:
/// the words after it are read, test items are not.
#[test]
fn the_scan_reads_past_a_test_module_declaration() {
    let found = shown_strings(crate::source_scan::SPLIT_FIXTURE);
    let texts: Vec<&str> = found.iter().map(|(_, text)| text.as_str()).collect();
    assert!(texts.contains(&"Open the WebRTC page"), "{texts:?}");
    assert!(texts.contains(&"Turn on the relay"), "{texts:?}");
    assert!(!texts.contains(&"test-only words"), "{texts:?}");
}

/// The string literals of `text` (outside its `#[cfg(test)]` items) a person may read,
/// with the line each starts on. Left out: log and assert lines, and
/// text matched against an error or a check's words.
fn shown_strings(text: &str) -> Vec<(usize, String)> {
    let text = &crate::source_scan::production(text);
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"//") {
            i = bytes[i..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |end| i + end);
            continue;
        }
        if bytes[i] == b'\'' {
            // A char literal holding a quote: '"' or '\"'.
            if bytes[i..].starts_with(b"'\"'") {
                i += 3;
                continue;
            }
            if bytes[i..].starts_with(b"'\\\"'") {
                i += 4;
                continue;
            }
        }
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i;
        let mut literal = String::new();
        i += 1;
        while i < bytes.len() && bytes[i] != b'"' {
            if bytes[i] == b'\\' {
                i += 1;
                match bytes.get(i) {
                    Some(b'\n') => {
                        while bytes.get(i + 1).is_some_and(|b| b.is_ascii_whitespace()) {
                            i += 1;
                        }
                    }
                    Some(b'n') => literal.push(' '),
                    Some(&b) => literal.push(b as char),
                    None => {}
                }
                i += 1;
                continue;
            }
            let c = text[i..].chars().next().unwrap();
            literal.push(c);
            i += c.len_utf8();
        }
        i += 1;
        let line_start = text[..start].rfind('\n').map_or(0, |at| at + 1);
        let line = &text[line_start..start];
        let before = text[..start].trim_end();
        // Error matchers, the jargon list itself and the search words
        // (never shown).
        let matcher = [
            ("has(&[", "]"),
            ("const JARGON", "];"),
            ("const ROWS", "];"),
        ]
        .iter()
        .any(|(open, close)| {
            text[..start]
                .rfind(open)
                .is_some_and(|at| !text[at..start].contains(close))
        });
        let skipped = [
            "eprintln!",
            "println!",
            "debug_log!",
            "log::",
            "panic!",
            "assert",
            ".expect(",
            "unreachable!",
        ]
        .iter()
        .any(|word| line.contains(word))
            || matcher
            || ["contains(", "starts_with(", "ends_with(", "find("]
                .iter()
                .any(|call| before.ends_with(call));
        // A raw error the debug build fakes on purpose.
        let fixture = literal == "the handshake failed: unexpected EOF";
        if !skipped && !fixture && literal.contains(' ') {
            out.push((text[..start].matches('\n').count() + 1, literal));
        }
    }
    out
}

fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            // First-run onboarding has its own owner and pass.
            && path
                .file_name()
                .is_some_and(|name| name != "onboarding.rs")
            && !crate::source_scan::is_test_file(&path)
        {
            out.push(path);
        }
    }
}

/// No window text uses a word only a developer knows (`JARGON`): the
/// System check (platform.rs, package names) and Details keep those.
#[test]
fn the_window_says_nothing_in_jargon() {
    let mut files = Vec::new();
    sources(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/window"),
        &mut files,
    );
    // Shell commands the person copies as they are (Help > Firewall): the
    // port and protocol words are the command. Found when the scan stopped
    // ending at the first `#[cfg(test)]`.
    const COMMANDS: [&str; 2] = ["sudo firewall-cmd ", "sudo ufw "];
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for (line, literal) in shown_strings(&text) {
            if super::has_jargon(&literal)
                && !COMMANDS.iter().any(|command| literal.starts_with(command))
            {
                found.push(format!("{}:{line}: {literal}", file.display()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "jargon in the window:\n{}",
        found.join("\n")
    );
}

#[test]
fn jargon_is_found_as_whole_words_and_capitals() {
    assert!(super::has_jargon("The WebRTC link failed"));
    assert!(super::has_jargon("Needs the D-Bus session bus"));
    assert!(super::has_jargon("Reached over the LAN"));
    assert!(!super::has_jargon("Turn on Bluetooth and Wi-Fi"));
    assert!(!super::has_jargon("Lock the screen when the ice melts"));
    assert!(
        !super::has_jargon("Unpaired peerless phones"),
        "whole words only"
    );
    let found = shown_strings(
        "fn f() {\n    // \"a WebRTC comment\"\n    eprintln!(\"the WebRTC log\");\n    \
             label(\"Shown \\\n         text\");\n    if has(&[\"tls error\", \"ok\"]) {}\n}\n",
    );
    assert_eq!(found, vec![(4, "Shown text".to_string())]);
}
