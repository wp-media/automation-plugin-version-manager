# shellcheck shell=bash
# The remote shell, not this one, expands the $… inside single-quoted command lines.
# shellcheck disable=SC2016
#
# Group `harness` — plan §3.5 "Harness basics", plus the versions under test.

# check_versions — records what the harness runs (INFO).
check_versions() {
	wp_run single eval 'echo "WP-CLI ", WP_CLI_VERSION, " · PHP ", PHP_VERSION, " · WordPress ", $GLOBALS["wp_version"];' \
		--skip-plugins --skip-themes || {
		note_run "wp eval"
		return 1
	}
	note "$RUN_OUT"
	run_remote deploy 'sudo -V | head -n 1; ssh -V 2>&1; . /etc/os-release; echo "$PRETTY_NAME"; mariadb --version'
	note "$RUN_OUT"
	return 2
}

# check_image_ownership — the official image's WordPress files belong to www-data.
check_image_ownership() {
	run_remote deploy "stat -c '%U:%G %a %n' /usr/src/wordpress /usr/src/wordpress/index.php \
		/usr/src/wordpress/wp-content/plugins $(plugins_dir single) $(plugins_dir multi)" || {
		note_run stat
		return 1
	}
	note "$RUN_OUT"
	expect_eq "entries not owned by www-data:www-data" "$(grep -cv '^www-data:www-data ' <<<"$RUN_OUT")" 0
}

# check_stat_fallback — a login user that cannot reach the plugins dir needs root `stat`
# (§6.2 step H): ftpsite lives in a 0750 ftpuser:www-data home.
check_stat_fallback() {
	local ok=0 plugins
	plugins=$(plugins_dir ftpsite)
	run_remote deploy "stat -c '%U:%G %a' $(q "$plugins")"
	expect_ran "stat as deploy" || ok=1
	expect_match "stat as deploy" "$RUN_ERR" "cannot statx? '.*': Permission denied" || ok=1
	run_remote deploy "sudo -n stat -c '%U:%G %a' $(q "$plugins")"
	expect_eq "sudo -n stat (root)" "$RUN_OUT" "ftpuser:www-data 2775" || ok=1
	return $ok
}

# check_wp_as_www_data — WP-CLI as www-data (HOME=/var/www, owned by root): normal work
# prints no warnings; only commands that need WP-CLI's cache dir warn.
check_wp_as_www_data() {
	local ok=0
	reset_sites single
	wp_run single eval 'echo getenv("HOME");' --skip-plugins --skip-themes
	expect_eq "HOME seen by WP-CLI" "$RUN_OUT" /var/www || ok=1
	plugin_rows single >/dev/null
	expect_eq "plugin list: exit, stderr" "$RUN_RC:$RUN_ERR" "0:" || ok=1
	if install_fixture single basic; then
		expect_eq "plugin install <zip>: stderr" "$RUN_ERR" "" || ok=1
	else
		ok=1
	fi
	wp_run single cli cache clear
	note_run "wp cli cache clear"
	expect_eq "cli cache clear: exit" "$RUN_RC" 1 || ok=1
	expect_match "cli cache clear: warning" "$RUN_ERR" "Failed to create directory '/var/www/.wp-cli/cache/'" || ok=1
	return $ok
}

# check_mktemp_shape — `mktemp -d /tmp/apvm.XXXXXXXXXX` as each identity: name and mode (§6.6).
check_mktemp_shape() {
	local ok=0 site prefix
	for site in single ftpsite; do
		prefix=$(identity_prefix "$site")
		run_remote deploy "d=\$($prefix mktemp -d /tmp/apvm.XXXXXXXXXX </dev/null) &&
			stat -c '%n %U:%G %a' \"\$d\" && $prefix rmdir -- \"\$d\" </dev/null"
		expect_match "$site" "$RUN_OUT" \
			"^/tmp/apvm\.[A-Za-z0-9]{10} $(site_field "$site" owner):$(site_field "$site" group) 700$" || ok=1
	done
	return $ok
}

