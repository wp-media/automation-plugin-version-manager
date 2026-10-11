# shellcheck shell=bash
# The remote shell, not this one, expands the $… inside single-quoted command lines.
# shellcheck disable=SC2016
#
# Group `wpcli` — plan §3.5 "WP-CLI 2.12.0", plus the WP-CLI and WordPress behaviors the
# planner and executor rely on (§3.2, §3.3, §6.1, §6.4, §6.14). Options read back through
# the database are strings, so counters compare as JSON strings ("1"). Every group starts
# from freshly reset sites (verify.sh), and every check that changes a site resets it first.

CRITICAL_ERROR='^Error: There has been a critical error on this website'

# check_wpcli_version — WP-CLI 2.12.0, and WP_CLI_VERSION visible inside `wp eval`.
check_wpcli_version() {
	local ok=0
	wp_run single cli version
	expect_eq "wp cli version" "$RUN_OUT" "WP-CLI 2.12.0" || ok=1
	wp_run single eval 'echo WP_CLI_VERSION;' --skip-plugins --skip-themes
	expect_eq "WP_CLI_VERSION in wp eval" "$RUN_OUT" 2.12.0 || ok=1
	return $ok
}

# check_list_read — the plan's read (§6.4) works and reports every status, with the row
# shapes the P4.1 parser must handle: a single-file plugin, a must-use row (name without
# `.php`) and a drop-in row (name with `.php`, empty version).
check_list_read() {
	local rows out rc
	rows=$(plugin_rows single) || {
		note_run "wp plugin list"
		return 1
	}
	out=$(ROWS=$rows python3 - <<'PY'
import json, os, sys
rows = json.loads(os.environ["ROWS"])
by_name = {r["name"]: r for r in rows}
failures = []
for r in rows:
    print("%-10s %-20s %-26s %-6s %s" % (r["status"], r["name"], r["file"], r["version"], r["title"]))
    if set(r) != {"name", "status", "version", "file", "title"}:
        failures.append("unexpected fields: %s" % sorted(r))
missing = {"inactive", "must-use", "dropin"} - {r["status"] for r in rows}
if missing:
    failures.append("statuses missing: %s" % sorted(missing))
expected = {
    "hello": ("inactive", "hello.php"),
    "apvm-e2e-mu": ("must-use", "apvm-e2e-mu.php"),
    "advanced-cache.php": ("dropin", "advanced-cache.php"),
}
for name, (status, file) in expected.items():
    row = by_name.get(name)
    if not row or (row["status"], row["file"]) != (status, file):
        failures.append("%s: expected %s %s, got %s" % (name, status, file, row))
if by_name.get("advanced-cache.php", {}).get("version", None) != "":
    failures.append("the drop-in row should have an empty version")
for failure in failures:
    print("FAIL  " + failure)
sys.exit(1 if failures else 0)
PY
	)
	rc=$?
	note_block "rows (status name file version title)" "$out"
	return $rc
}

# check_uninstall_skip_delete — `uninstall --skip-delete` runs uninstall.php, keeps files.
check_uninstall_skip_delete() {
	local ok=0
	reset_sites single
	install_fixture single basic || return 1
	{ wp_run single plugin activate basic && wp_run single plugin deactivate basic; } || {
		note_run "activate + deactivate"
		return 1
	}
	expect_eq "counters before" "$(option_json single apvm_fx_basic_activations) $(option_json single apvm_fx_basic_deactivations)" '"1" "1"' || ok=1
	wp_run single plugin uninstall basic --skip-delete
	note_run "wp plugin uninstall basic --skip-delete"
	expect_eq "exit" "$RUN_RC" 0 || ok=1
	expect_eq "status afterwards (files kept)" "$(plugin_status single basic)" inactive || ok=1
	expect_eq "counters after (uninstall.php ran)" "$(option_json single apvm_fx_basic_activations) $(option_json single apvm_fx_basic_deactivations)" "absent absent" || ok=1
	return $ok
}

