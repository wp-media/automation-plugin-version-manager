# shellcheck shell=bash
#
# Group `ssh` — plan §3.5 "Stderr samples": what the local OpenSSH client prints for each
# failure class (feeds the P3.3 classifier), and the stderr noise a classifier must ignore.
# Every sample is saved to <results>/ssh-stderr/<name>.txt. `-F /dev/null` keeps the
# user's ~/.ssh/config out of the samples.
#
# Then the ssh_config facts the P3.3 transport is built on (plan P3.3 "Jump hosts"): what
# a jump host gets from the main connection, that a generated `-F` file reaches it, the
# precedence and quoting rules of that file, and why a known_hosts file that keeps
# nothing is refused. Checks that need a client home of their own (`~/.ssh` comes from
# the password database, never from HOME) run the target's own client, as a throwaway
# user created and removed by the check: the user's real ~/.ssh is never written.

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

# apvm_config FILE KNOWN_HOSTS STRICT KEY [LINE...] — writes a generated-style ssh_config:
# the P3.3 transport's options at top level (quoted paths), then each LINE.
apvm_config() {
	local file=$1 known=$2 strict=$3 key=$4 line
	shift 4
	{
		printf 'BatchMode yes\nStrictHostKeyChecking %s\nUserKnownHostsFile "%s"\n' "$strict" "$known"
		printf 'IdentityFile "%s"\nIdentitiesOnly yes\nConnectTimeout 5\nLogLevel INFO\nRequestTTY no\n' "$key"
		printf 'ControlMaster no\nControlPath none\n'
		for line in "$@"; do printf '%s\n' "$line"; done
	} >"$file"
}

# check_jump_host_options — this machine's client: a jump host gets only `-l -p -F -v` from
# the main connection; the options of a generated `-F` file reach it, `-o` options do not.
check_jump_host_options() {
	local ok=0 hop="deploy@$SSH_HOST:$SSH_PORT" dest_kh="$WORK_DIR/jump_dest_kh" both_kh="$WORK_DIR/jump_both_kh"
	local gen="$WORK_DIR/jump.conf" err proxy
	awk '{ print "localhost", $2, $3 }' "$KNOWN_HOSTS" >"$dest_kh"
	cat "$KNOWN_HOSTS" "$dest_kh" >"$both_kh"
	apvm_config "$gen" "$both_kh" yes "$KEY_FILE"
	err=$(ssh -v -F "$gen" -J "$hop" -l deploy -- localhost true </dev/null 2>&1)
	expect_eq "generated -F, both keys known: exit" "$?" 0 || ok=1
	# ssh ends its own lines with CR LF.
	proxy=$(grep 'Executing proxy command' <<<"$err" | tr -d '\r' | sed 's/^debug1: //')
	expect_eq "proxy command" "$proxy" \
		"Executing proxy command: exec ssh -l deploy -p $SSH_PORT -F $gen -v -W '[localhost]:22' $SSH_HOST" || ok=1
	# The hop's key missing from the generated file's known_hosts: the hop refuses with
	# the message only `StrictHostKeyChecking yes` prints, so it read the file.
	apvm_config "$gen" "$dest_kh" yes "$KEY_FILE"
	ssh_sample jump-generated-hop-unknown 255 \
		"No [A-Z0-9]+ host key is known for \\[$SSH_HOST\\]:$SSH_PORT and you have requested strict checking" \
		-F "$gen" -J "$hop" -l deploy -- localhost true || ok=1
	# The same options as `-o` (with -F /dev/null, so no ssh_config is read): the hop
	# fails although the -o known_hosts file holds its key and the -o key is accepted
	# (how depends on this machine's ~/.ssh: an unknown host key, or no usable key).
	ssh_sample jump-o-options 255 'Host key verification failed|Permission denied' \
		-F /dev/null -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile="$both_kh" \
		-i "$KEY_FILE" -o IdentitiesOnly=yes -J "$hop" -l deploy -- localhost true || ok=1
	return $ok
}

