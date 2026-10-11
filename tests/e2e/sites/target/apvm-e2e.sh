#!/bin/bash
#
# apvm-e2e — provisions, checks and resets the APVM site-deployment E2E target.
# Installed in the image as /usr/local/bin/apvm-e2e. Layout and users: ../README.md.
#
# Usage:
#   apvm-e2e image            Build time: users, dotfiles, site files, wp-config.php.
#   apvm-e2e start            Container start: SSH keys, WordPress installs, snapshots.
#   apvm-e2e reset [SITE...]  Restore sites (default: all) to their post-install
#                             snapshot (files and database), and the shared state checks
#                             may touch: /tmp apvm leftovers, WP-CLI user configs, sudo
#                             timestamps, ftpuser's home mode, the chown sentinel.
#   apvm-e2e health           Exit 0 once provisioning finished, sshd and Apache run and
#                             the database answers.
#   apvm-e2e sites            Print the site table: id|path|url|owner|group|database.
set -euo pipefail

readonly STATE_DIR=/var/lib/apvm-e2e
readonly RUN_DIR=/run/apvm-e2e
readonly SENTINEL_DIR=/usr/local/share/apvm-e2e
# Must match the `db` service in ../compose.yaml.
readonly DB_HOST=db DB_USER=wp DB_PASSWORD=wp
readonly ADMIN_EMAIL=admin@example.com

# Sites: id|path|url|owner|group|database.
readonly SITE_ROWS=(
	"single|/var/www/single|http://single.test|www-data|www-data|wp_single"
	"multi|/var/www/multi|http://multi.test|www-data|www-data|wp_multi"
	"ftpsite|/home/ftpuser/public_html|http://ftpsite.test|ftpuser|www-data|wp_ftpsite"
)
readonly SITES=(single multi ftpsite)

# Login users with their own group: name:uid:login shell. ftpuser is added separately.
readonly LOGIN_USERS=(
	"deploy:2001:/bin/bash"
	"plain:2002:/bin/bash"
	"pwsudo:2003:/bin/bash"
	"ttyreq:2004:/bin/bash"
	"fishy:2005:/usr/bin/fish"
	"noisy:2006:/bin/bash"
	"dashy:2007:/bin/dash"
	"zshy:2008:/usr/bin/zsh"
	"tcshy:2009:/usr/bin/tcsh"
	"mixed:2011:/bin/bash"
)
readonly FTPUSER_UID=2010
readonly SENTINEL_TEXT='Symlink target for chown checks. Must stay root:root.'

# die MESSAGE... — prints an error and exits 1.
die() {
	echo "apvm-e2e: $*" >&2
	exit 1
}

# log MESSAGE... — prints a progress line.
log() {
	echo "apvm-e2e: $*"
}

# load_site SITE — sets SITE_PATH, SITE_URL, SITE_OWNER, SITE_GROUP and SITE_DB;
# exits for an unknown site.
load_site() {
	local row id
	for row in "${SITE_ROWS[@]}"; do
		IFS='|' read -r id SITE_PATH SITE_URL SITE_OWNER SITE_GROUP SITE_DB <<<"$row"
		[[ $id == "$1" ]] && return 0
	done
	die "unknown site '$1' (known: ${SITES[*]})"
}

# wp_as_owner ARG... — runs WP-CLI on the loaded site as its owner and group, with a
# private HOME so provisioning leaves nothing in the users' real homes.
wp_as_owner() {
	local home="$STATE_DIR/home/$SITE_OWNER"
	install -d -o "$SITE_OWNER" -g "$SITE_GROUP" -m 0700 "$home"
	runuser -u "$SITE_OWNER" -g "$SITE_GROUP" -- env HOME="$home" wp --path="$SITE_PATH" "$@"
}

# all_login_users — prints every user that gets the harness's authorized key.
all_login_users() {
	local entry
	echo root
	for entry in "${LOGIN_USERS[@]}"; do echo "${entry%%:*}"; done
	echo ftpuser
}