# check_uninstall_active — `uninstall` refuses an active plugin (§3.2) and changes nothing.
check_uninstall_active() {
	local ok=0
	reset_sites single
	{ install_fixture single basic && wp_run single plugin activate basic; } || return 1
	wp_run single plugin uninstall basic
	note_run "wp plugin uninstall basic (active)"
	expect_eq "exit" "$RUN_RC" 1 || ok=1
	expect_eq "stderr" "$RUN_ERR" "Warning: The 'basic' plugin is active."$'\n'"Error: No plugins uninstalled." || ok=1
	expect_eq "status afterwards" "$(plugin_status single basic)" active || ok=1
	return $ok
}

# check_force_over_active — `install --force` over an active plugin keeps it active, without
# running the activation hook (like an update).
check_force_over_active() {
	local ok=0
	reset_sites single
	{ install_fixture single basic && wp_run single plugin activate basic; } || return 1
	install_zip single "$(fixture_zip basic "" 1.0.1)" --force || return 1
	note_run "wp plugin install basic~1.0.1.zip --force"
	expect_eq "status, version" "$(plugin_status single basic) $(plugin_field single basic version)" "active 1.0.1" || ok=1
	expect_eq "activation hook runs (counter)" "$(option_json single apvm_fx_basic_activations)" '"1"' || ok=1
	return $ok
}

# check_exit_codes — WP_CLI::error exits 1. A PHP fatal exits 255 (254 once framed) only
# before WordPress loads: afterwards WordPress's fatal-error handler turns it into
# "There has been a critical error on this website." and exit 1. Batch errors carry
# WP-CLI's summary line and exit 1.
check_exit_codes() {
	local ok=0 throw='throw new RuntimeException("apvm-e2e");'
	reset_sites single
	wp_run single eval 'WP_CLI::error("apvm-e2e");' --skip-plugins --skip-themes
	expect_eq "WP_CLI::error: exit, stderr" "$RUN_RC: $RUN_ERR" "1: Error: apvm-e2e" || ok=1
	run_remote deploy "cd / && $(identity_prefix single) $(wp_cmdline single eval "$throw" --skip-wordpress) </dev/null"
	expect_ran "fatal before WordPress loads, unframed" || ok=1
	expect_eq "fatal before WordPress loads, unframed: ssh exit" "$RUN_RC" 255 || ok=1
	wp_run single eval "$throw" --skip-wordpress
	expect_eq "fatal before WordPress loads, framed (255 → 254)" "$RUN_RC" 254 || ok=1
	wp_run single eval "$throw" --skip-plugins --skip-themes
	expect_eq "fatal after WordPress loaded: exit" "$RUN_RC" 1 || ok=1
	expect_match "fatal after WordPress loaded: message" "$(tail -n 1 <<<"$RUN_ERR")" "$CRITICAL_ERROR" || ok=1
	wp_run single plugin activate hello no-such-plugin
	note_run "wp plugin activate hello no-such-plugin"
	expect_eq "partial batch: exit" "$RUN_RC" 1 || ok=1
	expect_eq "partial batch: stderr" "$RUN_ERR" \
		"Warning: The 'no-such-plugin' plugin could not be found."$'\n'"Error: Only activated 1 of 2 plugins." || ok=1
	wp_run single plugin activate no-such-plugin
	expect_eq "nothing activated: exit" "$RUN_RC" 1 || ok=1
	expect_eq "nothing activated: stderr" "$RUN_ERR" \
		"Warning: The 'no-such-plugin' plugin could not be found."$'\n'"Error: No plugins activated." || ok=1
	return $ok
}

# check_network_only — on multisite, activating a network-only plugin without --network
# network-activates it; deactivating a network-active plugin without --network is refused.
check_network_only() {
	local ok=0
	reset_sites multi
	install_fixture multi network-only || return 1
	wp_run multi plugin activate network-only
	note_run "wp plugin activate network-only (no --network)"
	expect_eq "no --network: exit" "$RUN_RC" 0 || ok=1
	expect_match "no --network: message" "$RUN_OUT" "^Plugin 'network-only' network activated\.$" || ok=1
	expect_eq "no --network: status" "$(plugin_status multi network-only)" active-network || ok=1
	wp_run multi plugin deactivate network-only
	note_run "wp plugin deactivate network-only (no --network)"
	expect_eq "deactivate without --network: exit" "$RUN_RC" 1 || ok=1
	expect_eq "deactivate without --network: stderr" "$RUN_ERR" \
		"Warning: Plugin 'network-only' is network active and must be deactivated with --network flag."$'\n'"Error: No plugins deactivated." || ok=1
	expect_eq "still network-active" "$(plugin_status multi network-only)" active-network || ok=1
	wp_run multi plugin deactivate network-only --network
	expect_eq "deactivate --network: status" "$(plugin_status multi network-only)" inactive || ok=1
	return $ok
}

