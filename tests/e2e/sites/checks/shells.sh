# shellcheck shell=bash
# The remote shell, not this one, expands the $… inside single-quoted command lines.
# shellcheck disable=SC2016
#
# Group `shells` — plan §3.5 "Shells and SSH": stdin delivery (`sh -s`), dotfile noise,
# upload-mode quoting (`sh -c '…'`) under every login shell, and the ssh argv shape.

# Login users per login shell: bash, dash, zsh, fish, tcsh.
SHELL_LOGINS="deploy dashy zshy fishy tcshy"
# Printable ASCII specials allowed in upload-mode values. Backslash and single quote are
# excluded: inside single quotes, fish reads `\\` and `\'` as escapes, and quoting a `'`
# (`'\''`) puts a backslash inside the outer quotes (see check_sh_c_quoting).
QUOTING_SPECIALS=' !"#$%&()*+,-./:;<=>?@[]^_`{|}~'

# check_stdin_delivery — a script on stdin behaves the same under dash, bash and busybox
# ash when every command has </dev/null. Without it, under bash (which reads a piped script
# unbuffered) a command eats the rest of the script; dash and ash have buffered it.
check_stdin_delivery() {
	local ok=0 shell body eaten
	body='cat </dev/null
head -c 1 </dev/null
echo after-1
sh -c '\''exit 255'\'' </dev/null; rc=$?; [ "$rc" -eq 255 ] && rc=254; echo "mapped=$rc"
exit "$rc"'
	for shell in sh bash 'busybox ash'; do
		run_remote_with deploy "$shell -s" "$body"
		expect_eq "$shell -s: stdout, exit" "$(printf '%s' "$RUN_OUT" | tr '\n' ' '), exit $RUN_RC" \
			"after-1 mapped=254, exit 254" || ok=1
		run_remote_with deploy "$shell -s" 'cat
echo the-shell-ran-this'
		if [ "$shell" = bash ]; then eaten='echo the-shell-ran-this'; else eaten='the-shell-ran-this'; fi
		expect_eq "$shell -s, bare cat (no </dev/null): stdout" "$RUN_OUT" "$eaten" || ok=1
	done
	return $ok
}

# check_bashrc_noise — dotfile output lands before the nonce, so framing discards it.
check_bashrc_noise() {
	local ok=0 nonce raw
	run_remote noisy 'echo payload'
	note_block "raw stdout" "$RUN_RAW"
	note_block "stderr" "$RUN_ERR"
	expect_match "noise on stdout" "$RUN_RAW" '^apvm-e2e: stdout noise' || ok=1
	expect_match "noise on stderr" "$RUN_ERR" '^apvm-e2e: stderr noise' || ok=1
	expect_eq "stdin mode: stdout after the nonce" "$RUN_OUT" payload || ok=1
	nonce=$(new_nonce)
	raw=$(ssh_to noisy "sh -c $(q "echo $nonce; echo payload")" </dev/null 2>/dev/null)
	expect_eq "upload mode: stdout after the nonce" "$(after_nonce "$nonce" "$raw")" payload || ok=1
	return $ok
}

# quoting_roundtrip LOGIN VALUE — prints what `sh -c '<printf VALUE>'` sent as one remote
# command prints under LOGIN's login shell (upload mode, §6.1 item 3).
quoting_roundtrip() {
	local script
	script="printf '%s\\n' $(q "$2") end"
	ssh_to "$1" "sh -c $(q "$script")" </dev/null 2>&1
}

# check_sh_c_quoting — allowlisted single-quoted values survive bash, dash, zsh, fish and
# tcsh login shells (on a mismatch, finds the characters at fault). A `'` and backslashes
# survive every shell but fish, which is why the upload allowlist excludes them.
check_sh_c_quoting() {
	local ok=0 login value got i char bad quote slashes
	value="a${QUOTING_SPECIALS}"$'\t'"é z"
	for login in $SHELL_LOGINS; do
		got=$(quoting_roundtrip "$login" "$value")
		if [ "$got" = "$value"$'\n'"end" ]; then
			note "ok    $login: every special character, tab and UTF-8 survive"
			continue
		fi
		ok=1
		bad=""
		i=0
		while [ $i -lt ${#QUOTING_SPECIALS} ]; do
			char=${QUOTING_SPECIALS:$i:1}
			[ "$(quoting_roundtrip "$login" "x${char}y")" = "x${char}y"$'\n'"end" ] || bad="${bad}[$char]"
			i=$((i + 1))
		done
		note "FAIL  $login: characters that break: ${bad:-none alone (combination)}; output: $got"
	done
	for login in $SHELL_LOGINS; do
		quote=$(quoting_roundtrip "$login" "it's" | head -n 1)
		slashes=$(quoting_roundtrip "$login" 'a\b\\c' | head -n 1)
		if [ "$login" = fishy ]; then
			expect_match "fish: a quoted ' breaks the command" "$quote" 'quotes are not balanced' || ok=1
			expect_eq "fish: backslashes" "$slashes" 'a\b\c' || ok=1
		else
			expect_eq "$login: \"it's\" and 'a\\b\\\\c' unchanged" "$quote $slashes" "it's "'a\b\\c' || ok=1
		fi
	done
	return $ok
}

# check_upload_mode — the staging upload (stdin → sh -c → sudo tee, then SHA-256) works
# through every login shell.
check_upload_mode() {
	local ok=0 login file prefix
	file="$WORK_DIR/upload.bin"
	python3 -c 'import os, sys; open(sys.argv[1], "wb").write(os.urandom(4 << 20))' "$file"
	prefix=$(identity_prefix single)
	for login in $SHELL_LOGINS; do
		if with_identity "$login" "$prefix" stage_zip single "$file"; then
			note "ok    $login: 4 MiB, SHA-256 equal"
		else
			note "FAIL  $login"
			ok=1
		fi
	done
	return $ok
}

# check_ssh_argv_shape — the argv apvm builds (options, `-l USER`, `--`, host, command).
check_ssh_argv_shape() {
	local out rc
	out=$(printf 'echo ok\n' | ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -i "$KEY_FILE" \
		-o UserKnownHostsFile="$KNOWN_HOSTS" -o StrictHostKeyChecking=yes -o ConnectTimeout=10 \
		-p "$SSH_PORT" -l deploy -- "$SSH_HOST" 'sh -s' 2>&1)
	rc=$?
	expect_eq "ssh [options] -l deploy -- 127.0.0.1 'sh -s'" "$rc:$out" "0:ok"
}

group_shells() {
	check shells.stdin-delivery "sh -s + </dev/null: same on dash, bash, busybox ash" check_stdin_delivery
	check shells.bashrc-noise ".bashrc noise stays before the nonce" check_bashrc_noise
	check shells.sh-c-quoting "sh -c '…' quoting under bash/dash/zsh/fish/tcsh" check_sh_c_quoting
	check shells.upload-mode "Upload via sh -c + sudo tee under every login shell" check_upload_mode
	check shells.ssh-argv "ssh … -- host cmd is accepted" check_ssh_argv_shape
}
