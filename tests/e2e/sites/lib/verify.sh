# shellcheck shell=bash
# Globals set here are read by verify.sh and checks/*.sh.
# shellcheck disable=SC2034
#
# Check framework and remote helpers for verify.sh and checks/*.sh.
# Source it after lib/common.sh. Bash 3.2 compatible.
#
# Remote commands follow the plan's execution contract (§6.1), so the checks exercise
# the shapes apvm will use: scripts go to `sh -s` on stdin behind a random nonce with
# LC_ALL=C; WP-CLI runs from / with stdin closed as the site identity, isolated from
# WP-CLI user configs and packages; 255 maps to 254.
#
# Grep always reads here-strings, never pipes: with pipefail, `printf … | grep -q` can
# report the writer's SIGPIPE instead of grep's verdict on large outputs.

# Per-call overrides must never leak in from the caller's environment.
unset ID_LOGIN ID_PREFIX SAMPLE_ENV STAGED

WORK_DIR=""    # scratch dir for this run (set by verify.sh)
REPORT=""      # Markdown report path (set by verify.sh)
SAMPLES_DIR="" # raw OpenSSH stderr samples (set by verify.sh)
SITE_TABLE=""  # `apvm-e2e sites` output (loaded by verify.sh)
VERBOSE=0
NOTES=""
COUNT_PASS=0
COUNT_FAIL=0
COUNT_INFO=0
COUNT_SKIP=0
FAILED_IDS=""

# ---------------------------------------------------------------------------
# Check framework
# ---------------------------------------------------------------------------

# note TEXT... — records an observation for the running check.
note() {
	NOTES="$NOTES$*"$'\n'
}

# indent TEXT — prints TEXT with every line indented by four spaces.
indent() {
	# shellcheck disable=SC2001 # a per-line anchor needs sed
	sed 's/^/    /' <<<"$1"
}

# shape COUNTS — prints COUNTS (e.g. `3/0/12`) with every positive number replaced by `+`.
shape() {
	# shellcheck disable=SC2001 # a regex needs sed
	sed 's/[1-9][0-9]*/+/g' <<<"$1"
}

# note_block LABEL TEXT — records a multi-line observation, indented under LABEL.
note_block() {
	note "$1:"
	if [ -n "$2" ]; then
		note "$(indent "$2")"
	fi
}

# check ID TITLE FUNCTION — runs FUNCTION and records the verdict it returns:
# 0 PASS, 2 INFO (observed and recorded, no expectation), 3 SKIP, anything else FAIL.
check() {
	local id=$1 title=$2 fn=$3 rc status
	NOTES=""
	"$fn"
	rc=$?
	case $rc in
	0) status=PASS COUNT_PASS=$((COUNT_PASS + 1)) ;;
	2) status=INFO COUNT_INFO=$((COUNT_INFO + 1)) ;;
	3) status=SKIP COUNT_SKIP=$((COUNT_SKIP + 1)) ;;
	*) status=FAIL COUNT_FAIL=$((COUNT_FAIL + 1)) FAILED_IDS="$FAILED_IDS $id" ;;
	esac
	printf '%-4s  %-44s %s\n' "$status" "$id" "$title"
	if [ "$VERBOSE" = 1 ] || [ "$status" != PASS ]; then
		printf '%s' "$NOTES" | sed 's/^/        /'
	fi
	# shellcheck disable=SC2016 # Markdown backticks, not command substitution.
	printf '### `%s` — %s\n\n**%s**\n\n```text\n%s```\n\n' "$id" "$title" "$status" "$NOTES" >>"$REPORT"
}

# expect_eq LABEL ACTUAL EXPECTED — notes the comparison; returns 1 on a mismatch.
expect_eq() {
	if [ "$2" = "$3" ]; then
		note "ok    $1: $2"
		return 0
	fi
	note "FAIL  $1: got [$2], expected [$3]"
	return 1
}

