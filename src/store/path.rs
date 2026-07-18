use std::{ffi::OsString, io, path::PathBuf};

const APP_DIRECTORY: &str = "nivalis-mail";
const DATABASE_FILE: &str = "mail.sqlite3";

pub(crate) fn database_path() -> io::Result<PathBuf> {
    if let Some(directory) = non_empty_env("NIVALIS_DATA_DIR") {
        let directory = PathBuf::from(directory);
        if !directory.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NIVALIS_DATA_DIR must be an absolute path",
            ));
        }
        return Ok(directory.join(DATABASE_FILE));
    }

    platform_data_root()
        .map(|root| root.join(APP_DIRECTORY).join(DATABASE_FILE))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not determine the per-user application data directory",
            )
        })
}

fn non_empty_env(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

#[cfg(target_os = "windows")]
fn platform_data_root() -> Option<PathBuf> {
    non_empty_env("LOCALAPPDATA").map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn platform_data_root() -> Option<PathBuf> {
    non_empty_env("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Application Support"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_data_root() -> Option<PathBuf> {
    non_empty_env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            non_empty_env("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local").join("share"))
        })
}

#[cfg(not(any(unix, target_os = "windows")))]
fn platform_data_root() -> Option<PathBuf> {
    None
}
