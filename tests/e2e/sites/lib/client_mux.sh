# shellcheck shell=bash
#
# Runs inside the target as a throwaway user (checks/ssh.sh in_target_client): the
# multiplexing facts of the P3.4 transport (remote/mux.rs), with the target's own OpenSSH
# client (Linux): the longest control socket path ssh can bind, a master that outlives the
# call with its output closed, calls that share one connection, a `-N` starter, what a
# jump host's failure looks like with and without `ControlPersist`,
# and what a call prints when a master at MaxSessions refuses it. Prints `RESULT <name>
# <value>` lines for the check to assert.
set -u

# gen FILE CONTROL_PATH — a generated-style ssh_config that shares connections through
# CONTROL_PATH (written as is: the caller doubles its `%`).
gen() {
	printf 'BatchMode yes\nStrictHostKeyChecking yes\nUserKnownHostsFile "%s"\n' "$PWD/kh" >"$1"
	printf 'IdentityFile "%s"\nIdentitiesOnly yes\nConnectTimeout 5\nLogLevel INFO\n' "$PWD/key" >>"$1"
	printf 'RequestTTY no\nControlMaster auto\nControlPath "%s"\nControlPersist 15s\n' "$2" >>"$1"
}

# stop FILE — asks the master that FILE's calls to deploy@localhost use to exit.
stop() {
	ssh -F "$1" -O exit -l deploy -- localhost 2>/dev/null
}

ssh -V 2>&1
awk '{ print "localhost", $1, $2 }' host_keys >kh
awk '{ print "127.0.0.1", $1, $2 }' host_keys >>kh

# The longest socket path: `<dir>/<40 hex>` of 90 bytes binds, 91 fails the call.
for total in 90 91; do
	dir=$PWD/len$total/
	dir=$dir$(head -c $((total - 41 - ${#dir})) /dev/zero | tr '\0' d)
	mkdir -p "$dir" && chmod 700 "$dir"
	gen "g$total" "$dir/%C"
	ssh -F "g$total" -l deploy -- localhost true </dev/null 2>"err$total"
	echo "RESULT socket-path-$total-exit $?"
	stop "g$total"
done
grep -q 'too long for Unix domain socket' err91 && echo "RESULT socket-path-91-says-too-long yes"

# A master: the first call ends with its output (the master's streams are /dev/null),
# `-O check` finds it live, and later calls share its connection.
mkdir -m 700 mux
gen g "$PWD/mux/%C"
# shellcheck disable=SC2016 # expanded by the remote shell
first=$(timeout 20 ssh -F g -l deploy -- localhost 'echo "$SSH_CONNECTION"' </dev/null 2>/dev/null)
echo "RESULT first-call-exit $?"
ssh -F g -O check -l deploy -- localhost 2>&1 | grep -q '^Master running' && echo "RESULT master-live yes"
# shellcheck disable=SC2016 # expanded by the remote shell
second=$(ssh -F g -l deploy -- localhost 'echo "$SSH_CONNECTION"' </dev/null 2>/dev/null)
[ -n "$first" ] && [ "$first" = "$second" ] && echo "RESULT same-connection yes"

# A stopped master: a call waits for the connect timeout, says so, and runs on a
# connection of its own.
master=$(ssh -F g -O check -l deploy -- localhost 2>&1 | sed -n 's/^Master running (pid=\([0-9]*\)).*/\1/p')
kill -STOP "$master"
ssh -F g -l deploy -- localhost true </dev/null 2>stopped
echo "RESULT stopped-master-exit $?"
kill -CONT "$master"
tr -d '\r' <stopped | grep -qx 'muxclient: master hello exchange failed' && echo "RESULT stopped-master-line yes"

# `-O stop`: the master takes no more calls, so the next call starts a new one.
ssh -F g -O stop -l deploy -- localhost 2>/dev/null
echo "RESULT stop-exit $?"
# shellcheck disable=SC2016 # expanded by the remote shell
third=$(ssh -F g -l deploy -- localhost 'echo "$SSH_CONNECTION"' </dev/null 2>/dev/null)
[ -n "$third" ] && [ "$third" != "$first" ] && echo "RESULT after-stop-new-connection yes"

# Eleven calls at once through one master (sshd's MaxSessions 10): every call runs, and
# the one the master could not open prints the fallback's two lines.
for i in 1 2 3 4 5 6 7 8 9 10 11; do
	ssh -F g -l deploy -- localhost 'sleep 2' </dev/null 2>"busy$i" &
done
failed=0
for job in $(jobs -p); do wait "$job" || failed=$((failed + 1)); done
echo "RESULT max-sessions-failed $failed"
cat busy* | tr -d '\r' >busy
grep -q '^mux_client_request_session: session request failed: Session open refused by peer$' busy &&
	echo "RESULT max-sessions-refused-line yes"
grep -q '^ControlSocket .* already exists, disabling multiplexing$' busy &&
	echo "RESULT max-sessions-fallback-line yes"
stop g

# A master's starter: `-N` authenticates, forks the master and exits 0 at once.
mkdir -m 700 nmux
gen gn "$PWD/nmux/%C"
timeout 20 ssh -F gn -N -l deploy -- localhost </dev/null >/dev/null 2>&1
echo "RESULT starter-exit $?"
ssh -F gn -O check -l deploy -- localhost 2>&1 | grep -q '^Master running' && echo "RESULT starter-master-live yes"

# Through a jump host whose key is unknown: plain ssh says why; with `ControlPersist` ssh
# sends the jump host's messages to /dev/null, so nothing tells (why apvm shares no
# connection through a proxy).
awk '{ print "127.0.0.1", $1, $2 }' host_keys >kh_dest
sed "s#$PWD/kh\"#$PWD/kh_dest\"#" g >gj
sed '/^ControlMaster\|^ControlPath\|^ControlPersist/d' gj >gj_plain
printf 'ControlMaster no\nControlPath none\n' >>gj_plain
ssh -F gj_plain -J deploy@localhost -l deploy -- 127.0.0.1 true </dev/null 2>jplain
echo "RESULT jump-plain-exit $?"
grep -q 'host key is known for localhost' jplain && echo "RESULT jump-plain-says-why yes"
mkdir -m 700 jmux
sed "s#$PWD/mux/#$PWD/jmux/#" gj >gj2
timeout 20 ssh -F gj2 -J deploy@localhost -l deploy -- 127.0.0.1 true </dev/null 2>jshared
echo "RESULT jump-shared-exit $?"
grep -q 'host key is known for localhost' jshared || echo "RESULT jump-shared-hides-why yes"
# The masters outlive the calls: end every ssh of this user.
pkill -u "$(id -u)" -x ssh
exit 0
