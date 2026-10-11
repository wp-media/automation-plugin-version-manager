# shellcheck shell=bash
# The remote shell, not this one, expands the $… inside single-quoted command lines.
# shellcheck disable=SC2016
#
# Group `sudo` — plan §3.5 "sudo": the exact refusal messages, classification with
# `sudo -n -l`, `-u U -g G`, the root-login `runuser` prefix (§6.1 item 4), the
# environment and umask identities get, and what sudo logs about refused probes.

# sudo_case LOGIN COMMAND [ERE] — runs a sudo COMMAND as LOGIN with stdin closed and notes
# the result; with ERE, returns 1 unless the script ran and stderr matches ERE.
sudo_case() {
	run_remote "$1" "$2 </dev/null"
	note "$1\$ $2  → exit $RUN_RC"
	note "    stderr: ${RUN_ERR:-<empty>}"
	[ -z "${3:-}" ] && return 0
	expect_ran "$1\$ $2" || return 1
	grep -Eq -- "$3" <<<"$RUN_ERR" && return 0
	note "    FAIL: expected stderr matching /$3/"
	return 1
}

# refused LOGIN COMMAND ERE — sudo_case that also requires exit status 1.
refused() {
	sudo_case "$1" "$2" "$3" || return 1
	expect_eq "    exit" "$RUN_RC" 1
}

# check_sudo_messages — what `sudo -n` prints for each refusal class (feeds §4.5 codes).
# sudo authenticates before it applies the policy, so with -n every refusal except
# requiretty reads "a password is required", even "not in sudoers" or "not allowed".
check_sudo_messages() {
	local ok=0
	refused plain 'sudo -n true' '^sudo: a password is required$' || ok=1
	refused pwsudo 'sudo -n true' '^sudo: a password is required$' || ok=1
	refused ttyreq 'sudo -n -u www-data true' '^sudo: sorry, you must have a tty to run sudo$' || ok=1
	# deploy has least-privilege NOPASSWD rules; commands outside them look the same.
	refused deploy 'sudo -n true' '^sudo: a password is required$' || ok=1
	refused deploy 'sudo -n -u root id' '^sudo: a password is required$' || ok=1
	refused deploy 'sudo -n -u ftpuser -g root true' '^sudo: a password is required$' || ok=1
	return $ok
}

# check_sudo_classification — `sudo -n -l [-u U -g G] CMD` separates the refusal classes
# (§6.2): allowed (prints the command path), not allowed for this target although the user
# has password-less rules (exit 1, silent), no password-less rule (password required). It
# also succeeds for a rule that needs a password (listpw=any), which the real run refuses.
check_sudo_classification() {
	local ok=0
	sudo_case deploy 'sudo -n -l -u www-data -g www-data true' || ok=1
	expect_eq "allowed target: exit, stdout" "$RUN_RC:$RUN_OUT" "0:/usr/bin/true" || ok=1
	sudo_case deploy 'sudo -n -l -u ftpuser -g www-data true' || ok=1
	expect_eq "allowed via group membership: exit, stdout" "$RUN_RC:$RUN_OUT" "0:/usr/bin/true" || ok=1
	sudo_case deploy 'sudo -n -l chown' || ok=1
	expect_eq "allowed root command: exit, stdout" "$RUN_RC:$RUN_OUT" "0:/usr/bin/chown" || ok=1
	sudo_case deploy 'sudo -n -l -u root id' || ok=1
	expect_eq "not allowed (has NOPASSWD rules): exit, stdout, stderr" "$RUN_RC:$RUN_OUT:$RUN_ERR" "1::" || ok=1
	sudo_case deploy 'sudo -n -l -u ftpuser -g root true' || ok=1
	expect_eq "group the target user lacks: exit, stdout, stderr" "$RUN_RC:$RUN_OUT:$RUN_ERR" "1::" || ok=1
	refused plain 'sudo -n -l -u www-data true' '^sudo: a password is required$' || ok=1
	refused pwsudo 'sudo -n -l -u www-data true' '^sudo: a password is required$' || ok=1
	refused ttyreq 'sudo -n -l -u www-data true' '^sudo: sorry, you must have a tty to run sudo$' || ok=1
	# mixed: a password rule for www-data next to a NOPASSWD rule for ftpuser:www-data.
	sudo_case mixed 'sudo -n -l -u www-data true' || ok=1
	expect_eq "rule needing a password: -l exit, stdout" "$RUN_RC:$RUN_OUT" "0:/usr/bin/true" || ok=1
	refused mixed 'sudo -n -u www-data true' '^sudo: a password is required$' || ok=1
	sudo_case deploy 'sudo -n -l' || ok=1
	note_block "sudo -n -l (deploy)" "$RUN_OUT"
	return $ok
}