# expect_match LABEL ACTUAL ERE — notes the comparison; returns 1 when ACTUAL does not
# match the extended regular expression ERE.
expect_match() {
	if grep -Eq -- "$3" <<<"$2"; then
		note "ok    $1: $2"
		return 0
	fi
	note "FAIL  $1: [$2] does not match /$3/"
	return 1
}

# expect_ran LABEL — returns 1 (and notes why) unless the last remote script actually
# started on the target (its nonce came back), so an SSH failure never passes as a
# remote result.
expect_ran() {
	[ "$RUN_FRAMED" = 1 ] && return 0
	note "FAIL  $1: the remote script never started (ssh exit $RUN_RC): $RUN_ERR"
	return 1
}

# ---------------------------------------------------------------------------
# Sites and identities
# ---------------------------------------------------------------------------

# site_field SITE FIELD — prints path | url | owner | group | db of a harness site.
site_field() {
	local column
	case $2 in
	path) column=2 ;;
	url) column=3 ;;
	owner) column=4 ;;
	group) column=5 ;;
	db) column=6 ;;
	*) return 1 ;;
	esac
	awk -F'|' -v site="$1" -v column="$column" '$1 == site { print $column }' <<<"$SITE_TABLE"
}

# plugins_dir SITE — prints the site's plugins directory.
plugins_dir() {
	printf '%s/wp-content/plugins' "$(site_field "$1" path)"
}

# identity_prefix SITE — prints the command prefix that runs as SITE's identity. The
# default is the plan's `run_as = auto` with sudo (§6.2): `sudo -n -u OWNER -g GROUP --`.
# `with_identity` overrides it (an empty prefix runs as the login user).
identity_prefix() {
	if [ -n "${ID_PREFIX+set}" ]; then
		printf '%s' "$ID_PREFIX"
		return 0
	fi
	printf 'sudo -n -u %s -g %s --' "$(site_field "$1" owner)" "$(site_field "$1" group)"
}

# with_identity LOGIN PREFIX COMMAND... — runs COMMAND with every remote helper logging
# in as LOGIN and using PREFIX as the identity prefix (bash dynamic scoping).
with_identity() {
	local ID_LOGIN=$1 ID_PREFIX=$2
	shift 2
	"$@"
}

# reset_sites [SITE...] — restores sites (default: all) and the target's shared state.
# A failed reset leaves every later result meaningless, so it aborts the run.
reset_sites() {
	local err
	if ! err=$(target_exec apvm-e2e reset "$@" 2>&1 >/dev/null); then
		echo "error: resetting the harness failed, aborting: $err" >&2
		exit 1
	fi
}

# ---------------------------------------------------------------------------
# Remote execution
# ---------------------------------------------------------------------------

# run_remote_with LOGIN REMOTE_COMMAND BODY — sends `printf nonce`, `LC_ALL=C` and BODY
# on stdin to REMOTE_COMMAND over SSH as LOGIN. Sets RUN_RC (the ssh exit status),
# RUN_RAW (all stdout), RUN_OUT (stdout after the nonce line), RUN_ERR (stderr) and
# RUN_FRAMED (1 when the nonce came back, i.e. the script started); returns RUN_RC.
run_remote_with() {
	local login=$1 remote=$2 body=$3 nonce script
	nonce=$(new_nonce)
	script="printf '%s\\n' $(q "$nonce")
LC_ALL=C; export LC_ALL
$body"
	RUN_RAW=$(printf '%s\n' "$script" | ssh_to "$login" "$remote" 2>"$WORK_DIR/stderr")
	RUN_RC=$?
	RUN_ERR=$(cat "$WORK_DIR/stderr")
	RUN_OUT=$(after_nonce "$nonce" "$RUN_RAW")
	RUN_FRAMED=0
	if grep -qxF -- "$nonce" <<<"$RUN_RAW"; then RUN_FRAMED=1; fi
	return "$RUN_RC"
}