# ---------------------------------------------------------------------------
# image stage
# ---------------------------------------------------------------------------

# create_users — adds the login users, ftpuser and the noisy user's dotfile.
create_users() {
	local entry name uid shell
	for entry in "${LOGIN_USERS[@]}"; do
		IFS=: read -r name uid shell <<<"$entry"
		useradd --create-home --uid "$uid" --user-group --shell "$shell" "$name"
		# '*' = no password but not locked, so key logins work.
		usermod --password '*' "$name"
	done
	# Real passwords, so their password-requiring sudo rules are genuine.
	printf '%s\n' 'pwsudo:pwsudo-password' 'mixed:mixed-password' | chpasswd

	# ftpuser owns the FTP-style site: primary group ftpuser, member of www-data, no sudo.
	# Its home is 0750 ftpuser:www-data, like shared hosting: Apache (www-data) can reach
	# public_html, other users (deploy) cannot even stat it without root.
	useradd --create-home --uid "$FTPUSER_UID" --user-group --groups www-data --shell /bin/bash ftpuser
	usermod --password '*' ftpuser
	chgrp www-data /home/ftpuser
	chmod 0750 /home/ftpuser

	# noisy: dotfile output on every SSH command, like many real servers.
	{
		echo '# APVM E2E: unconditional output, also for non-interactive SSH commands.'
		echo "echo 'apvm-e2e: stdout noise from ~/.bashrc'"
		echo "echo 'apvm-e2e: stderr noise from ~/.bashrc' >&2"
		cat /etc/skel/.bashrc
	} >/home/noisy/.bashrc
	chown noisy:noisy /home/noisy/.bashrc
}

# layout_site SITE — copies WordPress into place with the site's owner and modes and
# writes wp-config.php. The database install happens at start.
layout_site() {
	load_site "$1"
	install -d -o "$SITE_OWNER" -g "$SITE_GROUP" "$SITE_PATH"
	cp -a /usr/src/wordpress/. "$SITE_PATH/"
	chown -R "$SITE_OWNER:$SITE_GROUP" "$SITE_PATH"
	wp_as_owner config create --dbname="$SITE_DB" --dbuser="$DB_USER" --dbpass="$DB_PASSWORD" \
		--dbhost="$DB_HOST" --skip-check --skip-salts --quiet
	if [[ $1 == ftpsite ]]; then
		# FTP-style: group-writable, setgid dirs so new entries inherit www-data.
		find "$SITE_PATH" -type d -exec chmod 2775 {} +
		find "$SITE_PATH" -type f -exec chmod 0664 {} +
	fi
}

# add_single_extras — gives `single` a must-use plugin and a drop-in, so plugin lists
# contain every status WP-CLI reports.
add_single_extras() {
	load_site single
	local content="$SITE_PATH/wp-content"
	install -d -o "$SITE_OWNER" -g "$SITE_GROUP" -m 0755 "$content/mu-plugins"
	cat >"$content/mu-plugins/apvm-e2e-mu.php" <<'PHP'
<?php
/**
 * Plugin Name: APVM E2E Must-Use
 * Description: Baseline must-use plugin of the E2E harness. Does nothing.
 *
 * @package ApvmFixtures
 */
PHP
	cat >"$content/advanced-cache.php" <<'PHP'
<?php
/**
 * Baseline drop-in of the E2E harness. WordPress loads it only when WP_CACHE is
 * true, which the harness never sets.
 *
 * @package ApvmFixtures
 */
PHP
	chown "$SITE_OWNER:$SITE_GROUP" "$content/mu-plugins/apvm-e2e-mu.php" "$content/advanced-cache.php"
}

# cmd_image — the build-time stage.
cmd_image() {
	create_users

	# A root-owned file that symlink checks point at; it must never change owner.
	restore_sentinel

	# Client settings for root's snapshot/restore commands (MariaDB clients ≥ 11.4
	# otherwise insist on verifying the server's self-signed TLS certificate).
	printf '[client]\nhost=%s\nuser=%s\npassword=%s\nskip-ssl\n' "$DB_HOST" "$DB_USER" "$DB_PASSWORD" >/root/.my.cnf
	chmod 0600 /root/.my.cnf

	local site
	for site in "${SITES[@]}"; do layout_site "$site"; done
	add_single_extras
}

