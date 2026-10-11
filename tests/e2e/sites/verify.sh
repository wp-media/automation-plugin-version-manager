#!/usr/bin/env bash
#
# verify.sh — verify the site-deployment plan's ground truth (§3.5) on the running harness.
#
# Usage: verify.sh [-v] [GROUP...]
#   GROUP  harness | sudo | perms | shells | wpcli | family | ssh  (default: all, in order)
#   -v     also print the observations of passing checks
#
# Each check prints PASS, FAIL, INFO (observed and recorded; the plan expects nothing
# specific) or SKIP. All observations go to <results>/report.md, raw OpenSSH stderr
# samples to <results>/ssh-stderr/, where <results> is $APVM_E2E_RESULTS_DIR (default
# .state/results). Exit status: 0 when no check failed.
#
# Optional: APVM_E2E_BACKWPUP_FREE_ZIP and APVM_E2E_BACKWPUP_PRO_ZIP (real BackWPup builds)
# enable `family.real-backwpup`.
#
# Requires a running harness (harness.sh up), bash ≥ 3.2, OpenSSH and python3 ≥ 3.9.
# One run at a time per checkout (a lock in .state). Every group starts from freshly
# reset sites, and the run resets them again when it ends or is interrupted.
set -uo pipefail

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" >/dev/null && pwd)
# shellcheck source=lib/common.sh
. "$SCRIPT_DIR/lib/common.sh"
# shellcheck source=lib/verify.sh
. "$SCRIPT_DIR/lib/verify.sh"
for check_file in "$SCRIPT_DIR"/checks/*.sh; do
	# shellcheck source=/dev/null
	. "$check_file"
done

ALL_GROUPS="harness sudo perms shells wpcli family ssh"
LOCKED=0

# usage — prints this file's header comment.
usage() {
	awk 'NR > 2 && /^#/ { sub(/^# ?/, ""); print; next } NR > 2 { exit }' "$0"
}

# acquire_lock — takes the per-checkout run lock, or exits when a live run holds it (a
# lock left by a killed run is taken over).
acquire_lock() {
	local pid
	if ! mkdir "$LOCK_DIR" 2>/dev/null; then
		pid=$(verify_running) && die "verify.sh (PID $pid) is already running on this harness"
		rm -rf "$LOCK_DIR"
		mkdir "$LOCK_DIR" || die "cannot take the run lock $LOCK_DIR"
	fi
	echo $$ >"$LOCK_DIR/pid"
	LOCKED=1
}

# cleanup — EXIT trap: stops a background listener, removes the scratch dir and the lock.
cleanup() {
	[ -n "${LISTENER_PID:-}" ] && kill "$LISTENER_PID" 2>/dev/null
	[ -n "$WORK_DIR" ] && rm -rf "$WORK_DIR"
	[ "$LOCKED" = 1 ] && rm -rf "$LOCK_DIR"
	return 0
}

# interrupted — INT/TERM trap: leaves the harness clean for the next run, then exits.
interrupted() {
	trap - INT TERM
	echo "interrupted: resetting the sites" >&2
	target_exec apvm-e2e reset >/dev/null 2>&1
	exit 130
}

# write_report_header GROUPS — starts the Markdown report.
write_report_header() {
	{
		echo "# APVM site-deployment E2E — ground truth (plan §3.5)"
		echo
		echo "- Date: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
		echo "- Host: $(uname -sm), $(ssh -V 2>&1)"
		echo "- Groups: $1"
		echo
	} >"$REPORT"
}

# summary — prints and records the totals.
summary() {
	local line="PASS $COUNT_PASS · FAIL $COUNT_FAIL · INFO $COUNT_INFO · SKIP $COUNT_SKIP"
	echo
	echo "$line${FAILED_IDS:+ — failed:$FAILED_IDS}"
	echo "Report: $REPORT"
	printf '## Summary\n\n%s\n' "$line" >>"$REPORT"
}

main() {
	local groups="" group
	while [ $# -gt 0 ]; do
		case $1 in
		-v) VERBOSE=1 ;;
		-h | --help)
			usage
			return 0
			;;
		*)
			case " $ALL_GROUPS " in
			*" $1 "*)
				case " $groups " in
				*" $1 "*) ;;
				*) groups="$groups $1" ;;
				esac
				;;
			*) die "unknown group '$1' (groups: $ALL_GROUPS)" ;;
			esac
			;;
		esac
		shift
	done
	[ -n "$groups" ] || groups=$ALL_GROUPS

	[ -f "$ENV_FILE" ] || die "the harness is not up (run: $SCRIPT_DIR/harness.sh up)"
	# The address `harness.sh up` used, whatever this shell's APVM_E2E_* say.
	# shellcheck source=/dev/null
	. "$ENV_FILE"
	# shellcheck disable=SC2034 # read by checks/*.sh
	SSH_HOST=$APVM_E2E_SSH_HOST SSH_PORT=$APVM_E2E_SSH_PORT
	command -v python3 >/dev/null || die "python3 is required"
	trap cleanup EXIT
	acquire_lock
	WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/apvm-e2e-verify.XXXXXX") || die "mktemp failed"
	trap interrupted INT TERM
	mkdir -p "$RESULTS_DIR"
	REPORT="$RESULTS_DIR/report.md"
	SAMPLES_DIR="$RESULTS_DIR/ssh-stderr"
	rm -rf "$SAMPLES_DIR"
	mkdir -p "$SAMPLES_DIR"
	SITE_TABLE=$(target_exec apvm-e2e sites) || die "cannot reach the target container"

	write_report_header "$groups"
	for group in $groups; do
		echo "== $group"
		printf '## %s\n\n' "$group" >>"$REPORT"
		# shellcheck disable=SC2119 # no SITE argument = every site
		reset_sites
		"group_$group"
	done
	# shellcheck disable=SC2119
	reset_sites
	summary
	[ "$COUNT_FAIL" -eq 0 ]
}

main "$@"
