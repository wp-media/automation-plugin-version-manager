# shellcheck shell=bash
#
# Runs inside the target as a throwaway user (checks/ssh.sh in_target_client): jump-host
# facts of the P3.3 transport, with the target's own OpenSSH client and a home nobody else
# uses. The hop is `deploy@localhost`, the destination `127.0.0.1` behind it. Prints
# `RESULT <name> <value>` lines for the check to assert.
set -u

# gen FILE KNOWN_HOSTS STRICT — a generated-style ssh_config with the transport's options.
gen() {
	printf 'BatchMode yes\nStrictHostKeyChecking %s\nUserKnownHostsFile "%s"\n' "$3" "$2" >"$1"
	printf 'IdentityFile "%s"\nIdentitiesOnly yes\nConnectTimeout 5\n' "$PWD/key" >>"$1"
}

# o_options KNOWN_HOSTS — the same options as `-o`, with no ssh_config read.
o_options() {
	printf '%s\n' -F /dev/null -o BatchMode=yes -o StrictHostKeyChecking=yes \
		-o "UserKnownHostsFile=$1" -i key -o IdentitiesOnly=yes
}

ssh -V 2>&1
awk '{ print "localhost", $1, $2 }' host_keys >kh_hop
awk '{ print "127.0.0.1", $1, $2 }' host_keys >kh_dest
mkdir -m 700 .ssh

# The hop's key and a usable default key only in ~/.ssh: with -o options the hop uses them.
cp kh_hop .ssh/known_hosts
cp key .ssh/id_ed25519
mapfile -t opts < <(o_options "$PWD/kh_dest")
if ssh "${opts[@]}" -J deploy@localhost -l deploy -- 127.0.0.1 'echo RAN' </dev/null 2>/dev/null | grep -q RAN; then
	echo "RESULT o-options-use-home yes"
fi

# The same ~/.ssh, but a generated -F file whose known_hosts lacks the hop's key.
gen g_strict "$PWD/kh_dest" yes
ssh -F g_strict -J deploy@localhost -l deploy -- 127.0.0.1 true </dev/null 2>err
echo "RESULT generated-ignores-home-exit $?"
if grep -q 'No [A-Z0-9]* host key is known for localhost and you have requested strict checking' err; then
	echo "RESULT generated-strict-message yes"
fi

# Nothing known anywhere, under a pseudo-terminal: -o options let the hop ask.
rm .ssh/known_hosts
printf 'exec ssh %s -J deploy@localhost -l deploy -- 127.0.0.1 true\n' "${opts[*]}" >o.sh
timeout 5 script -qec 'sh o.sh' /dev/null </dev/null >o.out 2>&1
grep -q 'Are you sure you want to continue connecting' o.out && echo "RESULT o-options-prompt yes"
printf 'exec ssh -F g_strict -J deploy@localhost -l deploy -- 127.0.0.1 true\n' >g.sh
timeout 5 script -qec 'sh g.sh' /dev/null </dev/null >g.out 2>&1
grep -q 'Are you sure' g.out || echo "RESULT generated-no-prompt yes"

# accept-new through the hop: both keys go to the generated file's known_hosts.
: >kh_new
gen g_new "$PWD/kh_new" accept-new
if ssh -F g_new -J deploy@localhost -l deploy -- 127.0.0.1 'echo RAN' </dev/null 2>/dev/null | grep -q RAN; then
	echo "RESULT accept-new-ran yes"
fi
echo "RESULT accept-new-recorded $(cut -d ' ' -f 1 kh_new | sort -u | tr '\n' ,)"
[ -e .ssh/known_hosts ] || echo "RESULT home-untouched yes"
