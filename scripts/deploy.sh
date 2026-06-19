#!/usr/bin/env bash

# --- Bash guard (POSIX-compatible) -------------------------------------------
# When invoked as "curl ... | sh" on systems where sh is dash (e.g. Ubuntu 24),
# the script must re-launch itself under bash. This block uses only POSIX syntax.
if [ -z "${BASH_VERSION:-}" ]; then
  if ! command -v bash >/dev/null 2>&1; then
    echo "Error: This script requires bash, which was not found." >&2
    echo "       Install bash and re-run:  curl -fsSL https://temps.sh/deploy.sh | bash" >&2
    exit 1
  fi
  # Running from a real file (e.g. "sh deploy.sh") — re-exec under bash.
  if [ -f "$0" ] 2>/dev/null; then
    exec bash "$0" "$@"
  fi
  # Piped via stdin (e.g. "curl ... | sh") — we cannot reliably re-exec
  # because part of stdin has already been consumed by the current shell.
  echo "Error: This script requires bash. Please re-run with:" >&2
  echo "       curl -fsSL https://temps.sh/deploy.sh | bash" >&2
  exit 1
fi
# ------------------------------------------------------------------------------

set -euo pipefail

# ============================================================================
#  Temps Provisioning Wizard
#  A beautiful TUI to set up Docker, TimescaleDB, SSL certificates, and Temps
# ============================================================================

# Wrap in block to ensure bash reads entire script before executing (needed for curl | bash)
{

# ---------------------------------------------------------------------------
# TUI Framework: Colors, Symbols, Drawing
# ---------------------------------------------------------------------------

BOLD='' DIM='' RESET='' UNDERLINE=''
RED='' GREEN='' YELLOW='' BLUE='' CYAN='' MAGENTA='' WHITE=''
BG_BLUE='' BG_GREEN='' BG_RED='' BG_YELLOW=''

if [[ -t 1 ]]; then
  BOLD='\033[1m'       DIM='\033[2m'        RESET='\033[0m'
  UNDERLINE='\033[4m'
  RED='\033[0;31m'     GREEN='\033[0;32m'   YELLOW='\033[0;33m'
  BLUE='\033[0;34m'    CYAN='\033[0;36m'    MAGENTA='\033[0;35m'
  WHITE='\033[0;37m'
  BG_BLUE='\033[44m'   BG_GREEN='\033[42m'  BG_RED='\033[41m'
  BG_YELLOW='\033[43m'
fi

# Symbols
CHECK="${GREEN}✓${RESET}"
CROSS="${RED}✗${RESET}"
ARROW="${CYAN}→${RESET}"
BULLET="${DIM}•${RESET}"
SPINNER_CHARS='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'

# ---------------------------------------------------------------------------
# Platform detection
# ---------------------------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"
IS_MACOS=false
IS_LINUX=false

case "$OS" in
  Darwin) IS_MACOS=true ;;
  Linux)  IS_LINUX=true ;;
  *)      echo "Unsupported OS: $OS"; exit 1 ;;
esac

# Home directory — /root on Linux servers running as root, $HOME otherwise
if [[ "$IS_LINUX" == "true" ]] && [[ $EUID -eq 0 ]]; then
  HOME_DIR="/root"
else
  HOME_DIR="${HOME:-$( eval echo ~"$(whoami)" )}"
fi

# Sudo prefix — empty when root, "sudo" when non-root
SUDO=""
RUN_USER="$(whoami)"
if [[ $EUID -ne 0 ]]; then
  SUDO="sudo"
fi

# Portable sed in-place: GNU sed uses -i, BSD (macOS) sed uses -i ''
if [[ "$IS_MACOS" == "true" ]]; then
  sed_i() { sed -i '' "$@"; }
else
  sed_i() { sed -i "$@"; }
fi

# Portable lowercase: Bash 4+ has ${var,,}, Bash 3.2 (macOS default) does not
to_lower() {
  echo "$1" | tr '[:upper:]' '[:lower:]'
}

# State directory for idempotency
STATE_DIR="$HOME_DIR/.temps/.wizard-state"

# Terminal width
term_width() {
  tput cols 2>/dev/null || echo 80
}

# ---------------------------------------------------------------------------
# Drawing helpers
# ---------------------------------------------------------------------------

hr() {
  local w
  w=$(term_width)
  printf "${DIM}"
  printf '%.0s─' $(seq 1 "$w")
  printf "${RESET}\n"
}

banner() {
  clear
  echo ""
  hr
  printf "${BOLD}${CYAN}"
  cat << 'LOGO'

   ████████╗███████╗███╗   ███╗██████╗ ███████╗
   ╚══██╔══╝██╔════╝████╗ ████║██╔══██╗██╔════╝
      ██║   █████╗  ██╔████╔██║██████╔╝███████╗
      ██║   ██╔══╝  ██║╚██╔╝██║██╔═══╝ ╚════██║
      ██║   ███████╗██║ ╚═╝ ██║██║     ███████║
      ╚═╝   ╚══════╝╚═╝     ╚═╝╚═╝     ╚══════╝

LOGO
  printf "${RESET}"
  printf "   ${DIM}Self-hosted deployment platform${RESET}\n"
  printf "   ${DIM}https://temps.sh${RESET}\n"
  hr
  echo ""
}

step_header() {
  local step_num="$1" total="$2" title="$3"
  local w
  w=$(term_width)
  echo ""
  printf "  ${BG_BLUE}${BOLD}${WHITE} STEP %s/%s ${RESET}  ${BOLD}%s${RESET}\n" "$step_num" "$total" "$title"
  printf "  ${DIM}"
  printf '%.0s─' $(seq 1 $((w - 4)))
  printf "${RESET}\n"
  echo ""
}

info()    { printf "  ${BULLET} %b\n" "$*"; }
success() { printf "  ${CHECK} ${GREEN}%b${RESET}\n" "$*"; }
warn()    { printf "  ${YELLOW}! %b${RESET}\n" "$*"; }
error()   { printf "  ${CROSS} ${RED}%b${RESET}\n" "$*"; }
fatal()   { error "$@"; echo ""; exit 1; }

# port_in_use PORT
# Returns 0 (true) if something is already listening on TCP PORT on localhost.
# Tries lsof, then ss, then netstat — whichever exists. If none are available
# we cannot tell, so we return 1 (false) rather than block the install.
port_in_use() {
  local port="$1"
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 && return 0
    return 1
  fi
  if command -v ss >/dev/null 2>&1; then
    ss -ltn 2>/dev/null | grep -qE "[:.]${port}[[:space:]]" && return 0
    return 1
  fi
  if command -v netstat >/dev/null 2>&1; then
    netstat -an 2>/dev/null | grep -qE "[:.]${port}[[:space:]].*LISTEN" && return 0
    return 1
  fi
  return 1
}

# port_holder PORT
# Best-effort description of what is listening on PORT (for diagnostics only).
# Prints a human-readable line or nothing. Never fails the caller.
port_holder() {
  local port="$1"
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"$port" -sTCP:LISTEN 2>/dev/null | awk 'NR>1 {print "    "$1" (pid "$2")"}' | sort -u | head -3
  fi
}

# next_free_port START [MAX_TRIES]
# Prints the first free TCP port at or after START. Scans up to MAX_TRIES
# consecutive ports (default 20). Prints nothing and returns 1 if none are
# free in range — the caller decides how to handle that.
next_free_port() {
  local start="$1" tries="${2:-20}" p
  for ((p = start; p < start + tries; p++)); do
    if ! port_in_use "$p"; then
      printf '%s' "$p"
      return 0
    fi
  done
  return 1
}

# container_db_host_port NAME
# Prints the host port that container NAME publishes for postgres (5432/tcp),
# or nothing if it can't be determined. Used to recover DB_PORT on a re-run
# when the wizard state was lost but the container still exists.
container_db_host_port() {
  local name="$1"
  $DOCKER_SUDO docker inspect -f \
    '{{range $p, $conf := .NetworkSettings.Ports}}{{if eq $p "5432/tcp"}}{{(index $conf 0).HostPort}}{{end}}{{end}}' \
    "$name" 2>/dev/null | head -1
}

# prompt_input LABEL DEFAULT VARNAME
# Reads user input and writes it to the named global variable.
# All output goes to stderr (/dev/tty) so it's safe in subshells,
# and we use printf -v instead of eval for safety.
prompt_input() {
  local label="$1" default="${2:-}" var_name="$3"
  local _input
  if [[ -n "$default" ]]; then
    printf "  ${ARROW} ${BOLD}%s${RESET} ${DIM}[%s]${RESET}: " "$label" "$default" >&2
  else
    printf "  ${ARROW} ${BOLD}%s${RESET}: " "$label" >&2
  fi
  read -r _input < /dev/tty
  _input="${_input:-$default}"
  printf -v "$var_name" '%s' "$_input"
}

# prompt_secret LABEL VARNAME
# Reads input one character at a time, printing * for each keystroke.
prompt_secret() {
  local label="$1" var_name="$2"
  local _input="" _char
  printf "  ${ARROW} ${BOLD}%s${RESET}: " "$label" >&2
  while IFS= read -rs -n1 _char < /dev/tty; do
    # Enter (empty read) terminates input
    if [[ -z "$_char" ]]; then
      break
    fi
    # Backspace / Delete
    if [[ "$_char" == $'\x7f' || "$_char" == $'\b' ]]; then
      if [[ -n "$_input" ]]; then
        _input="${_input%?}"
        printf '\b \b' >&2
      fi
      continue
    fi
    _input+="$_char"
    printf '*' >&2
  done
  echo "" >&2
  printf -v "$var_name" '%s' "$_input"
}

prompt_yesno() {
  local label="$1" default="${2:-y}"
  local hint answer
  if [[ "$default" == "y" ]]; then hint="Y/n"; else hint="y/N"; fi
  printf "  ${ARROW} ${BOLD}%s${RESET} ${DIM}[%s]${RESET}: " "$label" "$hint"
  read -r answer < /dev/tty
  answer="${answer:-$default}"
  local lower
  lower=$(to_lower "$answer")
  [[ "$lower" == "y" || "$lower" == "yes" ]]
}

# prompt_choice VARNAME LABEL OPTION1 OPTION2 ...
# Writes the numeric choice (1, 2, ...) into the named global variable.
# Prints menu to stdout directly — never call this inside $().
prompt_choice() {
  local var_name="$1" label="$2"
  shift 2
  local options=("$@")
  echo ""
  printf "  ${BOLD}%s${RESET}\n" "$label"
  echo ""
  local i=1
  for opt in "${options[@]}"; do
    printf "    ${CYAN}%d)${RESET}  %s\n" "$i" "$opt"
    ((i++))
  done
  echo ""
  local _choice
  printf "  ${ARROW} ${BOLD}Enter choice${RESET} ${DIM}[1-%d]${RESET}: " "${#options[@]}"
  read -r _choice < /dev/tty
  printf -v "$var_name" '%s' "$_choice"
}

