#!/bin/bash
set -euo pipefail

# Deploy xctl to production servers.
#
# Bot (axadmin on LT egress):
#   cargo zigbuild → scp binary to egress → docker build ON egress → push ghcr.io from egress
#   → scp compose → compose pull/up
#
# Bridge agent (RU bridge):
#   cargo zigbuild → scp binary via /tmp → mv + systemctl restart
#
# NOTE: Docker build/push happens on the egress server, NOT locally.
# Local Docker daemon is NOT required. See specs/008-operations.md.
#
# Usage:
#   ./scripts/deploy.sh                 # bot + bridge agent
#   ./scripts/deploy.sh --bot-only      # only axadmin on egress
#   ./scripts/deploy.sh --agent-only    # only bridge-agent on RU bridge
#
# Ref: specs/000-constitution.md §III, specs/008-operations.md

EGRESS="root@94.131.13.243"
BRIDGE="root@51.250.73.78"
IMAGE="ghcr.io/petrochen/xctl:latest"
TARGET="x86_64-unknown-linux-musl"
BINARY="target/${TARGET}/release/xctl"
COMPOSE_SRC="deploy/docker-compose.yml"
COMPOSE_DST="/opt/axadmin"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()  { echo -e "${GREEN}[+]${NC} $*"; }
warn()  { echo -e "${YELLOW}[!]${NC} $*"; }
error() { echo -e "${RED}[x]${NC} $*"; exit 1; }

DEPLOY_BOT=true
DEPLOY_AGENT=true
case "${1:-all}" in
    --bot-only)   DEPLOY_AGENT=false ;;
    --agent-only) DEPLOY_BOT=false ;;
    all|"")       ;;
    *) echo "Usage: $0 [--bot-only | --agent-only]"; exit 1 ;;
esac

cd "$(dirname "$0")/.."

# ── Step 1: Build ────────────────────────────────────────────────────────────
info "Building musl binary..."
source ~/.cargo/env 2>/dev/null || true
cargo zigbuild --release --target "$TARGET"
[ -f "$BINARY" ] || error "Binary not found: $BINARY"
info "Built: $(du -h "$BINARY" | cut -f1)"

# ── Step 2: Bot → build on egress → registry → compose ──────────────────────
if [ "$DEPLOY_BOT" = true ]; then
    [ -f "$COMPOSE_SRC" ] || error "Compose file not found: $COMPOSE_SRC"

    info "Uploading binary to egress..."
    scp "$BINARY" "${EGRESS}:/tmp/axa"

    info "Building Docker image on egress and pushing to registry..."
    GH_TOKEN=$(gh auth token)
    ssh "$EGRESS" "
        mkdir -p /tmp/docker-build && cp /tmp/axa /tmp/docker-build/xctl
        cat > /tmp/docker-build/Dockerfile << 'EOF'
FROM alpine:3.21
RUN apk add --no-cache docker-cli
COPY xctl /usr/local/bin/xctl
ENTRYPOINT [\"xctl\"]
CMD [\"--telegram-bot\", \"--local\", \"--container\", \"amnezia-xray\"]
EOF
        cd /tmp/docker-build && docker build -t ${IMAGE} .
        echo '${GH_TOKEN}' | docker login ghcr.io -u petrochen --password-stdin
        docker push ${IMAGE}
        docker logout ghcr.io && rm -rf /tmp/docker-build /tmp/axa
    "

    info "Syncing compose config to server..."
    ssh "$EGRESS" "mkdir -p $COMPOSE_DST"
    scp "$COMPOSE_SRC" "${EGRESS}:${COMPOSE_DST}/docker-compose.yml"

    info "Deploying on egress..."
    GH_TOKEN=$(gh auth token)
    ssh "$EGRESS" "
        echo '${GH_TOKEN}' | docker login ghcr.io -u petrochen --password-stdin
        cd $COMPOSE_DST && docker compose pull && docker compose up -d
        docker logout ghcr.io
    "

    info "Verifying bot..."
    sleep 4
    BOT_VER=$(ssh "$EGRESS" "docker exec axadmin xctl --version 2>&1")
    echo "$BOT_VER" | grep -q "xctl" || error "Verification failed: $BOT_VER"
    BOT_STATE=$(ssh "$EGRESS" 'docker inspect axadmin --format "{{.State.Status}}"')
    info "Bot: $BOT_VER | state: $BOT_STATE"
fi

# ── Step 3: Bridge agent → scp via tmp → mv → restart ───────────────────────
if [ "$DEPLOY_AGENT" = true ]; then
    info "Deploying bridge agent to RU bridge..."
    # scp to /tmp first — direct write to /usr/local/bin/xctl fails while process is running
    scp "$BINARY" "${BRIDGE}:/tmp/xctl_new"
    ssh "$BRIDGE" "mv /tmp/xctl_new /usr/local/bin/xctl && chmod +x /usr/local/bin/xctl && systemctl restart bridge-agent"

    sleep 2
    AGENT=$(ssh "$BRIDGE" "systemctl is-active bridge-agent 2>&1")
    [ "$AGENT" = "active" ] || error "Bridge agent failed: $AGENT"
    info "Bridge agent: active"
fi

# ── Step 4: Summary ──────────────────────────────────────────────────────────
echo ""
info "Post-deploy status:"
[ "$DEPLOY_BOT" = true ] && \
    ssh "$EGRESS" 'docker ps --format "  {{.Names}}: {{.Status}}" \
        --filter name=axadmin --filter name=amnezia-xray'
[ "$DEPLOY_AGENT" = true ] && \
    echo "  bridge-agent: $(ssh "$BRIDGE" 'systemctl is-active bridge-agent')"
echo ""
info "Done. Verify end-to-end: /status in Telegram."