# run_remote LOGIN BODY — run_remote_with LOGIN 'sh -s' BODY (stdin delivery, §6.1).
run_remote() {
	run_remote_with "$1" 'sh -s' "$2"
}

# framed COMMAND — prints the plan's command tail (§6.1): run COMMAND from / with stdin
# closed, map exit 255 (PHP fatal) to 254 so 255 always means an SSH failure.
framed() {
	# shellcheck disable=SC2016 # $? and $rc are for the remote shell.
	printf 'cd / && %s </dev/null; rc=$?; [ "$rc" -eq 255 ] && rc=254; exit "$rc"' "$1"
}

# as_identity SITE COMMAND — runs one simple COMMAND on the target as SITE's identity.
# The prefix covers only the first command of a list: use as_identity_sh for lists.
as_identity() {
	run_remote "${ID_LOGIN:-deploy}" "$(framed "$(identity_prefix "$1") $2")"
}

# as_identity_sh SITE SCRIPT — runs a shell SCRIPT (lists, pipes) wholly as SITE's identity.
as_identity_sh() {
	as_identity "$1" "sh -c $(q "$2")"
}

# wp_args SITE ARG... — prints ` --path=… --url=… ARG…`, every word quoted.
wp_args() {
	local site=$1 args="" arg
	shift
	for arg in "$@"; do args="$args $(q "$arg")"; done
	printf ' --path=%s --url=%s%s' "$(q "$(site_field "$site" path)")" "$(q "$(site_field "$site" url)")" "$args"
}

# wp_cmdline SITE ARG... — prints the plan's hermetic WP-CLI command line (§6.1): no
# WP-CLI user config (WP_CLI_CONFIG_PATH=/dev/null) and no packages (--skip-packages).
wp_cmdline() {
	printf 'env WP_CLI_CONFIG_PATH=/dev/null wp --skip-packages%s' "$(wp_args "$@")"
}

# wp_cmdline_raw SITE ARG... — prints a plain `wp …` command line, without the isolation.
wp_cmdline_raw() {
	printf 'wp%s' "$(wp_args "$@")"
}

# wp_run SITE ARG... — runs WP-CLI (hermetic) on SITE as its identity.
wp_run() {
	as_identity "$1" "$(wp_cmdline "$@")"
}

# note_run LABEL — notes the last remote command's exit status, stdout and stderr.
note_run() {
	if [ "$RUN_FRAMED" = 1 ]; then note "$1: exit $RUN_RC"; else note "$1: ssh failed (exit $RUN_RC)"; fi
	[ -n "$RUN_OUT" ] && note_block "  stdout" "$RUN_OUT"
	[ -n "$RUN_ERR" ] && note_block "  stderr" "$RUN_ERR"
	return 0
}

# ---------------------------------------------------------------------------
# WordPress state (the plan's reads, §6.4)
# ---------------------------------------------------------------------------

# plugin_rows SITE — prints `wp plugin list` as JSON, with the plan's fields and flags.
plugin_rows() {
	wp_run "$1" plugin list --format=json --fields=name,status,version,file,title \
		--skip-update-check --skip-plugins --skip-themes || return 1
	printf '%s' "$RUN_OUT"
}

# plugin_field SITE SLUG FIELD — prints FIELD of SLUG's row or `absent`. On a failed read
# it prints `read-failed: <error>`, which matches no expectation.
plugin_field() {
	local rows
	if ! rows=$(plugin_rows "$1"); then
		printf 'read-failed: %s' "$(tail -n 1 <<<"$RUN_ERR")"
		return 1
	fi
	python3 -c '
import json, sys
rows = [row for row in json.loads(sys.argv[3]) if row["name"] == sys.argv[1]]
print(rows[0][sys.argv[2]] if rows else "absent")' "$2" "$3" "$rows"
}

# plugin_status SITE SLUG — prints the plugin's status (active, inactive, …) or `absent`.
plugin_status() {
	plugin_field "$1" "$2" status
}