# Spinner — runs a command with an animated spinner
spinner() {
  local msg="$1"
  shift
  local pid i=0

  # Run command in background (stdin from /dev/null to prevent subprocesses
  # from consuming the script pipe when running under curl | bash)
  "$@" < /dev/null > /tmp/temps-wizard-cmd-out.log 2>&1 &
  pid=$!

  # Animate
  printf "  "
  while kill -0 "$pid" 2>/dev/null; do
    local char="${SPINNER_CHARS:$i:1}"
    printf "\r  ${CYAN}%s${RESET} %s" "$char" "$msg"
    i=$(( (i + 1) % ${#SPINNER_CHARS} ))
    sleep 0.1
  done

  # Check exit status
  wait "$pid"
  local exit_code=$?
  if [[ $exit_code -eq 0 ]]; then
    printf "\r  ${CHECK} %s\n" "$msg"
  else
    printf "\r  ${CROSS} %s ${RED}(failed)${RESET}\n" "$msg"
  fi
  return $exit_code
}

# Progress bar
progress_bar() {
  local current="$1" total="$2" label="${3:-}"
  local tw pct label_len bar_width filled empty bar_str

  tw=$(term_width)
  pct=$(( current * 100 / total ))

  # Fixed visible overhead: "  [" (3) + "]" (1) + " " (1) + "NNN%" (4) = 9 chars
  # Plus label: " " (1) + label text
  label_len=${#label}
  if [[ $label_len -gt 0 ]]; then
    bar_width=$(( tw - 10 - label_len ))
  else
    bar_width=$(( tw - 9 ))
  fi

  # If terminal too narrow for label, drop it
  if [[ $bar_width -lt 10 ]]; then
    label=""
    label_len=0
    bar_width=$(( tw - 9 ))
    [[ $bar_width -lt 10 ]] && bar_width=10
  fi

  filled=$(( current * bar_width / total ))
  empty=$(( bar_width - filled ))

  # Build the bar with ASCII characters (#, -)
  local fill_str="" empty_str=""
  [[ $filled -gt 0 ]] && fill_str=$(printf '%*s' "$filled" '' | tr ' ' '#')
  [[ $empty -gt 0 ]]  && empty_str=$(printf '%*s' "$empty" '' | tr ' ' '-')

  # \r\033[K = carriage return + erase to end of line (prevents stacking/overflow ghosts)
  printf "\r\033[K  %b[%b%s%b%s%b]%b %b%3d%%%b" \
    "$DIM" "$GREEN" "$fill_str" "$DIM" "$empty_str" "$RESET$DIM" "$RESET" "$BOLD" "$pct" "$RESET"
  [[ -n "$label" ]] && printf " %b%s%b" "$DIM" "$label" "$RESET"
}

summary_row() {
  local label="$1" value="$2"
  printf "  ${DIM}%-22s${RESET} ${BOLD}%s${RESET}\n" "$label" "$value"
}

# ---------------------------------------------------------------------------
# Utility
# ---------------------------------------------------------------------------

generate_password() {
  if check_command openssl; then
    openssl rand -base64 24 | tr -d '/+=' | head -c 32
  else
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' | head -c 32
  fi
}

# Simple alphanumeric password (hex chars only, easy to copy/paste)
generate_admin_password() {
  if check_command openssl; then
    openssl rand -hex 12
  else
    head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n' | head -c 24
  fi
}

require_root() {
  if [[ "$IS_MACOS" == "true" ]]; then
    # macOS: running as regular user is fine, sudo used when needed
    return 0
  fi
  if [[ $EUID -ne 0 ]]; then
    # Non-root on Linux: check that sudo is available
    if ! command -v sudo &>/dev/null; then
      fatal "This script must be run as root or with sudo available. Try: ${BOLD}sudo bash deploy.sh${RESET}"
    fi
    # Validate sudo access — use "sudo true" instead of "sudo -v" because
    # "sudo -v" prompts for a password even with NOPASSWD rules (it updates
    # the credential timestamp, not a command, so NOPASSWD doesn't apply).
    info "Running as ${BOLD}$RUN_USER${RESET} — sudo will be used for privileged operations."
    if ! sudo true 2>/dev/null; then
      fatal "Could not acquire sudo privileges. Try: ${BOLD}sudo bash deploy.sh${RESET}"
    fi
  fi
}

check_command() {
  command -v "$1" &>/dev/null
}

# Persist a key=value pair to wizard state (for idempotency across re-runs)
state_set() {
  mkdir -p "$STATE_DIR"
  printf '%s' "$2" > "$STATE_DIR/$1"
  chmod 600 "$STATE_DIR/$1"
}

# Read a persisted value (returns empty string if not found)
state_get() {
  local file="$STATE_DIR/$1"
  if [[ -f "$file" ]]; then
    cat "$file"
  fi
}

# Portable way to extract JSON string value (no jq dependency)
# Usage: json_value '{"key":"val"}' "key"
json_value() {
  local json="$1" key="$2"
  echo "$json" | sed -n 's/.*"'"$key"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1
}

# Ensure the acme.sh Let's Encrypt account is registered with the given email.
# acme.sh only stores the email at install time, so a stale/bad ACCOUNT_EMAIL
# in ~/.acme.sh/account.conf survives across retries. Always (re-)register the
# account with the current email and, if an account already exists, update it.
# Usage: acme_ensure_account "user@example.com"
acme_ensure_account() {
  local email="$1"
  local acme_bin="$HOME_DIR/.acme.sh/acme.sh"
  local account_conf="$HOME_DIR/.acme.sh/ca/acme-v02.api.letsencrypt.org/directory/account.conf"

  [[ -z "$email" ]] && return 0
  [[ -x "$acme_bin" ]] || return 0

  # Register the account with the current email. This is idempotent: if no
  # account exists yet it creates one; if one exists with the same email it is
  # a no-op. Let's Encrypt rejects an invalid email here, surfacing the real
  # error early instead of hiding it behind a token-extraction failure later.
  if ! "$acme_bin" --register-account -m "$email" --server letsencrypt 2>&1 \
       | while IFS= read -r line; do printf "  ${DIM}  %s${RESET}\n" "$line"; done; then
    return 1
  fi

  # If account.conf still carries a different ACCOUNT_EMAIL (set on a previous
  # run with a bad address), push the corrected email to the existing account.
  if [[ -f "$account_conf" ]] && ! grep -q "ACCOUNT_EMAIL='${email}'" "$account_conf"; then
    "$acme_bin" --update-account -m "$email" --server letsencrypt > /dev/null 2>&1 || true
  fi

  return 0
}

TOTAL_STEPS=5

# Setup mode — chosen on the first screen (or via --mode flag):
#   local    — run on this machine via 127.0.0.1.sslip.io, HTTP only (dogfooding)
#   quick    — sslip.io domain from the public IP, HTTP only (~90s)
#   testing  — sslip.io domain + a real Let's Encrypt cert (HTTP-01) for the
#              console host; apps stay on HTTP. No real domain needed.
#   advanced — your own domain + wildcard Let's Encrypt cert via manual DNS
SETUP_MODE=""

# Release channel for the Temps binary: "stable" (default) or "beta".
# CLI-flag-only by design (no env-var fallback), matching `temps upgrade
# --channel` and install.sh:
#   stable -> GitHub /releases/latest (newest non-prerelease)
#   beta   -> newest tag from /releases?per_page=20 (stable OR prerelease)
CHANNEL="stable"

# Anonymous product telemetry. Temps reports anonymous usage events (e.g.
# "an instance attempted a deploy" vs "an instance deployed successfully") so
# the maintainers can tell whether the product is working for self-hosters.
# It is anonymous by design — no PII, repo names, domains, or secrets — and
# enabled by default (opt-in). Operators can opt out with --no-telemetry, which
# sets TEMPS_TELEMETRY=0 in the service so the binary never reports anything.
TELEMETRY_OPTOUT="false"

# sslip.io wildcard domain for local/quick/testing modes, e.g. "1.2.3.4.sslip.io"
# (or "127.0.0.1.sslip.io" in local mode). The console is reachable at
# console.<SSLIP_DOMAIN>; apps at <app>.<SSLIP_DOMAIN>.
SSLIP_DOMAIN=""
SERVER_IP=""

# HTTP port the proxy binds in local/quick/testing modes. Local mode tries 80
# and falls back to 8080 when binding the privileged port fails (e.g. macOS,
# non-root); when the port is not 80 it is shown in console/app URLs.
LOCAL_PORT=80

# resolve_channel_version
# Prints the tag name for the newest release on the selected $CHANNEL.
#   stable -> /releases/latest (GitHub returns the newest non-prerelease)
#   beta   -> /releases?per_page=20, first tag (newest of stable + prerelease)
# Mirrors `temps upgrade`: beta tracks the freshest version regardless of kind.
resolve_channel_version() {
  local ver=""
  if [[ "$CHANNEL" == "beta" ]]; then
    ver=$(curl -fsSL "https://api.github.com/repos/gotempsh/temps/releases?per_page=20" 2>/dev/null \
      | grep '"tag_name":' | head -1 | cut -d'"' -f4)
  else
    ver=$(curl -fsSL "https://api.github.com/repos/gotempsh/temps/releases/latest" 2>/dev/null \
      | grep '"tag_name":' | head -1 | cut -d'"' -f4)
  fi
  echo "$ver"
}

# Docker may need sudo for non-root users not in the docker group.
# DOCKER_SUDO is set after Docker is confirmed working in step_docker().
DOCKER_SUDO=""

# install_linux_docker
# Prefer native Amazon Linux packages because Docker's convenience installer
# does not support the `amzn` distribution id.
install_linux_docker() {
  local os_id=""

  if [[ -r /etc/os-release ]]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    os_id="${ID:-}"
  fi

  if [[ "$os_id" == "amzn" ]]; then
    if check_command dnf; then
      info "Installing Docker via ${BOLD}dnf${RESET}..."
      spinner "Installing Docker packages" $SUDO dnf install -y docker
      return $?
    fi
    if check_command yum; then
      info "Installing Docker via ${BOLD}yum${RESET}..."
      spinner "Installing Docker packages" $SUDO yum install -y docker
      return $?
    fi
  fi

  info "Installing Docker via ${UNDERLINE}https://get.docker.com${RESET}..."
  spinner "Downloading and installing Docker" bash -c "curl -fsSL https://get.docker.com | $SUDO sh"
}

# ---------------------------------------------------------------------------
# Step 1: Docker
# ---------------------------------------------------------------------------

step_docker() {
  step_header 1 $TOTAL_STEPS "Docker Engine"

  # Helper: check if docker daemon is reachable (directly or via sudo)
  docker_is_ready() {
    if docker info &>/dev/null; then
      DOCKER_SUDO=""
      return 0
    elif [[ -n "$SUDO" ]] && $SUDO docker info &>/dev/null; then
      DOCKER_SUDO="$SUDO"
      return 0
    fi
    return 1
  }

  if check_command docker && docker_is_ready; then
    local docker_version
    docker_version=$($DOCKER_SUDO docker --version 2>/dev/null | head -1)
    success "Docker is already installed"
    info "${DIM}$docker_version${RESET}"
    if [[ -n "$DOCKER_SUDO" ]]; then
      info "Using ${BOLD}sudo${RESET} for docker commands"
    fi
    return 0
  fi

  if check_command docker && ! docker_is_ready; then
    warn "Docker is installed but the daemon is not running"
    info "Attempting to start Docker..."
    if [[ "$IS_LINUX" == "true" ]]; then
      $SUDO systemctl start docker 2>/dev/null || true
    elif [[ "$IS_MACOS" == "true" ]]; then
      open -a Docker 2>/dev/null || true
      info "Waiting for Docker Desktop to start..."
      for _i in $(seq 1 30); do
        docker_is_ready && break
        sleep 2
      done
    fi
    sleep 2
    if docker_is_ready; then
      success "Docker daemon started"
      if [[ -n "$DOCKER_SUDO" ]]; then
        info "Using ${BOLD}sudo${RESET} for docker commands"
      fi
      return 0
    fi
  fi

  warn "Docker is not installed"
  echo ""

  if [[ "$IS_MACOS" == "true" ]]; then
    if check_command brew; then
      if ! prompt_yesno "Install Docker via Homebrew?" "y"; then
        fatal "Docker is required. Install Docker Desktop from ${UNDERLINE}https://docker.com/products/docker-desktop${RESET} and re-run."
      fi
      echo ""
      if spinner "Installing Docker via Homebrew" brew install --cask docker; then
        info "Starting Docker Desktop..."
        open -a Docker 2>/dev/null || true
        # Wait for Docker to become ready
        local docker_ready=false
        for _i in $(seq 1 30); do
          if docker_is_ready; then docker_ready=true; break; fi
          sleep 2
        done
        if [[ "$docker_ready" == "true" ]]; then
          success "Docker Desktop installed and running"
        else
          warn "Docker Desktop installed but not yet ready."
          info "Open Docker Desktop manually and re-run the wizard."
          fatal "Docker daemon not responding."
        fi
      else
        fatal "Docker installation failed. Install Docker Desktop manually from ${UNDERLINE}https://docker.com/products/docker-desktop${RESET}"
      fi
    else
      fatal "Docker is required. Install Docker Desktop from ${UNDERLINE}https://docker.com/products/docker-desktop${RESET} and re-run."
    fi
  else
    if ! prompt_yesno "Install Docker now?" "y"; then
      fatal "Docker is required to continue. Install it manually and re-run the wizard."
    fi

    echo ""
    info "Installing Docker..."
    echo ""

    if install_linux_docker; then
      # Add current user to docker group so subsequent runs don't need sudo for docker
      if [[ -n "$SUDO" ]] && [[ "$IS_LINUX" == "true" ]]; then
        $SUDO usermod -aG docker "$RUN_USER" 2>/dev/null || true
        info "Added ${BOLD}$RUN_USER${RESET} to docker group (takes effect on next login)"
      fi

      # Enable and start Docker
      $SUDO systemctl enable docker &>/dev/null || true
      $SUDO systemctl start docker &>/dev/null || true
      sleep 2

      if docker_is_ready; then
        local docker_version
        docker_version=$($DOCKER_SUDO docker --version 2>/dev/null | head -1)
        success "Docker installed and running"
        info "${DIM}$docker_version${RESET}"
        if [[ -n "$DOCKER_SUDO" ]]; then
          info "Using ${BOLD}sudo${RESET} for docker commands"
        fi
      else
        fatal "Docker installed but failed to start. Check: ${BOLD}${SUDO:+sudo }systemctl status docker${RESET}"
      fi
    else
      fatal "Docker installation failed. Check /tmp/temps-wizard-cmd-out.log for details."
    fi
  fi
}

# ---------------------------------------------------------------------------
# Step 2: TimescaleDB
# ---------------------------------------------------------------------------

DB_PASSWORD=""
# Host port the TimescaleDB container publishes on. Defaults to 5432, but if
# 5432 is already taken by another process (e.g. a host PostgreSQL or another
# Temps install) we fall back to the next free port so the install still
# succeeds. Recovered from wizard state on re-run; threaded into every
# connection string and `temps setup`.
DB_PORT="5432"

# reconcile_db_password
# Ensure the `temps` role's password inside the container matches $DB_PASSWORD.
# Needed because Postgres ignores POSTGRES_PASSWORD when the data volume already
# exists, so a reused volume keeps its OLD password. Strategy:
#   1. If $DB_PASSWORD already authenticates over TCP, do nothing.
#   2. Otherwise ALTER the role over the container's LOCAL socket (local conns
#      are trusted in these images, so no password is required there) and
#      re-verify over TCP.
# Returns 0 on success, 1 if the password still doesn't authenticate.
reconcile_db_password() {
  # 1. Does the current password already work? (TCP, so it exercises md5/scram.)
  if $DOCKER_SUDO docker exec -e PGPASSWORD="$DB_PASSWORD" temps-timescaledb \
       psql -h 127.0.0.1 -U temps -d temps -tAc 'SELECT 1' &>/dev/null; then
    return 0
  fi

  # 2. Reset it over the trusted local socket. Escape single quotes for the SQL
  #    string literal by doubling them (standard SQL escaping). The
  #    timescaledb-ha image restarts Postgres once during first init, so a
  #    command can transiently hit "system is shutting down / starting up" or a
  #    refused socket — retry across that window before giving up.
  local escaped="${DB_PASSWORD//\'/\'\'}"
  local attempt
  for attempt in $(seq 1 30); do
    if $DOCKER_SUDO docker exec temps-timescaledb \
         psql -U temps -d temps -v ON_ERROR_STOP=1 \
         -c "ALTER USER temps WITH PASSWORD '${escaped}';" &>/dev/null; then
      # 3. Verify the new password now authenticates over TCP.
      if $DOCKER_SUDO docker exec -e PGPASSWORD="$DB_PASSWORD" temps-timescaledb \
           psql -h 127.0.0.1 -U temps -d temps -tAc 'SELECT 1' &>/dev/null; then
        return 0
      fi
    fi
    sleep 1
  done
  return 1
}

# recover_db_port
# Resolve the DB host port for an EXISTING container: prefer the value saved in
# wizard state, fall back to the container's actual published port, then 5432.
# Persists the result so later steps and re-runs stay consistent.
recover_db_port() {
  DB_PORT="$(state_get db_port)"
  if [[ -z "$DB_PORT" ]]; then
    DB_PORT="$(container_db_host_port temps-timescaledb)"
  fi
  [[ -z "$DB_PORT" ]] && DB_PORT="5432"
  state_set db_port "$DB_PORT"
  [[ "$DB_PORT" != "5432" ]] && info "Database port: ${BOLD}${DB_PORT}${RESET}"
}

step_timescaledb() {
  step_header 2 $TOTAL_STEPS "TimescaleDB Database"

  # Check if container already exists and is running
  if $DOCKER_SUDO docker ps --format '{{.Names}}' 2>/dev/null | grep -q '^temps-timescaledb$'; then
    if $DOCKER_SUDO docker exec temps-timescaledb pg_isready -U temps &>/dev/null; then
      success "TimescaleDB is already running"
      info "Container: ${BOLD}temps-timescaledb${RESET}"

      # Recover the published host port: wizard state first, then the running
      # container's actual port mapping, then default to 5432.
      recover_db_port

      # Recover password from wizard state first, then from docker inspect
      DB_PASSWORD=$(state_get db_password)
      if [[ -z "$DB_PASSWORD" ]]; then
        # Try docker inspect: look for POSTGRES_PASSWORD env var
        DB_PASSWORD=$($DOCKER_SUDO docker inspect temps-timescaledb 2>/dev/null \
          | sed -n 's/.*"POSTGRES_PASSWORD=\([^"]*\)".*/\1/p' | head -1 || true)
      fi

      if [[ -z "$DB_PASSWORD" ]]; then
        warn "Could not recover existing database password — generating a new one"
        DB_PASSWORD=$(generate_password)
      fi
      # The recovered/generated password may not match what's actually in the
      # data volume; force it to match so `temps setup` can authenticate.
      if ! reconcile_db_password; then
        fatal "Could not reconcile the database password on the running container"
      fi
      state_set db_password "$DB_PASSWORD"
      return 0
    fi
  fi

  # Check if container exists but is stopped
  if $DOCKER_SUDO docker ps -a --format '{{.Names}}' 2>/dev/null | grep -q '^temps-timescaledb$'; then
    warn "TimescaleDB container exists but is stopped"
    info "Starting existing container..."
    $DOCKER_SUDO docker start temps-timescaledb &>/dev/null

    sleep 3
    if $DOCKER_SUDO docker exec temps-timescaledb pg_isready -U temps &>/dev/null; then
      success "TimescaleDB container restarted"
      recover_db_port
      DB_PASSWORD=$(state_get db_password)
      if [[ -z "$DB_PASSWORD" ]]; then
        DB_PASSWORD=$($DOCKER_SUDO docker inspect temps-timescaledb 2>/dev/null \
          | sed -n 's/.*"POSTGRES_PASSWORD=\([^"]*\)".*/\1/p' | head -1 || true)
      fi
      if [[ -z "$DB_PASSWORD" ]]; then
        warn "Could not recover existing database password — generating a new one"
        DB_PASSWORD=$(generate_password)
      fi
      if ! reconcile_db_password; then
        fatal "Could not reconcile the database password on the restarted container"
      fi
      state_set db_password "$DB_PASSWORD"
      return 0
    fi
  fi

  # Fresh install. Reuse a port chosen on a previous run if we have one.
  DB_PORT="$(state_get db_port)"
  [[ -z "$DB_PORT" ]] && DB_PORT="5432"

  # 5432 being busy is NOT fatal — it may be a host PostgreSQL or another Temps
  # instance, which we must not disturb. Bind our container to the next free
  # port instead, and thread that port through every connection string below.
  if port_in_use "$DB_PORT"; then
    local holder
    holder="$(port_holder "$DB_PORT")"
    warn "Port ${BOLD}${DB_PORT}${RESET} is already in use — likely another PostgreSQL or Temps instance."
    if [[ -n "$holder" ]]; then
      info "Currently listening on ${DB_PORT}:"
      printf "%b\n" "$holder"
    fi
    local fallback
    if ! fallback="$(next_free_port 5433)"; then
      error "Could not find a free port in the range 5433–5452."
      info "Free a port (e.g. stop the conflicting service) and re-run the installer."
      fatal "Cannot continue without a free host port for the database"
    fi
    DB_PORT="$fallback"
    info "Using ${BOLD}${DB_PORT}${RESET} for the Temps database instead."
  fi
  state_set db_port "$DB_PORT"

  info "Pulling ${BOLD}timescale/timescaledb-ha:pg18${RESET}..."
  echo ""

  DB_PASSWORD=$(generate_password)
  state_set db_password "$DB_PASSWORD"

  spinner "Pulling TimescaleDB image" $DOCKER_SUDO docker pull timescale/timescaledb-ha:pg18 || \
    fatal "Failed to pull TimescaleDB image"

  echo ""
  info "Starting TimescaleDB container..."
  echo ""

  # Capture stderr so a failure (e.g. port bind, missing image) surfaces the
  # real reason instead of silently falling through into the readiness loop.
  local run_err
  if ! run_err=$($DOCKER_SUDO docker run -d \
    --name temps-timescaledb \
    --restart unless-stopped \
    --shm-size=2g \
    -e POSTGRES_USER=temps \
    -e POSTGRES_PASSWORD="$DB_PASSWORD" \
    -e POSTGRES_DB=temps \
    -p 127.0.0.1:"$DB_PORT":5432 \
    -v temps-db-data:/home/postgres/pgdata/data \
    timescale/timescaledb-ha:pg18 2>&1); then
    echo ""
    error "Failed to start the TimescaleDB container."
    info "Docker reported:"
    printf "    ${DIM}%b${RESET}\n" "$run_err"
    if printf '%s' "$run_err" | grep -qiE "address already in use|port is already allocated|bind"; then
      info "Port ${BOLD}${DB_PORT}${RESET} could not be bound — another process took it"
      info "between our check and container start. Re-run the installer to pick"
      info "a different port."
    fi
    fatal "Cannot continue without a working database"
  fi

  # Wait for database readiness
  local ready=false
  for i in $(seq 1 30); do
    progress_bar "$i" 30 "Waiting for database..."
    if $DOCKER_SUDO docker exec temps-timescaledb pg_isready -U temps &>/dev/null; then
      ready=true
      break
    fi
    sleep 1
  done
  progress_bar 30 30 "Waiting for database..."
  echo ""

  if [[ "$ready" == "true" ]]; then
    # Postgres only applies POSTGRES_PASSWORD when it initializes an EMPTY data
    # dir. If the temps-db-data volume already existed (e.g. a prior install),
    # the container keeps the OLD password and silently ignores the new env var
    # — so the freshly generated DB_PASSWORD we hand to `temps setup` would fail
    # with "password authentication failed". Force the role password now over
    # the container's local socket (trusted) so the DB always matches our state.
    if ! reconcile_db_password; then
      echo ""
      error "Could not set the database password on the temps role."
      info "The data volume ${BOLD}temps-db-data${RESET} may hold a database whose"
      info "password we cannot reset automatically. Inspect with:"
      info "  ${BOLD}docker exec -it temps-timescaledb psql -U temps -d temps${RESET}"
      info "or remove the stale volume to start clean (DESTROYS existing data):"
      info "  ${BOLD}docker rm -f temps-timescaledb && docker volume rm temps-db-data${RESET}"
      fatal "Cannot continue with a mismatched database password"
    fi

    success "TimescaleDB is ready"
    info "Container: ${BOLD}temps-timescaledb${RESET}"
    info "Port:      ${BOLD}127.0.0.1:${DB_PORT}${RESET}"
    info "User:      ${BOLD}temps${RESET}"
    info "Database:  ${BOLD}temps${RESET}"
    info "Password:  ${DIM}(auto-generated, stored securely)${RESET}"
  else
    echo ""
    warn "TimescaleDB failed to become ready within 30 seconds."
    info "Troubleshooting steps:"
    info "  1. Check Docker is running:  ${BOLD}docker ps${RESET}"
    info "  2. Check container logs:     ${BOLD}docker logs temps-timescaledb${RESET}"
    info "  3. Check available disk:     ${BOLD}df -h${RESET}"
    info "  4. Restart and retry:        ${BOLD}docker restart temps-timescaledb${RESET}"
    fatal "Cannot continue without a working database"
  fi
}

# ---------------------------------------------------------------------------
# Step 3: Domain & SSL Certificate
# ---------------------------------------------------------------------------

DOMAIN=""
WILDCARD_DOMAIN=""
CERT_DIR="$HOME_DIR/.temps/certs"
FULLCHAIN_PATH=""
KEY_PATH=""

step_domain_ssl() {
  step_header 3 $TOTAL_STEPS "Domain & SSL Certificate"

  # Idempotency: check if valid certs already exist
  FULLCHAIN_PATH="$CERT_DIR/fullchain.pem"
  KEY_PATH="$CERT_DIR/key.pem"
  DOMAIN=$(state_get domain)

  if [[ -n "$DOMAIN" ]] \
    && [[ -f "$FULLCHAIN_PATH" ]] \
    && [[ -f "$KEY_PATH" ]] \
    && openssl x509 -in "$FULLCHAIN_PATH" -noout -checkend 86400 &>/dev/null; then
    WILDCARD_DOMAIN="*.$DOMAIN"
    success "SSL certificate already provisioned and valid"
    info "Domain:    ${BOLD}$DOMAIN${RESET}"
    info "Wildcard:  ${BOLD}$WILDCARD_DOMAIN${RESET}"

    local cert_exp
    cert_exp=$(openssl x509 -in "$FULLCHAIN_PATH" -noout -enddate 2>/dev/null | sed 's/.*=//' || true)
    info "Expires:   ${BOLD}$cert_exp${RESET}"
    echo ""

    if ! prompt_yesno "Re-provision certificate?" "n"; then
      return 0
    fi
  fi

  # Reset for fresh provisioning
  DOMAIN=""
  WILDCARD_DOMAIN=""

  # Self-hosted setup uses your own domain with a wildcard Let's Encrypt
  # certificate, validated by adding DNS TXT records yourself.
  domain_manual

  # Persist domain for idempotency
  state_set domain "$DOMAIN"
}

domain_manual() {
  echo ""
  info "We'll use ${BOLD}acme.sh${RESET} with manual DNS-01 validation to provision"
  info "a wildcard Let's Encrypt certificate for your domain."
  echo ""
  info "${DIM}You'll need:${RESET}"
  info "  1. A domain you own"
  info "  2. Access to your DNS provider to add TXT records"
  echo ""

  prompt_input "Your domain (e.g. example.com)" "" DOMAIN

  if [[ -z "$DOMAIN" ]]; then
    fatal "Domain is required"
  fi

  WILDCARD_DOMAIN="*.$DOMAIN"

  local admin_email=""
  prompt_input "Email for Let's Encrypt notifications" "" admin_email

  if [[ -z "$admin_email" ]]; then
    fatal "Email is required for Let's Encrypt"
  fi

  echo ""
  info "Provisioning wildcard certificate for ${BOLD}$DOMAIN${RESET}..."
  echo ""

  # Install acme.sh if not present
  if [[ ! -f $HOME_DIR/.acme.sh/acme.sh ]]; then
    spinner "Installing acme.sh" bash -c \
      "curl -fsSL https://get.acme.sh | sh -s email='$admin_email'" || \
      fatal "Failed to install acme.sh"
  else
    success "acme.sh already installed"
  fi

  # Set default CA
  $HOME_DIR/.acme.sh/acme.sh --set-default-ca --server letsencrypt > /dev/null 2>&1 || true

  # Register/refresh the Let's Encrypt account with the email entered above.
  # acme.sh keeps the email from its first install, so without this an invalid
  # address from an earlier attempt would persist and block issuance.
  info "Registering ACME account (${BOLD}$admin_email${RESET})..."
  acme_ensure_account "$admin_email" || \
    fatal "ACME account registration failed. Verify the email address is valid."

  mkdir -p "$CERT_DIR"

  # --- Retry loop: create ACME order, show TXT records, validate ---
  local attempt=0
  local max_attempts=5

  while true; do
    attempt=$((attempt + 1))

    if [[ $attempt -gt $max_attempts ]]; then
      fatal "Maximum attempts ($max_attempts) reached. Please verify your DNS setup and try again."
    fi

    if [[ $attempt -gt 1 ]]; then
      echo ""
      warn "Attempt $attempt of $max_attempts"
      echo ""
    fi

    # Step 1: Request ACME challenge tokens (manual DNS mode)
    info "Requesting ACME challenge tokens..."

    local acme_output
    acme_output=$($HOME_DIR/.acme.sh/acme.sh --issue \
      -d "$DOMAIN" \
      -d "$WILDCARD_DOMAIN" \
      --dns \
      --yes-I-know-dns-manual-mode-enough-go-ahead-please \
      --force 2>&1 || true)

    # Extract TXT record name and values from acme.sh output
    local txt_name="_acme-challenge.$DOMAIN"
    local tokens=()
    while IFS= read -r line; do
      local tkn
      tkn=$(echo "$line" | sed -n "s/.*TXT value: *'\([^']*\)'.*/\1/p")
      if [[ -n "$tkn" ]]; then
        tokens+=("$tkn")
      fi
    done <<< "$acme_output"

    if [[ ${#tokens[@]} -eq 0 ]]; then
      error "Failed to extract ACME challenge tokens from acme.sh output"
      echo ""
      info "${DIM}acme.sh output:${RESET}"
      while IFS= read -r line; do
        printf "  ${DIM}  %s${RESET}\n" "$line"
      done <<< "$acme_output"
      echo ""
      if prompt_yesno "Retry with a new order?" "y"; then
        continue
      else
        fatal "Cannot proceed without ACME challenge tokens."
      fi
    fi

    success "Got ${#tokens[@]} ACME challenge token(s)"
    echo ""

    # Step 2: Display the TXT records the user needs to add
    hr
    printf "\n"
    printf "  ${BOLD}${YELLOW}ACTION REQUIRED:${RESET} ${BOLD}Add the following DNS TXT record(s)${RESET}\n"
    printf "\n"
    printf "  Go to your DNS provider and create these TXT records:\n"
    printf "\n"

    local token_idx=0
    for tkn in "${tokens[@]}"; do
      token_idx=$((token_idx + 1))
      if [[ ${#tokens[@]} -gt 1 ]]; then
        printf "  ${CYAN}Record %d:${RESET}\n" "$token_idx"
      fi
      printf "    ${DIM}Name:${RESET}   ${BOLD}%s${RESET}\n" "$txt_name"
      printf "    ${DIM}Type:${RESET}   ${BOLD}TXT${RESET}\n"
      printf "    ${DIM}Value:${RESET}  ${BOLD}%s${RESET}\n" "$tkn"
      printf "\n"
    done

    if [[ ${#tokens[@]} -gt 1 ]]; then
      info "${DIM}Both records use the same name — add them as separate TXT entries.${RESET}"
      echo ""
    fi

    info "${DIM}Tip: Set TTL to the lowest value your provider allows (e.g. 60s or 1 min).${RESET}"
    printf "\n"
    hr
    echo ""

    # Step 3: Wait for user confirmation
    printf "  ${ARROW} ${BOLD}Press Enter after you've added the TXT record(s)${RESET}..."
    read -r < /dev/tty
    echo ""

    # Step 4: Verify DNS propagation
    info "Checking DNS propagation (this may take up to 2 minutes)..."
    echo ""

    local propagated=false
    for i in $(seq 1 24); do
      progress_bar "$i" 24 "Checking DNS..."

      # Query Google DoH for TXT records
      local dns_answer
      dns_answer=$(curl -sf "https://dns.google/resolve?name=${txt_name}&type=TXT" 2>/dev/null || true)

      if [[ -n "$dns_answer" ]]; then
        local all_found=true
        for tkn in "${tokens[@]}"; do
          if ! echo "$dns_answer" | grep -q "$tkn"; then
            all_found=false
            break
          fi
        done
        if [[ "$all_found" == "true" ]]; then
          propagated=true
          break
        fi
      fi
      sleep 5
    done
    progress_bar 24 24 "Checking DNS..."
    echo ""
    echo ""

    if [[ "$propagated" == "true" ]]; then
      success "DNS records verified"
    else
      warn "DNS records not yet visible via Google DNS."
      info "This doesn't necessarily mean they're wrong — propagation can be slow."
      echo ""
      local dns_action=""
      prompt_choice dns_action "What would you like to do?" \
        "Continue anyway  — attempt ACME validation now" \
        "Wait & recheck   — give DNS more time to propagate" \
        "Start over       — create a new ACME order with fresh tokens" \
        "Abort            — exit the wizard"

      case "$dns_action" in
        1) info "Proceeding with ACME validation..." ;;
        2)
          echo ""
          info "Waiting an additional 60 seconds..."
          for i in $(seq 1 12); do
            progress_bar "$i" 12 "Waiting..."
            sleep 5
          done
          progress_bar 12 12 "Waiting..."
          echo ""
          echo ""
          info "Proceeding with ACME validation..."
          ;;
        3) continue ;;
        4) fatal "Aborted by user." ;;
        *) fatal "Invalid choice" ;;
      esac
      echo ""
    fi

    # Step 5: Complete the ACME challenge (renew to validate)
    info "Completing ACME validation..."
    echo ""

    local renew_output renew_exit=0
    renew_output=$($HOME_DIR/.acme.sh/acme.sh --renew \
      -d "$DOMAIN" \
      -d "$WILDCARD_DOMAIN" \
      --yes-I-know-dns-manual-mode-enough-go-ahead-please \
      2>&1) || renew_exit=$?

    # Show acme.sh output
    while IFS= read -r line; do
      printf "  ${DIM}  %s${RESET}\n" "$line"
    done <<< "$renew_output"

    # Check if cert was actually issued
    local acme_cert_dir="$HOME_DIR/.acme.sh/${DOMAIN}_ecc"
    if [[ ! -f "$acme_cert_dir/fullchain.cer" ]]; then
      acme_cert_dir="$HOME_DIR/.acme.sh/${DOMAIN}"
    fi

    if [[ -f "$acme_cert_dir/fullchain.cer" ]]; then
      # Success!
      echo ""
      success "Certificate issued by Let's Encrypt"
      break
    fi

    # Validation failed
    echo ""
    error "ACME validation failed."
    info "This usually means the TXT records were not found by Let's Encrypt."
    echo ""
    info "${DIM}Common causes:${RESET}"
    info "  1. DNS propagation hasn't completed yet"
    info "  2. TXT record values were entered incorrectly"
    info "  3. Wrong DNS zone (e.g. adding to a subdomain's zone instead of root)"
    echo ""

    local retry_action=""
    prompt_choice retry_action "What would you like to do?" \
      "Retry with new tokens  — create a fresh ACME order (recommended)" \
      "Abort                  — exit the wizard"

    case "$retry_action" in
      1)
        info "Creating a new ACME order..."
        info "${DIM}Please remove the old TXT records before adding new ones.${RESET}"
        echo ""
        printf "  ${ARROW} ${BOLD}Press Enter when you've removed the old TXT records${RESET}..."
        read -r < /dev/tty
        continue
        ;;
      2) fatal "Aborted by user." ;;
      *) fatal "Invalid choice" ;;
    esac
  done

  # Step 6: Install certificate
  echo ""
  $HOME_DIR/.acme.sh/acme.sh --install-cert \
    -d "$DOMAIN" \
    -d "$WILDCARD_DOMAIN" \
    --ecc \
    --cert-file "$CERT_DIR/cert.pem" \
    --key-file "$CERT_DIR/key.pem" \
    --fullchain-file "$CERT_DIR/fullchain.pem" \
    > /dev/null 2>&1 || {
      # Fallback: copy directly
      local acme_cert_dir="$HOME_DIR/.acme.sh/${DOMAIN}_ecc"
      if [[ ! -d "$acme_cert_dir" ]]; then
        acme_cert_dir="$HOME_DIR/.acme.sh/${DOMAIN}"
      fi
      cp "$acme_cert_dir/fullchain.cer" "$CERT_DIR/fullchain.pem"
      cp "$acme_cert_dir/${DOMAIN}.key" "$CERT_DIR/key.pem" 2>/dev/null || \
        cp "$acme_cert_dir"/*.key "$CERT_DIR/key.pem"
    }

  chmod 600 "$CERT_DIR"/*.pem

  FULLCHAIN_PATH="$CERT_DIR/fullchain.pem"
  KEY_PATH="$CERT_DIR/key.pem"

  success "Certificate installed to ${BOLD}$CERT_DIR${RESET}"

  # Verify cert
  local cert_cn cert_exp
  cert_cn=$(openssl x509 -in "$FULLCHAIN_PATH" -noout -subject 2>/dev/null | sed 's/.*CN = //' || true)
  cert_exp=$(openssl x509 -in "$FULLCHAIN_PATH" -noout -enddate 2>/dev/null | sed 's/.*=//' || true)

  if [[ -n "$cert_cn" ]]; then
    info "Subject:  ${BOLD}$cert_cn${RESET}"
    info "Expires:  ${BOLD}$cert_exp${RESET}"
  fi

  # Detect public IP and remind user about A records
  echo ""
  info "Detecting server public IP..."
  local server_ip=""
  server_ip=$(curl -4 -sf https://ifconfig.me 2>/dev/null || curl -4 -sf https://api.ipify.org 2>/dev/null || true)

  if [[ -n "$server_ip" ]]; then
    success "Public IP: ${BOLD}$server_ip${RESET}"
  fi

  echo ""
  hr
  printf "\n"
  printf "  ${BOLD}${YELLOW}IMPORTANT:${RESET} ${BOLD}Make sure your DNS A records are configured${RESET}\n"
  printf "\n"
  printf "  Your domain must point to this server for Temps to work.\n"
  printf "  Add these A records at your DNS provider (if not already done):\n"
  printf "\n"
  printf "    ${DIM}Name:${RESET}   ${BOLD}%s${RESET}      ${DIM}Type:${RESET} ${BOLD}A${RESET}   ${DIM}Value:${RESET}  ${BOLD}%s${RESET}\n" "$DOMAIN" "${server_ip:-<your-server-ip>}"
  printf "    ${DIM}Name:${RESET}   ${BOLD}*.%s${RESET}    ${DIM}Type:${RESET} ${BOLD}A${RESET}   ${DIM}Value:${RESET}  ${BOLD}%s${RESET}\n" "$DOMAIN" "${server_ip:-<your-server-ip>}"
  printf "\n"
  warn "If using Cloudflare, the wildcard (*.${DOMAIN}) record ${BOLD}must${RESET}${YELLOW} have proxy status OFF${RESET}"
  warn "${YELLOW}(DNS only / grey cloud). Cloudflare does not proxy wildcard records.${RESET}"
  printf "\n"
  info "${DIM}You can remove the _acme-challenge TXT records now — they're no longer needed.${RESET}"
  printf "\n"
  hr
  echo ""
  printf "  ${ARROW} ${BOLD}Press Enter to continue${RESET}..."
  read -r < /dev/tty
}

# ---------------------------------------------------------------------------
# Step 4: Temps Setup
# ---------------------------------------------------------------------------

ADMIN_EMAIL=""
ADMIN_PASSWORD=""

# ---------------------------------------------------------------------------
# Binary installer: download temps from GitHub releases (stable only)
# Usage: install_temps_binary [VERSION]
# ---------------------------------------------------------------------------
install_temps_binary() {
  local version="${1:-}"
  local platform target bin_dir exe

  platform="$(uname -ms)"
  case "$platform" in
    'Darwin x86_64') target=darwin-amd64 ;;
    'Darwin arm64')  target=darwin-arm64 ;;
    *)               target=linux-amd64  ;;
  esac

  # Alpine / musl
  case "$target" in
    linux*) [[ -f /etc/alpine-release ]] && target="$target-musl" ;;
  esac

  # Resolve the newest version on the selected channel if none was pinned.
  if [[ -z "$version" ]]; then
    version=$(resolve_channel_version)
    [[ -z "$version" ]] && fatal "Failed to fetch latest $CHANNEL Temps release from GitHub"
    info "Latest $CHANNEL version: ${BOLD}$version${RESET}"
  fi

  bin_dir="$HOME_DIR/.temps/bin"
  exe="$bin_dir/temps"
  mkdir -p "$bin_dir"

  local url="https://github.com/gotempsh/temps/releases/download/$version/temps-$target.tar.gz"
  curl --fail --location --progress-bar --output "$exe.tar.gz" "$url" || \
    fatal "Failed to download Temps from $url"
  tar -xzf "$exe.tar.gz" -C "$bin_dir" || fatal "Failed to extract Temps binary"
  chmod +x "$exe"
  rm -f "$exe.tar.gz"
  success "Temps $version installed to ${DIM}$bin_dir${RESET}"
}

# Ensure GeoLite2-City.mmdb is reachable by `temps serve`.
#
# `temps setup` downloads the DB into the data dir ($HOME_DIR/.temps/data), but
# the serve geo plugin opens "GeoLite2-City.mmdb" relative to its working
# directory ($HOME_DIR/.temps). If the file only exists in one place, serve
# crash-loops with "Failed to open MaxMind database". This makes both the
# data-dir copy and the working-dir copy resolve to a real file, downloading
# once if neither exists.
ensure_geolite2() {
  local home_db="$HOME_DIR/.temps/GeoLite2-City.mmdb"
  local data_db="$HOME_DIR/.temps/data/GeoLite2-City.mmdb"
  local geo_url="https://raw.githubusercontent.com/gotempsh/temps/refs/heads/main/crates/temps-cli/GeoLite2-City.mmdb"

  mkdir -p "$HOME_DIR/.temps/data"

  # If neither location has the DB (setup's download was skipped or failed),
  # fetch it into the data dir.
  if [[ ! -e "$data_db" ]] && [[ ! -e "$home_db" ]]; then
    echo ""
    info "Downloading GeoLite2 geolocation database..."
    if curl -sfL "$geo_url" -o "$data_db" 2>/dev/null; then
      success "GeoLite2 database installed"
    else
      warn "Could not download GeoLite2 database automatically."
      info "Temps serve requires it. Install it later with:"
      info "  ${BOLD}curl -sfL $geo_url -o $data_db${RESET}"
      return 0
    fi
  fi

  # Make the working-dir path resolve to the real file. Prefer the data-dir
  # copy as the source of truth; otherwise point data at the home copy.
  if [[ -e "$data_db" ]] && [[ ! -e "$home_db" ]]; then
    ln -sf "$data_db" "$home_db" 2>/dev/null || cp "$data_db" "$home_db" 2>/dev/null || true
  elif [[ -e "$home_db" ]] && [[ ! -e "$data_db" ]]; then
    ln -sf "$home_db" "$data_db" 2>/dev/null || cp "$home_db" "$data_db" 2>/dev/null || true
  fi
}

step_temps_setup() {
  step_header 4 $TOTAL_STEPS "Temps Platform Setup"

  # Check if temps binary is installed
  if ! check_command temps && [[ ! -f $HOME_DIR/.temps/bin/temps ]]; then
    info "Installing Temps binary..."
    echo ""
    install_temps_binary
    echo ""

    # Ensure it's in PATH
    export PATH="$HOME_DIR/.temps/bin:$PATH"
    $SUDO ln -sf $HOME_DIR/.temps/bin/temps /usr/local/bin/temps 2>/dev/null || true
  fi

  local temps_bin
  if [[ -f $HOME_DIR/.temps/bin/temps ]]; then
    temps_bin="$HOME_DIR/.temps/bin/temps"
  else
    temps_bin="$(command -v temps)"
  fi

  local temps_version
  temps_version=$("$temps_bin" --version 2>/dev/null || echo "unknown")
  success "Temps binary: ${DIM}$temps_version${RESET}"
  echo ""

  # Idempotency: check if temps setup has already been completed
  local setup_skipped=false
  if [[ -f $HOME_DIR/.temps/data/encryption_key ]]; then
    success "Temps platform already configured"

    ADMIN_EMAIL=$(state_get admin_email)
    ADMIN_PASSWORD=$(state_get admin_password)

    if [[ -n "$ADMIN_EMAIL" ]]; then
      info "Admin: ${BOLD}$ADMIN_EMAIL${RESET}"
    fi
    echo ""

    if ! prompt_yesno "Re-run temps setup?" "n"; then
      setup_skipped=true
    fi
  fi

  if [[ "$setup_skipped" != "true" ]]; then
    # Collect admin credentials
    prompt_input "Admin email" "" ADMIN_EMAIL
    if [[ -z "$ADMIN_EMAIL" ]]; then
      fatal "Admin email is required"
    fi

    echo ""
    local default_password
    default_password=$(generate_admin_password)
    prompt_input "Admin password" "$default_password" ADMIN_PASSWORD
    if [[ ${#ADMIN_PASSWORD} -lt 8 ]]; then
      fatal "Password must be at least 8 characters"
    fi

    # Preview configuration
    echo ""
    hr
    printf "\n  ${BOLD}Configuration Preview${RESET}\n\n"
    summary_row "Domain:" "$DOMAIN"
    summary_row "Wildcard:" "$WILDCARD_DOMAIN"
    summary_row "Admin email:" "$ADMIN_EMAIL"
    summary_row "Admin password:" "$ADMIN_PASSWORD"
    summary_row "Database:" "postgresql://temps:***@localhost:${DB_PORT}/temps"
    summary_row "Certificate:" "$FULLCHAIN_PATH"
    summary_row "Key:" "$KEY_PATH"
    summary_row "Data directory:" "$HOME_DIR/.temps/data"
    echo ""
    hr
    echo ""

    if ! prompt_yesno "Proceed with this configuration?" "y"; then
      fatal "Setup cancelled by user"
    fi

    echo ""
    info "Running ${BOLD}temps setup${RESET}..."
    echo ""

    mkdir -p $HOME_DIR/.temps/data

    local db_url="postgresql://temps:${DB_PASSWORD}@localhost:${DB_PORT}/temps?sslmode=disable"

    # Run temps setup and stream output, capturing exit code properly
    local setup_exit=0
    "$temps_bin" setup \
      --database-url "$db_url" \
      --admin-email "$ADMIN_EMAIL" \
      --admin-password "$ADMIN_PASSWORD" \
      --wildcard-domain "$WILDCARD_DOMAIN" \
      --wildcard-domain-cert "$FULLCHAIN_PATH" \
      --wildcard-domain-key "$KEY_PATH" \
      --skip-dns-records \
      --skip-git \
      --data-dir $HOME_DIR/.temps/data \
      --non-interactive \
      2>&1 | while IFS= read -r line; do
        printf "  ${DIM}  %s${RESET}\n" "$line"
      done || setup_exit=$?

    if [[ $setup_exit -ne 0 ]]; then
      echo ""
      warn "Temps setup failed (exit code $setup_exit)."
      info "Troubleshooting:"
      info "  1. Check the output above for specific errors"
      info "  2. Verify database is running:  ${BOLD}docker ps | grep timescale${RESET}"
      info "  3. Check database connectivity: ${BOLD}docker exec temps-timescaledb pg_isready${RESET}"
      info "  4. Re-run this script to retry:  ${BOLD}curl -fsSL https://temps.sh/deploy.sh | bash${RESET}"
      fatal "Setup cannot continue"
    fi

    echo ""
    success "Temps setup completed"

    # Persist credentials for idempotency
    state_set admin_email "$ADMIN_EMAIL"
    state_set admin_password "$ADMIN_PASSWORD"
  fi

  # --- Post-setup tasks (always run, even on idempotency skip) ---

  # Sync encryption key
  if [[ -f $HOME_DIR/.temps/data/encryption_key ]]; then
    cp $HOME_DIR/.temps/data/encryption_key $HOME_DIR/.temps/encryption_key 2>/dev/null || true
  fi

  # Import base domain certificate (temps setup only imports wildcard;
  # base domain needs its own import for SNI matching)
  local db_url_import="postgresql://temps:${DB_PASSWORD}@localhost:${DB_PORT}/temps?sslmode=disable"
  info "Importing base domain certificate for ${BOLD}$DOMAIN${RESET}..."
  if "$temps_bin" domain import \
    --domain "$DOMAIN" \
    --certificate "$FULLCHAIN_PATH" \
    --private-key "$KEY_PATH" \
    --database-url "$db_url_import" \
    --data-dir $HOME_DIR/.temps/data \
    --force 2>&1 | while IFS= read -r line; do
      printf "  ${DIM}  %s${RESET}\n" "$line"
    done; then
    success "Base domain certificate imported"
  else
    warn "Could not import base domain certificate (non-fatal)."
    info "You can run manually: temps domain import --domain $DOMAIN --certificate $FULLCHAIN_PATH --private-key $KEY_PATH"
  fi

  # GeoLite2 is required by `temps serve` (see ensure_geolite2 for the
  # data-dir vs working-dir path gap this resolves).
  ensure_geolite2
}

# ---------------------------------------------------------------------------
# Step 5: Background Service (systemd on Linux, launchd on macOS)
# ---------------------------------------------------------------------------

# Platform-agnostic service helpers
service_is_active() {
  local name="$1"
  if [[ "$IS_LINUX" == "true" ]]; then
    $SUDO systemctl is-active "$name" &>/dev/null
  elif [[ "$IS_MACOS" == "true" ]]; then
    launchctl list "dev.temps.$name" &>/dev/null 2>&1
  fi
}

service_start() {
  local name="$1"
  if [[ "$IS_LINUX" == "true" ]]; then
    $SUDO systemctl start "$name" 2>/dev/null
  elif [[ "$IS_MACOS" == "true" ]]; then
    launchctl bootstrap "gui/$(id -u)" "$HOME_DIR/Library/LaunchAgents/dev.temps.$name.plist" 2>/dev/null || \
      launchctl kickstart "gui/$(id -u)/dev.temps.$name" 2>/dev/null || true
  fi
}

service_stop() {
  local name="$1"
  if [[ "$IS_LINUX" == "true" ]]; then
    $SUDO systemctl stop "$name" 2>/dev/null || true
  elif [[ "$IS_MACOS" == "true" ]]; then
    launchctl bootout "gui/$(id -u)/dev.temps.$name" 2>/dev/null || true
  fi
}

service_restart() {
  local name="$1"
  if [[ "$IS_LINUX" == "true" ]]; then
    $SUDO systemctl restart "$name" 2>/dev/null || true
  elif [[ "$IS_MACOS" == "true" ]]; then
    service_stop "$name"
    sleep 1
    service_start "$name"
  fi
}

service_enable() {
  local name="$1"
  if [[ "$IS_LINUX" == "true" ]]; then
    $SUDO systemctl daemon-reload
    $SUDO systemctl enable "$name" > /dev/null 2>&1
  fi
  # launchd agents in ~/Library/LaunchAgents auto-load on login
}

# Service status/log commands for display
service_status_cmd() {
  local name="$1"
  if [[ "$IS_LINUX" == "true" ]]; then
    echo "${SUDO:+sudo }systemctl status $name"
  else
    echo "launchctl list dev.temps.$name"
  fi
}

service_logs_cmd() {
  local name="$1"
  if [[ "$IS_LINUX" == "true" ]]; then
    echo "${SUDO:+sudo }journalctl -u $name -f"
  else
    echo "tail -f $HOME_DIR/.temps/logs/$name.log"
  fi
}

service_restart_cmd() {
  local name="$1"
  if [[ "$IS_LINUX" == "true" ]]; then
    echo "${SUDO:+sudo }systemctl restart $name"
  else
    echo "launchctl kickstart -k gui/\$(id -u)/dev.temps.$name"
  fi
}

step_service() {
  local svc_type="systemd"
  [[ "$IS_MACOS" == "true" ]] && svc_type="launchd"

  step_header 5 $TOTAL_STEPS "Background Service ($svc_type)"

  info "Temps can run as a background service that starts automatically"
  info "on boot and restarts if it crashes."
  echo ""

  # Check if already running and healthy
  if service_is_active temps \
    && curl -sf "http://localhost:8081/health" > /dev/null 2>&1; then
    success "Temps service is already active and healthy"
    info "Status: ${GREEN}active${RESET}"

    if prompt_yesno "Restart the service with new configuration?" "n"; then
      write_service_unit
      service_restart temps
      sleep 3
      success "Service restarted"
    fi
    return 0
  fi

  local status_cmd logs_cmd
  status_cmd=$(service_status_cmd temps)
  logs_cmd=$(service_logs_cmd temps)

  echo ""
  printf "  ${CYAN}┌─────────────────────────────────────────────────────┐${RESET}\n"
  printf "  ${CYAN}│${RESET}  ${BOLD}Recommended:${RESET} Install as a background service       ${CYAN}│${RESET}\n"
  printf "  ${CYAN}│${RESET}                                                     ${CYAN}│${RESET}\n"
  printf "  ${CYAN}│${RESET}  ${DIM}This ensures Temps runs in the background,${RESET}          ${CYAN}│${RESET}\n"
  printf "  ${CYAN}│${RESET}  ${DIM}starts on boot, and auto-restarts on failure.${RESET}       ${CYAN}│${RESET}\n"
  printf "  ${CYAN}│${RESET}                                                     ${CYAN}│${RESET}\n"
  printf "  ${CYAN}│${RESET}  ${DIM}You can manage it with:${RESET}                             ${CYAN}│${RESET}\n"
  printf "  ${CYAN}│${RESET}    ${GREEN}%-47s${RESET} ${CYAN}│${RESET}\n" "$status_cmd"
  printf "  ${CYAN}│${RESET}    ${GREEN}%-47s${RESET} ${CYAN}│${RESET}\n" "$logs_cmd"
  printf "  ${CYAN}│${RESET}                                                     ${CYAN}│${RESET}\n"
  printf "  ${CYAN}└─────────────────────────────────────────────────────┘${RESET}\n"
  echo ""

  if ! prompt_yesno "Install background service? (recommended)" "y"; then
    echo ""
    warn "Skipping service installation."
    info "You can start Temps manually with:"
    echo ""
    printf "  ${GREEN}  temps serve \\\\${RESET}\n"
    printf "  ${GREEN}    --address=\"0.0.0.0:80\" \\\\${RESET}\n"
    printf "  ${GREEN}    --tls-address=\"0.0.0.0:443\" \\\\${RESET}\n"
    printf "  ${GREEN}    --database-url=\"postgresql://temps:***@localhost:${DB_PORT}/temps?sslmode=disable\" \\\\${RESET}\n"
    printf "  ${GREEN}    --data-dir=\"$HOME_DIR/.temps/data\" \\\\${RESET}\n"
    printf "  ${GREEN}    --console-address=\"0.0.0.0:8081\"${RESET}\n"
    echo ""
    return 0
  fi

  echo ""
  write_service_unit
  service_enable temps

  if ! service_start temps; then
    warn "Service failed to start on first attempt. Retrying in 3 seconds..."
    sleep 3
    service_restart temps
  fi

  # Wait for health
  info "Waiting for Temps to start..."
  local healthy=false
  for i in $(seq 1 20); do
    progress_bar "$i" 20 "checking health"
    if curl -sf "http://localhost:8081/health" > /dev/null 2>&1; then
      healthy=true
      break
    fi
    sleep 3
  done
  progress_bar 20 20 "checking health"
  echo ""

  if [[ "$healthy" == "true" ]]; then
    echo ""
    success "Temps service is running and healthy"
  else
    echo ""
    warn "Temps started but health check not yet passing"
    info "This is normal — it may need a few more seconds."
    info "Check logs with: ${BOLD}$(service_logs_cmd temps)${RESET}"
    info "Check status with: ${BOLD}$(service_status_cmd temps)${RESET}"
  fi
}

write_service_unit() {
  local db_url="postgresql://temps:${DB_PASSWORD}@localhost:${DB_PORT}/temps?sslmode=disable"
  local temps_bin="$HOME_DIR/.temps/bin/temps"

  # When the operator opted out (--no-telemetry), add TEMPS_TELEMETRY=0 to the
  # service environment so the binary's anonymous telemetry stays off. Empty
  # string when telemetry is left on (the default), so nothing is injected.
  local systemd_telemetry_env=""
  local plist_telemetry_env=""
  if [[ "$TELEMETRY_OPTOUT" == "true" ]]; then
    systemd_telemetry_env=$'\nEnvironment=TEMPS_TELEMETRY=0'
    # launchd EnvironmentVariables dict, injected before WorkingDirectory.
    plist_telemetry_env=$'  <key>EnvironmentVariables</key>\n  <dict>\n    <key>TEMPS_TELEMETRY</key>\n    <string>0</string>\n  </dict>\n'
  fi

  if [[ "$IS_LINUX" == "true" ]]; then
    $SUDO bash -c "cat > /etc/systemd/system/temps.service" << EOF
[Unit]
Description=Temps Platform Server
After=network.target docker.service
Wants=docker.service

[Service]
Type=simple
User=$RUN_USER
WorkingDirectory=$HOME_DIR/.temps
ExecStart=$temps_bin serve \\
  --address="0.0.0.0:80" \\
  --tls-address="0.0.0.0:443" \\
  --database-url="$db_url" \\
  --data-dir="$HOME_DIR/.temps/data" \\
  --console-address="0.0.0.0:8081"
Restart=always
RestartSec=5
LimitNOFILE=65535
AmbientCapabilities=CAP_NET_BIND_SERVICE
Environment=HOME=$HOME_DIR$systemd_telemetry_env

[Install]
WantedBy=multi-user.target
EOF
    success "Systemd unit written to ${BOLD}/etc/systemd/system/temps.service${RESET}"

  elif [[ "$IS_MACOS" == "true" ]]; then
    mkdir -p "$HOME_DIR/Library/LaunchAgents"
    mkdir -p "$HOME_DIR/.temps/logs"
    cat > "$HOME_DIR/Library/LaunchAgents/dev.temps.temps.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.temps.temps</string>
  <key>ProgramArguments</key>
  <array>
    <string>$temps_bin</string>
    <string>serve</string>
    <string>--address=0.0.0.0:80</string>
    <string>--tls-address=0.0.0.0:443</string>
    <string>--database-url=$db_url</string>
    <string>--data-dir=$HOME_DIR/.temps/data</string>
    <string>--console-address=0.0.0.0:8081</string>
  </array>
${plist_telemetry_env}  <key>WorkingDirectory</key>
  <string>$HOME_DIR/.temps</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$HOME_DIR/.temps/logs/temps.log</string>
  <key>StandardErrorPath</key>
  <string>$HOME_DIR/.temps/logs/temps.log</string>
</dict>
</plist>
EOF
    success "LaunchAgent written to ${BOLD}~/Library/LaunchAgents/dev.temps.temps.plist${RESET}"
  fi
}

# ---------------------------------------------------------------------------
# Verification: generate API key + verify console URL is reachable
# ---------------------------------------------------------------------------

API_KEY=""

step_verify() {
  echo ""
  hr
  echo ""
  printf "  ${BG_BLUE}${BOLD}${WHITE} VERIFYING ${RESET}  ${BOLD}Checking your Temps instance${RESET}\n"
  echo ""

  # Recover globals from state if needed (idempotent re-runs)
  [[ -z "$DOMAIN" ]] && DOMAIN=$(state_get domain)
  [[ -z "$DB_PASSWORD" ]] && DB_PASSWORD=$(state_get db_password)
  [[ -z "$ADMIN_EMAIL" ]] && ADMIN_EMAIL=$(state_get admin_email)

  local console_url="https://$DOMAIN"

  # --- 1. Generate API key ---
  local temps_bin="$HOME_DIR/.temps/bin/temps"
  if [[ ! -f "$temps_bin" ]]; then
    temps_bin="$(command -v temps 2>/dev/null || echo "$HOME_DIR/.temps/bin/temps")"
  fi

  local db_url="postgresql://temps:${DB_PASSWORD}@localhost:${DB_PORT}/temps?sslmode=disable"

  # Check if we already have an API key from a previous run
  API_KEY=$(state_get api_key)
  if [[ -z "$API_KEY" ]]; then
    info "Generating API key..."
    local apikey_output
    apikey_output=$("$temps_bin" api-key \
      --database-url "$db_url" \
      --name "wizard-setup" \
      --role admin \
      --user-email "$ADMIN_EMAIL" \
      --output-format json \
      2>/dev/null) || true

    if [[ -n "$apikey_output" ]]; then
      # Extract the key from JSON output
      API_KEY=$(json_value "$apikey_output" "key")
      # Fallback: try "api_key" field name
      [[ -z "$API_KEY" ]] && API_KEY=$(json_value "$apikey_output" "api_key")
      # Fallback: try "token" field name
      [[ -z "$API_KEY" ]] && API_KEY=$(json_value "$apikey_output" "token")
    fi

    if [[ -n "$API_KEY" ]]; then
      state_set api_key "$API_KEY"
      success "API key generated"
    else
      warn "Could not generate API key automatically."
      info "You can create one manually: ${BOLD}temps api-key --database-url \"$db_url\" --name \"my-key\"${RESET}"
    fi
  else
    success "API key already generated"
  fi

  # --- 2. Verify console URL is reachable ---
  echo ""
  info "Verifying ${BOLD}$console_url${RESET} is reachable..."

  local verified=false
  for i in $(seq 1 15); do
    progress_bar "$i" 15 "waiting for response"
    local http_status
    http_status=$(curl -sk -o /dev/null -w "%{http_code}" "$console_url" 2>/dev/null || echo "000")

    # Any real HTTP response (even 302 redirect to login) means it's working
    if [[ "$http_status" =~ ^[2-3][0-9][0-9]$ ]]; then
      verified=true
      break
    fi
    sleep 3
  done
  progress_bar 15 15 "waiting for response"
  echo ""

  if [[ "$verified" == "true" ]]; then
    echo ""
    success "Console is live at ${BOLD}${UNDERLINE}$console_url${RESET}"
  else
    echo ""
    warn "Console did not respond yet at ${BOLD}$console_url${RESET}"
    info "DNS propagation may still be in progress."
    info "Check service logs: ${BOLD}$(service_logs_cmd temps)${RESET}"
  fi
}

# ---------------------------------------------------------------------------
# Test Deployments (optional)
# ---------------------------------------------------------------------------

step_test_deploy() {
  echo ""
  hr
  echo ""
  printf "  ${BG_BLUE}${BOLD}${WHITE} TEST DEPLOY ${RESET}  ${BOLD}Verify your platform with sample apps${RESET}\n"
  echo ""
  info "This will deploy a static page and a Next.js Docker app"
  info "to confirm everything is working end-to-end."
  echo ""

  if ! prompt_yesno "Deploy test applications?" "y"; then
    info "Skipping test deploy."
    return 0
  fi

  # Recover globals from state if needed
  if [[ -z "$DOMAIN" ]]; then
    DOMAIN=$(state_get domain)
  fi
  if [[ -z "$API_KEY" ]]; then
    API_KEY=$(state_get api_key)
  fi

  if [[ -z "$DOMAIN" ]] || [[ -z "$API_KEY" ]]; then
    warn "Domain or API key not available — cannot run test deploy."
    return 0
  fi

  local base_url="https://$DOMAIN"

  # --- Ensure Node.js / npx is available ---
  local npx_cmd=""

  if check_command npx; then
    npx_cmd="npx"
  elif check_command bunx; then
    npx_cmd="bunx"
  else
    info "Installing Node.js for CLI tools..."
    spinner "Installing Node.js 22..." bash -c '
      if command -v apt-get &>/dev/null; then
        curl -fsSL https://deb.nodesource.com/setup_22.x | '"$SUDO"' bash - &>/dev/null
        '"$SUDO"' apt-get install -y nodejs &>/dev/null
      elif command -v dnf &>/dev/null; then
        curl -fsSL https://rpm.nodesource.com/setup_22.x | '"$SUDO"' bash - &>/dev/null
        '"$SUDO"' dnf install -y nodejs &>/dev/null
      elif command -v yum &>/dev/null; then
        curl -fsSL https://rpm.nodesource.com/setup_22.x | '"$SUDO"' bash - &>/dev/null
        '"$SUDO"' yum install -y nodejs &>/dev/null
      elif command -v brew &>/dev/null; then
        brew install node &>/dev/null
      else
        exit 1
      fi
    '
    hash -r 2>/dev/null || true

    if check_command npx; then
      npx_cmd="npx"
      success "Node.js installed"
    else
      warn "Could not install Node.js — skipping test deploy."
      return 0
    fi
  fi

  # Configure Temps CLI
  info "Configuring Temps CLI..."
  local api_url="${base_url}/api"
  $npx_cmd -y @temps-sdk/cli configure set apiUrl "$api_url" --no-color 2>/dev/null
  $npx_cmd -y @temps-sdk/cli login --api-key "$API_KEY" --no-color 2>/dev/null
  success "CLI authenticated"

  local test_work_dir
  test_work_dir=$(mktemp -d)

  # Track results
  local static_ok=false
  local nextjs_ok=false

  # ===========================================================
  # Test A — Static Files
  # ===========================================================

  echo ""
  printf "  ${BOLD}${CYAN}Test A${RESET} ${DIM}─${RESET} ${BOLD}Static Files${RESET}\n"
  echo ""

  local static_slug="hello-world"

  # Check if project exists
  local existing
  existing=$(curl -sk \
    -H "Authorization: Bearer $API_KEY" \
    "${base_url}/api/projects" 2>/dev/null || echo "")

  if echo "$existing" | grep -q "\"slug\":\"$static_slug\""; then
    info "Project ${BOLD}$static_slug${RESET} already exists — redeploying"
  else
    info "Creating project ${BOLD}$static_slug${RESET}..."
    local create_code create_body create_resp
    create_resp=$(curl -sk -w "\n%{http_code}" \
      -H "Authorization: Bearer $API_KEY" \
      -H "Content-Type: application/json" \
      -X POST "${base_url}/api/projects" \
      -d "{\"name\":\"$static_slug\",\"directory\":\"/\",\"main_branch\":\"main\",\"preset\":\"nextjs\",\"storage_service_ids\":[],\"source_type\":\"static_files\"}" 2>/dev/null)
    create_code=$(echo "$create_resp" | tail -1)
    if [[ "$create_code" != "200" ]] && [[ "$create_code" != "201" ]]; then
      create_body=$(echo "$create_resp" | sed '$d')
      error "Failed to create project (HTTP $create_code): $create_body"
      warn "Skipping static test."
    else
      success "Project created"
    fi
  fi

  # Build static site
  local static_dir="$test_work_dir/site"
  mkdir -p "$static_dir"
  cat > "$static_dir/index.html" << 'STATICHTML'
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Hello from Temps</title>
  <style>
    *{margin:0;padding:0;box-sizing:border-box}
    body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;background:linear-gradient(135deg,#0f172a,#1e293b);color:#e2e8f0;min-height:100vh;display:flex;align-items:center;justify-content:center}
    .c{text-align:center;padding:2rem}
    h1{font-size:3rem;background:linear-gradient(90deg,#38bdf8,#818cf8,#c084fc);-webkit-background-clip:text;-webkit-text-fill-color:transparent;margin-bottom:1rem}
    p{font-size:1.25rem;color:#94a3b8;margin-bottom:2rem}
    .b{display:inline-block;background:rgba(56,189,248,.1);border:1px solid rgba(56,189,248,.3);border-radius:9999px;padding:.5rem 1.5rem;font-size:.875rem;color:#38bdf8}
  </style>
</head>
<body>
  <div class="c">
    <h1>Hello from Temps</h1>
    <p>Your self-hosted deployment platform is working.</p>
    <span class="b">Static Deploy Test</span>
  </div>
</body>
</html>
STATICHTML

  local archive="$test_work_dir/site.tar.gz"
  tar -czf "$archive" -C "$static_dir" .

  info "Deploying static files..."
  local deploy_exit=0
  local deploy_out
  deploy_out=$($npx_cmd -y @temps-sdk/cli deploy:static \
    --path "$archive" \
    --project "$static_slug" \
    --environment "production" \
    --yes \
    --no-color 2>&1) || deploy_exit=$?

  if [[ $deploy_exit -ne 0 ]]; then
    error "Static deploy failed (exit $deploy_exit)"
    echo "    $deploy_out" | head -5
  else
    success "Static files deployed"

    # Verify
    local static_url="https://${static_slug}-production.${DOMAIN}"
    local http_ok=false
    for i in $(seq 1 10); do
      progress_bar "$i" 10 "verifying"
      local code
      code=$(curl -sk -o /dev/null -w "%{http_code}" "$static_url" 2>/dev/null || echo "000")
      if [[ "$code" =~ ^[2-3][0-9][0-9]$ ]]; then
        http_ok=true
        break
      fi
      sleep 2
    done
    progress_bar 10 10 "done"
    echo ""

    if [[ "$http_ok" == "true" ]]; then
      success "Static app live at ${BOLD}${UNDERLINE}$static_url${RESET}"
      static_ok=true
    else
      warn "Static app not responding at $static_url"
    fi
  fi

  # ===========================================================
  # Test B — Next.js Docker
  # ===========================================================

  local has_docker=false
  if $DOCKER_SUDO docker info &>/dev/null 2>&1; then
    has_docker=true
  fi

  if [[ "$has_docker" == "true" ]]; then
    echo ""
    printf "  ${BOLD}${CYAN}Test B${RESET} ${DIM}─${RESET} ${BOLD}Next.js Docker${RESET}\n"
    echo ""

    local nextjs_slug="hello-nextjs"
    local nextjs_image="hello-nextjs-temps-test:latest"

    # Create project
    if echo "$existing" | grep -q "\"slug\":\"$nextjs_slug\""; then
      info "Project ${BOLD}$nextjs_slug${RESET} already exists — redeploying"
    else
      info "Creating project ${BOLD}$nextjs_slug${RESET}..."
      create_resp=$(curl -sk -w "\n%{http_code}" \
        -H "Authorization: Bearer $API_KEY" \
        -H "Content-Type: application/json" \
        -X POST "${base_url}/api/projects" \
        -d "{\"name\":\"$nextjs_slug\",\"directory\":\"/\",\"main_branch\":\"main\",\"preset\":\"nextjs\",\"storage_service_ids\":[],\"source_type\":\"docker_image\"}" 2>/dev/null)
      create_code=$(echo "$create_resp" | tail -1)
      if [[ "$create_code" != "200" ]] && [[ "$create_code" != "201" ]]; then
        create_body=$(echo "$create_resp" | sed '$d')
        error "Failed to create project (HTTP $create_code): $create_body"
      else
        success "Project created"
      fi
    fi

    # Scaffold Next.js app
    local nextjs_dir="$test_work_dir/nextjs-app"
    mkdir -p "$nextjs_dir/app"

    cat > "$nextjs_dir/package.json" << 'PKGJSON'
{
  "name": "hello-nextjs",
  "version": "1.0.0",
  "private": true,
  "scripts": { "dev": "next dev", "build": "next build", "start": "next start" },
  "dependencies": { "next": "15.3.4", "react": "^19.0.0", "react-dom": "^19.0.0" }
}
PKGJSON

    cat > "$nextjs_dir/next.config.js" << 'NEXTCFG'
/** @type {import('next').NextConfig} */
const nextConfig = { output: 'standalone' }
module.exports = nextConfig
NEXTCFG

    cat > "$nextjs_dir/app/layout.js" << 'LAYOUT'
export const metadata = { title: 'Hello from Temps' }
export default function RootLayout({ children }) {
  return <html lang="en"><body>{children}</body></html>
}
LAYOUT

    cat > "$nextjs_dir/app/page.js" << 'PAGEJS'
export default function Home() {
  return (
    <div style={{minHeight:'100vh',display:'flex',alignItems:'center',justifyContent:'center',background:'linear-gradient(135deg,#0f172a,#1e293b)',color:'#e2e8f0',fontFamily:'-apple-system,BlinkMacSystemFont,sans-serif'}}>
      <div style={{textAlign:'center'}}>
        <h1 style={{fontSize:'3rem',marginBottom:'1rem'}}>Hello from Temps</h1>
        <p style={{fontSize:'1.25rem',color:'#94a3b8'}}>Next.js running in Docker on your self-hosted platform.</p>
        <span style={{display:'inline-block',background:'rgba(56,189,248,.1)',border:'1px solid rgba(56,189,248,.3)',borderRadius:'9999px',padding:'.5rem 1.5rem',fontSize:'.875rem',color:'#38bdf8',marginTop:'1rem'}}>Next.js Docker Deploy</span>
      </div>
    </div>
  )
}
PAGEJS

    cat > "$nextjs_dir/Dockerfile" << 'DKRFILE'
FROM node:22-alpine AS deps
WORKDIR /app
COPY package.json ./
RUN npm install --production=false

FROM node:22-alpine AS builder
WORKDIR /app
COPY --from=deps /app/node_modules ./node_modules
COPY . .
RUN npm run build

FROM node:22-alpine AS runner
WORKDIR /app
ENV NODE_ENV=production
ENV PORT=3000
RUN addgroup --system --gid 1001 nodejs && adduser --system --uid 1001 nextjs
COPY --from=builder /app/.next/standalone ./
COPY --from=builder /app/.next/static ./.next/static
USER nextjs
EXPOSE 3000
CMD ["node", "server.js"]
DKRFILE

    info "Building Docker image (30-90 seconds)..."
    local build_exit=0
    spinner "Building Next.js Docker image..." $DOCKER_SUDO docker build -t "$nextjs_image" "$nextjs_dir" || build_exit=$?

    if [[ $build_exit -ne 0 ]]; then
      error "Docker build failed. Check output above."
    else
      local image_size
      image_size=$($DOCKER_SUDO docker images "$nextjs_image" --format "{{.Size}}" 2>/dev/null || echo "unknown")
      success "Docker image built (${image_size})"

      # Get project ID and environment ID
      local proj_data proj_id env_data env_id
      proj_data=$(curl -sk \
        -H "Authorization: Bearer $API_KEY" \
        "${base_url}/api/projects" 2>/dev/null || echo "")

      if check_command python3; then
        proj_id=$(echo "$proj_data" | python3 -c "
import json,sys
for p in json.load(sys.stdin).get('projects',[]):
  if p.get('slug')=='$nextjs_slug': print(p['id']); break
" 2>/dev/null || echo "")
      fi
      if [[ -z "$proj_id" ]]; then
        proj_id=$(json_value "$proj_data" "id")
      fi

      if [[ -n "$proj_id" ]]; then
        env_data=$(curl -sk \
          -H "Authorization: Bearer $API_KEY" \
          "${base_url}/api/projects/$proj_id/environments" 2>/dev/null || echo "")

        if check_command python3; then
          env_id=$(echo "$env_data" | python3 -c "
import json,sys
for e in json.load(sys.stdin):
  if e.get('name')=='production': print(e['id']); break
" 2>/dev/null || echo "")
        fi
        if [[ -z "$env_id" ]]; then
          env_id=$(json_value "$env_data" "id")
        fi
      fi

      if [[ -n "$proj_id" ]] && [[ -n "$env_id" ]]; then
        # Export and upload
        info "Uploading Docker image to Temps..."
        local image_tar="$test_work_dir/nextjs-image.tar"
        spinner "Exporting Docker image..." $DOCKER_SUDO docker save "$nextjs_image" -o "$image_tar"

        local upload_resp upload_code upload_body
        upload_resp=$(curl -sk -w "\n%{http_code}" \
          -H "Authorization: Bearer $API_KEY" \
          -F "file=@${image_tar};type=application/x-tar" \
          "${base_url}/api/projects/${proj_id}/environments/${env_id}/deploy/image-upload" 2>/dev/null)
        upload_code=$(echo "$upload_resp" | tail -1)
        upload_body=$(echo "$upload_resp" | sed '$d')

        rm -f "$image_tar"

        if [[ "$upload_code" != "200" ]] && [[ "$upload_code" != "201" ]] && [[ "$upload_code" != "202" ]]; then
          error "Image upload failed (HTTP $upload_code): $upload_body"
        else
          success "Image uploaded — deployment started"

          # Wait for deployment
          info "Waiting for deployment..."
          local dep_status="pending" dep_ok=false
          for i in $(seq 1 40); do
            progress_bar "$i" 40 "$dep_status"
            local status_resp new_status=""
            status_resp=$(curl -sk \
              -H "Authorization: Bearer $API_KEY" \
              "${base_url}/api/projects/${proj_id}/deployments" 2>/dev/null || echo "")
            if check_command python3; then
              new_status=$(echo "$status_resp" | python3 -c "
import json,sys
d=json.load(sys.stdin).get('deployments',[])
if d: print(d[0].get('status',''))
" 2>/dev/null || echo "")
            fi
            if [[ -z "$new_status" ]]; then
              new_status=$(json_value "$status_resp" "status")
            fi
            if [[ -n "$new_status" ]]; then
              dep_status="$new_status"
            fi
            case "$dep_status" in
              completed|ready|active|succeeded) dep_ok=true; break ;;
              failed|error|cancelled) break ;;
            esac
            sleep 3
          done
          progress_bar 40 40 "$dep_status"
          echo ""

          if [[ "$dep_ok" == "true" ]]; then
            success "Next.js deployment completed"

            # Verify
            local nextjs_url="https://${nextjs_slug}-production.${DOMAIN}"
            for i in $(seq 1 10); do
              progress_bar "$i" 10 "verifying"
              local code
              code=$(curl -sk -o /dev/null -w "%{http_code}" "$nextjs_url" 2>/dev/null || echo "000")
              if [[ "$code" =~ ^[2-3][0-9][0-9]$ ]]; then
                nextjs_ok=true
                break
              fi
              sleep 2
            done
            progress_bar 10 10 "done"
            echo ""

            if [[ "$nextjs_ok" == "true" ]]; then
              success "Next.js app live at ${BOLD}${UNDERLINE}$nextjs_url${RESET}"
            else
              warn "Next.js app not responding at $nextjs_url"
            fi
          else
            error "Next.js deployment failed (status: $dep_status)"
          fi
        fi
      else
        error "Could not determine project/environment IDs"
      fi

      # Clean up image
      $DOCKER_SUDO docker rmi "$nextjs_image" &>/dev/null || true
    fi
  else
    info "Docker not available — skipping Next.js Docker test"
  fi

  # --- Summary ---
  echo ""
  printf "  ${BOLD}Test Results${RESET}\n"
  echo ""

  if [[ "$static_ok" == "true" ]]; then
    printf "  ${CHECK} ${BOLD}Static Files${RESET}    ${GREEN}$static_url${RESET}\n"
  else
    printf "  ${CROSS} ${BOLD}Static Files${RESET}    ${YELLOW}not verified${RESET}\n"
  fi

  if [[ "$has_docker" == "true" ]]; then
    if [[ "$nextjs_ok" == "true" ]]; then
      printf "  ${CHECK} ${BOLD}Next.js Docker${RESET}  ${GREEN}$nextjs_url${RESET}\n"
    else
      printf "  ${CROSS} ${BOLD}Next.js Docker${RESET}  ${YELLOW}not verified${RESET}\n"
    fi
  else
    printf "  ${DIM}  Next.js Docker   skipped (no Docker)${RESET}\n"
  fi
  echo ""

  # Clean up work dir
  rm -rf "$test_work_dir"
}

# ---------------------------------------------------------------------------
# Completion
# ---------------------------------------------------------------------------

show_completion() {
  # Recover credentials from state if step was skipped on re-run
  [[ -z "$ADMIN_EMAIL" ]] && ADMIN_EMAIL=$(state_get admin_email)
  [[ -z "$ADMIN_PASSWORD" ]] && ADMIN_PASSWORD=$(state_get admin_password)

  echo ""
  hr
  echo ""
  printf "  ${BG_GREEN}${BOLD}${WHITE} SETUP COMPLETE ${RESET}\n"
  echo ""

  local console_url="https://$DOMAIN"

  printf "  ${BOLD}Your Temps instance is ready!${RESET}\n"
  echo ""
  summary_row "Console:" "$console_url"
  summary_row "Admin email:" "$ADMIN_EMAIL"
  summary_row "Admin password:" "$ADMIN_PASSWORD"
  if [[ -n "$API_KEY" ]]; then
    summary_row "API key:" "$API_KEY"
  fi
  summary_row "Domain:" "$DOMAIN"
  summary_row "Wildcard:" "$WILDCARD_DOMAIN"
  if [[ "$TELEMETRY_OPTOUT" == "true" ]]; then
    summary_row "Telemetry:" "disabled (opt-out)"
  else
    summary_row "Telemetry:" "on (anonymous, no PII) — disable with --no-telemetry"
  fi
  echo ""

  warn "Save your admin password and API key now — they won't be shown again."
  echo ""

  hr
  echo ""
  printf "  ${BOLD}Next steps${RESET}\n"
  echo ""
  info "1. Open your console at ${GREEN}${UNDERLINE}${console_url}${RESET}"
  info "2. Log in with ${BOLD}$ADMIN_EMAIL${RESET} and your password"
  info "3. Deploy your first app with ${GREEN}temps deploy${RESET}"
  echo ""

  hr
  echo ""
  printf "  ${BOLD}Useful commands${RESET}\n"
  echo ""
  info "${GREEN}$(service_status_cmd temps)${RESET}"
  info "                                  Service status"
  info "${GREEN}$(service_restart_cmd temps)${RESET}"
  info "                                  Restart the server"
  info "${GREEN}$(service_logs_cmd temps)${RESET}"
  info "                                  Live logs"
  info "${GREEN}temps --help${RESET}                     CLI reference"
  echo ""

  hr
  echo ""
  info "Documentation: ${UNDERLINE}https://temps.sh/docs${RESET}"
  info "Support:       ${UNDERLINE}https://github.com/gotempsh/temps/issues${RESET}"
  echo ""
}

# ---------------------------------------------------------------------------
# QuickStart / Testing flow (sslip.io)
# ---------------------------------------------------------------------------
#
# A streamlined path that gets Temps running without a custom domain:
#   - QuickStart: HTTP only, near-zero prompts (~90s). Console at
#       http://console.<ip>.sslip.io
#   - Testing:    additionally provisions a real Let's Encrypt certificate for
#       the console host via HTTP-01 (requires port 80 reachable). Apps stay on
#       HTTP because *.<ip>.sslip.io wildcards cannot be issued by Let's Encrypt.
#
# Both reuse step_docker and step_timescaledb (mode-agnostic, idempotent), then
# install the binary on the chosen channel and run `temps setup --auto`.

# Detect this server's public IPv4, falling back to a private IP, then 127.0.0.1.
detect_public_ip_quick() {
  local ip=""
  ip=$(curl -4 -sf --max-time 5 https://api.ipify.org 2>/dev/null \
    || curl -4 -sf --max-time 5 https://ifconfig.me 2>/dev/null || true)
  if [[ -z "$ip" ]]; then
    # Private IP fallback (Linux `ip` route, then hostname -I)
    ip=$(ip -4 route get 8.8.8.8 2>/dev/null | grep -oE 'src [0-9.]+' | awk '{print $2}' | head -1 || true)
    [[ -z "$ip" ]] && ip=$(hostname -I 2>/dev/null | awk '{print $1}' || true)
  fi
  [[ -z "$ip" ]] && ip="127.0.0.1"
  echo "$ip"
}

# Probe whether $1:80 is reachable from the public internet. Spins up a tiny
# listener, curls it back via the public IP, and compares a random token.
# Returns 0 if reachable.
probe_port_80() {
  local ip="$1"
  local probe_token probe_pid probe_result
  probe_token="temps-probe-$(openssl rand -hex 4 2>/dev/null || echo $$)"
  (echo -e "HTTP/1.1 200 OK\r\nContent-Length: ${#probe_token}\r\n\r\n${probe_token}" | nc -l -p 80 -q 1 2>/dev/null || \
   echo -e "HTTP/1.1 200 OK\r\nContent-Length: ${#probe_token}\r\n\r\n${probe_token}" | nc -l 80 2>/dev/null) >/dev/null 2>&1 &
  probe_pid=$!
  sleep 1
  probe_result=$(curl -sf --max-time 5 "http://${ip}/" 2>/dev/null || true)
  kill "$probe_pid" 2>/dev/null || true
  wait "$probe_pid" 2>/dev/null || true
  [[ "$probe_result" == "$probe_token" ]]
}

# Write the systemd unit / launchd plist for quick/testing mode.
# Usage: write_quick_service_unit <tls: true|false>
# HTTP listens on :80 with --disable-https-redirect; when tls=true we also
# listen on :443 (the cert is served from the data dir provisioned via HTTP-01).
write_quick_service_unit() {
  local tls="${1:-false}"
  local db_url="postgresql://temps:${DB_PASSWORD}@localhost:${DB_PORT}/temps?sslmode=disable"
  local temps_bin="$HOME_DIR/.temps/bin/temps"

  # Telemetry opt-out (--no-telemetry): inject TEMPS_TELEMETRY=0 into the
  # service env. Empty when telemetry is on (default). See write_service_unit.
  local systemd_telemetry_env=""
  local plist_telemetry_env=""
  if [[ "$TELEMETRY_OPTOUT" == "true" ]]; then
    systemd_telemetry_env=$'\nEnvironment=TEMPS_TELEMETRY=0'
    plist_telemetry_env=$'  <key>EnvironmentVariables</key>\n  <dict>\n    <key>TEMPS_TELEMETRY</key>\n    <string>0</string>\n  </dict>\n'
  fi

  if [[ "$IS_LINUX" == "true" ]]; then
    local tls_line=""
    [[ "$tls" == "true" ]] && tls_line="  --tls-address=\"0.0.0.0:443\" \\"
    $SUDO bash -c "cat > /etc/systemd/system/temps.service" << EOF
[Unit]
Description=Temps Platform Server
After=network.target docker.service
Wants=docker.service

[Service]
Type=simple
User=$RUN_USER
WorkingDirectory=$HOME_DIR/.temps
ExecStart=$temps_bin serve \\
  --address="0.0.0.0:${LOCAL_PORT}" \\
${tls_line:+$tls_line
}  --database-url="$db_url" \\
  --data-dir="$HOME_DIR/.temps/data" \\
  --console-address="0.0.0.0:8081" \\
  --disable-https-redirect
Restart=always
RestartSec=5
LimitNOFILE=65535
AmbientCapabilities=CAP_NET_BIND_SERVICE
Environment=HOME=$HOME_DIR$systemd_telemetry_env

[Install]
WantedBy=multi-user.target
EOF
    success "Systemd unit written to ${BOLD}/etc/systemd/system/temps.service${RESET}"

  elif [[ "$IS_MACOS" == "true" ]]; then
    mkdir -p "$HOME_DIR/Library/LaunchAgents"
    mkdir -p "$HOME_DIR/.temps/logs"
    local tls_arg=""
    [[ "$tls" == "true" ]] && tls_arg="    <string>--tls-address=0.0.0.0:443</string>"
    cat > "$HOME_DIR/Library/LaunchAgents/dev.temps.temps.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.temps.temps</string>
  <key>ProgramArguments</key>
  <array>
    <string>$temps_bin</string>
    <string>serve</string>
    <string>--address=0.0.0.0:${LOCAL_PORT}</string>
${tls_arg:+$tls_arg
}    <string>--database-url=$db_url</string>
    <string>--data-dir=$HOME_DIR/.temps/data</string>
    <string>--console-address=0.0.0.0:8081</string>
    <string>--disable-https-redirect</string>
  </array>
${plist_telemetry_env}  <key>WorkingDirectory</key>
  <string>$HOME_DIR/.temps</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$HOME_DIR/.temps/logs/temps.log</string>
  <key>StandardErrorPath</key>
  <string>$HOME_DIR/.temps/logs/temps.log</string>
</dict>
</plist>
EOF
    success "LaunchAgent written to ${BOLD}~/Library/LaunchAgents/dev.temps.temps.plist${RESET}"
  fi
}

run_quick_flow() {
  local mode_label="QuickStart"
  [[ "$SETUP_MODE" == "testing" ]] && mode_label="Testing"
  [[ "$SETUP_MODE" == "local" ]] && mode_label="Local"

  TOTAL_STEPS=5

  echo ""
  info "Setting up Temps in ${BOLD}$mode_label${RESET} mode."
  case "$SETUP_MODE" in
    local)
      info "Running on ${BOLD}this machine${RESET} via ${BOLD}127.0.0.1.sslip.io${RESET}, HTTP only — nothing exposed publicly."
      ;;
    quick)
      info "Instant ${BOLD}sslip.io${RESET} domain, HTTP only — no domain or DNS configuration needed."
      ;;
    testing)
      info "Instant ${BOLD}sslip.io${RESET} domain with a real HTTPS certificate for the console."
      ;;
  esac
  echo ""

  if ! prompt_yesno "Ready to begin?" "y"; then
    echo ""
    info "Cancelled. Run this wizard again when you're ready."
    echo ""
    exit 0
  fi

  # --- Steps 1 & 2: Docker + database (shared with advanced mode) ---
  step_docker
  step_timescaledb

  # --- Step 3: Binary + sslip.io domain ---
  step_header 3 $TOTAL_STEPS "Temps Binary & Domain"

  # Install the binary on the chosen channel (idempotent)
  if ! check_command temps && [[ ! -f $HOME_DIR/.temps/bin/temps ]]; then
    info "Installing Temps binary..."
    echo ""
    install_temps_binary
    echo ""
    export PATH="$HOME_DIR/.temps/bin:$PATH"
    $SUDO ln -sf $HOME_DIR/.temps/bin/temps /usr/local/bin/temps 2>/dev/null || true
  else
    success "Temps binary already installed"
  fi

  local temps_bin="$HOME_DIR/.temps/bin/temps"
  [[ ! -f "$temps_bin" ]] && temps_bin="$(command -v temps)"

  # Resolve the sslip.io domain. Local mode pins the loopback address so the
  # whole platform stays on this machine; other modes use the public IP.
  if [[ "$SETUP_MODE" == "local" ]]; then
    SERVER_IP="127.0.0.1"
    # Linux systemd grants CAP_NET_BIND_SERVICE so :80 works even unprivileged;
    # macOS launchd user agents cannot bind privileged ports, so use :8080.
    if [[ "$IS_MACOS" == "true" ]]; then
      LOCAL_PORT=8080
    else
      LOCAL_PORT=80
    fi
  else
    info "Detecting server IP..."
    SERVER_IP=$(detect_public_ip_quick)
    LOCAL_PORT=80
  fi
  SSLIP_DOMAIN="${SERVER_IP}.sslip.io"

  local port_suffix=""
  [[ "$LOCAL_PORT" != "80" ]] && port_suffix=":${LOCAL_PORT}"

  success "Using domain: ${BOLD}*.${SSLIP_DOMAIN}${RESET}"
  info "Console will be at ${BOLD}console.${SSLIP_DOMAIN}${port_suffix}${RESET}"
  state_set domain "$SSLIP_DOMAIN"
  state_set server_ip "$SERVER_IP"
  state_set local_port "$LOCAL_PORT"

  # --- Step 4: temps setup --auto ---
  step_header 4 $TOTAL_STEPS "Temps Platform Setup"

  local db_url="postgresql://temps:${DB_PASSWORD}@localhost:${DB_PORT}/temps?sslmode=disable"
  mkdir -p "$HOME_DIR/.temps/data"

  ADMIN_EMAIL=$(state_get admin_email)
  ADMIN_PASSWORD=$(state_get admin_password)

  if [[ -f $HOME_DIR/.temps/data/encryption_key ]] && [[ -n "$ADMIN_PASSWORD" ]]; then
    success "Temps platform already configured"
    [[ -n "$ADMIN_EMAIL" ]] && info "Admin: ${BOLD}$ADMIN_EMAIL${RESET}"
  else
    [[ -z "$ADMIN_EMAIL" ]] && ADMIN_EMAIL="admin@${SSLIP_DOMAIN}"
    ADMIN_PASSWORD=$(generate_admin_password)

    info "Running ${BOLD}temps setup --auto${RESET}..."
    echo ""
    local setup_exit=0
    # The external URL carries the proxy port when it isn't 80 (local mode on
    # macOS uses :8080), so generated app/console links are correct.
    local external_url="http://${SSLIP_DOMAIN}${port_suffix}"
    # Pass the domain/IP explicitly so deploy.sh stays the source of truth
    # (it already detected the IP for the reachability probe + completion
    # screen). --auto still forces skip-ssl/skip-dns/skip-git and generates an
    # HTTP-only config; we supply --external-url because providing an explicit
    # --wildcard-domain bypasses --auto's own external-URL defaulting.
    "$temps_bin" setup \
      --auto \
      --database-url "$db_url" \
      --admin-email "$ADMIN_EMAIL" \
      --admin-password "$ADMIN_PASSWORD" \
      --wildcard-domain "*.${SSLIP_DOMAIN}" \
      --server-ip "$SERVER_IP" \
      --external-url "$external_url" \
      --data-dir "$HOME_DIR/.temps/data" \
      2>&1 | while IFS= read -r line; do
        printf "  ${DIM}  %s${RESET}\n" "$line"
      done || setup_exit=$?

    if [[ $setup_exit -ne 0 ]]; then
      echo ""
      warn "Temps setup failed (exit code $setup_exit)."
      info "Check database: ${BOLD}docker ps | grep timescale${RESET}"
      fatal "Setup cannot continue"
    fi

    echo ""
    success "Temps setup completed"
    state_set admin_email "$ADMIN_EMAIL"
    state_set admin_password "$ADMIN_PASSWORD"
  fi

  # Sync encryption key to where `temps serve` looks for it
  if [[ -f $HOME_DIR/.temps/data/encryption_key ]]; then
    cp $HOME_DIR/.temps/data/encryption_key $HOME_DIR/.temps/encryption_key 2>/dev/null || true
  fi

  # `temps setup` downloads GeoLite2 into the data dir, but `temps serve`'s geo
  # plugin opens GeoLite2-City.mmdb relative to its working directory
  # ($HOME_DIR/.temps). Bridge that gap or serve crash-loops with
  # "Failed to open MaxMind database".
  ensure_geolite2

  # --- Step 5: Background service (HTTP-first) ---
  step_header 5 $TOTAL_STEPS "Background Service"

  write_quick_service_unit "false"
  service_enable temps
  if ! service_start temps; then
    warn "Service failed to start. Retrying..."
    sleep 3
    service_restart temps
  fi

  info "Waiting for Temps to start..."
  local healthy=false
  for i in $(seq 1 20); do
    progress_bar "$i" 20 "checking health"
    if curl -sf "http://localhost:8081/health" >/dev/null 2>&1; then
      healthy=true
      break
    fi
    sleep 3
  done
  progress_bar 20 20 "checking health"
  echo ""
  if [[ "$healthy" == "true" ]]; then
    success "Temps service is running and healthy"
  else
    warn "Temps started but health check not yet passing — it may need a few more seconds."
    info "Check logs: ${BOLD}$(service_logs_cmd temps)${RESET}"
  fi

  # --- Testing mode only: provision a real cert for the console host ---
  # (Local mode is loopback-only and Quick mode is HTTP by design, so neither
  # provisions a certificate — only Testing does.)
  local console_scheme="http"
  if [[ "$SETUP_MODE" == "testing" ]]; then
    quick_provision_console_cert "$temps_bin" "$db_url" && console_scheme="https"
  fi

  local console_url="${console_scheme}://console.${SSLIP_DOMAIN}${port_suffix}"

  # Verify the console responds before declaring success.
  echo ""
  info "Verifying the console responds at ${BOLD}${console_url}${RESET}..."
  local verified=false
  for i in $(seq 1 10); do
    progress_bar "$i" 10 "waiting for response"
    local code
    code=$(curl -sk -o /dev/null -w "%{http_code}" "$console_url" 2>/dev/null || echo "000")
    if [[ "$code" =~ ^[2-3][0-9][0-9]$ ]]; then
      verified=true
      break
    fi
    sleep 3
  done
  progress_bar 10 10 "done"
  echo ""
  if [[ "$verified" == "true" ]]; then
    success "Console is live"
  else
    warn "Console did not respond yet — it may need a few more seconds."
  fi

  # The full end-to-end test deploy (step_test_deploy) assumes an HTTPS custom
  # domain, so it only runs in advanced mode. Local/quick/testing rely on the
  # health check above plus the next-steps guidance below.
  show_quick_completion "$console_scheme" "$port_suffix"
}

# Provision a real Let's Encrypt certificate for console.<ip>.sslip.io via the
# HTTP-01 challenge. Requires port 80 to be reachable from the internet. On
# success, restarts the service with TLS enabled. Returns 0 if HTTPS is live.
quick_provision_console_cert() {
  local temps_bin="$1" db_url="$2"
  local console_host="console.${SSLIP_DOMAIN}"

  echo ""
  hr
  echo ""
  printf "  ${BG_BLUE}${BOLD}${WHITE} HTTPS ${RESET}  ${BOLD}Provisioning a certificate for the console${RESET}\n"
  echo ""

  # HTTP-01 needs port 80 reachable from Let's Encrypt's validation servers.
  info "Checking if ${BOLD}${SERVER_IP}:80${RESET} is reachable from the internet..."
  if ! probe_port_80 "$SERVER_IP"; then
    warn "Port 80 is not publicly reachable — cannot complete the HTTP-01 challenge."
    info "The console stays on HTTP for now. Once ports 80/443 are open, run:"
    info "  ${BOLD}temps domain provision -d ${console_host}${RESET}"
    echo ""
    return 1
  fi
  success "Port 80 is reachable"

  # Mint a short-lived admin API key for the provision call, then revoke it.
  info "Generating a temporary API key..."
  local apikey_output api_key=""
  apikey_output=$("$temps_bin" api-key \
    --database-url "$db_url" \
    --name "quick-provision" \
    --role admin \
    --user-email "$ADMIN_EMAIL" \
    --output-format json \
    2>/dev/null) || true
  api_key=$(json_value "$apikey_output" "api_key")
  [[ -z "$api_key" ]] && api_key=$(json_value "$apikey_output" "key")
  [[ -z "$api_key" ]] && api_key=$(json_value "$apikey_output" "token")

  if [[ -z "$api_key" ]]; then
    warn "Could not mint an API key — skipping certificate provisioning."
    info "Provision manually later: ${BOLD}temps domain provision -d ${console_host}${RESET}"
    return 1
  fi

  local api_url="http://localhost:8081/api"

  # Register the console host with an HTTP-01 challenge type. The cert isn't
  # issued here — `domain add` only creates the record (and is a no-op / harmless
  # error if it already exists from a previous run), so treat failure as
  # non-fatal and let `domain provision` below drive the actual issuance.
  info "Registering ${BOLD}${console_host}${RESET}..."
  "$temps_bin" domain add \
    -d "$console_host" \
    -c http-01 \
    --api-url "$api_url" \
    --api-token "$api_key" \
    >/dev/null 2>&1 || true

  info "Requesting a Let's Encrypt certificate for ${BOLD}${console_host}${RESET}..."
  echo ""
  local provision_exit=0
  "$temps_bin" domain provision \
    -d "$console_host" \
    --api-url "$api_url" \
    --api-token "$api_key" \
    2>&1 | while IFS= read -r line; do
      printf "  ${DIM}  %s${RESET}\n" "$line"
    done || provision_exit=$?

  if [[ $provision_exit -ne 0 ]]; then
    echo ""
    warn "Certificate provisioning did not complete. The console stays on HTTP."
    info "Retry later: ${BOLD}temps domain provision -d ${console_host}${RESET}"
    return 1
  fi

  echo ""
  success "Certificate provisioned for ${BOLD}${console_host}${RESET}"

  # Restart with TLS enabled so :443 serves the new cert.
  info "Restarting Temps with HTTPS enabled..."
  write_quick_service_unit "true"
  service_enable temps
  service_restart temps
  sleep 3
  return 0
}

# Mode-aware completion screen for the local/quick/testing flow.
# Usage: show_quick_completion <console_scheme: http|https> [port_suffix e.g. ":8080"]
show_quick_completion() {
  local scheme="${1:-http}"
  local port_suffix="${2:-}"
  local console_url="${scheme}://console.${SSLIP_DOMAIN}${port_suffix}"
  local apps_url="http://<project>.${SSLIP_DOMAIN}${port_suffix}"
  local is_local=false
  [[ "$SETUP_MODE" == "local" ]] && is_local=true

  echo ""
  hr
  echo ""
  printf "  ${BG_GREEN}${BOLD}${WHITE} SETUP COMPLETE ${RESET}\n"
  echo ""
  printf "  ${BOLD}Your Temps instance is ready!${RESET}\n"
  echo ""
  summary_row "Console:" "$console_url"
  summary_row "Apps at:" "$apps_url"
  summary_row "Admin email:" "$ADMIN_EMAIL"
  summary_row "Admin password:" "$ADMIN_PASSWORD"
  summary_row "Domain:" "*.${SSLIP_DOMAIN}"
  summary_row "Channel:" "$CHANNEL"
  echo ""
  warn "Save your admin password now — it won't be shown again."
  info "${DIM}It's also stored at $STATE_DIR/admin_password${RESET}"
  echo ""

  hr
  echo ""
  printf "  ${BOLD}About this setup${RESET}\n"
  echo ""
  if [[ "$is_local" == "true" ]]; then
    info "Temps is running on ${BOLD}this machine${RESET} via ${BOLD}127.0.0.1.sslip.io${RESET},"
    info "which resolves the console and every app subdomain to ${BOLD}127.0.0.1${RESET}."
    info "Nothing is exposed to the internet — this is for local dogfooding."
  else
    info "You're using a free ${BOLD}sslip.io${RESET} domain that maps ${BOLD}*.${SSLIP_DOMAIN}${RESET}"
    info "to this server's IP (${BOLD}${SERVER_IP}${RESET}) — no DNS configuration required."
    if [[ "$scheme" == "https" ]]; then
      echo ""
      info "The console has a real Let's Encrypt certificate. Deployed apps are"
      info "served over HTTP, because wildcard sslip.io domains can't be issued a"
      info "public certificate."
    else
      echo ""
      info "Everything is served over ${BOLD}HTTP${RESET} for now. That's fine for trying"
      info "Temps out, but use a real domain before exposing anything publicly."
    fi
  fi
  echo ""

  if [[ "$is_local" == "true" ]]; then
    hr
    echo ""
    printf "  ${BOLD}Going beyond local${RESET}\n"
    echo ""
    info "When you're ready to host this for real, re-run the installer on a"
    info "server with a public IP:"
    info "     ${GREEN}curl -fsSL https://temps.sh/deploy.sh | bash -s -- --mode quick${RESET}     ${DIM}# instant sslip.io${RESET}"
    info "     ${GREEN}curl -fsSL https://temps.sh/deploy.sh | bash -s -- --mode advanced${RESET}  ${DIM}# your own domain${RESET}"
    echo ""
  else
    hr
    echo ""
    printf "  ${BOLD}Add a real domain when you're ready${RESET}\n"
    echo ""
    info "1. Point your domain's DNS at this server (${BOLD}${SERVER_IP}${RESET}):"
    info "     ${DIM}A     yourdomain.com        → ${SERVER_IP}${RESET}"
    info "     ${DIM}A     *.yourdomain.com      → ${SERVER_IP}${RESET}"
    info "2. Add it to Temps and provision a certificate:"
    info "     ${GREEN}temps domain provision -d yourdomain.com${RESET}   ${DIM}# HTTP-01, single host${RESET}"
    info "   ${DIM}or, for a wildcard cert, re-run this installer in Advanced mode:${RESET}"
    info "     ${GREEN}curl -fsSL https://temps.sh/deploy.sh | bash -s -- --mode advanced${RESET}"
    info "3. In the console, point a project at your domain under ${BOLD}Project → Domains${RESET}."
    echo ""
  fi

  hr
  echo ""
  printf "  ${BOLD}Next steps${RESET}\n"
  echo ""
  info "1. Open your console at ${GREEN}${UNDERLINE}${console_url}${RESET}"
  info "2. Log in with ${BOLD}$ADMIN_EMAIL${RESET} and your password"
  info "3. Deploy your first app with ${GREEN}temps deploy${RESET}"
  echo ""

  hr
  echo ""
  printf "  ${BOLD}Useful commands${RESET}\n"
  echo ""
  info "${GREEN}$(service_status_cmd temps)${RESET}   ${DIM}Service status${RESET}"
  info "${GREEN}$(service_logs_cmd temps)${RESET}   ${DIM}Live logs${RESET}"
  info "${GREEN}temps --help${RESET}   ${DIM}CLI reference${RESET}"
  echo ""

  hr
  echo ""
  info "Documentation: ${UNDERLINE}https://temps.sh/docs${RESET}"
  info "Support:       ${UNDERLINE}https://github.com/gotempsh/temps/issues${RESET}"
  echo ""
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

check_prerequisites() {
  local missing=()

  check_command curl    || missing+=("curl")
  check_command openssl || missing+=("openssl")

  if [[ ${#missing[@]} -eq 0 ]]; then
    return 0
  fi

  warn "Missing required tools: ${BOLD}${missing[*]}${RESET}"

  if check_command brew; then
    info "Installing via Homebrew..."
    brew install "${missing[@]}" > /dev/null 2>&1 || true
  elif check_command apt-get; then
    info "Installing via apt..."
    $SUDO apt-get update -qq > /dev/null 2>&1 || true
    $SUDO apt-get install -y -qq "${missing[@]}" > /dev/null 2>&1 || true
  elif check_command yum; then
    info "Installing via yum..."
    $SUDO yum install -y -q "${missing[@]}" > /dev/null 2>&1 || true
  elif check_command apk; then
    info "Installing via apk..."
    $SUDO apk add --quiet "${missing[@]}" > /dev/null 2>&1 || true
  fi

  # Re-check after install attempt
  local still_missing=()
  for cmd in "${missing[@]}"; do
    check_command "$cmd" || still_missing+=("$cmd")
  done

  if [[ ${#still_missing[@]} -gt 0 ]]; then
    fatal "Could not install: ${BOLD}${still_missing[*]}${RESET}. Install them manually and re-run."
  fi

  success "Installed missing dependencies: ${BOLD}${missing[*]}${RESET}"
}

# Parse CLI flags: --mode <local|quick|testing|advanced> and --channel <stable|beta>.
# Both also accept the --flag=value form. Unknown flags are rejected so typos
# don't silently fall through to the interactive flow.
parse_flags() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --mode)       SETUP_MODE="${2:-}"; shift 2 || fatal "--mode requires a value (local, quick, testing, or advanced)" ;;
      --mode=*)     SETUP_MODE="${1#*=}"; shift ;;
      --channel)    CHANNEL="${2:-}"; shift 2 || fatal "--channel requires a value (stable or beta)" ;;
      --channel=*)  CHANNEL="${1#*=}"; shift ;;
      --no-telemetry) TELEMETRY_OPTOUT="true"; shift ;;
      -h|--help)
        echo "Usage: deploy.sh [--mode local|quick|testing|advanced] [--channel stable|beta] [--no-telemetry]"
        echo ""
        echo "  --no-telemetry   Disable anonymous product telemetry (opt-out)."
        echo "                   Telemetry is anonymous and on by default; this sets"
        echo "                   TEMPS_TELEMETRY=0 in the service."
        exit 0
        ;;
      *) fatal "Unknown argument: $1" ;;
    esac
  done

  case "$CHANNEL" in
    stable|beta) ;;
    *) fatal "Invalid --channel '$CHANNEL'. Valid channels: stable, beta" ;;
  esac

  if [[ -n "$SETUP_MODE" ]]; then
    case "$SETUP_MODE" in
      local|quick|testing|advanced) ;;
      *) fatal "Invalid --mode '$SETUP_MODE'. Valid modes: local, quick, testing, advanced" ;;
    esac
  fi
}

