# shellcheck shell=bash
#
# Group `ssh` — plan §3.5 "Stderr samples": what the local OpenSSH client prints for each
# failure class (feeds the P3.3 classifier), and the stderr noise a classifier must ignore.
# Every sample is saved to <results>/ssh-stderr/<name>.txt. `-F /dev/null` keeps the
# user's ~/.ssh/config out of the samples.

# ssh_opts KEY KNOWN_HOSTS STRICT [CONNECT_TIMEOUT] — sets SSH_OPTS to a sample's ssh
# options (timeout default 5 s). Each sample sets its own: for `-o` too, the first value
# given wins, so options appended later are ignored.
ssh_opts() {
	SSH_OPTS=(-F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -i "$1" -o UserKnownHostsFile="$2"
		-o StrictHostKeyChecking="$3" -o ConnectTimeout="${4:-5}")
}

# ssh_sample NAME EXPECTED_EXIT ERE SSH_ARG... — runs ssh with stdin closed, saves its stderr
# as a sample and notes it; returns 1 unless the exit status is EXPECTED_EXIT and stderr
# matches ERE (an empty ERE means stderr must be empty). EXPECTED_EXIT `-` only records.
# SAMPLE_ENV (`NAME=value`, optional) is set for the ssh process only.
ssh_sample() {
	local name=$1 want_rc=$2 ere=$3 rc err
	shift 3
	env ${SAMPLE_ENV:+"$SAMPLE_ENV"} ssh "$@" </dev/null >/dev/null 2>"$SAMPLES_DIR/$name.txt"
	rc=$?
	err=$(cat "$SAMPLES_DIR/$name.txt")
	note "[$name] exit $rc"
	note "$(indent "$err")"
	[ "$want_rc" = - ] && return 0
	if [ -z "$ere" ]; then
		[ "$rc:$err" = "$want_rc:" ] && return 0
	elif [ "$rc" = "$want_rc" ] && grep -Eq -- "$ere" <<<"$err"; then
		return 0
	fi
	note "    FAIL: expected exit $want_rc and stderr matching /$ere/"
	return 1
}

# silent_listener PORT_FILE — accepts TCP connections and never answers (no SSH banner);
# writes its port to PORT_FILE. Run it in the background: it execs, so `$!` is python
# itself, and it holds no inherited output open.
silent_listener() {
	exec python3 -c '
import socket, sys
server = socket.socket()
server.bind(("127.0.0.1", 0))
server.listen(8)
open(sys.argv[1], "w").write(str(server.getsockname()[1]))
held = []
while True:
    held.append(server.accept()[0])' "$1" </dev/null >/dev/null 2>&1
}

# start_silent_listener PORT_FILE — starts silent_listener in the background and waits up to
# 5 s for its port; sets LISTENER_PID (verify.sh kills it on exit) and returns 1 on failure.
start_silent_listener() {
	local tries=0
	silent_listener "$1" &
	LISTENER_PID=$!
	while [ ! -s "$1" ]; do
		if ! kill -0 "$LISTENER_PID" 2>/dev/null || [ $tries -ge 50 ]; then
			note "FAIL  the silent listener did not start"
			return 1
		fi
		tries=$((tries + 1))
		sleep 0.1
	done
}

# stop_silent_listener — stops the background listener, if any.
stop_silent_listener() {
	[ -n "${LISTENER_PID:-}" ] || return 0
	kill "$LISTENER_PID" 2>/dev/null
	wait "$LISTENER_PID" 2>/dev/null
	LISTENER_PID=""
}