# check_skip_plugins_hook — `deactivate --skip-plugins=<slug>` does not run that plugin's
# deactivation hook (the plan's --no-load recovery, §6.14).
check_skip_plugins_hook() {
	local ok=0
	reset_sites single
	{ install_fixture single basic && wp_run single plugin activate basic; } || return 1
	wp_run single plugin deactivate basic --skip-plugins=basic
	expect_eq "--skip-plugins=basic: exit, status" "$RUN_RC $(plugin_status single basic)" "0 inactive" || ok=1
	expect_eq "--skip-plugins=basic: hook ran (counter)" "$(option_json single apvm_fx_basic_deactivations)" absent || ok=1
	{ wp_run single plugin activate basic && wp_run single plugin deactivate basic; } || ok=1
	expect_eq "plain deactivate: hook ran (counter)" "$(option_json single apvm_fx_basic_deactivations)" '"1"' || ok=1
	return $ok
}

# check_disallow_file_mods — WP-CLI ignores DISALLOW_FILE_MODS: install, activate and
# delete all succeed (the plan blocks file changes itself, §6.7).
check_disallow_file_mods() {
	local ok=0
	reset_sites single
	wp_run single config set DISALLOW_FILE_MODS true --raw --type=constant || {
		note_run "wp config set DISALLOW_FILE_MODS"
		return 1
	}
	wp_run single eval 'var_export(DISALLOW_FILE_MODS);' --skip-plugins --skip-themes
	expect_eq "DISALLOW_FILE_MODS in effect" "$RUN_OUT" true || ok=1
	install_zip single "$(fixture_zip basic)" || ok=1
	expect_eq "install: status of basic" "$(plugin_status single basic)" inactive || ok=1
	wp_run single plugin activate akismet
	expect_eq "activate akismet: exit, status" "$RUN_RC $(plugin_status single akismet)" "0 active" || ok=1
	wp_run single plugin delete hello
	expect_eq "delete hello: exit, status" "$RUN_RC $(plugin_status single hello)" "0 absent" || ok=1
	return $ok
}

# check_activation_failures — a fatal activation (WordPress's fatal handler is off while it
# sandboxes the plugin) exits 255 and leaves the plugin inactive; a plugin that breaks every
# request fails the health check with WordPress's critical-error exit 1; reads still work;
# --skip-plugins recovers, repeatably.
check_activation_failures() {
	local ok=0 round
	reset_sites single
	install_fixture single fatal-activate || return 1
	wp_run single plugin activate fatal-activate
	note_run "wp plugin activate fatal-activate"
	expect_eq "fatal activation: exit, status" "$RUN_RC $(plugin_status single fatal-activate)" "254 inactive" || ok=1
	install_fixture single fatal-load || return 1
	for round in 1 2; do
		wp_run single plugin activate fatal-load
		expect_eq "round $round: fatal-load activation (exit)" "$RUN_RC" 0 || ok=1
		wp_run single option get siteurl
		expect_eq "round $round: health check (exit)" "$RUN_RC" 1 || ok=1
		expect_match "round $round: health check (message)" "$(tail -n 1 <<<"$RUN_ERR")" "$CRITICAL_ERROR" || ok=1
		expect_eq "round $round: read with --skip-plugins (status)" "$(plugin_status single fatal-load)" active || ok=1
		wp_run single plugin deactivate fatal-load --skip-plugins=fatal-load
		expect_eq "round $round: recovery, deactivate --skip-plugins=fatal-load (exit)" "$RUN_RC" 0 || ok=1
		wp_run single option get siteurl
		expect_eq "round $round: health check after recovery (exit)" "$RUN_RC" 0 || ok=1
	done
	return $ok
}

