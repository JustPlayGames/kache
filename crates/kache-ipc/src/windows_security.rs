//! Apply the application's delegation policy to shared token evidence.

use kunobi_daemon::local::security::SecurityContextComparison;

#[cfg(windows)]
pub(crate) fn matches_daemon(
    process: std::os::windows::io::BorrowedHandle<'_>,
    trusted_pipeline: bool,
) -> std::io::Result<bool> {
    kunobi_daemon::local::security::compare_with_current_process(process)
        .map(|comparison| permits_delegation(comparison, trusted_pipeline))
}

/// Upstream lets a context that differs only by Windows 11's per-process
/// nonce (`TSA://ProcUnique`) in through the trusted pipeline alone. This
/// fork has let that one difference in without it since tasty.47 (every
/// process, a child of the daemon's own included, has its own nonce), so
/// either rule passing is enough.
const FORK_ACCEPTS_PROCESS_NONCE: bool = true;

fn permits_delegation(comparison: SecurityContextComparison, trusted_pipeline: bool) -> bool {
    match comparison {
        SecurityContextComparison::Equivalent => true,
        SecurityContextComparison::ProcessIdentityDiffers => {
            trusted_pipeline || FORK_ACCEPTS_PROCESS_NONCE
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_process_identity_differences_pass_without_the_trust_opt_in() {
        for trusted in [false, true] {
            assert!(permits_delegation(
                SecurityContextComparison::Equivalent,
                trusted
            ));
            assert!(permits_delegation(
                SecurityContextComparison::ProcessIdentityDiffers,
                trusted
            ));
            assert!(!permits_delegation(
                SecurityContextComparison::Incompatible,
                trusted
            ));
        }
    }

    /// Fixture for the test below: a process started on its own whose token
    /// has another generation (ModifiedId) than the test's.
    #[cfg(windows)]
    #[test]
    #[ignore = "subprocess fixture for a token of another generation"]
    fn token_child_of_another_generation_waits_for_input() {
        use std::io::{Read, Write};
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Security::{
            AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
            SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        if std::env::var_os("KACHE_IPC_TOKEN_TEST_CHILD").is_none() {
            return;
        }
        let mut handle = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                    &mut handle,
                )
            },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        let token = unsafe { OwnedHandle::from_raw_handle(handle) };
        let name = "SeChangeNotifyPrivilege"
            .encode_utf16()
            .chain([0])
            .collect::<Vec<u16>>();
        let mut change_notify = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: unsafe { std::mem::zeroed() },
                Attributes: 0,
            }],
        };
        assert_ne!(
            unsafe {
                LookupPrivilegeValueW(
                    std::ptr::null(),
                    name.as_ptr(),
                    &mut change_notify.Privileges[0].Luid,
                )
            },
            0
        );
        // Disabled and enabled again: the privileges end as they began, in a
        // token of a new generation.
        for attributes in [0, SE_PRIVILEGE_ENABLED] {
            change_notify.Privileges[0].Attributes = attributes;
            assert_ne!(
                unsafe {
                    AdjustTokenPrivileges(
                        token.as_raw_handle(),
                        0,
                        &change_notify,
                        0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                },
                0
            );
        }
        println!("token child ready");
        std::io::stdout().flush().unwrap();
        let mut finish = [0];
        std::io::stdin().read_exact(&mut finish).unwrap();
    }

    #[cfg(windows)]
    fn modified_id(process: std::os::windows::io::BorrowedHandle<'_>) -> (i32, u32) {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Security::{
            GetTokenInformation, TOKEN_QUERY, TOKEN_STATISTICS, TokenStatistics,
        };
        use windows_sys::Win32::System::Threading::OpenProcessToken;
        let mut handle = std::ptr::null_mut();
        assert_ne!(
            unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut handle) },
            0
        );
        let token = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut statistics: TOKEN_STATISTICS = unsafe { std::mem::zeroed() };
        let mut length = 0;
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token.as_raw_handle(),
                    TokenStatistics,
                    (&raw mut statistics).cast(),
                    std::mem::size_of::<TOKEN_STATISTICS>() as u32,
                    &mut length,
                )
            },
            0
        );
        (
            statistics.ModifiedId.HighPart,
            statistics.ModifiedId.LowPart,
        )
    }

    #[cfg(windows)]
    #[test]
    fn a_process_of_another_token_generation_matches_without_the_trust_opt_in() {
        // Two processes the same user starts apart (a terminal, and the
        // daemon from kache's logon task or another terminal) carry tokens of
        // different generations, each with its own TSA://ProcUnique. A child
        // inherits its parent's generation, so the fixture starts a new one.
        use std::io::{BufRead, Write};
        use std::os::windows::io::AsHandle;
        use std::process::{Command, Stdio};

        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let mut child = Child(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "windows_security::tests::token_child_of_another_generation_waits_for_input",
                    "--nocapture",
                ])
                .env("KACHE_IPC_TOKEN_TEST_CHILD", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut output = std::io::BufReader::new(child.0.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "token child exited early"
            );
            if line.contains("token child ready") {
                break;
            }
        }
        let current = unsafe {
            std::os::windows::io::BorrowedHandle::borrow_raw(
                windows_sys::Win32::System::Threading::GetCurrentProcess(),
            )
        };
        assert_ne!(modified_id(child.0.as_handle()), modified_id(current));
        assert!(matches_daemon(child.0.as_handle(), false).unwrap());
        child.0.stdin.take().unwrap().write_all(&[1]).unwrap();
        assert!(child.0.wait().unwrap().success());
    }

    #[cfg(windows)]
    #[test]
    fn an_appcontainer_child_of_the_same_user_is_refused() {
        use std::os::windows::io::BorrowedHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::Isolation::{
            CreateAppContainerProfile, DeleteAppContainerProfile,
            DeriveAppContainerSidFromAppContainerName,
        };
        use windows_sys::Win32::Security::{FreeSid, PSID, SECURITY_CAPABILITIES};
        use windows_sys::Win32::System::Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList,
            EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
            LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            PROCESS_INFORMATION, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
        };
        let wide = |text: &str| text.encode_utf16().chain([0]).collect::<Vec<u16>>();
        // The same user and logon session as the daemon, with an AppContainer
        // token (its own SID, package attributes): the pipeline stays local,
        // with the trust opt-in too.
        let name = wide(&format!("kache-test-{}", std::process::id()));
        let mut sid: PSID = std::ptr::null_mut();
        let created = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                name.as_ptr(),
                name.as_ptr(),
                std::ptr::null(),
                0,
                &mut sid,
            )
        };
        if created < 0 {
            assert_eq!(
                unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) },
                0,
                "CreateAppContainerProfile {created:#x}"
            );
        }
        let mut capabilities = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: std::ptr::null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        let mut size = 0;
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        let mut list = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let attributes: LPPROC_THREAD_ATTRIBUTE_LIST = list.as_mut_ptr().cast();
        assert_ne!(
            unsafe { InitializeProcThreadAttributeList(attributes, 1, 0, &mut size) },
            0
        );
        assert_ne!(
            unsafe {
                UpdateProcThreadAttribute(
                    attributes,
                    0,
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                    (&raw mut capabilities).cast(),
                    std::mem::size_of::<SECURITY_CAPABILITIES>(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            },
            0
        );
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attributes;
        let mut command = wide(r"C:\Windows\System32\cmd.exe /d /q /c exit");
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let spawned = unsafe {
            CreateProcessW(
                std::ptr::null(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                std::ptr::null(),
                &startup.StartupInfo,
                &mut process,
            )
        };
        let spawn_error = std::io::Error::last_os_error();
        unsafe { DeleteProcThreadAttributeList(attributes) };
        let matches = (spawned != 0).then(|| {
            let child = unsafe { BorrowedHandle::borrow_raw(process.hProcess) };
            let matches = [false, true].map(|trusted| matches_daemon(child, trusted));
            unsafe {
                TerminateProcess(process.hProcess, 1);
                CloseHandle(process.hThread);
                CloseHandle(process.hProcess);
            }
            matches
        });
        unsafe {
            FreeSid(sid);
            DeleteAppContainerProfile(name.as_ptr());
        }
        let matches = matches.unwrap_or_else(|| panic!("CreateProcessW: {spawn_error}"));
        // The daemon declines on an error as on a mismatch.
        for matches in matches {
            assert!(!matches.unwrap_or(false));
        }
    }
}
