# shellcheck shell=bash
#
# Runs as root inside the target (checks/ssh.sh check_system_config_include): how the
# target's OpenSSH client treats the system-wide /etc/ssh/ssh_config on its own and once a
# generated-style `-F` file includes it, for a throwaway user and a file owned by another
# user, one its group may write, and one the user cannot read; then the user's own file,
# writable by its private group or by others, included. The file is restored, and the
# users removed, on exit. Prints `RESULT <name> <value>` lines.
set -u

user=apvm-e2e-sysconf owner=apvm-e2e-sysowner conf=/etc/ssh/ssh_config
# Outside /tmp/apvm-e2e-*, which `harness.sh reset` deletes: a run killed before its
# cleanup leaves the only unchanged copy here, and the next run restores it first.
backup=/etc/ssh/ssh_config.apvm-e2e-orig

# cleanup — restores the system ssh_config (owner and mode too) and removes the users.
cleanup() {
	[ -f "$backup" ] && cp -p "$backup" "$conf" && rm -f "$backup"
	userdel -r "$user" >/dev/null 2>&1
	userdel "$owner" >/dev/null 2>&1
}

# hostname_of ARG... — what `ssh ARG... -G apvm-sysalias` says as the user: the HostName
# it resolves, or the error that stopped it (ssh ends its own lines with CR LF).
hostname_of() {
	su -s /bin/bash "$user" -c "cd && ssh $* -G apvm-sysalias 2>&1" | tr -d '\r' |
		sed -n 's/^hostname //p; /Bad owner/p; /open user config/p' | head -n 1
}

if [ -f "$backup" ]; then
	cp -p "$backup" "$conf" || exit 1
fi
cp -p "$conf" "$backup" || exit 1
trap cleanup EXIT
userdel -r "$user" >/dev/null 2>&1
userdel "$owner" >/dev/null 2>&1
useradd --create-home --shell /bin/bash "$user" && useradd --no-create-home "$owner" || exit 1
printf '\nHost apvm-sysalias\n  HostName 127.0.0.9\n' >>"$conf"
printf 'Include "%s"\n' "$conf" >"/home/$user/gen"
chown "$user" "/home/$user/gen"
chmod 600 "/home/$user/gen"

chown "$owner" "$conf"
echo "RESULT other-owner-alone $(hostname_of)"
echo "RESULT other-owner-included $(hostname_of -F gen)"
chown root "$conf"
chmod 664 "$conf"
echo "RESULT group-writable-alone $(hostname_of)"
echo "RESULT group-writable-included $(hostname_of -F gen)"
chmod 600 "$conf"
echo "RESULT unreadable-alone $(hostname_of)"
echo "RESULT unreadable-included $(hostname_of -F gen)"

# Debian's ssh (its user-group-modes patch) also includes a file of the user's that its
# group may write, when that group is the user's own private group (useradd's default);
# never one others may write. apvm keeps upstream's stricter rule.
own=/home/$user/own
printf 'Host apvm-sysalias\n  HostName 127.0.0.8\n' >"$own"
chown "$user:$user" "$own"
printf 'Include "%s"\n' "$own" >"/home/$user/gen-own"
chown "$user" "/home/$user/gen-own"
chmod 600 "/home/$user/gen-own"
chmod 664 "$own"
echo "RESULT private-group-writable-included $(hostname_of -F gen-own)"
chmod 666 "$own"
echo "RESULT other-writable-included $(hostname_of -F gen-own)"
