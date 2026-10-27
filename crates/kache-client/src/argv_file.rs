//! A forwarded argv written to a file, for a command line Windows refuses.
//!
//! The frontend hands the engine the compiler's whole argv after five
//! envelope fields of its own. A command line Cargo could start the frontend
//! with can then pass Windows' limit of 32,767 characters by those fields, and
//! Cargo writes an argfile only when its own start fails, so it never wrote
//! one. The frontend writes the envelope here instead and passes the engine
//! [`crate::FORWARDED_ARGV_FILE_FLAG`] and this file's path.
//!
//! Each argument is its native form (UTF-16 code units on Windows, bytes
//! elsewhere) followed by a NUL, which no argument can hold. The frontend
//! removes the file once the engine has exited; the engine reads it whenever
//! it asks for its arguments.

use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::Path;
#[cfg(any(windows, test))]
use std::{io::Write, path::PathBuf};

/// Windows' `ERROR_FILENAME_EXCED_RANGE`: a command line past 32,767
/// characters, Cargo's own test for writing an argfile.
#[cfg(windows)]
pub(crate) fn command_line_too_long(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(206)
}

/// A file holding a forwarded argv, removed when dropped. Only Windows
/// writes one.
#[cfg(any(windows, test))]
pub(crate) struct ArgvFile {
    path: PathBuf,
}

#[cfg(any(windows, test))]
impl ArgvFile {
    /// Write `args` to a new file in `directory`.
    pub(crate) fn create<'a>(
        directory: &Path,
        args: impl IntoIterator<Item = &'a OsStr>,
    ) -> Result<Self> {
        let (path, mut file) = create_new(directory)?;
        let this = Self { path };
        file.write_all(&encode(args)?)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", this.path.display()))?;
        Ok(this)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(any(windows, test))]
impl Drop for ArgvFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(any(windows, test))]
fn create_new(directory: &Path) -> Result<(PathBuf, std::fs::File)> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    for attempt in 0..100u32 {
        let path = directory.join(format!(
            "kache-argv-{}-{nanos}-{attempt}",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
    anyhow::bail!("no free argv file name in {}", directory.display())
}

/// The arguments a file the frontend wrote holds.
pub(crate) fn read(path: &Path) -> Result<Vec<OsString>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading forwarded argv {}", path.display()))?;
    decode(&bytes).with_context(|| format!("decoding forwarded argv {}", path.display()))
}

#[cfg(windows)]
fn encode<'a>(args: impl IntoIterator<Item = &'a OsStr>) -> Result<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    let mut bytes = Vec::new();
    for arg in args {
        for unit in arg.encode_wide() {
            anyhow::ensure!(unit != 0, "an argument holds a NUL");
            bytes.extend(unit.to_le_bytes());
        }
        bytes.extend(0u16.to_le_bytes());
    }
    Ok(bytes)
}

#[cfg(windows)]
fn decode(bytes: &[u8]) -> Result<Vec<OsString>> {
    use std::os::windows::ffi::OsStringExt;
    let (pairs, odd) = bytes.as_chunks::<2>();
    anyhow::ensure!(odd.is_empty(), "odd length");
    let units = pairs
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect::<Vec<_>>();
    let units = units.strip_suffix(&[0]).context("unterminated argument")?;
    Ok(units
        .split(|unit| *unit == 0)
        .map(OsString::from_wide)
        .collect())
}

#[cfg(all(not(windows), test))]
fn encode<'a>(args: impl IntoIterator<Item = &'a OsStr>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for arg in args {
        let arg = arg.as_encoded_bytes();
        anyhow::ensure!(!arg.contains(&0), "an argument holds a NUL");
        bytes.extend_from_slice(arg);
        bytes.push(0);
    }
    Ok(bytes)
}

#[cfg(not(windows))]
fn decode(bytes: &[u8]) -> Result<Vec<OsString>> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = bytes.strip_suffix(&[0]).context("unterminated argument")?;
    Ok(bytes
        .split(|byte| *byte == 0)
        .map(|arg| OsStr::from_bytes(arg).to_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_written_argv_reads_back_whole_and_its_file_goes_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let args = os(&["--flag", "", "two words", "C:\\a\\\"b\\", "\u{e9}t\u{e9}"]);
        let file =
            ArgvFile::create(directory.path(), args.iter().map(OsString::as_os_str)).unwrap();
        let path = file.path().to_owned();
        assert!(path.starts_with(directory.path()));
        assert_eq!(read(&path).unwrap(), args);
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn two_files_in_one_folder_get_names_of_their_own() {
        let directory = tempfile::tempdir().unwrap();
        let first = ArgvFile::create(directory.path(), [OsStr::new("a")]).unwrap();
        let second = ArgvFile::create(directory.path(), [OsStr::new("b")]).unwrap();
        assert_ne!(first.path(), second.path());
        assert_eq!(read(first.path()).unwrap(), os(&["a"]));
        assert_eq!(read(second.path()).unwrap(), os(&["b"]));
    }

    #[test]
    fn a_file_whose_last_argument_is_cut_off_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let file = ArgvFile::create(directory.path(), [OsStr::new("abc")]).unwrap();
        let bytes = std::fs::read(file.path()).unwrap();
        let cut = directory.path().join("cut");
        std::fs::write(&cut, &bytes[..bytes.len() - 1]).unwrap();
        assert!(read(&cut).is_err());
        assert!(read(&directory.path().join("missing")).is_err());
    }

    #[test]
    fn an_argument_holding_a_nul_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        assert!(ArgvFile::create(directory.path(), [OsStr::new("a\0b")]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn only_a_command_line_too_long_counts_as_one() {
        assert!(command_line_too_long(&std::io::Error::from_raw_os_error(
            206
        )));
        assert!(!command_line_too_long(&std::io::Error::from_raw_os_error(
            2
        )));
        assert!(!command_line_too_long(&std::io::Error::other("206")));
    }
}
