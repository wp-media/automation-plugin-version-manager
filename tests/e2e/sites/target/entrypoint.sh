#!/bin/sh
#
# Container entrypoint: provision (SSH keys, WordPress installs, snapshots), start
# Apache in the background, then run sshd in the foreground as the main process,
# so the container lives exactly as long as SSH, the service under test.
set -eu

apvm-e2e start
apache2ctl start
touch /run/apvm-e2e/ready
echo "apvm-e2e: ready"
exec /usr/sbin/sshd -D -e
