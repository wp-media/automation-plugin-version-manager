# shellcheck shell=bash
# The settings below are read by the scripts that source this file.
# shellcheck disable=SC2034
#
# Shared helpers for harness.sh and verify.sh. Source it; do not run it.
# Compatible with bash 3.2 (macOS /bin/bash): no associative arrays, no mapfile.

E2E_DIR=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." >/dev/null && pwd)
STATE_DIR="$E2E_DIR/.state"
KEY_FILE="$STATE_DIR/id_ed25519"
KNOWN_HOSTS="$STATE_DIR/known_hosts"
SSH_CONFIG="$STATE_DIR/ssh_config"
ENV_FILE="$STATE_DIR/env"
LOCK_DIR="$STATE_DIR/verify.lock"
RESULTS_DIR=${APVM_E2E_RESULTS_DIR:-$STATE_DIR/results}
SSH_PORT=${APVM_E2E_SSH_PORT:-2722}
HTTP_PORT=${APVM_E2E_HTTP_PORT:-8722}
SSH_HOST=127.0.0.1

# One compose project (containers, network, image) per checkout, so worktrees never
# recreate or remove each other's harness.
PROJECT="apvm-e2e-$(printf '%s' "$E2E_DIR" | cksum | cut -d ' ' -f 1)"

# Every user that can log in with the harness key (see README.md).
LOGIN_USERS="root deploy plain pwsudo mixed ttyreq fishy noisy dashy zshy tcshy ftpuser"

# die MESSAGE... — prints an error and exits 1.
die() {
	echo "error: $*" >&2
	exit 1
}

# compose ARG... — runs docker compose on this checkout's project. compose.yaml requires
# APVM_E2E_AUTHORIZED_KEY for every command; `up` sets the real key (see harness.sh),
# other commands never recreate the target, so a placeholder satisfies them.
compose() {
	APVM_E2E_AUTHORIZED_KEY=${APVM_E2E_AUTHORIZED_KEY:-unused} \
		docker compose --project-name "$PROJECT" --project-directory "$E2E_DIR" \
		-f "$E2E_DIR/compose.yaml" "$@"
}

# target_exec ARG... — runs a command as root in the target container, without a TTY.
target_exec() {
	compose exec -T target "$@"
}

# check_state_path — refuses checkout paths ssh_config cannot express: OpenSSH expands
# `%` tokens in IdentityFile/UserKnownHostsFile, and a `"` or newline breaks the quoting.
check_state_path() {
	case $STATE_DIR in
	*%* | *\"* | *$'\n'*) die "the checkout path contains %, \" or a newline, which ssh_config cannot express: $STATE_DIR" ;;
	esac
}

# verify_running — true while a live verify.sh holds the run lock (prints its PID).
verify_running() {
	local pid
	pid=$(cat "$LOCK_DIR/pid" 2>/dev/null) || return 1
	[ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && printf '%s' "$pid"
}

# known_hosts_name — prints the host as OpenSSH looks it up in known_hosts: `[host]:port`,
# or the bare host on port 22.
known_hosts_name() {
	if [ "$SSH_PORT" = 22 ]; then printf '%s' "$SSH_HOST"; else printf '[%s]:%s' "$SSH_HOST" "$SSH_PORT"; fi
}

# q VALUE — prints VALUE as one POSIX single-quoted shell word (' → '\'').
q() {
	local quote="'" escaped="'\\''"
	printf "'%s'" "${1//$quote/$escaped}"
}

# new_nonce — prints a random framing marker, like the plan's per-call nonce (§6.1).
new_nonce() {
	printf 'apvm-%s' "$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
}

# after_nonce NONCE TEXT — prints the lines of TEXT after the first line equal to NONCE.
after_nonce() {
	printf '%s\n' "$2" | awk -v nonce="$1" 'seen { print } $0 == nonce { seen = 1 }'
}

# ssh_to USER ARG... — ssh to the target as USER through the generated ssh_config.
ssh_to() {
	local user=$1
	shift
	ssh -F "$SSH_CONFIG" "apvm-e2e-$user" "$@"
}
