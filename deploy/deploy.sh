#!/usr/bin/env bash
# Put rootmode.ai on this box: the site, the collector, and TLS.
#
#   ./deploy.sh                     # first run, or to update
#   ROOTMODE_DOMAIN=example.com ./deploy.sh
#
# Safe to run again: it rebuilds, restarts, and leaves the database and the
# certificates where they are.
set -euo pipefail

cd "$(dirname "$0")"
ENV_FILE=.env

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
ok()  { printf '  ✓ %s\n' "$*"; }
bad() { printf '  ✗ %s\n' "$*" >&2; }

command -v docker >/dev/null || { bad "docker is not installed"; exit 1; }
docker compose version >/dev/null 2>&1 || { bad "docker compose v2 is not available"; exit 1; }

# ---------------------------------------------------------------- settings
#
# Written once and reused, so a second run does not generate a new salt — that
# would make every worker look like a new machine to the collector.
if [ ! -f "$ENV_FILE" ]; then
    say "First run — writing $ENV_FILE"
    read -rp "  Domain [rootmode.ai]: " domain
    read -rp "  Email for certificate notices (optional): " email
    cat > "$ENV_FILE" <<EOF
ROOTMODE_DOMAIN=${domain:-rootmode.ai}
ACME_EMAIL=${email:-}
# Lets the collector recognise repeat reports from one machine without keeping
# its address. Rotate this line to forget everything it has learned.
ROOTMODE_IP_SALT=$(openssl rand -hex 16 2>/dev/null || head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
EOF
    ok "wrote $ENV_FILE"
fi
# shellcheck disable=SC1090
set -a; . "./$ENV_FILE"; set +a
# Caddy's `email` directive needs an address. An empty ACME_EMAIL would
# leave `email` on a line by itself and Caddy would refuse to start.
if [ -n "${ACME_EMAIL:-}" ]; then
    CADDY_EMAIL="email ${ACME_EMAIL}"
else
    CADDY_EMAIL=""
fi
export CADDY_EMAIL

# ---------------------------------------------------------------- preflight
#
# The two things that actually stop a certificate being issued, checked before
# Caddy burns an ACME attempt on them.
say "Checking $ROOTMODE_DOMAIN before asking for a certificate"

here=$(curl -s -m 10 https://api.ipify.org 2>/dev/null || true)
there=$(getent hosts "$ROOTMODE_DOMAIN" 2>/dev/null | awk '{print $1}' | head -1 || true)
if [ -z "$there" ]; then
    bad "$ROOTMODE_DOMAIN does not resolve — point an A record at this box first"
    exit 1
elif [ -n "$here" ] && [ "$here" != "$there" ]; then
    bad "$ROOTMODE_DOMAIN resolves to $there, but this box is $here"
    bad "Let's Encrypt reaches the address in DNS, so the certificate would fail"
    exit 1
else
    ok "$ROOTMODE_DOMAIN -> $there"
fi

for port in 80 443; do
    if ss -tln 2>/dev/null | grep -q ":$port "; then
        bad "port $port is already in use — Caddy needs both (80 is how the certificate is issued)"
        ss -tlnp 2>/dev/null | grep ":$port " | head -2 >&2
        exit 1
    fi
done
ok "ports 80 and 443 are free"

# ---------------------------------------------------------------- go
say "Building and starting"
docker compose up -d --build

say "Waiting for the certificate"
# First request for a hostname triggers the ACME exchange; it is usually a few
# seconds, and slow DNS is the reason it sometimes is not.
for i in $(seq 1 30); do
    code=$(curl -s -o /dev/null -w '%{http_code}' -m 10 "https://$ROOTMODE_DOMAIN/healthz" 2>/dev/null || true)
    if [ "$code" = "200" ]; then
        ok "https://$ROOTMODE_DOMAIN is up with a valid certificate"
        break
    fi
    [ "$i" = 30 ] && {
        bad "no answer over https yet. What Caddy is doing:"
        docker compose logs --tail 20 caddy >&2
        exit 1
    }
    sleep 3
done

say "Live"
cat <<EOF
  site       https://$ROOTMODE_DOMAIN/
  explorer   https://$ROOTMODE_DOMAIN/explorer
  workers    POST https://$ROOTMODE_DOMAIN/report   (the default every worker ships with)
  feed       https://$ROOTMODE_DOMAIN/stats.json

  Back up the database — nothing can re-derive it:
    docker run --rm -v rootmode_stats-data:/d -v "\$PWD":/out alpine \\
      tar czf /out/stats-backup.tgz -C /d .
EOF