# check_jump_host_shell_startup — ssh runs a jump host's command through the user's $SHELL,
# which reads its startup file first (BASH_ENV here; zsh reads .zshenv, fish config.fish):
# one that changes directory makes a relative `-F` miss, while an absolute one still works.
check_jump_host_shell_startup() {
	local ok=0 hop="deploy@$SSH_HOST:$SSH_PORT" dir="$WORK_DIR/jump-startup" got
	mkdir -p "$dir"
	{
		cat "$KNOWN_HOSTS"
		awk '{ print "localhost", $2, $3 }' "$KNOWN_HOSTS"
	} >"$dir/kh"
	apvm_config "$dir/apvm-ssh.conf" "$dir/kh" yes "$KEY_FILE"
	printf 'cd /\n' >"$dir/startup.sh"
	got=$(cd "$dir" && SHELL=/bin/bash BASH_ENV="$dir/startup.sh" ssh -F apvm-ssh.conf -J "$hop" \
		-l deploy -- localhost 'echo RAN' </dev/null 2>&1 | tr -d '\r')
	expect_match "a relative -F misses after the startup file's cd" "$got" \
		"Can't open user config file apvm-ssh.conf" || ok=1
	got=$(cd "$dir" && SHELL=/bin/bash BASH_ENV="$dir/startup.sh" ssh -F "$dir/apvm-ssh.conf" \
		-J "$hop" -l deploy -- localhost 'echo RAN' </dev/null 2>&1 | tr -d '\r')
	expect_eq "an absolute -F still works" "$got" "RAN" || ok=1
	return $ok
}

