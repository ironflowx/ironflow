//! Local key delivery for the hand-written recovery and seed commands.

use std::fs;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::Path;

use ironflowx_core::{ErrorKind, KeyId, Secret};
use ironflowx_operations::{HostClock, RecoverError, key_id, recover_admin_key};
use ironflowx_proto::OpError;

use crate::{key_file, logging::public_text};

/// Recovers a new admin key and writes it to `admin.key`, or prints it when `stdout` is true.
///
/// # Errors
///
/// Refuses a missing data directory, an unfinished bootstrap, an occupied file destination
/// and any failed file preflight. Reports a control-plane or key delivery failure.
pub fn recover_key(data_dir: &Path, stdout: bool) -> Result<(), OpError> {
    let directory = key_file::diagnostic_directory(data_dir);
    match fs::metadata(data_dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(error(
                ErrorKind::Invalid,
                "the data path is not a directory",
            ));
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Err(error(
                ErrorKind::Invalid,
                "the data directory does not exist",
            ));
        }
        Err(e) => {
            return Err(error(
                ErrorKind::Unavailable,
                format!("cannot inspect {directory}: {e}"),
            ));
        }
    }
    if !stdout {
        preflight(data_dir)?;
    }
    let key = recover_admin_key(data_dir, HostClock).map_err(recovery_error)?;
    let id = key_id(&key).expect("the engine recovered a key");
    let (result, destination) = if stdout {
        (print_key(id, &key), "stdout".to_owned())
    } else {
        (
            key_file::write(data_dir, &key),
            format!("admin.key in {directory}"),
        )
    };
    result.map_err(|e| OpError {
        hint: Some(format!(
            "use a working admin key or recover one with ironflowx recover-key --stdout, then revoke {id}"
        )),
        ..error(
            ErrorKind::Internal,
            format!("cannot deliver recovered key {id} to {destination}: {e}"),
        )
    })
}

fn preflight(data_dir: &Path) -> Result<(), OpError> {
    let directory = key_file::diagnostic_directory(data_dir);
    match fs::symlink_metadata(key_file::path(data_dir)) {
        Ok(_) => {
            return Err(OpError {
                hint: Some("use --stdout to recover without changing admin.key".into()),
                ..error(
                    ErrorKind::Invalid,
                    format!("admin.key already exists in {directory}"),
                )
            });
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(error(
                ErrorKind::Unavailable,
                format!("cannot inspect admin.key in {directory}: {e}"),
            ));
        }
    }
    key_file::probe(data_dir)
        .map_err(|e| crate::ServeError::Probe(data_dir.to_owned(), e).envelope())
}

fn recovery_error(e: RecoverError) -> OpError {
    let kind = match &e {
        RecoverError::NotReady => ErrorKind::Invalid,
        RecoverError::Store(e) => crate::store_kind(e),
    };
    let mut error = error(kind, e.to_string());
    if kind == ErrorKind::Unavailable {
        error.hint = Some(
            "list recovered keys before retrying and again after reopening the engine. Revoke any key whose secret was lost".into()
        );
    }
    error
}

fn error(kind: ErrorKind, message: impl AsRef<str>) -> OpError {
    OpError {
        kind,
        message: public_text(message.as_ref()),
        hint: None,
    }
}

/// Generates a seed and prints it with its key ID.
///
/// # Errors
///
/// Reports a failure to write the answer to stdout.
pub fn generate_seed() -> Result<(), OpError> {
    let (id, key) = ironflowx_operations::generate_seed();
    print_key(id, &key)
        .map_err(|e| error(ErrorKind::Internal, format!("cannot write to stdout: {e}")))
}

fn print_key(id: KeyId, key: &Secret) -> io::Result<()> {
    // The key format needs no JSON escaping. Write it straight to its intended destination.
    let mut out = io::stdout().lock();
    if out.is_terminal() {
        writeln!(
            out,
            "{{\n  \"key\": \"{}\",\n  \"key_id\": \"{id}\"\n}}",
            key.expose()
        )?;
    } else {
        writeln!(out, r#"{{"key":"{}","key_id":"{id}"}}"#, key.expose())?;
    }
    out.flush()
}
