# shellcheck shell=bash
#
# Runs inside the target as a throwaway user (checks/ssh.sh in_target_client): what ssh
# does with accept-new when the known_hosts file cannot keep a key. Prints `RESULT <name>
# <value>` lines for the check to assert.
set -u

# gen FILE KNOWN_HOSTS — a generated-style ssh_config with accept-new.
gen() {
	printf 'BatchMode yes\nStrictHostKeyChecking accept-new\nUserKnownHostsFile "%s"\n' "$2" >"$1"
	printf 'IdentityFile "%s"\nIdentitiesOnly yes\nConnectTimeout 5\n' "$PWD/key" >>"$1"
}

ln -s /dev/null null_kh
gen g_null "$PWD/null_kh"
ssh -F g_null -l deploy -- localhost 'echo RAN' </dev/null 2>err | grep -q RAN && echo "RESULT dev-null-ran yes"
grep -q 'Permanently added' err && echo "RESULT dev-null-claims-added yes"

gen g_nodir "$PWD/nodir/kh"
ssh -F g_nodir -l deploy -- localhost 'echo RAN' </dev/null 2>err | grep -q RAN && echo "RESULT missing-dir-ran yes"
grep -q 'Failed to add the host to the list of known hosts' err && echo "RESULT missing-dir-failed-to-add yes"
exit 0