# in_target_client SCRIPT — runs the file SCRIPT (lib/client_*.sh) with bash in the
# target, as a throwaway user created with its own home and removed afterwards, so its
# client is the target's OpenSSH and its ~/.ssh nobody else's. The harness key is in its
# ~/key and the target's host public keys in ~/host_keys. Sets IN_TARGET_OUT to the output
# (also noted); returns non-zero when the user cannot be set up or SCRIPT fails.
in_target_client() {
	local stage=/tmp/apvm-e2e-client rc
	target_exec sh -c "umask 077; rm -rf $stage; mkdir $stage && cat >$stage/key" <"$KEY_FILE" || return 1
	target_exec sh -c "cat >$stage/script.sh" <"$1" || return 1
	# shellcheck disable=SC2016 # expanded by the target's shell
	IN_TARGET_OUT=$(target_exec bash -c '
		user=apvm-e2e-client stage=$1 home=/home/apvm-e2e-client
		userdel -r "$user" >/dev/null 2>&1
		useradd --create-home --shell /bin/bash "$user" || exit 1
		install -o "$user" -m 0600 "$stage/key" "$home/key"
		install -o "$user" -m 0644 "$stage/script.sh" "$home/script.sh"
		cat /etc/ssh/ssh_host_*_key.pub >"$home/host_keys"
		rm -rf "$stage"
		su -s /bin/bash "$user" -c "cd && bash script.sh"
		rc=$?
		userdel -r "$user" >/dev/null 2>&1
		exit "$rc"' sh "$stage" 2>&1)
	rc=$?
	note "$(indent "$IN_TARGET_OUT")"
	return "$rc"
}

# expect_result LABEL NAME VALUE — expects the `RESULT NAME VALUE` line of the last
# in_target_client run.
expect_result() {
	expect_eq "$1" "$(sed -n "s/^RESULT $2 //p" <<<"$IN_TARGET_OUT")" "$3"
}

# check_jump_host_isolation — the target's client (OpenSSH 10.0p2), with a home of its
# own (lib/client_jump.sh): with `-o` options a jump host reads ~/.ssh (known_hosts, the
# default key) and prompts on a terminal; with a generated `-F` file it uses that file
# only, never prompts, and accept-new records both keys there.
check_jump_host_isolation() {
	local ok=0
	in_target_client "$E2E_DIR/lib/client_jump.sh" || return 1
	expect_result "with -o, the hop uses ~/.ssh" o-options-use-home 'yes' || ok=1
	expect_result "generated -F: the hop ignores ~/.ssh" generated-ignores-home-exit '255' || ok=1
	expect_result "generated -F: strict checking reached the hop" generated-strict-message 'yes' || ok=1
	expect_result "with -o, the hop prompts on a terminal" o-options-prompt 'yes' || ok=1
	expect_result "generated -F: no prompt" generated-no-prompt 'yes' || ok=1
	expect_result "accept-new through the hop runs" accept-new-ran 'yes' || ok=1
	expect_result "accept-new records both keys in the generated file's known_hosts" accept-new-recorded '127.0.0.1,localhost,' || ok=1
	expect_result "the client's own known_hosts untouched" home-untouched 'yes' || ok=1
	return $ok
}

# check_known_hosts_persistence — why apvm refuses a known_hosts file that keeps nothing
# (lib/client_known_hosts.sh): with accept-new and a link to /dev/null, or a file in a
# missing directory, ssh trusts any key for the call and runs the command.
check_known_hosts_persistence() {
	local ok=0
	in_target_client "$E2E_DIR/lib/client_known_hosts.sh" || return 1
	expect_result "a link to /dev/null: the command runs" dev-null-ran 'yes' || ok=1
	expect_result "a link to /dev/null: 'Permanently added', nothing kept" dev-null-claims-added 'yes' || ok=1
	expect_result "a missing directory: the command runs" missing-dir-ran 'yes' || ok=1
	expect_result "a missing directory: 'Failed to add the host'" missing-dir-failed-to-add 'yes' || ok=1
	return $ok
}

# check_generated_config_rules — this machine's client, `ssh -G` and one strict
# connection: top-level options beat an included `Host *` and `Match all`; a missing
# Include is silent; `-l`/`-p` beat ssh_config; `-F none -G` names ssh's directory through
# its compiled-in globalknownhostsfile; ssh ignores HOME; `%%` is a literal `%`, and
# quoted paths keep spaces, `#`, `'` and `=`.
check_generated_config_rules() {
	local ok=0 dir="$WORK_DIR/gen rules#'=x" got
	mkdir -p "$dir"
	cat >"$dir/user.conf" <<'EOF'
Host *
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  BatchMode no
  LogLevel QUIET
  RequestTTY force
  ControlMaster auto
  ForwardAgent yes
  User fromuserconf
Match all
  ConnectTimeout 99
EOF
	apvm_config "$dir/gen.conf" "$dir/kh" yes "$dir/k%%y" "ForwardAgent no" "ConnectTimeout 15" \
		"Include \"$dir/user.conf\"" "Include \"$dir/missing/none.conf\""
	got=$(ssh -F "$dir/gen.conf" -G somehost 2>/dev/null | grep -E '^(batchmode|stricthostkeychecking|userknownhostsfile|loglevel|requesttty|controlmaster|forwardagent|connecttimeout|user) ' | sort | tr '\n' '|')
	expect_eq "top-level options win, a missing Include is silent" "$got" \
		"batchmode yes|connecttimeout 5|controlmaster false|forwardagent no|loglevel INFO|requesttty false|stricthostkeychecking true|user fromuserconf|userknownhostsfile $dir/kh|" || ok=1
	got=$(ssh -F "$dir/gen.conf" -G -l cli -p 2722 somehost 2>/dev/null | grep -E '^(user|port) ' | sort | tr '\n' '|')
	expect_eq "-l and -p beat ssh_config" "$got" "port 2722|user cli|" || ok=1
	got=$(ssh -F none -G somehost 2>/dev/null | sed -n 's/^globalknownhostsfile //p')
	expect_match "-F none -G: the compiled-in globalknownhostsfile" "$got" '^/[^ ]*/ssh_known_hosts( |$)' || ok=1
	got=$(HOME="$WORK_DIR" ssh -F none -G somehost 2>/dev/null | sed -n 's/^userknownhostsfile //p')
	case $got in
	"$WORK_DIR"*) note "FAIL  ssh used HOME for ~: $got"; ok=1 ;;
	*) note "ok    ssh ignores HOME for ~: $got" ;;
	esac
	cp "$KEY_FILE" "$dir/k%y" && chmod 600 "$dir/k%y"
	awk '{ print $1, $2, $3 }' "$KNOWN_HOSTS" >"$dir/kh%1"
	apvm_config "$dir/conn.conf" "$dir/kh%%1" yes "$dir/k%%y"
	got=$(ssh -F "$dir/conn.conf" -p "$SSH_PORT" -l deploy -- "$SSH_HOST" 'echo escaped-ok' </dev/null 2>&1)
	expect_eq "%% and quoted paths: a strict connection" "$got" "escaped-ok" || ok=1
	return $ok
}

