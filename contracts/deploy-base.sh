#!/usr/bin/env bash
# Deploy FeeVault + RootmodePot on Base. Writes contracts/deployments/base.json
# and apps/desktop/src-tauri/chain.base.json (baked into the next desktop build).
#
#   BASE_RPC_URL=https://mainnet.base.org PRIVATE_KEY=0x... ./contracts/deploy-base.sh
#
# Redeploying the pot only: keep the treasury by naming the existing vault.
#   FEE_VAULT=0x17De... PRIVATE_KEY=0x... ./contracts/deploy-base.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/contracts"

: "${PRIVATE_KEY:?set PRIVATE_KEY to the deployer (needs a little ETH on Base)}"
RPC="${BASE_RPC_URL:-https://mainnet.base.org}"

if ! command -v vyper >/dev/null 2>&1; then
    echo "vyper is not on PATH. pipx install vyper==0.4.3" >&2
    exit 1
fi

echo "deploying to Base via $RPC"
if [ -n "${FEE_VAULT:-}" ]; then echo "reusing fee vault $FEE_VAULT"; fi
OUT="$(forge script script/DeployBase.s.sol:DeployBase --broadcast --rpc-url "$RPC" --private-key "$PRIVATE_KEY" -vv)"
echo "$OUT"

grab() { echo "$OUT" | awk -v k="$1" '$1==k {print $2; exit}'; }
USDC="$(grab USDC)"
VAULT="$(grab FEE_VAULT)"
POT="$(grab POT)"
if [ -z "$POT" ] || [ -z "$VAULT" ]; then
    echo "deploy did not print POT / FEE_VAULT" >&2
    exit 1
fi

mkdir -p deployments
JSON=$(cat <<EOF
{
  "rpc": "$RPC",
  "chainId": 8453,
  "usdc": "$USDC",
  "pot": "$POT",
  "feeVault": "$VAULT",
  "worker": "0x0000000000000000000000000000000000000000",
  "client": ""
}
EOF
)
# The block the pot went live at bounds every later scan of its events.
BLOCK="$(cast block-number --rpc-url "$RPC")"
JSON="$(printf '%s' "$JSON" | sed "s/\"client\": \"\"/\"client\": \"\",\n  \"deployBlock\": $BLOCK/")"
printf '%s\n' "$JSON" > deployments/base.json
printf '%s\n' "$JSON" > "$ROOT/apps/desktop/src-tauri/chain.base.json"

echo
echo "Base contracts are live."
echo "  USDC      $USDC"
echo "  pot       $POT"
echo "  fee vault $VAULT"
echo
echo "Commit chain.base.json and cut a new desktop tag so clients deposit on Base."
