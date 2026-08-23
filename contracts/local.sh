#!/usr/bin/env bash
# Local Base-shaped chain for the desktop app + MetaMask.
#
#   ./contracts/local.sh
#
# Then in MetaMask: add network Localhost 8545, chain id 31337, import the
# printed account-0 key. The desktop pot screen opens a browser page that
# talks to it.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/contracts"

RPC=http://127.0.0.1:8545
# Anvil account 0. This is a well-known test key — never use it on a real chain.
PK=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
CLIENT=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
WORKER=0x70997970C51812dc3A010C7d01b50e0d17dc79C8

if ! command -v vyper >/dev/null 2>&1; then
    echo "vyper is not on PATH. Install 0.4.3:  pipx install vyper==0.4.3" >&2
    exit 1
fi

if ! curl -s -m 1 -X POST -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' "$RPC" \
    | grep -q 0x7a69; then
    echo "starting anvil on $RPC (chain id 31337)"
    anvil --chain-id 31337 --block-time 1 --host 127.0.0.1 --port 8545 >/tmp/rootmode-anvil.log 2>&1 &
    echo $! >/tmp/rootmode-anvil.pid
    for _ in $(seq 1 30); do
        curl -s -m 1 -X POST -H 'content-type: application/json' \
            --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' "$RPC" \
            | grep -q 0x7a69 && break
        sleep 0.2
    done
else
    echo "anvil already running"
fi

echo "deploying"
OUT="$(forge script script/DeployLocal.s.sol:DeployLocal --broadcast --rpc-url "$RPC" --private-key "$PK" -vv)"
echo "$OUT"

grab() { echo "$OUT" | awk -v k="$1" '$1==k {print $2; exit}'; }
USDC="$(grab USDC)"
VAULT="$(grab FEE_VAULT)"
POT="$(grab POT)"

mkdir -p deployments "$HOME/.rootmode"
JSON=$(cat <<EOF
{
  "rpc": "$RPC",
  "chainId": 31337,
  "usdc": "$USDC",
  "pot": "$POT",
  "feeVault": "$VAULT",
  "worker": "$WORKER",
  "client": "$CLIENT"
}
EOF
)
printf '%s\n' "$JSON" > deployments/local.json
printf '%s\n' "$JSON" > "$HOME/.rootmode/local-chain.json"

cat <<EOF

Local chain is up.

  RPC          $RPC
  chain id     31337
  USDC         $USDC
  pot          $POT
  fee vault    $VAULT
  worker (A1)  $WORKER

MetaMask
  1. Add network: Localhost 8545, chain id 31337, currency ETH
  2. Import account (Anvil #0 — test key, not real money):
     $PK
  3. Open rootmode → Settings → Your pot → Fund pot

Anvil logs: /tmp/rootmode-anvil.log
Stop:        kill \$(cat /tmp/rootmode-anvil.pid)
EOF
