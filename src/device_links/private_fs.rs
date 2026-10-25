//! Files and directories only this user may read: everything DeskLink keeps
//! (the pairing config with the session-password hash, partial downloads, the
//! connection and transfer logs, caches of private pictures, crash reports).
//! They used to be created 0644 in 0755 directories, safe only while the
//! home directory happened to be 0700.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub(crate) const DIR_MODE: u32 = 0o700;
pub(crate) const FILE_MODE: u32 = 0o600;

/// `create_dir_all` with every directory it makes 0700; `path` itself is
/// tightened to 0700 if it already existed wider.
pub(crate) fn create_dir_all(path: &Path) -> std::io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(DIR_MODE))
}

/// Options for a file created 0600 (an existing one keeps its mode: see
/// [`tighten_tree`]).
pub(crate) fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.mode(FILE_MODE);
    options
}

/// A new file at `path` (truncating), 0600 from the first byte.
pub(crate) fn create(path: &Path) -> std::io::Result<File> {
    options().write(true).create(true).truncate(true).open(path)
}

/// `fs::write` at 0600.
pub(crate) fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = create(path)?;
    let _ = file.set_permissions(fs::Permissions::from_mode(FILE_MODE));
    file.write_all(bytes)
}

/// Makes what an older version created 0644/0755 private: every directory
/// under (and including) `root` 0700, every regular file 0600. Links are left
/// alone, never followed. Quiet; idempotent; cheap when nothing is wide.
pub(crate) fn tighten_tree(root: &Path) {
    fn visit(path: &Path, depth: u32) {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        let want = if metadata.is_dir() {
            DIR_MODE
        } else if metadata.is_file() {
            FILE_MODE
        } else {
            return;
        };
        if metadata.permissions().mode() & 0o777 & !want != 0 {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(want));
        }
        if metadata.is_dir() && depth < 8 {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    visit(&entry.path(), depth + 1);
                }
            }
        }
    }
    visit(root, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("desklink-private-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn directories_and_files_are_made_private() {
        let root = scratch("make");
        let nested = root.join("a/b");
        create_dir_all(&nested).unwrap();
        assert_eq!(
            (mode(&root), mode(&root.join("a")), mode(&nested)),
            (0o700, 0o700, 0o700)
        );
        write(&nested.join("f"), b"x").unwrap();
        assert_eq!(mode(&nested.join("f")), 0o600);
        // An existing wide directory is tightened.
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o755)).unwrap();
        create_dir_all(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn older_wide_files_are_tightened_once_and_links_are_left_alone() {
        let root = scratch("tighten");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/config.json"), b"{}").unwrap();
        fs::set_permissions(
            root.join("sub/config.json"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let outside = scratch("outside");
        fs::write(&outside, b"mine").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        tighten_tree(&root);
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("sub")), 0o700);
        assert_eq!(mode(&root.join("sub/config.json")), 0o600);
        assert_eq!(
            mode(&outside),
            0o644,
            "a link's target is not ours to change"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_file(&outside);
    }
}
