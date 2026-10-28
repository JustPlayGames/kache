#!/usr/bin/env bash
# nextest's run-wrapper on Unix (.config/nextest.toml): runs one test process
# with a temporary folder of its own, and removes that folder once the test
# has ended.
#
# Tests leave things in the temporary folder that no TempDir owns: kache's
# warn-once markers (kache-*-warn-*), which it keeps in the invocation's
# temporary folder on purpose, and folders a TempDir could not remove because
# a daemon the test started still had them open (Windows refuses that; see
# nextest-own-temp.cmd). With TMPDIR, TMP and TEMP pointing into a folder of
# the test's own, all of it goes with the folder.
#
# Stored files are read-only, so everything is made writable first. A daemon
# the test started drains within seconds of the test ending
# (KACHE_DAEMON_OWNER_PID), so the removal is tried again for up to two
# minutes; a folder still there after that is named on stderr and kept.
#
# With NEXTEST_OWN_TEMP_LOG set, every folder that took a retry or was left
# gets a line in that file: the seconds waited, the outcome and the test.
#
# The test's exit status is passed on unchanged, a signal as the same signal.
set -u
base=${TMPDIR:-/tmp}
if ! dir=$(mktemp -d "${base%/}/kt.XXXXXX" 2>/dev/null); then
    exec "$@"
fi
export TMPDIR="$dir" TMP="$dir" TEMP="$dir"
"$@"
status=$?
waited=0
while :; do
    chmod -R u+rwX "$dir" 2>/dev/null
    rm -rf "$dir" 2>/dev/null
    [ -e "$dir" ] || break
    [ "$waited" -ge 120 ] && break
    sleep 1
    waited=$((waited + 1))
done
outcome=removed
if [ -e "$dir" ]; then
    outcome=left
    echo "nextest-own-temp: $dir is still in use after the test; left in place" >&2
fi
if [ -n "${NEXTEST_OWN_TEMP_LOG:-}" ] && { [ "$waited" -gt 0 ] || [ "$outcome" = left ]; }; then
    printf '%s\t%ss\t%s\t%s\n' "$dir" "$waited" "$outcome" "$*" >> "$NEXTEST_OWN_TEMP_LOG"
fi
if [ "$status" -gt 128 ] && [ "$status" -lt 160 ]; then
    signal=$((status - 128))
    trap - "$signal" 2>/dev/null
    kill -s "$signal" $$ 2>/dev/null
fi
exit "$status"