# check_requires_plugins — `Requires Plugins` blocks activating a child before its parent,
# while deactivating the parent under an active child is not checked (§3.3).
check_requires_plugins() {
	local ok=0
	reset_sites single
	{ install_fixture single parent && install_fixture single child; } || return 1
	wp_run single plugin activate child
	note_run "wp plugin activate child (parent inactive)"
	expect_eq "child first: exit, status" "$RUN_RC $(plugin_status single child)" "1 inactive" || ok=1
	expect_match "child first: reason" "$RUN_ERR" 'requires 1 plugin to be installed and activated: APVM Fixture Parent' || ok=1
	{ wp_run single plugin activate parent && wp_run single plugin activate child; } || ok=1
	expect_eq "parent, then child" "$(both_statuses single parent child)" "active/active" || ok=1
	wp_run single plugin deactivate parent
	expect_eq "deactivate parent under an active child: exit" "$RUN_RC" 0 || ok=1
	expect_eq "statuses afterwards (parent/child)" "$(both_statuses single parent child)" "inactive/active" || ok=1
	return $ok
}

# check_config_isolation — WP-CLI runs code from the identity's user config (`require:` in
# ~/.wp-cli/config.yml) and packages on every call, even with --skip-plugins; the plan's
# hermetic invocation (§6.1) runs neither. Uses ftpsite, whose identity owns its HOME.
check_config_isolation() {
	local ok=0 marker_config='apvm-e2e: user config code ran' marker_package='apvm-e2e: package code ran'
	as_identity_sh ftpsite "mkdir -p ~/.wp-cli/packages/vendor &&
		printf '<?php fwrite(STDERR, \"$marker_config\\\\n\");\\n' >~/.wp-cli/config-code.php &&
		printf 'require:\\n  - %s\\n' ~/.wp-cli/config-code.php >~/.wp-cli/config.yml &&
		printf '<?php fwrite(STDERR, \"$marker_package\\\\n\");\\n' >~/.wp-cli/packages/vendor/autoload.php" || {
		note_run "setup"
		return 1
	}
	as_identity ftpsite "$(wp_cmdline_raw ftpsite option get siteurl --skip-plugins --skip-themes)"
	note_run "plain wp"
	expect_match "plain wp: user config code runs" "$RUN_ERR" "$marker_config" || ok=1
	expect_match "plain wp: package code runs" "$RUN_ERR" "$marker_package" || ok=1
	as_identity ftpsite "$(wp_cmdline_raw ftpsite option get siteurl --skip-plugins --skip-themes --skip-packages)"
	expect_match "--skip-packages alone: user config code still runs" "$RUN_ERR" "$marker_config" || ok=1
	wp_run ftpsite option get siteurl --skip-plugins --skip-themes
	expect_eq "hermetic (WP_CLI_CONFIG_PATH=/dev/null + --skip-packages): exit, stdout, stderr" \
		"$RUN_RC|$RUN_OUT|$RUN_ERR" "0|http://ftpsite.test|" || ok=1
	return $ok
}

# check_mu_plugins_load — --skip-plugins skips regular plugins only: must-use plugins and
# drop-ins still load, so one that fatals breaks every read (§6.14).
check_mu_plugins_load() {
	local ok=0 content
	content="$(site_field single path)/wp-content"
	as_identity_sh single "printf '<?php fwrite(STDERR, \"apvm-e2e: must-use plugin loaded\\\\n\");\\n' >$(q "$content/mu-plugins/apvm-e2e-probe.php") &&
		printf '<?php fwrite(STDERR, \"apvm-e2e: object-cache drop-in loaded\\\\n\");\\n' >$(q "$content/object-cache.php")" || {
		note_run "setup"
		return 1
	}
	wp_run single option get siteurl --skip-plugins --skip-themes
	note_run "read with --skip-plugins --skip-themes"
	expect_match "must-use plugin loads" "$RUN_ERR" 'must-use plugin loaded' || ok=1
	expect_match "object-cache drop-in loads" "$RUN_ERR" 'object-cache drop-in loaded' || ok=1
	as_identity_sh single "printf '<?php throw new RuntimeException(\"apvm-e2e\");\\n' >$(q "$content/mu-plugins/apvm-e2e-probe.php")"
	plugin_rows single >/dev/null
	expect_eq "fatal must-use plugin: plugin list exit" "$RUN_RC" 1 || ok=1
	expect_match "fatal must-use plugin: message" "$(tail -n 1 <<<"$RUN_ERR")" "$CRITICAL_ERROR" || ok=1
	reset_sites single
	return $ok
}