# ---------------------------------------------------------------------------
# start stage
# ---------------------------------------------------------------------------

# install_authorized_keys — trusts $APVM_E2E_AUTHORIZED_KEY for every login user.
install_authorized_keys() {
	local key=${APVM_E2E_AUTHORIZED_KEY:-} user home group
	[[ $key =~ ^(ssh-|ecdsa-|sk-)[^[:space:]]+\ [A-Za-z0-9+/=]+ && $key != *$'\n'* ]] ||
		die "APVM_E2E_AUTHORIZED_KEY must hold one OpenSSH public key; start the harness with harness.sh up"
	for user in $(all_login_users); do
		home=$(getent passwd "$user" | cut -d: -f6)
		group=$(id -gn "$user")
		install -d -o "$user" -g "$group" -m 0700 "$home/.ssh"
		printf '%s\n' "$key" >"$home/.ssh/authorized_keys"
		chown "$user:$group" "$home/.ssh/authorized_keys"
		chmod 0600 "$home/.ssh/authorized_keys"
	done
}

# wait_for_db — waits up to 60 s for the database to answer.
wait_for_db() {
	local attempt
	for attempt in $(seq 1 60); do
		mariadb -e 'SELECT 1' >/dev/null 2>&1 && return 0
		sleep 1
	done
	mariadb -e 'SELECT 1' || die "database '$DB_HOST' did not answer within 60 s (attempts: $attempt)"
}

# install_site SITE — installs WordPress for a laid-out site (multisite for `multi`) into
# an emptied database: a recreated target may meet tables left by an earlier one.
install_site() {
	load_site "$1"
	mariadb -e "DROP DATABASE IF EXISTS \`$SITE_DB\`; CREATE DATABASE \`$SITE_DB\`"
	local install=(--url="$SITE_URL" --title="APVM E2E $1" --admin_user=admin
		--admin_password=admin --admin_email="$ADMIN_EMAIL" --skip-email)
	if [[ $1 == multi ]]; then
		# A start that failed after multisite-install already holds the constants in
		# wp-config.php; adding them twice would fail, so they are skipped then.
		if wp_as_owner config has MULTISITE --type=constant 2>/dev/null; then install+=(--skip-config); fi
		wp_as_owner core multisite-install "${install[@]}"
		wp_as_owner --url="$SITE_URL" site create --slug=sub1 --title='APVM E2E multi / sub1' \
			--email="$ADMIN_EMAIL" --quiet
	else
		wp_as_owner core install "${install[@]}"
	fi
}

# take_snapshot SITE — saves the site's files and database for `reset`.
take_snapshot() {
	load_site "$1"
	local dir="$STATE_DIR/snapshots/$1"
	install -d -m 0700 "$dir"
	rsync -a --delete "$SITE_PATH/" "$dir/tree/"
	mariadb-dump --add-drop-database --databases "$SITE_DB" >"$dir/db.sql.partial"
	mv "$dir/db.sql.partial" "$dir/db.sql"
}

# restore_site SITE — puts a site's files (owners and modes included) and database
# back to the snapshot. Never writes through a symlink: a check may have replaced the
# site dir with one, and root must not follow it.
restore_site() {
	load_site "$1"
	local dir="$STATE_DIR/snapshots/$1"
	[[ -f $dir/db.sql ]] || die "site '$1' has no snapshot yet"
	[[ ! -L $(dirname "$SITE_PATH") ]] || die "parent of $SITE_PATH is a symlink; refusing to restore"
	if [[ -L $SITE_PATH ]]; then rm -f -- "$SITE_PATH"; fi
	# rsync compares size and mtime: enough here, because no check forges an mtime.
	rsync -a --delete "$dir/tree/" "$SITE_PATH/"
	mariadb <"$dir/db.sql"
}

