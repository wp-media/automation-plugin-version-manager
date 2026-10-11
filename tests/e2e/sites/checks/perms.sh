# shellcheck shell=bash
# The remote shell, not this one, expands the $… inside single-quoted command lines.
# shellcheck disable=SC2016
#
# Group `perms` — plan §3.5 "Groups and permissions", on the FTP-style site (ftpuser:www-data,
# dirs 2775, files 0664), plus the no-root rungs of the repair ladder (§6.8).
#
# WordPress creates dirs with mkdir() and then chmods them to FS_CHMOD_DIR (0775 here) only
# when `fileperms & 0777` differs; PHP's chmod() clears setgid. So whether setgid (and with
# it the www-data group of nested entries) survives depends on the identity's umask. The
# plan's fix: every WP-CLI call relaxes the group bits of the umask with
# `--exec='umask(umask() & ~0<group bits P grants>);'` (§6.1).

# The plan's umask relaxation for a plugins dir that grants group rwx (P = 2775).
UMASK_EXEC='--exec=umask(umask() & ~0070);'

# tree_listing SITE PATH — prints mode, owner:group, type and relative path (`.` = PATH)
# of every entry under PATH.
tree_listing() {
	as_identity_sh "$1" "cd -- $(q "$2") && find . -printf '%m %u:%g %y %p\n'"
	printf '%s' "$RUN_OUT"
}

# count_offenders SITE TARGET — prints `G/D/F`: entries not in group www-data, dirs lacking
# g+rwx or setgid, files lacking g+rw — the plan's `find` checks (§6.8) for P = 2775. A
# missing target or a failing `find` prints `error: …` and returns 1, never a count.
count_offenders() {
	as_identity_sh "$1" "t=$(q "$2")"'
		[ -d "$t" ] || { echo "error: $t is not a directory"; exit 3; }
		count() { o=$(find "$@") || return 1; if [ -n "$o" ]; then printf "%s\n" "$o" | wc -l; else echo 0; fi; }
		g=$(count "$t" ! -group www-data) && d=$(count "$t" -type d ! -perm -2070) &&
			f=$(count "$t" -type f ! -perm -060) || { echo "error: find failed"; exit 4; }
		echo "$g/$d/$f"'
	if [ "$RUN_RC" != 0 ] || ! grep -Eq '^[0-9]+/[0-9]+/[0-9]+$' <<<"$RUN_OUT"; then
		printf 'error (exit %s): %s %s' "$RUN_RC" "$RUN_OUT" "$RUN_ERR"
		return 1
	fi
	printf '%s' "$RUN_OUT"
}

# install_with_umask SITE UMASK SLUG [WP_ARG...] — installs a fixture with the identity's
# umask forced to UMASK (as a server's default would be).
install_with_umask() {
	local site=$1 umask=$2 slug=$3
	shift 3
	stage_zip "$site" "$(fixture_zip "$slug")" || return 1
	as_identity "$site" "sh -c $(q "umask $umask && exec $(wp_cmdline "$site" plugin install "$STAGED" "$@")")" || {
		note_run "wp plugin install (umask $umask)"
		return 1
	}
}

# matrix_cell LABEL LOGIN PREFIX UMASK EXPECTED [WP_ARG...] — installs `guarded` on a fresh
# ftpsite as one identity and umask, notes the tree and checks the G/D/F offender counts.
# EXPECTED uses `+` for "more than zero".
matrix_cell() {
	local label=$1 login=$2 prefix=$3 umask=$4 expected=$5 target got
	shift 5
	reset_sites ftpsite
	target="$(plugins_dir ftpsite)/guarded"
	with_identity "$login" "$prefix" install_with_umask ftpsite "$umask" guarded "$@" || return 1
	got=$(with_identity "$login" "$prefix" count_offenders ftpsite "$target")
	note_block "$label, umask $umask$([ $# -gt 0 ] && printf ', %s' "$*") → offenders G/D/F $got" \
		"$(with_identity "$login" "$prefix" tree_listing ftpsite "$target")"
	expect_eq "$label, umask $umask$([ $# -gt 0 ] && printf ' + exec')" "$(shape "$got")" "$expected"
}