# check_php_notices — with every PHP notice displayed on stdout (a php.ini some servers
# run), the JSON payload stays the last stdout line and parses; WP-CLI 2.12.0 prints
# deprecation lines before it on PHP >= 8.5 and none on 8.4 (§9 parser rule).
check_php_notices() {
	local ok=0 php notices
	wp_run single eval 'echo PHP_VERSION_ID;' --skip-plugins --skip-themes
	php=$RUN_OUT
	as_identity single "env WP_CLI_CONFIG_PATH=/dev/null php -d display_errors=stdout -d error_reporting=-1 \
		/usr/local/bin/wp --skip-packages$(wp_args single plugin list --format=json --fields=name,status \
		--skip-update-check --skip-plugins --skip-themes)"
	expect_eq "exit" "$RUN_RC" 0 || ok=1
	python3 -c 'import json, sys; json.loads(sys.argv[1])' "$(tail -n 1 <<<"$RUN_OUT")" 2>/dev/null
	expect_eq "last stdout line parses as JSON" "$?" 0 || ok=1
	notices=$(sed '$d' <<<"$RUN_OUT")
	note_block "lines before the payload (PHP_VERSION_ID $php)" "$notices"
	if [ "$php" -ge 80500 ]; then
		expect_match "PHP >= 8.5: deprecation lines precede the payload" "$notices" '^(PHP )?Deprecated:' || ok=1
	else
		expect_eq "PHP < 8.5: lines before the payload" "$notices" "" || ok=1
	fi
	return $ok
}

# check_search_replace_dry_run — `search-replace --dry-run` is not write-free: `--log=<file>`
# still writes the log, and `--export` is rejected (§6.12 classifies both as may-write).
check_search_replace_dry_run() {
	local ok=0 log=/tmp/apvm-e2e-search-replace.log
	wp_run single search-replace single.test example.test --dry-run "--log=$log" --skip-plugins --skip-themes
	expect_eq "--dry-run --log: exit" "$RUN_RC" 0 || ok=1
	as_identity single "test -s $(q "$log")"
	expect_eq "--dry-run --log: log file written" "$RUN_RC" 0 || ok=1
	wp_run single search-replace single.test example.test --dry-run --export=/tmp/apvm-e2e-search-replace.sql \
		--skip-plugins --skip-themes
	note_run "search-replace --dry-run --export"
	expect_eq "--dry-run --export: exit" "$RUN_RC" 1 || ok=1
	wp_run single option get siteurl --skip-plugins --skip-themes
	expect_eq "database unchanged" "$RUN_OUT" http://single.test || ok=1
	return $ok
}

group_wpcli() {
	check wpcli.version "WP-CLI 2.12.0; WP_CLI_VERSION visible in wp eval" check_wpcli_version
	check wpcli.list-read "plugin list read: fields, statuses, row shapes" check_list_read
	check wpcli.uninstall-skip-delete "uninstall --skip-delete: data gone, files kept" check_uninstall_skip_delete
	check wpcli.uninstall-active "uninstall refuses an active plugin" check_uninstall_active
	check wpcli.force-over-active "install --force over an active plugin keeps it active" check_force_over_active
	check wpcli.exit-codes "Exit codes: WP_CLI::error, PHP fatal, batch errors" check_exit_codes
	check wpcli.network-only "Multisite: network-only activation and deactivation" check_network_only
	check wpcli.skip-plugins-hook "--skip-plugins=<slug> skips its deactivation hook" check_skip_plugins_hook
	check wpcli.disallow-file-mods "WP-CLI ignores DISALLOW_FILE_MODS" check_disallow_file_mods
	check wpcli.activation-failures "Fatal activation, broken site, health check, recovery" check_activation_failures
	check wpcli.requires-plugins "Requires Plugins ordering" check_requires_plugins
	check wpcli.config-isolation "User config and packages run unless isolated" check_config_isolation
	check wpcli.mu-plugins-load "Must-use plugins and drop-ins load under --skip-plugins" check_mu_plugins_load
	check wpcli.php-notices "Displayed PHP notices precede the JSON payload" check_php_notices
	check wpcli.search-replace-dry-run "search-replace --dry-run still writes with --log" check_search_replace_dry_run
}
