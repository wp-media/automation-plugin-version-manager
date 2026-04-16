#!/bin/sh
# shellcheck disable=SC3043  # 'local' works on dash, bash, ash, zsh — all modern /bin/sh
# shellcheck disable=SC2059  # Variables in printf format strings are safe (ANSI color codes only)
#
# APVM Installer for macOS and Linux
# https://github.com/wp-media/automation-plugin-version-manager
#
# Downloads the latest pre-built APVM CLI binary for the current platform
# and installs it to ~/.apvm/bin/ (or $APVM_INSTALL/bin/).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/wp-media/automation-plugin-version-manager/develop/install.sh | sh
#
# Environment variables:
#   APVM_INSTALL  — Override the install directory (default: $HOME/.apvm)
#
# Requirements:
#   - curl or wget
#   - uname (POSIX standard, pre-installed on all Unix systems)
#   - sha256sum (Linux) or shasum (macOS) — for optional checksum verification
#
# Sources:
#   - GitHub release download pattern:
#     https://docs.github.com/en/repositories/releasing-projects-on-github/linking-to-releases
#   - POSIX uname specification:
#     https://pubs.opengroup.org/onlinepubs/9699919799/utilities/uname.html
#   - POSIX trap specification:
#     https://pubs.opengroup.org/onlinepubs/9699919799/utilities/V3_chap02.html#trap
#   - curl --proto / --tlsv1.2 (same as rustup):
#     https://curl.se/docs/manpage.html
#   - wget --https-only:
#     https://www.gnu.org/software/wget/manual/wget.html

set -e

# ── Configuration ──────────────────────────────────────────────────────────

REPO="wp-media/automation-plugin-version-manager"
BINARY_NAME="apvm"
INSTALL_DIR="${APVM_INSTALL:-$HOME/.apvm}"
BIN_DIR="${INSTALL_DIR}/bin"
BASE_URL="https://github.com/${REPO}/releases/latest/download"

# ── Colors ─────────────────────────────────────────────────────────────────

setup_colors() {
    # Only use colors if stdout is a terminal and TERM is set and not "dumb".
    # Reference: https://no-color.org/
    if [ -t 1 ] && [ "${TERM-}" != "dumb" ] && [ -z "${NO_COLOR-}" ]; then
        RED='\033[0;31m'
        GREEN='\033[0;32m'
        YELLOW='\033[0;33m'
        BLUE='\033[0;34m'
        BOLD='\033[1m'
        DIM='\033[2m'
        RESET='\033[0m'
    else
        RED=''
        GREEN=''
        YELLOW=''
        BLUE=''
        BOLD=''
        DIM=''
        RESET=''
    fi
}

# ── Logging ────────────────────────────────────────────────────────────────

info() {
    printf "${BLUE}info${RESET}  %s\n" "$1"
}

success() {
    printf "${GREEN}  ✓${RESET}  %s\n" "$1"
}

warn() {
    printf "${YELLOW}warn${RESET}  %s\n" "$1" >&2
}

error() {
    printf "${RED}error${RESET} %s\n" "$1" >&2
}

# ── Utility ────────────────────────────────────────────────────────────────

# Check if a command exists on PATH.
# Uses POSIX 'command -v' (preferred over 'which').
# https://pubs.opengroup.org/onlinepubs/9699919799/utilities/command.html
has() {
    command -v "$1" >/dev/null 2>&1
}

# ── OS and Architecture Detection ─────────────────────────────────────────