# check_owner_and_group — `sudo -n -u U -g G` gives owner and primary group, with the
# plain (ftpuser) Runas form (deploy) and the explicit (ftpuser : www-data) form (mixed).
check_owner_and_group() {
	local ok=0 show='sh -c '\''echo "$(id -un):$(id -gn)"'\'' </dev/null'
	run_remote deploy "sudo -n -u ftpuser -g www-data -- $show"
	expect_eq "deploy, (ftpuser): -u ftpuser -g www-data" "$RUN_OUT" "ftpuser:www-data" || ok=1
	run_remote mixed "sudo -n -u ftpuser -g www-data -- $show"
	expect_eq "mixed, (ftpuser : www-data): -u ftpuser -g www-data" "$RUN_OUT" "ftpuser:www-data" || ok=1
	run_remote deploy "sudo -n -u www-data -g www-data -- $show"
	expect_eq "deploy: -u www-data -g www-data" "$RUN_OUT" "www-data:www-data" || ok=1
	run_remote deploy "sudo -n -u ftpuser -- $show"
	expect_eq "deploy: -u ftpuser (no -g)" "$RUN_OUT" "ftpuser:ftpuser" || ok=1
	return $ok
}

# check_runuser — a root login switches identity with `runuser -u U -g G --`.
check_runuser() {
	local ok=0 show='sh -c '\''echo "$(id -un):$(id -gn)"'\'' </dev/null'
	run_remote root "runuser -u ftpuser -g www-data -- $show"
	expect_eq "runuser -u ftpuser -g www-data" "$RUN_OUT" "ftpuser:www-data" || ok=1
	run_remote root "runuser -u www-data -g www-data -- $show"
	expect_eq "runuser -u www-data -g www-data" "$RUN_OUT" "www-data:www-data" || ok=1
	return $ok
}

# check_env_and_umask — the environment and umask each identity path gets: HOME matters to
# WP-CLI, the umask to group bits and setgid (perms.install-matrix), LC_ALL to message
# matching (the plan's scripts set LC_ALL=C before sudo).
check_env_and_umask() {
	local ok=0 login
	as_identity single 'sh -c '\''echo "umask=$(umask) HOME=$HOME LC_ALL=$LC_ALL"'\'''
	expect_eq "sudo -u www-data -g www-data" "$RUN_OUT" "umask=0002 HOME=/var/www LC_ALL=C" || ok=1
	as_identity ftpsite 'sh -c '\''echo "umask=$(umask) HOME=$HOME LC_ALL=$LC_ALL"'\'''
	expect_eq "sudo -u ftpuser -g www-data" "$RUN_OUT" "umask=0002 HOME=/home/ftpuser LC_ALL=C" || ok=1
	for login in deploy ftpuser fishy; do
		run_remote "$login" 'umask'
		expect_eq "$login login: umask" "$RUN_OUT" 0002 || ok=1
	done
	run_remote root 'runuser -u ftpuser -g www-data -- sh -c umask </dev/null'
	expect_eq "root login, runuser -u ftpuser -g www-data: umask" "$RUN_OUT" 0022 || ok=1
	return $ok
}

# check_audit_trail — refused `sudo -n` probes leave entries in sudo's log (here a file;
# syslog/journald on a real server), which operators and intrusion detection will see.
check_audit_trail() {
	local before after
	before=$(target_exec sh -c 'wc -l </var/log/apvm-e2e-sudo.log 2>/dev/null || echo 0')
	run_remote plain 'sudo -n true </dev/null'
	run_remote deploy 'sudo -n -u root id </dev/null'
	after=$(target_exec sh -c 'wc -l </var/log/apvm-e2e-sudo.log')
	note_block "new log entries" "$(target_exec tail -n "$((after - before))" /var/log/apvm-e2e-sudo.log)"
	expect_match "two refused probes are logged" "$((after - before))" '^[2-9]$|^[1-9][0-9]+$'
}

group_sudo() {
	check sudo.messages "sudo -n refusal messages (password, sudoers, tty, runas)" check_sudo_messages
	check sudo.classification "sudo -n -l separates the refusal classes" check_sudo_classification
	check sudo.owner-and-group "sudo -n -u U -g G sets owner and primary group" check_owner_and_group
	check sudo.runuser "root login: runuser -u U -g G" check_runuser
	check sudo.env-and-umask "Environment, umask and LC_ALL per identity path" check_env_and_umask
	check sudo.audit-trail "Refused sudo probes are logged" check_audit_trail
}
