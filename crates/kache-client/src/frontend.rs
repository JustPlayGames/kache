use anyhow::{Context, Result};
use std::ffi::OsString;
use std::process::Command;

#[cfg(windows)]
#[path = "frontend_windows.rs"]
mod windows;

/// Run the thin compiler client, or replace it with the companion CLI engine.
pub fn run(version: &str) -> Result<i32> {
    let started_at = std::time::Instant::now();
    let args = std::env::args_os().collect::<Vec<_>>();
    anyhow::ensure!(
        args.get(1).is_none_or(|arg| {
            arg != crate::FORWARDED_ARGV_FLAG && arg != crate::FORWARDED_ARGV_FILE_FLAG
        }),
        "kache-engine points to the client; reinstall both Kache binaries together"
    );
    let executable = std::env::current_exe().context("locating the Kache client")?;
    let executable = if executable.is_absolute() {
        executable
    } else {
        std::env::current_dir()?.join(executable)
    };
    if engine_only(&args)
        || std::env::var_os("KACHE_FAMILY_PROBE_ACTIVE").is_some()
        || std::env::var_os("KACHE_BUILD_SCRIPT_PATH").is_some()
    {
        return forward_engine(&executable, version, &args, started_at, false);
    }
    #[cfg(any(unix, windows))]
    {
        let env = kache_config::InvocationEnv::capture();
        if env
            .var_os("KACHE_ACTIVE")
            .is_none_or(|value| value.is_empty())
        {
            let prepared = (|| -> Result<_> {
                let config = kache_config::Config::load_for(&env)?;
                #[cfg(unix)]
                let (files, terminal) = crate::Stdio::capture()?.into_files();
                #[cfg(windows)]
                let (files, terminal) = (Vec::new(), false);
                Ok((config, files, terminal))
            })();
            if let Ok((config, files, terminal)) = prepared {
                let execution = crate::Execution {
                    entry: kache_protocol::execution::ExecutionEntry::Original,
                    args: &args,
                    env: &env,
                    started_at,
                    executable: &executable,
                    files: &files,
                    terminal,
                    client_version: version,
                };
                let socket = config.socket_path();
                let attempt =
                    execution.try_execute_attempt(&socket, config.trust_daemon_execution)?;
                if let kache_ipc::execution::Attempt::Completed(exit) = attempt {
                    return Ok(exit);
                }
                if attempt == kache_ipc::execution::Attempt::Declined {
                    return forward_engine(&executable, version, &args, started_at, true);
                }
                // A daemon that is there but not answering is not started
                // again, nor one that answered and declined: starting one
                // would only find it, at the cost of an engine process.
                #[cfg(windows)]
                if attempt == kache_ipc::execution::Attempt::Unavailable
                    && !kache_ipc::execution::daemon_unresponsive(&socket)
                    && let Some(exit) = windows::try_bootstrap(
                        &execution,
                        &socket,
                        config.disabled,
                        config.trust_daemon_execution,
                    )?
                {
                    return Ok(exit);
                }
            }
        }
    }
    forward_engine(&executable, version, &args, started_at, false)
}

fn forward_engine(
    executable: &std::path::Path,
    version: &str,
    args: &[OsString],
    started_at: std::time::Instant,
    declined: bool,
) -> Result<i32> {
    let mut command = engine_command(executable, version, args, started_at)?;
    if declined {
        command.env(crate::FRONTEND_DECLINED_ENV, "1");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("starting the Kache engine")
    }
    #[cfg(not(unix))]
    {
        let status = match command.status() {
            #[cfg(windows)]
            Err(error) if crate::argv_file::command_line_too_long(&error) => {
                // The engine reads the file whenever it asks for its
                // arguments, so it stays until the engine has exited.
                let (mut short, _file) = through_argv_file(&command, &std::env::temp_dir())?;
                short
                    .status()
                    .context("starting the Kache engine with its arguments in a file")?
            }
            status => status.context("starting the Kache engine")?,
        };
        Ok(status.code().unwrap_or(1))
    }
}