# check_failure_samples — one sample per failure class of plan §4.5 (ssh_*).
check_failure_samples() {
	local ok=0 wrong_key="$WORK_DIR/wrong_key" changed="$WORK_DIR/changed_known_hosts" port_file
	local target=(-p "$SSH_PORT" -l deploy -- "$SSH_HOST" true)
	rm -f "$wrong_key" "$wrong_key.pub"
	ssh-keygen -q -t ed25519 -N '' -f "$wrong_key" </dev/null || {
		note "FAIL  ssh-keygen"
		return 1
	}
	ssh_opts "$wrong_key" "$KNOWN_HOSTS" yes
	ssh_sample auth-wrong-key 255 'Permission denied \(publickey\)' "${SSH_OPTS[@]}" "${target[@]}" || ok=1
	ssh_opts "$KEY_FILE" "$KNOWN_HOSTS" yes
	ssh_sample auth-unknown-user 255 'Permission denied \(publickey\)' \
		"${SSH_OPTS[@]}" -p "$SSH_PORT" -l nosuchuser -- "$SSH_HOST" true || ok=1
	ssh_sample connection-refused 255 'Connection refused' "${SSH_OPTS[@]}" -p 1 -l deploy -- "$SSH_HOST" true || ok=1
	ssh_sample dns-failure 255 'Could not resolve hostname' \
		"${SSH_OPTS[@]}" -l deploy -- apvm-e2e-nonexistent.invalid true || ok=1
	: >"$WORK_DIR/empty_known_hosts"
	ssh_opts "$KEY_FILE" "$WORK_DIR/empty_known_hosts" yes
	ssh_sample host-key-unknown 255 'Host key verification failed' "${SSH_OPTS[@]}" "${target[@]}" || ok=1
	awk -v host="$(known_hosts_name)" '{ print host, $1, $2 }' "$wrong_key.pub" >"$changed"
	ssh_opts "$KEY_FILE" "$changed" yes
	ssh_sample host-key-changed 255 'REMOTE HOST IDENTIFICATION HAS CHANGED' "${SSH_OPTS[@]}" "${target[@]}" || ok=1

	port_file="$WORK_DIR/listener.port"
	rm -f "$port_file"
	if start_silent_listener "$port_file"; then
		ssh_opts "$KEY_FILE" "$KNOWN_HOSTS" yes 2
		ssh_sample banner-timeout 255 'timed out' \
			"${SSH_OPTS[@]}" -p "$(cat "$port_file")" -l deploy -- "$SSH_HOST" true || ok=1
	else
		ok=1
	fi
	stop_silent_listener
	return $ok
}

# check_connect_timeout — a TCP connect to an unroutable address (TEST-NET-1). Depends on
# the network (some answer "unreachable" at once), so it is recorded, not asserted.
check_connect_timeout() {
	ssh_opts "$KEY_FILE" "$KNOWN_HOSTS" yes 2
	ssh_sample connect-timeout - '' "${SSH_OPTS[@]}" -l deploy -- 192.0.2.1 true
	return 2
}

# check_exit_255_ambiguity — a remote command that exits 255 is indistinguishable from an
# SSH failure by status alone (why the framing maps 255 → 254, §6.1).
check_exit_255_ambiguity() {
	ssh_opts "$KEY_FILE" "$KNOWN_HOSTS" yes
	ssh_sample remote-exit-255 255 '' "${SSH_OPTS[@]}" -p "$SSH_PORT" -l deploy -- "$SSH_HOST" 'exit 255'
}

# check_noise_samples — stderr that does not mean failure: accept-new's notice, and the
# remote shell's locale warning when the client forwards an unknown locale.
check_noise_samples() {
	local ok=0
	: >"$WORK_DIR/new_known_hosts"
	ssh_opts "$KEY_FILE" "$WORK_DIR/new_known_hosts" accept-new
	ssh_sample accept-new-notice 0 'Permanently added' "${SSH_OPTS[@]}" -p "$SSH_PORT" -l deploy -- "$SSH_HOST" true || ok=1
	ssh_opts "$KEY_FILE" "$KNOWN_HOSTS" yes
	SAMPLE_ENV=LC_ALL=xx_XX.UTF-8 ssh_sample locale-warning 0 'setlocale' "${SSH_OPTS[@]}" -o SendEnv=LC_ALL \
		-p "$SSH_PORT" -l deploy -- "$SSH_HOST" true || ok=1
	return $ok
}

group_ssh() {
	check ssh.failure-samples "OpenSSH stderr per failure class (auth, host key, network)" check_failure_samples
	check ssh.connect-timeout "Connect timeout to an unroutable address (network-dependent)" check_connect_timeout
	check ssh.exit-255 "Remote exit 255 looks like an SSH failure" check_exit_255_ambiguity
	check ssh.noise "Benign stderr: accept-new notice, locale warning" check_noise_samples
}