# both_statuses SITE SLUG... — prints the statuses of SLUGs joined by `/`.
both_statuses() {
	local site=$1 slug out=""
	shift
	for slug in "$@"; do out="$out${out:+/}$(plugin_status "$site" "$slug")"; done
	printf '%s' "$out"
}

# option_json SITE NAME — prints the option as JSON, `absent` when it does not exist, or
# `read-failed: <error>`.
option_json() {
	if wp_run "$1" option get "$2" --format=json --skip-plugins --skip-themes; then
		printf '%s' "$RUN_OUT"
	elif [ "$RUN_FRAMED" = 1 ] && grep -q 'Could not get' <<<"$RUN_ERR"; then
		echo absent
	else
		printf 'read-failed: %s' "$(tail -n 1 <<<"$RUN_ERR")"
	fi
}

# ---------------------------------------------------------------------------
# Packages and staging (§6.6)
# ---------------------------------------------------------------------------

# sha256_of FILE — prints the file's SHA-256 (hex).
sha256_of() {
	python3 -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$1"
}

# fixture_zip SLUG [VARIANT] [VERSION] — builds a fixture zip once and prints its path.
# Written to a temp name first, so a failed build never leaves a reusable partial zip.
fixture_zip() {
	local out="$WORK_DIR/zips/$1${2:+@$2}${3:+~$3}.zip"
	if [ ! -f "$out" ]; then
		python3 "$E2E_DIR/lib/fixture_zip.py" "$E2E_DIR/fixtures" "$1" "$out.partial" \
			${2:+--variant "$2"} ${3:+--version "$3"} || return 1
		mv -- "$out.partial" "$out"
	fi
	printf '%s' "$out"
}

# stage_zip SITE FILE — uploads FILE like the plan's staging step: `mktemp -d` as the
# identity, then the bytes over the SSH channel's stdin into `tee` (upload mode, remote
# command `sh -c '…'`), then a SHA-256 comparison. Sets STAGED to the remote path.
stage_zip() {
	local site=$1 file=$2 login=${ID_LOGIN:-deploy} prefix nonce script remote
	[ -f "$file" ] || {
		note "no such file to upload: $file"
		return 1
	}
	prefix=$(identity_prefix "$site")
	as_identity "$site" "mktemp -d /tmp/apvm.XXXXXXXXXX" || {
		note_run "staging mktemp"
		return 1
	}
	remote="$RUN_OUT/package-1.zip"
	nonce=$(new_nonce)
	# shellcheck disable=SC2016 # $? and $rc are for the remote shell.
	script="echo $nonce; $prefix tee -- $(q "$remote") >/dev/null; "'rc=$?; [ "$rc" -eq 255 ] && rc=254; exit "$rc"'
	if ! ssh_to "$login" "sh -c $(q "$script")" <"$file" >"$WORK_DIR/upload.out" 2>"$WORK_DIR/upload.err"; then
		note "upload as $login failed: $(cat "$WORK_DIR/upload.err")"
		return 1
	fi
	as_identity "$site" "sha256sum -- $(q "$remote")" || {
		note_run "staging sha256sum"
		return 1
	}
	if [ "${RUN_OUT%% *}" != "$(sha256_of "$file")" ]; then
		note "upload corrupted: remote ${RUN_OUT%% *}, local $(sha256_of "$file")"
		return 1
	fi
	STAGED=$remote
}

# install_zip SITE ZIP [WP_ARG...] — stages ZIP and runs `wp plugin install` on it.
install_zip() {
	local site=$1 zip=$2
	shift 2
	stage_zip "$site" "$zip" || return 1
	wp_run "$site" plugin install "$STAGED" "$@" || {
		note_run "wp plugin install $(basename "$zip")"
		return 1
	}
}

# install_fixture SITE SLUG [VARIANT] [VERSION] — installs a fixture plugin (fixture_zip).
install_fixture() {
	install_zip "$1" "$(fixture_zip "$2" "${3:-}" "${4:-}")"
}