# check_install_matrix — group and bits of a fresh install for each identity and umask,
# and with the plan's umask relaxation.
check_install_matrix() {
	local ok=0 sudo_prefix
	sudo_prefix=$(identity_prefix ftpsite)
	matrix_cell "sudo -u ftpuser -g www-data" deploy "$sudo_prefix" 0002 0/0/0 || ok=1
	matrix_cell "sudo -u ftpuser -g www-data" deploy "$sudo_prefix" 0022 0/+/0 || ok=1
	matrix_cell "sudo -u ftpuser -g www-data" deploy "$sudo_prefix" 0022 0/0/0 "$UMASK_EXEC" || ok=1
	matrix_cell "ftpuser login (primary group ftpuser)" ftpuser "" 0002 0/0/0 || ok=1
	matrix_cell "ftpuser login (primary group ftpuser)" ftpuser "" 0022 +/+/0 || ok=1
	return $ok
}

# check_umask_exec — the relaxation clears only the group bits it names and keeps the
# identity's own "other" bits (a 0027 or 0077 identity never becomes world-readable).
check_umask_exec() {
	local ok=0 umask expected
	for umask in 0022:0002 0027:0007 0077:0007; do
		expected=${umask#*:} umask=${umask%%:*}
		as_identity ftpsite "sh -c $(q "umask $umask && exec $(wp_cmdline ftpsite eval 'printf("%04o", umask());' \
			--skip-plugins --skip-themes "$UMASK_EXEC")")"
		expect_eq "identity umask $umask → umask inside WP-CLI" "$RUN_OUT" "$expected" || ok=1
	done
	return $ok
}

# check_repair_without_root — the worst cell (own primary group, umask 0022) repaired by
# rung 1 (`chgrp`) and rung 3 (`chmod g+`), both as ftpuser, without root (§6.8).
check_repair_without_root() {
	local ok=0 target before
	reset_sites ftpsite
	target="$(plugins_dir ftpsite)/guarded"
	with_identity ftpuser "" install_with_umask ftpsite 0022 guarded || return 1
	before=$(with_identity ftpuser "" count_offenders ftpsite "$target")
	expect_eq "before: offender shape G/D/F" "$(shape "$before")" "+/+/0" || ok=1
	with_identity ftpuser "" as_identity ftpsite "chgrp -R -h -- www-data $(q "$target")"
	expect_eq "rung 1: chgrp -R -h as ftpuser (exit)" "$RUN_RC" 0 || ok=1
	with_identity ftpuser "" as_identity_sh ftpsite "find $(q "$target") -type d -exec chmod g+rwxs {} + &&
		find $(q "$target") -type f -exec chmod g+rw {} +"
	expect_eq "rung 3: chmod g+ as ftpuser (exit)" "$RUN_RC" 0 || ok=1
	expect_eq "after rungs 1 and 3: offenders G/D/F" \
		"$(with_identity ftpuser "" count_offenders ftpsite "$target")" "0/0/0" || ok=1
	return $ok
}

# check_side_effect_ownership — files a plugin's activation writes into wp-content get the
# identity's owner and group (D4).
check_side_effect_ownership() {
	local listing
	reset_sites ftpsite
	{ install_fixture ftpsite writes-content && wp_run ftpsite plugin activate writes-content; } || {
		note_run "install + activate"
		return 1
	}
	listing=$(tree_listing ftpsite "$(site_field ftpsite path)/wp-content/apvm-fx-writes-content")
	note_block "as sudo -u ftpuser -g www-data" "$listing"
	expect_eq "entries (dir + file)" "$(grep -c . <<<"$listing")" 2 || return 1
	expect_eq "entries not ftpuser:www-data" "$(grep -cv ' ftpuser:www-data ' <<<"$listing")" 0
}

group_perms() {
	check perms.install-matrix "Install group/setgid/bits per identity and umask" check_install_matrix
	check perms.umask-exec "--exec umask relaxation keeps the identity's other bits" check_umask_exec
	check perms.repair-without-root "Repair rungs 1 + 3 (chgrp, chmod g+) need no root" check_repair_without_root
	check perms.side-effects "Activation side effects get owner + group" check_side_effect_ownership
}