# check_root_repair_guards — the hardened root repair of plan §6.8 (opt-in rung 2):
# `chown -R -P -h --from=<foreign uid>` on an absolute path changes only entries owned by
# that uid and never follows links; its preconditions (root `stat` of the slug, no
# multi-linked files) detect a symlinked slug and a hardlinked file.
check_root_repair_guards() {
	local ok=0 plugins prefix tree from to
	reset_sites single
	plugins=$(plugins_dir single)
	tree="$plugins/apvm-repair-probe"
	prefix=$(identity_prefix single)
	# The plugin's own files (www-data), links out of the tree, a hardlink inside it, and
	# a root-owned file that is not the repair's business.
	run_remote deploy "cd -- $(q "$plugins") &&
		$prefix mkdir -p apvm-repair-probe/sub </dev/null &&
		$prefix tee apvm-repair-probe/file </dev/null >/dev/null &&
		$prefix ln apvm-repair-probe/file apvm-repair-probe/sub/hardlink </dev/null &&
		$prefix ln -s /usr/local/share/apvm-e2e/sentinel apvm-repair-probe/link </dev/null &&
		$prefix ln -s /usr/local/share/apvm-e2e apvm-repair-probe/dirlink </dev/null &&
		$prefix ln -s apvm-repair-probe apvm-repair-alias </dev/null" || {
		note_run "setup"
		return 1
	}
	target_exec sh -c "touch $(q "$tree/sub/root-owned") && chown root:root $(q "$tree/sub/root-owned")" || return 1

	# Preconditions, as the plan runs them.
	run_remote deploy "sudo -n stat -c %F -- $(q "$tree") $(q "$plugins/apvm-repair-alias")"
	expect_eq "root stat %F: real slug, symlinked slug" "$(tr '\n' '/' <<<"$RUN_OUT")" "directory/symbolic link/" || ok=1
	as_identity single "find $(q "$tree") ! -type d -links +1 -print"
	expect_eq "multi-linked files found" "$(grep -c . <<<"$RUN_OUT")" 2 || ok=1
	run_remote deploy 'cat /proc/sys/fs/protected_hardlinks'
	note "fs.protected_hardlinks on this kernel: $RUN_OUT"

	# The repair itself: www-data-owned entries become ftpuser:www-data.
	from=$(target_exec id -u www-data) to="$(target_exec id -u ftpuser):$(target_exec id -g www-data)"
	run_remote deploy "sudo -n chown -R -P -h --from=$from -- $to $(q "$tree") </dev/null"
	expect_eq "chown --from: exit" "$RUN_RC" 0 || ok=1
	run_remote deploy "cd -- $(q "$tree") && stat -c '%U:%G %n' . sub file sub/hardlink link dirlink sub/root-owned &&
		stat -c '%U:%G %n' /usr/local/share/apvm-e2e /usr/local/share/apvm-e2e/sentinel"
	note_block "owners afterwards (links: the link itself)" "$RUN_OUT"
	expect_eq "www-data entries changed (incl. links)" "$(grep -c '^ftpuser:www-data ' <<<"$RUN_OUT")" 6 || ok=1
	expect_eq "root-owned entry kept by --from" "$(grep -c '^root:root sub/root-owned$' <<<"$RUN_OUT")" 1 || ok=1
	expect_eq "link targets untouched" "$(grep -c '^root:root /usr/local/share/apvm-e2e' <<<"$RUN_OUT")" 2 || ok=1
	return $ok
}

group_harness() {
	check harness.versions "Versions under test" check_versions
	check harness.image-ownership "Official wordpress image files are owned by www-data" check_image_ownership
	check harness.stat-fallback "Unreachable plugins dir: stat fails, root stat works" check_stat_fallback
	check harness.wp-as-www-data "WP-CLI as www-data: HOME and warnings" check_wp_as_www_data
	check harness.mktemp "mktemp -d /tmp/apvm.XXXXXXXXXX: name, owner, 0700" check_mktemp_shape
	check harness.root-repair-guards "Hardened root repair: --from, no links, preconditions" check_root_repair_guards
}