# agent_keys_after CONFIG SOCKET — connects strictly with CONFIG and an agent at SOCKET,
# then prints what `ssh-add -l` lists (its first line).
agent_keys_after() {
	SSH_AUTH_SOCK=$2 ssh -F "$1" -p "$SSH_PORT" -l deploy -- "$SSH_HOST" true </dev/null >/dev/null 2>&1
	SSH_AUTH_SOCK=$2 ssh-add -l 2>&1 | head -n 1
}

# check_agent_isolation — this machine's client and a throwaway ssh-agent: an ssh_config's
# `AddKeysToAgent yes` puts the key apvm names into the agent, unless the generated file
# says `AddKeysToAgent no` first (the first value wins).
check_agent_isolation() {
	local ok=0 dir agent_pid got
	# A short path: agent sockets are limited to about 100 bytes.
	dir=$(mktemp -d /tmp/apvm-e2e-agent.XXXXXX) || return 1
	printf 'Host *\n  AddKeysToAgent yes\n' >"$dir/user.conf"
	chmod 600 "$dir/user.conf"
	agent_pid=$(ssh-agent -a "$dir/agent.sock" -s | sed -n 's/^SSH_AGENT_PID=\([0-9]*\);.*/\1/p')
	if [ -z "$agent_pid" ]; then
		note "FAIL  ssh-agent did not start"
		rm -rf "$dir"
		return 1
	fi
	apvm_config "$dir/plain.conf" "$KNOWN_HOSTS" yes "$KEY_FILE" "Include \"$dir/user.conf\""
	got=$(agent_keys_after "$dir/plain.conf" "$dir/agent.sock")
	expect_match "without the pin, the key lands in the agent" "$got" 'apvm-e2e-harness' || ok=1
	SSH_AUTH_SOCK="$dir/agent.sock" ssh-add -D >/dev/null 2>&1
	apvm_config "$dir/pinned.conf" "$KNOWN_HOSTS" yes "$KEY_FILE" "AddKeysToAgent no" \
		"Include \"$dir/user.conf\""
	got=$(agent_keys_after "$dir/pinned.conf" "$dir/agent.sock")
	expect_eq "with AddKeysToAgent no first, the agent stays empty" "$got" "The agent has no identities." || ok=1
	kill "$agent_pid" 2>/dev/null
	rm -rf "$dir"
	return $ok
}

# check_system_config_include — the target's client, as root (lib/client_system_config.sh):
# ssh reads the system-wide ssh_config unchecked on its own, but checks it like any other
# file once a generated `-F` file includes it (another owner, or a group that may write it:
# refused), and an Include of a file the user cannot read fails where ssh alone skips it.
# Why apvm checks that file before it includes it, and leaves an unreadable one out. Also
# Debian's exception for a file its owner's private group may write (apvm refuses it).
check_system_config_include() {
	local ok=0 refused='Bad owner or permissions on /etc/ssh/ssh_config'
	IN_TARGET_OUT=$(target_exec bash -s <"$E2E_DIR/lib/client_system_config.sh" 2>&1) || {
		note "$(indent "$IN_TARGET_OUT")"
		return 1
	}
	note "$(indent "$IN_TARGET_OUT")"
	expect_result "another owner: ssh alone reads it" other-owner-alone 127.0.0.9 || ok=1
	expect_result "another owner: included, refused" other-owner-included "$refused" || ok=1
	expect_result "group-writable: ssh alone reads it" group-writable-alone 127.0.0.9 || ok=1
	expect_result "group-writable: included, refused" group-writable-included "$refused" || ok=1
	expect_result "unreadable: ssh alone skips it" unreadable-alone apvm-sysalias || ok=1
	expect_result "unreadable: included, the call fails" unreadable-included \
		"gen line 1: Can't open user config file /etc/ssh/ssh_config: Permission denied" || ok=1
	# Debian's patch, not upstream's rule (which apvm keeps): the user's own group may write.
	expect_result "own file, private group may write: included (Debian)" \
		private-group-writable-included 127.0.0.8 || ok=1
	expect_result "own file, others may write: included, refused" other-writable-included \
		"Bad owner or permissions on /home/apvm-e2e-sysconf/own" || ok=1
	return $ok
}