detect_platform() {
    # uname -s returns the operating system name (POSIX)
    # https://pubs.opengroup.org/onlinepubs/9699919799/utilities/uname.html
    local os
    os="$(uname -s)"
    case "$os" in
        Darwin) os="darwin" ;;
        Linux)  os="linux" ;;
        CYGWIN*|MINGW*|MSYS*)
            error "Windows detected via MSYS/Cygwin/MinGW."
            error "Please use the PowerShell installer instead:"
            error ""
            error "  irm https://raw.githubusercontent.com/${REPO}/develop/install.ps1 | iex"
            error ""
            error "Alternatively, if you have the Rust toolchain installed (>= 1.94.1):"
            error "  cargo install --path crates/cli"
            error "  https://github.com/${REPO}#install-from-source"
            exit 1
            ;;
        *)
            error "No pre-built binary available for operating system: ${os}"
            error "APVM provides pre-built binaries for macOS, Linux and Windows."
            error ""
            error "If you have the Rust toolchain installed (>= 1.94.1), build from source:"
            error "  cargo install --path crates/cli"
            error "  https://github.com/${REPO}#install-from-source"
            exit 1
            ;;
    esac

    # uname -m returns the machine hardware name (POSIX)
    local arch
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64)  arch="x64" ;;
        aarch64|arm64) arch="arm64" ;;
        *)
            error "No pre-built binary available for architecture: ${arch}"
            error "APVM provides pre-built binaries for x86_64 (x64) and aarch64 (arm64)."
            error ""
            error "If you have the Rust toolchain installed (>= 1.94.1), build from source:"
            error "  cargo install --path crates/cli"
            error "  https://github.com/${REPO}#install-from-source"
            exit 1
            ;;
    esac

    # Map OS+arch to the artifact name published by release-cli.yml
    case "${os}-${arch}" in
        darwin-arm64) ARTIFACT="apvm-darwin-arm64" ;;
        darwin-x64)   ARTIFACT="apvm-darwin-x64" ;;
        linux-x64)    ARTIFACT="apvm-linux-x64-gnu" ;;
        linux-arm64)  ARTIFACT="apvm-linux-arm64-gnu" ;;
        *)
            error "No pre-built binary available for ${os}-${arch}."
            error ""
            error "If you have the Rust toolchain installed (>= 1.94.1), build from source:"
            error "  cargo install --path crates/cli"
            error "  https://github.com/${REPO}#install-from-source"
            exit 1
            ;;
    esac

    PLATFORM_OS="$os"
    PLATFORM_ARCH="$arch"
}

# ── Download ───────────────────────────────────────────────────────────────

# Download a URL to a local file, using curl (preferred) or wget.
# Both tools follow HTTP redirects (needed for GitHub release URLs).
download() {
    local url="$1"
    local output="$2"

    if has curl; then
        # --proto '=https'  Restrict to HTTPS only (security, same as rustup)
        # --tlsv1.2         Require TLS 1.2 minimum (same as rustup)
        # -f                Fail silently on HTTP errors (returns exit code 22)
        # -s                Silent mode (no progress bar)
        # -S                Show errors even in silent mode
        # -L                Follow redirects (GitHub uses 302 for asset downloads)
        # -o                Write output to file
        # https://curl.se/docs/manpage.html
        curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$output"
    elif has wget; then
        # -q                Quiet mode
        # -O                Write output to file
        # Note: --https-only only applies in recursive mode (GNU wget docs §2.8);
        # for a single file download the URL itself already enforces HTTPS.
        # https://www.gnu.org/software/wget/manual/wget.html
        wget -q "$url" -O "$output"
    else
        error "Neither 'curl' nor 'wget' found in PATH."
        error "Please install curl or wget and try again."
        error ""
        error "  macOS:  curl is pre-installed"
        error "  Debian: sudo apt-get install curl"
        error "  Fedora: sudo dnf install curl"
        error "  Alpine: apk add curl"
        exit 1
    fi
}

# ── Checksum Verification ─────────────────────────────────────────────────

# Verify the SHA-256 checksum of a downloaded file against checksums.txt.
# The checksums.txt file is generated by the release workflow using sha256sum.
# Format: "<64-char hex hash>  <filename>"
verify_checksum() {
    local file="$1"
    local checksums_file="$2"
    local artifact_name="$3"

    # Extract expected hash for our artifact from checksums.txt.
    # grep for the artifact name at end of line; awk extracts the first field (hash).
    local expected
    expected=$(grep "  ${artifact_name}$" "$checksums_file" | awk '{ print $1 }')

    if [ -z "$expected" ]; then
        warn "Could not find checksum for '${artifact_name}' in checksums.txt — skipping verification"
        return 0
    fi

    # Compute actual hash. macOS ships shasum (Perl), Linux ships sha256sum (coreutils).
    # https://www.gnu.org/software/coreutils/manual/html_node/sha2-utilities.html
    # https://perldoc.perl.org/shasum
    local actual
    if has sha256sum; then
        actual=$(sha256sum "$file" | awk '{ print $1 }')
    elif has shasum; then
        actual=$(shasum -a 256 "$file" | awk '{ print $1 }')
    else
        warn "Neither 'sha256sum' nor 'shasum' found — skipping checksum verification"
        return 0
    fi

    if [ "$actual" != "$expected" ]; then
        error "Checksum verification failed!"
        error "  Expected: ${expected}"
        error "  Actual:   ${actual}"
        error ""
        error "The downloaded file may be corrupted or tampered with."
        error "Please try again. If the problem persists, open an issue:"
        error "  https://github.com/${REPO}/issues"
        exit 1
    fi
}

