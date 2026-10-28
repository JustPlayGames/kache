@echo off
rem nextest's run-wrapper on Windows (.config/nextest.toml): runs one test
rem process with a temporary folder of its own, and removes that folder once
rem the test has ended. See nextest-own-temp.sh for why.
rem
rem Windows keeps what a TempDir could not delete: a folder a daemon the test
rem started still has open (its index.db, its lock files), and files with the
rem read-only attribute that the store gives its blobs. So the attribute is
rem cleared everywhere first, and the removal is tried again for up to two
rem minutes while the test's daemons drain (KACHE_DAEMON_OWNER_PID); a folder
rem still there after that is named on stderr and kept.
rem
rem With NEXTEST_OWN_TEMP_LOG set, every folder that took a retry or was
rem left gets a line in that file: the seconds waited, the outcome and the
rem test.
rem
rem The test's exit status is passed on unchanged.
setlocal EnableExtensions DisableDelayedExpansion
set "KT_BASE=%TEMP%"
if not defined KT_BASE set "KT_BASE=%TMP%"
set /a KT_TRIES=0
:pick
set "KT_DIR=%KT_BASE%\kt%RANDOM%%RANDOM%"
mkdir "%KT_DIR%" 2>nul && goto run
set /a KT_TRIES+=1
if %KT_TRIES% lss 100 goto pick
%*
exit /b %ERRORLEVEL%
:run
set "TEMP=%KT_DIR%"
set "TMP=%KT_DIR%"
set "TMPDIR=%KT_DIR%"
%*
set "KT_STATUS=%ERRORLEVEL%"
set /a KT_WAIT=0
:clean
attrib -R "%KT_DIR%\*" /S /D >nul 2>&1
rd /s /q "%KT_DIR%" >nul 2>&1
if not exist "%KT_DIR%\" goto done
set /a KT_WAIT+=1
if %KT_WAIT% geq 120 goto stuck
ping -n 2 127.0.0.1 >nul
goto clean
:stuck
echo nextest-own-temp: %KT_DIR% is still in use after the test; left in place 1>&2
if defined NEXTEST_OWN_TEMP_LOG echo %KT_DIR%	%KT_WAIT%s	left	%*>> "%NEXTEST_OWN_TEMP_LOG%"
exit /b %KT_STATUS%
:done
if %KT_WAIT% gtr 0 if defined NEXTEST_OWN_TEMP_LOG echo %KT_DIR%	%KT_WAIT%s	removed	%*>> "%NEXTEST_OWN_TEMP_LOG%"
exit /b %KT_STATUS%