# check_multiplexing — the target's client (OpenSSH 10.0p2, Linux), with a home of its own
# (lib/client_mux.sh): the P3.4 transport's facts. A control socket path of 90 bytes binds
# and one of 91 fails the call; the first call ends with its output while its master lives
# on (`-O check`); later calls share its connection; a stopped master makes a call wait
# and fall back; after `-O stop` the next call starts a new master; past MaxSessions a call
# opens its own connection after two lines the classifier skips; a `-N` starter leaves a
# live master; with `ControlPersist`, a jump host's failure loses its reason.
check_multiplexing() {
	local ok=0
	in_target_client "$E2E_DIR/lib/client_mux.sh" || return 1
	expect_result "a 90-byte socket path binds" socket-path-90-exit 0 || ok=1
	expect_result "a 91-byte socket path fails the call" socket-path-91-exit 255 || ok=1
	expect_result "… saying it is too long" socket-path-91-says-too-long yes || ok=1
	expect_result "the first call ends with its output" first-call-exit 0 || ok=1
	expect_result "its master is live after it" master-live yes || ok=1
	expect_result "a later call shares its connection" same-connection yes || ok=1
	expect_result "a stopped master: the call still runs" stopped-master-exit 0 || ok=1
	expect_result "… after 'master hello exchange failed'" stopped-master-line yes || ok=1
	expect_result "-O stop is accepted" stop-exit 0 || ok=1
	expect_result "after -O stop, a new connection" after-stop-new-connection yes || ok=1
	expect_result "eleven calls at once all run" max-sessions-failed 0 || ok=1
	expect_result "the refused session's line" max-sessions-refused-line yes || ok=1
	expect_result "the fallback's line" max-sessions-fallback-line yes || ok=1
	expect_result "a -N starter exits 0" starter-exit 0 || ok=1
	expect_result "… and leaves its master live" starter-master-live yes || ok=1
	expect_result "unknown jump host key, plain: fails" jump-plain-exit 255 || ok=1
	expect_result "… and says why" jump-plain-says-why yes || ok=1
	expect_result "unknown jump host key, ControlPersist: fails" jump-shared-exit 255 || ok=1
	expect_result "… and the reason is lost" jump-shared-hides-why yes || ok=1
	return $ok
}

group_ssh() {
	check ssh.failure-samples "OpenSSH stderr per failure class (auth, host key, network)" check_failure_samples
	check ssh.connect-timeout "Connect timeout to an unroutable address (network-dependent)" check_connect_timeout
	check ssh.exit-255 "Remote exit 255 looks like an SSH failure" check_exit_255_ambiguity
	check ssh.noise "Benign stderr: accept-new notice, locale warning" check_noise_samples
	check ssh.jump-host-options "Jump hosts get -l -p -F -v; a generated -F reaches them" check_jump_host_options
	check ssh.jump-host-shell-startup "Jump hosts: a shell startup that cds breaks a relative -F" check_jump_host_shell_startup
	check ssh.jump-host-isolation "Jump hosts: ~/.ssh and prompts with -o, never with -F" check_jump_host_isolation
	check ssh.known-hosts-persistence "accept-new with a known_hosts that keeps nothing runs" check_known_hosts_persistence
	check ssh.generated-config-rules "Generated ssh_config: precedence, quoting, defaults" check_generated_config_rules
	check ssh.agent-isolation "An ssh_config's AddKeysToAgent cannot add apvm's key" check_agent_isolation
	check ssh.system-config-include "The system ssh_config: unchecked alone, checked once included" check_system_config_include
	check ssh.multiplexing "Multiplexing: socket path limit, a live master, MaxSessions, proxies" check_multiplexing
}
