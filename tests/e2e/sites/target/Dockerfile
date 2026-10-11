# syntax=docker/dockerfile:1.27.1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
#
# APVM site-deployment E2E target: three WordPress sites behind OpenSSH and sudo,
# with WP-CLI. Users, sites and layout: ../README.md. Provisioning: apvm-e2e.sh.
#
# Every input is pinned (images and the Dockerfile frontend by digest, WP-CLI by SHA-512)
# except Debian packages, see below. After bumping one, rebuild and re-run ../verify.sh.

# WordPress 7.1.3 on PHP 8.4 (Debian 13), by digest. PHP 8.4 rather than 8.5:
# WP-CLI 2.12.0 raises E_DEPRECATED on PHP 8.5 (plan §3.5 results).
ARG WORDPRESS_IMAGE=wordpress:7.1.3-php8.4-apache@sha256:309307430abf80009378a63a79df73554a6449b700fac59a991b4959e6dda8ca

FROM ${WORDPRESS_IMAGE}

# WP-CLI 2.12.0 is the latest stable release (2025-05-07). The SHA-512 is the one
# published with the release (wp-cli-2.12.0.phar.sha512).
ARG WP_CLI_VERSION=2.12.0
ARG WP_CLI_SHA512=be928f6b8ca1e8dfb9d2f4b75a13aa4aee0896f8a9a0a1c45cd5d2c98605e6172e6d014dda2e27f88c98befc16c040cbb2bd1bfa121510ea5cdf5f6a30fe8832

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# openssh-server, sudo: the remote access under test.
# fish, zsh, tcsh, busybox: login shells / sh flavors the remote contract must survive.
# mariadb-client, rsync: snapshots for `apvm-e2e reset`.
# Not version-pinned: Debian drops superseded versions from its mirrors after security
# updates; the base image digest already pins the release (13). verify.sh records the
# sudo and OpenSSH versions it ran against. The host keys openssh-server's postinst
# generates are deleted in this same layer, so none ship in the image.
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      busybox fish mariadb-client openssh-server rsync sudo tcsh zsh \
 && rm -rf /var/lib/apt/lists/* /etc/ssh/ssh_host_*

RUN curl -fsSL --retry 3 --retry-all-errors -o /usr/local/bin/wp \
      "https://github.com/wp-cli/wp-cli/releases/download/v${WP_CLI_VERSION}/wp-cli-${WP_CLI_VERSION}.phar" \
 && echo "${WP_CLI_SHA512}  /usr/local/bin/wp" | sha512sum -c - \
 && chmod 0755 /usr/local/bin/wp

COPY --chmod=0440 sudoers /etc/sudoers.d/apvm-e2e
COPY --chmod=0644 sshd_config /etc/ssh/sshd_config.d/apvm-e2e.conf
COPY --chmod=0644 vhosts.conf /etc/apache2/sites-available/apvm-e2e.conf
COPY --chmod=0755 apvm-e2e.sh /usr/local/bin/apvm-e2e
COPY --chmod=0755 entrypoint.sh /usr/local/bin/apvm-e2e-entrypoint

# Validate the configs, then provision users and site files (no database yet).
# Host keys are generated per container at start, never baked into the image.
RUN visudo -cf /etc/sudoers.d/apvm-e2e \
 && ssh-keygen -A && sshd -t && rm -f /etc/ssh/ssh_host_* \
 && a2dissite 000-default && a2ensite apvm-e2e \
 && apvm-e2e image

EXPOSE 22 80

# The wordpress image stops Apache with SIGWINCH, which sshd (the main process) ignores.
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=10s --timeout=5s --start-period=180s --start-interval=1s --retries=3 \
  CMD ["apvm-e2e", "health"]

ENTRYPOINT ["apvm-e2e-entrypoint"]
