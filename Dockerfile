# Minimal runtime — static musl binary + Docker CLI for xray container management
FROM alpine:3.21

LABEL org.opencontainers.image.source="https://github.com/petrochen/xctl"
LABEL org.opencontainers.image.description="Telegram bot for Xray VPN user management"

# docker-cli: for `docker exec/cp/restart` against xray container
# curl + jq: exec_on_host commands run inside this container (upgrade: version check, download)
# python3: zip extraction during xray upgrade
RUN apk add --no-cache docker-cli curl jq python3

COPY xctl /usr/local/bin/xctl

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD xctl --version || exit 1

ENTRYPOINT ["xctl"]
CMD ["--telegram-bot", "--local", "--container", "amnezia-xray"]
