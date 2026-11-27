//! `admin.key`: the whole key or nothing, never over another (D60, SRV-7).

use std::ffi::OsStr;
use std::fs::{File, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use ironflowx_core::Secret;

use crate::logging::public_text;

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("admin.key")
}

/// Whether the name is taken, as by a symbolic link to nothing.
pub fn exists(data_dir: &Path) -> bool {
    std::fs::symlink_metadata(path(data_dir)).is_ok()
}

/// Writes `key` and a newline to `admin.key`, which must not exist. A crash leaves the whole key
/// there or no `admin.key`, and at most a temporary file beside it.
pub fn write(data_dir: &Path, key: &Secret) -> io::Result<()> {
    let mut temporary = tempfile::Builder::new()
        .prefix("admin.key.")
        .suffix(".tmp")
        .permissions(Permissions::from_mode(0o400))
        .tempfile_in(data_dir)
        .map_err(tempfile_error)?;
    // The process umask may have taken the owner's bit too.
    temporary
        .as_file()
        .set_permissions(Permissions::from_mode(0o400))?;
    let file = temporary.as_file_mut();
    file.write_all(key.expose().as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    // Not a rename, which would replace an `admin.key`.
    #[cfg(feature = "__fault-points")]
    ironflowx_core::reach_fault_point(ironflowx_core::FaultPoint::BeforeAdminKeyLink);
    std::fs::hard_link(temporary.path(), path(data_dir))?;
    #[cfg(feature = "__fault-points")]
    ironflowx_core::reach_fault_point(ironflowx_core::FaultPoint::AfterAdminKeyLink);
    close_temporary(temporary)?;
    File::open(data_dir)?.sync_all()
}

fn close_temporary(temporary: tempfile::NamedTempFile) -> io::Result<()> {
    temporary.close().map_err(tempfile_error)
}

// Tempfile wraps filesystem errors with its own rendering of the path. The caller already
// supplies a safe locator. Keep the error kind without including the wrapped path.
fn tempfile_error(error: io::Error) -> io::Error {
    error.kind().into()
}

const RELATIVE: &str = "configured data directory";

/// Exact text safe in both the JSON and text diagnostics.
fn diagnostic_path(path: &OsStr) -> Option<&str> {
    let text = path.to_str()?;
    let printable = text.bytes().all(|b| b.is_ascii_graphic() || b == b' ');
    (printable && !text.contains(['\\', '"', '\'']) && public_text(text) == text).then_some(text)
}

pub fn diagnostic_directory(data_dir: &Path) -> &str {
    if data_dir.is_absolute() {
        diagnostic_path(data_dir.as_os_str()).unwrap_or("the configured data directory")
    } else {
        "the configured data directory"
    }
}

fn locator(data_dir: &Path, name: &OsStr) -> Option<(String, Option<&'static str>)> {
    let filename = diagnostic_path(name)?;
    if data_dir.is_absolute() {
        let path = data_dir.join(name);
        if let Some(absolute) = diagnostic_path(path.as_os_str()) {
            return Some((absolute.to_owned(), None));
        }
    }
    Some((filename.to_owned(), Some(RELATIVE)))
}

/// Warns about names left by another writer or a crash. They may hold the only copy of a key.
pub fn warn_temporaries(data_dir: &Path) {
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                path = %public_text(&data_dir.to_string_lossy()),
                error = %public_text(&e.to_string()),
                "cannot look for temporary admin keys"
            );
            return;
        }
    };
    for entry in entries {
        match entry {
            Ok(entry) => {
                let name = entry.file_name();
                if name
                    .as_encoded_bytes()
                    .strip_prefix(b"admin.key.")
                    .is_some_and(|rest| rest.ends_with(b".tmp"))
                {
                    if let Some((path, base)) = locator(data_dir, &name) {
                        tracing::warn!(path, path_base = base, "temporary admin key is kept");
                    } else {
                        tracing::warn!(
                            "temporary admin key is kept with its name withheld; inspect \
                             admin.key.*.tmp in the configured data directory locally"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %public_text(&e.to_string()), "cannot read a directory entry")
            }
        }
    }
}

/// Exercises the write, sync, link and unlink in the same directory as the real key file.
pub fn probe(data_dir: &Path) -> io::Result<()> {
    probe_with_link(data_dir, |source, link| std::fs::hard_link(source, link))
}

fn probe_with_link(
    data_dir: &Path,
    link_file: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut temporary = tempfile::Builder::new()
        .prefix(".admin-key-probe.")
        .permissions(Permissions::from_mode(0o400))
        .tempfile_in(data_dir)
        .map_err(tempfile_error)?;
    let mut link = temporary.path().as_os_str().to_owned();
    link.push(".link");
    let link = PathBuf::from(link);

    let test = (|| {
        temporary
            .as_file()
            .set_permissions(Permissions::from_mode(0o400))?;
        // The final file holds an 84-byte key and a newline.
        temporary.as_file_mut().write_all(&[0; 85])?;
        temporary.as_file().sync_all()?;
        link_file(temporary.path(), &link)
    })();

    // Only a successful link gives us a second name to remove. Always try the first one too.
    let link_failure = test.is_ok().then(|| failed_removal(&link)).flatten();
    let source_failure = failed_removal(temporary.path());
    // Both unlinks are explicit so errors carry just the OS cause, without tempfile's path.
    temporary.disable_cleanup(true);
    let cleanup: Vec<_> = [link_failure, source_failure]
        .into_iter()
        .flatten()
        .map(|(name, cause)| {
            let (path, base) = locator(data_dir, &name).expect("safe generated probe name");
            let cause = public_text(&cause.to_string());
            match base {
                Some(base) => format!("cannot remove {path} (relative to the {base}): {cause}"),
                None => format!("cannot remove {path}: {cause}"),
            }
        })
        .collect();
    if !cleanup.is_empty() {
        let cleanup = cleanup.join("; ");
        return Err(match test {
            Ok(()) => io::Error::other(cleanup),
            Err(e) => io::Error::other(format!(
                "{}; cleanup: {cleanup}",
                public_text(&e.to_string())
            )),
        });
    }
    test?;
    File::open(data_dir)?.sync_all()
}

fn failed_removal(path: &Path) -> Option<(std::ffi::OsString, io::Error)> {
    std::fs::remove_file(path)
        .err()
        .map(|cause| (path.file_name().expect("generated name").to_owned(), cause))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_creation_error_omits_the_private_tempfile_path() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("quoted\"root");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, Permissions::from_mode(0o500)).unwrap();
        let (_, key) = ironflowx_operations::generate_seed();
        let result = write(&dir, &key);
        std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "permission denied");
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
    }

    #[test]
    fn writer_close_error_omits_the_private_tempfile_path() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("quoted\"root");
        std::fs::create_dir(&dir).unwrap();
        let temporary = tempfile::NamedTempFile::new_in(&dir).unwrap();
        std::fs::remove_file(temporary.path()).unwrap();
        let error = close_temporary(temporary).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(error.to_string(), "entity not found");
    }

    #[test]
    fn the_probe_keeps_an_entry_it_could_not_link_over() {
        let dir = tempfile::tempdir().unwrap();
        let mut collision = PathBuf::new();
        let error = probe_with_link(dir.path(), |source, link| {
            collision = link.to_owned();
            std::fs::write(link, b"another writer")?;
            std::fs::hard_link(source, link)
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&collision).unwrap(), b"another writer");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [collision.file_name().unwrap()]);
    }

    #[test]
    fn the_probe_reports_both_paths_if_the_os_refuses_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        for root in [
            temp.path().join("safe"),
            temp.path().join("a".repeat(80)),
            temp.path().join("A".repeat(43)),
        ] {
            std::fs::create_dir(&root).unwrap();
            let mut paths = None;
            let result = probe_with_link(&root, |source, link| {
                std::fs::hard_link(source, link)?;
                paths = Some([source.to_owned(), link.to_owned()]);
                std::fs::set_permissions(&root, Permissions::from_mode(0o500))
            });
            std::fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();

            let refused = crate::ServeError::Probe(root.clone(), result.unwrap_err());
            let envelope = refused.envelope();
            assert_eq!(refused.exit_code(), 1);
            assert_eq!(envelope.kind, ironflowx_core::ErrorKind::Internal);
            let error = envelope.message;
            for path in paths.unwrap() {
                let removal = if root.ends_with("safe") {
                    format!("cannot remove {}:", path.display())
                } else {
                    format!(
                        "cannot remove {} (relative to the configured data directory):",
                        path.file_name().unwrap().to_str().unwrap()
                    )
                };
                assert!(error.contains(&removal), "{error}");
                assert_eq!(std::fs::read(path).unwrap(), [0; 85]);
            }
            assert!(!root.join("control.db").exists());
            assert!(!path(&root).exists());
        }
    }

    #[test]
    fn a_locator_uses_the_configured_root_and_withholds_ambiguous_names() {
        use std::os::unix::ffi::OsStrExt as _;

        let name = OsStr::new("admin.key.leftover.tmp");
        let expected = Some((name.to_str().unwrap().into(), Some(RELATIVE)));
        for root in [
            OsStr::new("./data"),
            OsStr::new("/line\nbreak"),
            OsStr::new("/literal\\n"),
            OsStr::from_bytes(b"/byte\xff"),
        ] {
            assert_eq!(locator(Path::new(root), name), expected);
        }
        for withheld in [
            OsStr::new("admin.key.ixk_opaque.tmp"),
            OsStr::new("admin.key.line\nbreak.tmp"),
            OsStr::new("admin.key.literal\\n.tmp"),
            OsStr::from_bytes(b"admin.key.\xff.tmp"),
        ] {
            assert_eq!(locator(Path::new("/data"), withheld), None);
        }
    }
}