# ensure_site — installs and snapshots a site on first start; on later starts,
# restores it if its database is gone (the database lives on a tmpfs).
ensure_site() {
	load_site "$1"
	if [[ -f $STATE_DIR/snapshots/$1/db.sql ]]; then
		wp_as_owner core is-installed --skip-plugins --skip-themes 2>/dev/null || restore_site "$1"
		return 0
	fi
	log "installing site '$1'"
	install_site "$1"
	take_snapshot "$1"
}

# cmd_start — the container-start stage. /run is not a tmpfs in containers, so a
# restarted container still holds the previous run's ready marker and pid files; their
# PIDs may now belong to other processes. Drop them first.
cmd_start() {
	rm -f "$RUN_DIR/ready" /run/sshd.pid /var/run/apache2/apache2.pid
	install -d -m 0755 "$RUN_DIR" /run/sshd
	install_authorized_keys
	ssh-keygen -A
	wait_for_db
	local site
	for site in "${SITES[@]}"; do ensure_site "$site"; done
}

# ---------------------------------------------------------------------------
# reset, health, sites
# ---------------------------------------------------------------------------

# clean_tmp — removes apvm staging dirs and site locks (plan §6.6, §6.13) and the
# harness's own scratch entries from /tmp.
clean_tmp() {
	find /tmp -mindepth 1 -maxdepth 1 \
		\( -name 'apvm.*' -o -name 'apvm-lock-*' -o -name 'apvm-e2e-*' \) -exec rm -rf -- {} +
}

# restore_sentinel — (re)creates the root-owned chown sentinel.
restore_sentinel() {
	install -d -o root -g root -m 0755 "$SENTINEL_DIR"
	printf '%s\n' "$SENTINEL_TEXT" >"$SENTINEL_DIR/sentinel"
	chown root:root "$SENTINEL_DIR/sentinel"
	chmod 0644 "$SENTINEL_DIR/sentinel"
}

# restore_shared_state — undoes what checks may change outside the site trees.
restore_shared_state() {
	local user home
	for user in $(all_login_users) www-data; do
		home=$(getent passwd "$user" | cut -d: -f6)
		rm -rf -- "${home:?}/.wp-cli"
	done
	chgrp www-data /home/ftpuser
	chmod 0750 /home/ftpuser
	rm -rf /run/sudo/ts/*
	restore_sentinel
	clean_tmp
}

# cmd_reset [SITE...] — restores the given sites (default: all) and the shared state.
cmd_reset() {
	local site
	if [[ $# -eq 0 ]]; then set -- "${SITES[@]}"; fi
	for site in "$@"; do restore_site "$site"; done
	restore_shared_state
}

# pid_is FILE NAME — true when FILE holds the PID of a running process named NAME (a
# stale pid file whose number was reused by another process does not count).
pid_is() {
	local pid
	[[ -s $1 ]] || return 1
	pid=$(cat "$1")
	[[ $pid =~ ^[0-9]+$ && $(cat "/proc/$pid/comm" 2>/dev/null) == "$2" ]]
}

# cmd_health — true once provisioning finished, sshd and Apache run and the database
# answers (it lives in another container and may have been restarted).
cmd_health() {
	[[ -f $RUN_DIR/ready ]] && pid_is /run/sshd.pid sshd && pid_is /var/run/apache2/apache2.pid apache2 &&
		mariadb --connect-timeout=3 -e 'SELECT 1' >/dev/null 2>&1
}

# cmd_sites — prints the site table.
cmd_sites() {
	printf '%s\n' "${SITE_ROWS[@]}"
}

main() {
	local command=${1:-}
	[[ $# -gt 0 ]] && shift
	case $command in
	image) cmd_image ;;
	start) cmd_start ;;
	reset) cmd_reset "$@" ;;
	health) cmd_health ;;
	sites) cmd_sites ;;
	*) die "usage: apvm-e2e image | start | reset [SITE...] | health | sites" ;;
	esac
}

main "$@"