# ── PATH Configuration ────────────────────────────────────────────────────

# Add BIN_DIR to the user's shell profile if not already present.
# Detects the user's default shell ($SHELL) and modifies the appropriate RC file.
configure_path() {
    local path_entry="export PATH=\"${BIN_DIR}:\$PATH\""
    local fish_path_entry="fish_add_path \"${BIN_DIR}\""

    # If already in PATH, nothing to do.
    case ":${PATH}:" in
        *":${BIN_DIR}:"*)
            return 0
            ;;
    esac

    # Determine which shell config file to modify.
    # $SHELL contains the user's login shell (set by the OS, not the current process).
    # https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/V3_chap08.html
    local shell_name
    shell_name="$(basename "${SHELL:-sh}")"
    local rc_file=""
    local entry=""

    case "$shell_name" in
        bash)
            # bash: ~/.bashrc for interactive non-login; ~/.bash_profile for login.
            # Most users expect PATH modifications in ~/.bashrc.
            # https://www.gnu.org/software/bash/manual/bash.html#Bash-Startup-Files
            if [ -f "$HOME/.bashrc" ]; then
                rc_file="$HOME/.bashrc"
            elif [ -f "$HOME/.bash_profile" ]; then
                rc_file="$HOME/.bash_profile"
            else
                rc_file="$HOME/.bashrc"
            fi
            entry="$path_entry"
            ;;
        zsh)
            # zsh: ~/.zshrc for interactive shells.
            # https://zsh.sourceforge.io/Doc/Release/Files.html#Startup_002fShutdown-Files
            rc_file="${ZDOTDIR:-$HOME}/.zshrc"
            entry="$path_entry"
            ;;
        fish)
            # fish: ~/.config/fish/config.fish
            # fish_add_path is idempotent and persists.
            # https://fishshell.com/docs/current/cmds/fish_add_path.html
            rc_file="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
            entry="$fish_path_entry"
            ;;
        *)
            # Fallback for other POSIX shells: ~/.profile
            # https://pubs.opengroup.org/onlinepubs/9699919799/utilities/sh.html
            rc_file="$HOME/.profile"
            entry="$path_entry"
            ;;
    esac

    # Guard: don't add if already present in the file.
    # Set PATH_UPDATED so the summary tells the user to "restart your terminal"
    # instead of "add the following to your shell profile" on re-runs where PATH
    # hasn't been reloaded yet (same terminal session).
    if grep -qF "$BIN_DIR" "$rc_file" 2>/dev/null; then
        PATH_UPDATED="$rc_file"
        return 0
    fi

    # Create parent directory if needed (e.g. ~/.config/fish/ for first-time fish users).
    # Non-fatal: if we cannot create it, skip the PATH update and tell the user.
    if ! mkdir -p "$(dirname "$rc_file")" 2>/dev/null; then
        warn "Could not create directory for ${rc_file} — skipping PATH update"
        warn "Please add manually: export PATH=\"${BIN_DIR}:\$PATH\""
        return 0
    fi

    # Append the PATH export. Non-fatal so that a read-only RC file (e.g. managed
    # by a system administrator) does not abort an otherwise-successful install.
    if ! printf '\n# APVM — https://github.com/%s\n%s\n' "$REPO" "$entry" >> "$rc_file" 2>/dev/null; then
        warn "Could not update ${rc_file} — skipping PATH update"
        warn "Please add manually: export PATH=\"${BIN_DIR}:\$PATH\""
        return 0
    fi

    PATH_UPDATED="$rc_file"
}

# ── Main ───────────────────────────────────────────────────────────────────

# Declared at global scope so the EXIT trap can access it after main() returns.
# If declared local, bash destroys it before EXIT fires, causing rm -rf "" (no-op).
tmp_dir=""

