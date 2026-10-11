# shellcheck shell=bash
#
# Group `family` — plan §3.5 "BackWPup coexistence". The guarded/guarded-pro fixtures mirror
# BackWPup 5.7 free/pro: a shared main-file guard, no activation hook but an install routine
# that runs on `init` when the stored version differs, shared data, and an uninstall guard.
# Events are logged in `apvm_fx_guarded_log`. Plus an optional run on real BackWPup builds.
# Option values read back as JSON.

# family_boot — one request that loads the plugins (apvm's health check), which is when an
# install routine runs: WP-CLI's activation request has already passed `init`.
family_boot() {
	wp_run single option get siteurl || {
		note_run "boot"
		return 1
	}
}

# family_activate FIRST SECOND [PRO_VERSION] — fresh `single` with both editions installed
# (pro-en at PRO_VERSION, default 1.0.0), then FIRST and SECOND each activated in its own
# process followed by a booting request, as apvm does.
family_activate() {
	local slug
	reset_sites single
	{ install_fixture single guarded && install_fixture single guarded-pro en "${3:-}"; } || return 1
	for slug in "$1" "$2"; do
		wp_run single plugin activate "$slug" || {
			note_run "activate $slug"
			return 1
		}
		family_boot || return 1
	done
}

# family_log — prints the fixture's event log (JSON).
family_log() {
	option_json single apvm_fx_guarded_log
}

# check_both_active — with both editions active the site boots and one edition runs: the
# one WordPress loads first (active_plugins is sorted; "guarded-pro/" < "guarded/").
check_both_active() {
	local ok=0
	family_activate guarded guarded-pro || return 1
	expect_eq "statuses (free/pro)" "$(both_statuses single guarded guarded-pro)" active/active || ok=1
	expect_eq "active_plugins" "$(option_json single active_plugins)" '["guarded-pro\/guarded.php","guarded\/guarded.php"]' || ok=1
	wp_run single eval 'echo APVM_FX_GUARDED_LOADED;'
	expect_eq "edition that runs" "$RUN_OUT" pro || ok=1
	return $ok
}

# check_install_routine — only the loaded edition ever runs its install routine, and only
# when the stored version differs from its own: with both active that is pro, so pro's
# routine runs next to an active free only if the versions differ.
check_install_routine() {
	local ok=0
	family_activate guarded guarded-pro || return 1
	expect_eq "free, then pro (same version): log" "$(family_log)" '["installed:free@1.0.0"]' || ok=1
	family_activate guarded guarded-pro 1.1.0 || return 1
	expect_eq "free, then pro 1.1.0: log" "$(family_log)" '["installed:free@1.0.0","installed:pro@1.1.0"]' || ok=1
	expect_eq "free, then pro 1.1.0: shared data" "$(option_json single apvm_fx_guarded_shared)" '"written-by-pro"' || ok=1
	family_activate guarded-pro guarded || return 1
	expect_eq "pro, then free: log" "$(family_log)" '["installed:pro@1.0.0"]' || ok=1
	return $ok
}

# check_uninstall_keeps_shared — uninstalling one edition while the other is active keeps
# the shared data; uninstalling with no sibling active removes it (two-phase each time).
check_uninstall_keeps_shared() {
	local ok=0 step
	family_activate guarded guarded-pro || return 1
	for step in "deactivate guarded-pro" "uninstall guarded-pro"; do
		# shellcheck disable=SC2086 # step is "VERB SLUG"
		wp_run single plugin $step
		expect_eq "$step: exit" "$RUN_RC" 0 || ok=1
	done
	expect_eq "pro removed while free active: shared data" "$(option_json single apvm_fx_guarded_shared)" '"written-by-free"' || ok=1
	expect_eq "pro status" "$(plugin_status single guarded-pro)" absent || ok=1
	for step in "deactivate guarded" "uninstall guarded"; do
		# shellcheck disable=SC2086
		wp_run single plugin $step
		expect_eq "$step: exit" "$RUN_RC" 0 || ok=1
	done
	expect_eq "free removed, no sibling active: shared data" "$(option_json single apvm_fx_guarded_shared)" absent || ok=1
	expect_eq "event log" "$(family_log)" \
		'["installed:free@1.0.0","deactivated:pro","uninstall-kept:pro","deactivated:free","uninstall-cleaned:free"]' || ok=1
	return $ok
}

