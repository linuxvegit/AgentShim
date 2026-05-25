#!/usr/bin/env bash
#
# Install agent-shim as a Linux systemd service.
#
# Typical usage:
#   ./scripts/install-linux-service.sh --start
#   ./scripts/install-linux-service.sh --config ./my-gateway.yaml --start

set -euo pipefail

SERVICE_NAME=agent-shim
SERVICE_USER=agent-shim
SERVICE_GROUP=agent-shim
INSTALL_DIR=/usr/local/bin
CONFIG_DIR=/etc/agent-shim
LOG_DIR=/var/log/agent-shim
CONFIG_SOURCE=
ENV_FILE_SOURCE=
BUILD_IF_MISSING=1
FORCE_CONFIG=0
START_SERVICE=0
ENABLE_SERVICE=1
VALIDATE_CONFIG=1
ORIGINAL_PWD=$(pwd)

usage() {
    cat <<'USAGE'
Install agent-shim as a Linux systemd service.

Typical usage:
  ./scripts/install-linux-service.sh --start
  ./scripts/install-linux-service.sh --config ./my-gateway.yaml --start

Options:
  --service-name NAME    systemd unit name without ".service" (default: agent-shim)
  --user NAME            system user to run the service as (default: agent-shim)
  --group NAME           system group to run the service as (default: same as --user)
  --install-dir DIR      binary install directory (default: /usr/local/bin)
  --config-dir DIR       config directory (default: /etc/agent-shim)
  --log-dir DIR          writable log directory (default: /var/log/agent-shim)
  --config PATH          config to install (default: config/gateway.example.yaml when missing)
  --env-file PATH        optional EnvironmentFile to install as <config-dir>/<service-name>.env
  --force-config         overwrite an existing gateway.yaml
  --no-build             require target/release/agent-shim to already exist
  --no-enable            install but do not enable at boot
  --no-validate          skip "agent-shim validate-config" before installing the unit
  --start                enable and start/restart the service after installation
  -h, --help             show this help
USAGE
}

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
info() { printf '    \033[2m%s\033[0m\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

run_as_root() {
    if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
        "$@"
    elif have sudo; then
        sudo "$@"
    else
        die "sudo is required when not running as root."
    fi
}

run_as_service_user() {
    if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
        if have runuser; then
            runuser -u "$SERVICE_USER" -- "$@"
        elif have sudo; then
            sudo -u "$SERVICE_USER" "$@"
        else
            die "runuser or sudo is required to validate as service user '$SERVICE_USER'."
        fi
    elif have sudo; then
        sudo -u "$SERVICE_USER" "$@"
    else
        die "sudo is required to validate as service user '$SERVICE_USER'."
    fi
}

absolute_path() {
    local path=$1
    if [[ "$path" = /* ]]; then
        printf '%s\n' "$path"
    else
        printf '%s/%s\n' "$ORIGINAL_PWD" "$path"
    fi
}

validate_installed_config() {
    local validator_script
    validator_script=$(mktemp)

    {
        printf 'set -euo pipefail\n'
        if [[ -n "$ENV_FILE_TARGET" ]]; then
            printf 'set -a\n'
            printf '. %q\n' "$ENV_FILE_TARGET"
            printf 'set +a\n'
        fi
        printf 'exec %q validate-config --config %q\n' "$INSTALL_BINARY" "$TARGET_CONFIG"
    } > "$validator_script"

    chmod 0755 "$validator_script"
    if run_as_service_user "$validator_script"; then
        rm -f "$validator_script"
        return 0
    else
        local status=$?
        rm -f "$validator_script"
        return "$status"
    fi
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --service-name=*)
            SERVICE_NAME=${1#*=}
            shift
            ;;
        --service-name)
            SERVICE_NAME=${2:?missing value for --service-name}
            shift 2
            ;;
        --user=*)
            SERVICE_USER=${1#*=}
            if [[ "$SERVICE_GROUP" == agent-shim ]]; then
                SERVICE_GROUP=$SERVICE_USER
            fi
            shift
            ;;
        --user)
            SERVICE_USER=${2:?missing value for --user}
            if [[ "$SERVICE_GROUP" == agent-shim ]]; then
                SERVICE_GROUP=$SERVICE_USER
            fi
            shift 2
            ;;
        --group=*)
            SERVICE_GROUP=${1#*=}
            shift
            ;;
        --group)
            SERVICE_GROUP=${2:?missing value for --group}
            shift 2
            ;;
        --install-dir=*)
            INSTALL_DIR=${1#*=}
            shift
            ;;
        --install-dir)
            INSTALL_DIR=${2:?missing value for --install-dir}
            shift 2
            ;;
        --config-dir=*)
            CONFIG_DIR=${1#*=}
            shift
            ;;
        --config-dir)
            CONFIG_DIR=${2:?missing value for --config-dir}
            shift 2
            ;;
        --log-dir=*)
            LOG_DIR=${1#*=}
            shift
            ;;
        --log-dir)
            LOG_DIR=${2:?missing value for --log-dir}
            shift 2
            ;;
        --config=*)
            CONFIG_SOURCE=${1#*=}
            shift
            ;;
        --config)
            CONFIG_SOURCE=${2:?missing value for --config}
            shift 2
            ;;
        --env-file=*)
            ENV_FILE_SOURCE=${1#*=}
            shift
            ;;
        --env-file)
            ENV_FILE_SOURCE=${2:?missing value for --env-file}
            shift 2
            ;;
        --force-config)
            FORCE_CONFIG=1
            shift
            ;;
        --no-build)
            BUILD_IF_MISSING=0
            shift
            ;;
        --no-enable)
            ENABLE_SERVICE=0
            shift
            ;;
        --no-validate)
            VALIDATE_CONFIG=0
            shift
            ;;
        --start)
            START_SERVICE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

[[ "$SERVICE_NAME" =~ ^[A-Za-z0-9_.@-]+$ ]] || die "invalid --service-name: $SERVICE_NAME"
[[ "$SERVICE_USER" =~ ^[A-Za-z0-9_.@-]+$ ]] || die "invalid --user: $SERVICE_USER"
[[ "$SERVICE_GROUP" =~ ^[A-Za-z0-9_.@-]+$ ]] || die "invalid --group: $SERVICE_GROUP"
[[ "$INSTALL_DIR" = /* ]] || die "--install-dir must be absolute."
[[ "$CONFIG_DIR" = /* ]] || die "--config-dir must be absolute."
[[ "$LOG_DIR" = /* ]] || die "--log-dir must be absolute."

for path in "$INSTALL_DIR" "$CONFIG_DIR" "$LOG_DIR"; do
    [[ "$path" != *[[:space:]]* ]] || die "paths with whitespace are not supported: $path"
done

case "$(uname -s)" in
    Linux) ;;
    *) die "this installer only supports Linux/systemd." ;;
esac

have systemctl || die "systemctl not found. This installer requires systemd."

if [[ "$(ps -p 1 -o comm= 2>/dev/null || true)" != systemd ]]; then
    warn "PID 1 is not systemd. On WSL, enable systemd in /etc/wsl.conf before using systemctl."
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

SOURCE_BINARY="$REPO_ROOT/target/release/agent-shim"
INSTALL_BINARY="$INSTALL_DIR/agent-shim"
TARGET_CONFIG="$CONFIG_DIR/gateway.yaml"
UNIT_PATH="/etc/systemd/system/$SERVICE_NAME.service"
ENV_FILE_TARGET=

if [[ -z "$CONFIG_SOURCE" ]]; then
    CONFIG_SOURCE="$REPO_ROOT/config/gateway.example.yaml"
else
    CONFIG_SOURCE="$(absolute_path "$CONFIG_SOURCE")"
fi

if [[ -n "$ENV_FILE_SOURCE" ]]; then
    ENV_FILE_SOURCE="$(absolute_path "$ENV_FILE_SOURCE")"
    ENV_FILE_TARGET="$CONFIG_DIR/$SERVICE_NAME.env"
fi

step "Preparing release binary"
if [[ ! -x "$SOURCE_BINARY" ]]; then
    if [[ "${EUID:-$(id -u)}" -eq 0 && -n "${SUDO_USER:-}" ]]; then
        die "release binary is missing. Run scripts/setup-and-build.sh as your normal user first, then re-run this installer without sudo."
    fi
    if [[ "$BUILD_IF_MISSING" -eq 1 ]]; then
        info "release binary missing; running scripts/setup-and-build.sh --skip-nextest"
        "$REPO_ROOT/scripts/setup-and-build.sh" --skip-nextest
    else
        die "release binary not found at $SOURCE_BINARY. Run scripts/setup-and-build.sh first."
    fi
fi

[[ -x "$SOURCE_BINARY" ]] || die "release binary is not executable: $SOURCE_BINARY"
[[ -f "$CONFIG_SOURCE" ]] || die "config source not found: $CONFIG_SOURCE"
if [[ -n "$ENV_FILE_SOURCE" ]]; then
    [[ -f "$ENV_FILE_SOURCE" ]] || die "env file not found: $ENV_FILE_SOURCE"
fi

step "Creating service user"
if ! getent group "$SERVICE_GROUP" >/dev/null; then
    run_as_root groupadd --system "$SERVICE_GROUP"
fi

if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
    run_as_root useradd --system --no-create-home --shell /usr/sbin/nologin \
        --gid "$SERVICE_GROUP" "$SERVICE_USER"
fi

step "Installing files"
run_as_root install -d -m 0755 "$INSTALL_DIR"
run_as_root install -m 0755 "$SOURCE_BINARY" "$INSTALL_BINARY"

run_as_root install -d -m 0750 -o root -g "$SERVICE_GROUP" "$CONFIG_DIR"
if [[ -f "$TARGET_CONFIG" && "$FORCE_CONFIG" -eq 0 ]]; then
    info "keeping existing config: $TARGET_CONFIG"
else
    run_as_root install -m 0640 -o root -g "$SERVICE_GROUP" "$CONFIG_SOURCE" "$TARGET_CONFIG"
fi

if [[ -n "$ENV_FILE_TARGET" ]]; then
    run_as_root install -m 0640 -o root -g "$SERVICE_GROUP" "$ENV_FILE_SOURCE" "$ENV_FILE_TARGET"
fi

run_as_root install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$LOG_DIR"

if [[ "$VALIDATE_CONFIG" -eq 1 ]]; then
    step "Validating config"
    validate_installed_config
fi

step "Installing systemd unit"
tmp_unit=$(mktemp)
trap 'rm -f "$tmp_unit"' EXIT

{
    cat <<UNIT
[Unit]
Description=AgentShim Gateway - Protocol-translating API gateway for AI agents
Documentation=https://github.com/anthropics/agent-shim
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$SERVICE_USER
Group=$SERVICE_GROUP
UNIT

    if [[ -n "$ENV_FILE_TARGET" ]]; then
        printf 'EnvironmentFile=%s\n' "$ENV_FILE_TARGET"
    fi

    cat <<UNIT
ExecStart=$INSTALL_BINARY serve --config $TARGET_CONFIG
ExecReload=/bin/kill -HUP \$MAINPID
Restart=on-failure
RestartSec=5s

NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$LOG_DIR
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
LockPersonality=true
RestrictSUIDSGID=true
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT
} > "$tmp_unit"

run_as_root install -m 0644 "$tmp_unit" "$UNIT_PATH"
run_as_root systemctl daemon-reload

if [[ "$ENABLE_SERVICE" -eq 1 ]]; then
    step "Enabling service"
    run_as_root systemctl enable "$SERVICE_NAME"
fi

if [[ "$START_SERVICE" -eq 1 ]]; then
    step "Starting service"
    if run_as_root systemctl is-active --quiet "$SERVICE_NAME"; then
        run_as_root systemctl restart "$SERVICE_NAME"
    else
        run_as_root systemctl start "$SERVICE_NAME"
    fi
fi

step "Install complete"
info "unit:   $UNIT_PATH"
info "binary: $INSTALL_BINARY"
info "config: $TARGET_CONFIG"
info "logs:   journalctl -u $SERVICE_NAME -f"

if [[ "$START_SERVICE" -eq 0 ]]; then
    info "start:  sudo systemctl start $SERVICE_NAME"
fi