main() {
    setup_colors

    printf "\n"
    printf "${BOLD}  ┌──────────────────────────────────┐${RESET}\n"
    printf "${BOLD}  │     APVM Installer (Unix)        │${RESET}\n"
    printf "${BOLD}  └──────────────────────────────────┘${RESET}\n"
    printf "${DIM}  https://github.com/${REPO}${RESET}\n"
    printf "\n"

    # Track whether we updated any shell profile
    PATH_UPDATED=""

    # ── Step 1: Detect platform ────────────────────────────────────────────

    detect_platform
    info "Detected platform: ${BOLD}${PLATFORM_OS} ${PLATFORM_ARCH}${RESET}"
    info "Artifact: ${ARTIFACT}"

    # ── Step 2: Create temp directory ──────────────────────────────────────

    tmp_dir="$(mktemp -d)"
    # Ensure cleanup on exit (normal, error, or interrupt).
    # https://pubs.opengroup.org/onlinepubs/9699919799/utilities/V3_chap02.html#trap
    trap 'rm -rf "$tmp_dir"' EXIT INT TERM

    # ── Step 3: Download binary ────────────────────────────────────────────

    local binary_url="${BASE_URL}/${ARTIFACT}"
    local binary_path="${tmp_dir}/${ARTIFACT}"

    info "Downloading from latest release..."
    if ! download "$binary_url" "$binary_path"; then
        error "Failed to download: ${binary_url}"
        error ""
        error "Possible causes:"
        error "  - No internet connection"
        error "  - No release has been published yet"
        error "  - GitHub is experiencing an outage"
        error ""
        error "Check: https://github.com/${REPO}/releases/latest"
        exit 1
    fi
    success "Downloaded binary"

    # ── Step 4: Download and verify checksum ───────────────────────────────

    local checksums_url="${BASE_URL}/checksums.txt"
    local checksums_path="${tmp_dir}/checksums.txt"

    info "Verifying checksum (SHA-256)..."
    if download "$checksums_url" "$checksums_path" 2>/dev/null; then
        verify_checksum "$binary_path" "$checksums_path" "$ARTIFACT"
        success "Checksum verified"
    else
        warn "Could not download checksums.txt — skipping verification"
    fi

    # ── Step 5: Install binary ─────────────────────────────────────────────

    mkdir -p "$BIN_DIR"

    # Use cp + rm instead of mv to handle cross-device moves (tmp may be on
    # a different filesystem than $HOME).
    cp "$binary_path" "${BIN_DIR}/${BINARY_NAME}"
    rm -f "$binary_path"

    # Set executable permission.
    # https://pubs.opengroup.org/onlinepubs/9699919799/utilities/chmod.html
    chmod +x "${BIN_DIR}/${BINARY_NAME}"
    success "Installed to ${BIN_DIR}/${BINARY_NAME}"

    # ── Step 6: Configure PATH ─────────────────────────────────────────────

    configure_path

    # ── Step 7: Print summary ──────────────────────────────────────────────

    printf "\n"

    # Try to get the installed version
    local version_output
    version_output=$("${BIN_DIR}/${BINARY_NAME}" --version 2>/dev/null || true)

    if [ -z "$version_output" ]; then
        error "Installation completed but the binary could not be executed."
        error "This may indicate a platform mismatch. Please report an issue:"
        error "  https://github.com/${REPO}/issues"
        exit 1
    fi

    printf "${GREEN}${BOLD}  ┌──────────────────────────────────┐${RESET}\n"
    printf "${GREEN}${BOLD}  │   APVM installed successfully!   │${RESET}\n"
    printf "${GREEN}${BOLD}  └──────────────────────────────────┘${RESET}\n"
    printf "\n"
    printf "  ${DIM}Version :${RESET}  %s\n" "$version_output"
    printf "  ${DIM}Binary  :${RESET}  %s\n" "${BIN_DIR}/${BINARY_NAME}"

    # Show PATH instructions if needed
    case ":${PATH}:" in
        *":${BIN_DIR}:"*)
            printf "\n"
            printf "  Run ${BOLD}apvm --help${RESET} to get started.\n"
            ;;
        *)
            printf "\n"
            if [ -n "$PATH_UPDATED" ]; then
                printf "  ${YELLOW}Restart your terminal${RESET} to update your PATH, or run:\n"
            else
                printf "  ${YELLOW}Add the following to your shell profile, then restart your terminal:${RESET}\n"
            fi
            printf "\n"

            local shell_name
            shell_name="$(basename "${SHELL:-sh}")"
            case "$shell_name" in
                fish)
                    printf "    ${BOLD}fish_add_path \"%s\"${RESET}\n" "$BIN_DIR"
                    ;;
                *)
                    printf "    ${BOLD}export PATH=\"%s:\$PATH\"${RESET}\n" "$BIN_DIR"
                    ;;
            esac

            printf "\n"
            printf "  Then run ${BOLD}apvm --help${RESET} to get started.\n"
            ;;
    esac

    printf "\n"
}

main