# check_one_process_removal — `uninstall --deactivate` (one process that loaded the plugin)
# skips the guarded cleanup; deactivate + uninstall in separate processes runs it (D9).
check_one_process_removal() {
	local ok=0
	reset_sites single
	{ install_fixture single guarded-pro en && wp_run single plugin activate guarded-pro && family_boot; } || return 1
	wp_run single plugin uninstall guarded-pro --deactivate
	note_run "wp plugin uninstall guarded-pro --deactivate"
	expect_eq "one process: exit, status" "$RUN_RC $(plugin_status single guarded-pro)" "0 absent" || ok=1
	expect_eq "one process: shared data" "$(option_json single apvm_fx_guarded_shared)" '"written-by-pro"' || ok=1
	expect_eq "one process: log" "$(family_log)" '["installed:pro@1.0.0","deactivated:pro","uninstall-kept:pro"]' || ok=1
	reset_sites single
	{ install_fixture single guarded-pro en && wp_run single plugin activate guarded-pro && family_boot; } || return 1
	{ wp_run single plugin deactivate guarded-pro && wp_run single plugin uninstall guarded-pro; } || {
		note_run "two processes"
		return 1
	}
	expect_eq "two processes: status" "$(plugin_status single guarded-pro)" absent || ok=1
	expect_eq "two processes: shared data" "$(option_json single apvm_fx_guarded_shared)" absent || ok=1
	expect_eq "two processes: log" "$(family_log)" '["installed:pro@1.0.0","deactivated:pro","uninstall-cleaned:pro"]' || ok=1
	return $ok
}

# check_variant_markers — the plan's on-site detection (§6.9): grep -F of each variant's
# marker in the installed plugin, run as the identity.
check_variant_markers() {
	local file
	reset_sites single
	install_fixture single guarded-pro de || return 1
	file=$(q "$(plugins_dir single)/guarded-pro/inc/License.php")
	as_identity_sh single "grep -F -q -- 'https://de.example.com/' $file; echo de=\$?;
		grep -F -q -- 'https://en.example.com/' $file; echo en=\$?"
	expect_eq "pro-de installed: marker matches" "$(tr '\n' ' ' <<<"$RUN_OUT")" "de=0 en=1 "
}

# backwpup_option_count — prints how many backwpup* options the site has, or
# `read-failed`.
backwpup_option_count() {
	if wp_run single option list --search='backwpup*' --format=count --skip-plugins --skip-themes &&
		grep -Eq '^[0-9]+$' <<<"$RUN_OUT"; then
		printf '%s' "$RUN_OUT"
	else
		printf 'read-failed'
	fi
}

# expect_steps LABEL SLUG VERB... — runs `wp plugin VERB SLUG` for each VERB and requires
# exit 0 from each.
expect_steps() {
	local label=$1 slug=$2 verb ok=0
	shift 2
	for verb in "$@"; do
		wp_run single plugin "$verb" "$slug"
		expect_eq "$label: $verb $slug (exit)" "$RUN_RC" 0 || ok=1
	done
	return $ok
}

# check_real_backwpup — the coexistence facts on real BackWPup builds (optional).
check_real_backwpup() {
	local ok=0 free=${APVM_E2E_BACKWPUP_FREE_ZIP:-} pro=${APVM_E2E_BACKWPUP_PRO_ZIP:-} before after
	if [ -z "$free" ] || [ -z "$pro" ]; then
		note "set APVM_E2E_BACKWPUP_FREE_ZIP and APVM_E2E_BACKWPUP_PRO_ZIP to run it"
		return 3
	fi
	note "free: $(basename "$free"), pro: $(basename "$pro")"
	reset_sites single
	{ install_zip single "$free" && install_zip single "$pro"; } || return 1
	expect_steps "free" backwpup activate || ok=1
	family_boot || ok=1
	expect_steps "pro" backwpup-pro activate || ok=1
	family_boot || ok=1
	expect_eq "statuses (free/pro)" "$(both_statuses single backwpup backwpup-pro)" active/active || ok=1
	wp_run single eval 'echo (new ReflectionClass("BackWPup"))->getFileName();'
	expect_match "edition that runs" "$RUN_OUT" '/backwpup-pro/' || ok=1
	note "backwpup_version: $(option_json single backwpup_version)"
	before=$(backwpup_option_count)
	expect_match "backwpup* options with both active" "$before" '^[1-9][0-9]*$' || ok=1
	expect_steps "pro" backwpup-pro deactivate uninstall || ok=1
	expect_eq "pro status" "$(plugin_status single backwpup-pro)" absent || ok=1
	expect_eq "pro removed while free active: backwpup* options" "$(backwpup_option_count)" "$before" || ok=1
	expect_steps "free" backwpup deactivate uninstall || ok=1
	after=$(backwpup_option_count)
	expect_eq "free removed, no sibling active: backwpup* options" "$after" 0 || ok=1
	return $ok
}

group_family() {
	check family.both-active "Free + pro both active: boots, the loaded-first edition runs" check_both_active
	check family.install-routine "Only the loaded edition installs, on a version change" check_install_routine
	check family.uninstall-shared-data "Uninstall next to an active sibling keeps shared data" check_uninstall_keeps_shared
	check family.one-process-removal "uninstall --deactivate skips the guarded cleanup" check_one_process_removal
	check family.variant-markers "Variant markers detected on the site (pro-de)" check_variant_markers
	check family.real-backwpup "Real BackWPup builds: coexistence (optional)" check_real_backwpup
}