/// `command` with its arguments in a file: a compiler command line just short
/// of Windows' limit fits Cargo's start of the frontend (so Cargo wrote no
/// argfile) but not the engine's, which adds the envelope.
#[cfg(windows)]
fn through_argv_file(
    command: &Command,
    directory: &std::path::Path,
) -> Result<(Command, crate::argv_file::ArgvFile)> {
    let file = crate::argv_file::ArgvFile::create(directory, command.get_args())
        .context("writing the Kache engine's arguments to a file")?;
    let mut short = Command::new(command.get_program());
    short.arg(crate::FORWARDED_ARGV_FILE_FLAG).arg(file.path());
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => short.env(key, value),
            None => short.env_remove(key),
        };
    }
    Ok((short, file))
}

fn engine_command(
    executable: &std::path::Path,
    version: &str,
    args: &[OsString],
    started_at: std::time::Instant,
) -> Result<Command> {
    let engine = kache_shims::engine_for(executable)
        .context("locating kache-engine; reinstall both Kache binaries together")?;
    let mut command = Command::new(engine);
    command
        .arg(crate::FORWARDED_ARGV_FLAG)
        .arg(version)
        .arg(
            started_at
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX))
                .to_string(),
        )
        .arg(executable)
        .arg(
            kache_shims::running_identity()
                .map(|identity| format!("{}:{}", identity.dev, identity.ino))
                .unwrap_or_else(|| "-".to_owned()),
        )
        .args(args);
    Ok(command)
}

fn engine_only(args: &[OsString]) -> bool {
    let arg0 = args
        .first()
        .map(|arg| arg.to_string_lossy())
        .unwrap_or_default();
    if kache_shims::is_compiler_shim_name(&arg0) {
        return false;
    }
    let name = std::path::Path::new(arg0.as_ref())
        .file_stem()
        .and_then(|name| name.to_str());
    if name == Some("cargo") {
        return true;
    }
    let Some(first) = args.get(1) else {
        return true;
    };
    let Some(first) = first.to_str() else {
        return false;
    };
    crate::engine_command_names().contains(&first)
        || (first.starts_with('-') && !(first == "-E" && args.len() >= 3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_flags_and_cargo_go_to_the_engine_before_configuration() {
        for args in [
            vec!["kache", "init"],
            vec!["kache", "help"],
            vec!["kache", "-E"],
            vec!["kache", "status"],
            vec!["renamed-kache", "--help"],
            vec!["cargo", "build"],
            vec!["kache"],
        ] {
            assert!(engine_only(
                &args.into_iter().map(OsString::from).collect::<Vec<_>>()
            ));
        }
        for args in [
            vec!["cc", "init"],
            vec!["x86_64-linux-gnu-gcc-13", "--help"],
            vec!["kache", "-E", "probe.c"],
            vec!["kache", "rustc", "lib.rs"],
        ] {
            assert!(!engine_only(
                &args.into_iter().map(OsString::from).collect::<Vec<_>>()
            ));
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_command_line_windows_refuses_goes_to_the_engine_in_a_file() {
        use std::ffi::OsStr;
        let directory = tempfile::tempdir().unwrap();
        let long = "x".repeat(40_000);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg(crate::FORWARDED_ARGV_FLAG)
            .arg(&long)
            .env(crate::FRONTEND_DECLINED_ENV, "1")
            .env_remove("KACHE_ARGV_FILE_TEST_REMOVED");
        let error = command.status().unwrap_err();
        assert!(crate::argv_file::command_line_too_long(&error), "{error}");

        let (short, file) = through_argv_file(&command, directory.path()).unwrap();
        assert_eq!(short.get_program(), command.get_program());
        assert_eq!(
            short.get_args().collect::<Vec<_>>(),
            [
                OsStr::new(crate::FORWARDED_ARGV_FILE_FLAG),
                file.path().as_os_str()
            ]
        );
        let envs = short.get_envs().collect::<Vec<_>>();
        assert!(envs.contains(&(
            OsStr::new(crate::FRONTEND_DECLINED_ENV),
            Some(OsStr::new("1"))
        )));
        assert!(envs.contains(&(OsStr::new("KACHE_ARGV_FILE_TEST_REMOVED"), None)));
        assert_eq!(
            crate::argv_file::read(file.path()).unwrap(),
            [
                OsString::from(crate::FORWARDED_ARGV_FLAG),
                OsString::from(long)
            ]
        );
        let path = file.path().to_owned();
        drop(file);
        assert!(!path.exists());
    }
}

#[cfg(all(test, unix))]
#[path = "frontend/tests.rs"]
mod boundary_tests;