# Interactive mode picker — shown only when --mode was not supplied.
choose_mode() {
  [[ -n "$SETUP_MODE" ]] && return 0

  local choice=""
  prompt_choice choice "How would you like to set up Temps?" \
    "Local       — run on this machine via 127.0.0.1.sslip.io, HTTP only (try it out)" \
    "QuickStart  — server with a public IP, instant sslip.io domain, HTTP only (~90s)" \
    "Testing     — sslip.io domain with a real HTTPS cert for the console (HTTP-01)" \
    "Advanced    — your own domain + wildcard Let's Encrypt cert (manual DNS)"

  case "$choice" in
    1) SETUP_MODE="local" ;;
    2) SETUP_MODE="quick" ;;
    3) SETUP_MODE="testing" ;;
    4) SETUP_MODE="advanced" ;;
    *) fatal "Invalid choice" ;;
  esac
}

main() {
  parse_flags "$@"

  banner
  require_root
  check_prerequisites

  # Ensure state dir exists
  mkdir -p "$STATE_DIR"

  if [[ "$CHANNEL" == "beta" ]]; then
    warn "Installing the ${BOLD}beta${RESET}${YELLOW} channel — prereleases may be unstable.${RESET}"
    echo ""
  fi

  choose_mode

  # Local, QuickStart, and Testing modes share a dedicated sslip.io-based flow.
  if [[ "$SETUP_MODE" == "local" || "$SETUP_MODE" == "quick" || "$SETUP_MODE" == "testing" ]]; then
    run_quick_flow
    return
  fi

  # --- Advanced mode: your own domain + wildcard Let's Encrypt cert ---------

  info "This wizard will set up a complete Temps deployment platform"
  info "on this server. It takes about 2-5 minutes."
  echo ""

  if ! prompt_yesno "Ready to begin?" "y"; then
    echo ""
    info "Cancelled. Run this wizard again when you're ready."
    echo ""
    exit 0
  fi

  step_docker
  step_timescaledb
  step_domain_ssl
  step_temps_setup
  step_service
  step_verify
  show_completion
  step_test_deploy
}

main "$@"

}
