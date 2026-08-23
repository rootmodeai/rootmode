#!/bin/sh
# Render a worker.toml from environment variables, then run the worker.
#
# Env is the source of truth on every start, so a price (or an endpoint)
# changed in `.env` takes effect on `docker compose up -d` without a rebuild.
# Mount your own file and set ROOTMODE_CONFIG to its path — then none of
# this applies.
set -eu

STATE_DIR="${ROOTMODE_STATE_DIR:-/var/lib/rootmode}"

# Comma-separated env value -> TOML array items, one per line.
#
# Note the trailing newline in the printf: without it `read` swallows the last
# element, which silently produces an empty list for the common case of one
# bootstrap address.
toml_list() {
    printf '%s\n' "${1:-}" | tr ',' '\n' | while IFS= read -r item; do
        item=$(printf '%s' "$item" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
        if [ -n "$item" ]; then
            printf '  "%s",\n' "$item"
        fi
    done
}

# "prompt=6.inputs.text,seed=3.inputs.seed" -> TOML key/value lines.
toml_slots() {
    printf '%s\n' "${1:-}" | tr ',' '\n' | while IFS= read -r pair; do
        key=$(printf '%s' "$pair" | cut -d= -f1 | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
        value=$(printf '%s' "$pair" | cut -d= -f2- | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
        if [ -n "$key" ] && [ -n "$value" ]; then
            printf '%s = "%s"\n' "$key" "$value"
        fi
    done
}

# "model=0.15,other=0.40" -> TOML quoted keys, numeric values.
toml_prices() {
    printf '%s\n' "${1:-}" | tr ',' '\n' | while IFS= read -r pair; do
        key=$(printf '%s' "$pair" | cut -d= -f1 | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
        value=$(printf '%s' "$pair" | cut -d= -f2- | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
        if [ -n "$key" ] && [ -n "$value" ]; then
            printf '"%s" = %s\n' "$key" "$value"
        fi
    done
}

if [ -n "${ROOTMODE_CONFIG:-}" ]; then
    if [ ! -f "$ROOTMODE_CONFIG" ]; then
        echo "error: ROOTMODE_CONFIG=$ROOTMODE_CONFIG does not exist." >&2
        exit 1
    fi
    CONFIG="$ROOTMODE_CONFIG"
else
    if [ -z "${ROOTMODE_VLLM:-}" ] && [ -z "${ROOTMODE_COMFYUI:-}" ]; then
        echo "error: no backend configured." >&2
        echo "       set ROOTMODE_VLLM=http://host:8000 (and/or ROOTMODE_COMFYUI)," >&2
        echo "       or mount a worker.toml and set ROOTMODE_CONFIG to its path." >&2
        exit 1
    fi

    CONFIG="/tmp/worker.toml"
    mkdir -p "$(dirname "$CONFIG")"
    {
        echo "# Generated from environment variables."
        echo "# Edit .env and restart — this file is rewritten every start."
        echo "# Set ROOTMODE_CONFIG to a mounted file to take over."
        echo
        echo "[worker]"
        echo "label = \"${ROOTMODE_LABEL:-$(hostname)}\""
        echo "listen = \"${ROOTMODE_LISTEN:-0.0.0.0:9944}\""
        echo "max_concurrent = ${ROOTMODE_MAX_CONCURRENT:-1}"
        # Where the operator says the box is, for the client's peer list.
        # Declared, never looked up — see docs/WORKER.md.
        echo "country = \"${ROOTMODE_COUNTRY:-}\""
        echo "payout_address = \"${ROOTMODE_PAYOUT:-}\""
        echo "require_signature = ${ROOTMODE_REQUIRE_SIGNATURE:-true}"
        echo "allow_peers = ["
        toml_list "${ROOTMODE_ALLOW_PEERS:-}"
        echo "]"
        # How often to re-ask the backends what they have. A model loaded into
        # vLLM, or a checkpoint dropped into ComfyUI, becomes servable within
        # this long instead of at the next restart.
        echo "refresh_secs = ${ROOTMODE_REFRESH_SECS:-60}"
        # Absolute, and on the volume: the identity has to survive a recreate.
        echo "identity_file = \"${ROOTMODE_IDENTITY_FILE:-$STATE_DIR/worker.key}\""
        echo

        echo "[p2p]"
        echo "enabled = ${ROOTMODE_P2P_ENABLED:-true}"
        echo "bootstrap = ["
        toml_list "${ROOTMODE_BOOTSTRAP:-}"
        echo "]"
        echo "listen = ["
        toml_list "${ROOTMODE_P2P_LISTEN:-/ip4/0.0.0.0/tcp/4101}"
        echo "]"
        echo "relay = ${ROOTMODE_RELAY:-true}"
        echo "dht_server = ${ROOTMODE_DHT_SERVER:-false}"
        echo "local_discovery = ${ROOTMODE_LOCAL_DISCOVERY:-true}"
        echo "external = ["
        toml_list "${ROOTMODE_P2P_EXTERNAL:-}"
        echo "]"
        echo

        # Usage reporting, on by default so the explorer has something in it.
        # ROOTMODE_STATS_URL="" turns it off; the node serves jobs regardless.
        echo "[stats]"
        echo "url = \"${ROOTMODE_STATS_URL-https://rootmode.ai/report}\""
        echo "interval_secs = ${ROOTMODE_STATS_INTERVAL:-300}"
        echo

        echo "[payments]"
        echo "contract = \"${ROOTMODE_POT:-}\""
        echo "chain_id = ${ROOTMODE_CHAIN_ID:-8453}"
        echo "rpc = \"${ROOTMODE_RPC:-}\""
        echo "sender = \"${ROOTMODE_PAY_SENDER:-}\""
        echo "key_file = \"$STATE_DIR/pay.key\""
        echo "channels_file = \"$STATE_DIR/channels.json\""
        # The secret stays in pay.key on the volume (or ROOTMODE_PAY_KEY).
        # This file is printed on start, so it never contains the hex.
        echo

        if [ -n "${ROOTMODE_VLLM:-}" ]; then
            # Comma-separated: one OpenAI-compatible server per entry. The
            # worker asks each /v1/models and advertises whatever answers —
            # operators do not name the weights.
            printf '%s\n' "$ROOTMODE_VLLM" | tr ',' '\n' | while IFS= read -r endpoint; do
                endpoint=$(printf '%s' "$endpoint" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
                [ -n "$endpoint" ] || continue
                echo "[[backends]]"
                echo "kind = \"vllm\""
                echo "endpoint = \"$endpoint\""
                [ -n "${ROOTMODE_VLLM_API_KEY:-}" ] && echo "api_key = \"${ROOTMODE_VLLM_API_KEY}\""
                if [ -n "${ROOTMODE_VLLM_MODELS:-}" ]; then
                    echo "models = ["
                    toml_list "${ROOTMODE_VLLM_MODELS}"
                    echo "]"
                fi
                # One number covers every model this server reports. Per-id
                # overrides in ROOTMODE_VLLM_PRICES win. Unset = advertised free.
                [ -n "${ROOTMODE_VLLM_PRICE:-}" ] && echo "price = ${ROOTMODE_VLLM_PRICE}"
                [ -n "${ROOTMODE_CURRENCY:-}" ] && echo "currency = \"${ROOTMODE_CURRENCY}\""
                if [ -n "${ROOTMODE_VLLM_PRICES:-}" ]; then
                    echo "[backends.prices]"
                    toml_prices "${ROOTMODE_VLLM_PRICES}"
                fi
                echo
            done
        fi

        if [ -n "${ROOTMODE_COMFYUI:-}" ]; then
            echo "[[backends]]"
            echo "kind = \"comfyui\""
            echo "endpoint = \"${ROOTMODE_COMFYUI}\""
            echo "checkpoint_id = \"${ROOTMODE_COMFYUI_CHECKPOINT:-}\""
            [ -n "${ROOTMODE_COMFYUI_PRICE:-}" ] && echo "price = ${ROOTMODE_COMFYUI_PRICE}"
            [ -n "${ROOTMODE_CURRENCY:-}" ] && echo "currency = \"${ROOTMODE_CURRENCY}\""
            if [ -n "${ROOTMODE_COMFYUI_PRICES:-}" ]; then
                echo "[backends.prices]"
                toml_prices "${ROOTMODE_COMFYUI_PRICES}"
            fi
            # No workflow named: the worker builds a standard text-to-image
            # graph from whatever the server reports it has, so pointing at an
            # endpoint is the whole configuration. Name one for a pipeline with
            # a shape of its own — LoRAs, ControlNet, upscalers.
            # One graph per model: "krea2=/path/krea2.json,lustify=/path/l.json".
            # A box serving checkpoints of different shapes needs this — one
            # graph cannot load an all-in-one SDXL file and a Flux-style model
            # whose text encoders are separate.
            if [ -n "${ROOTMODE_COMFYUI_WORKFLOWS:-}" ]; then
                printf '%s\n' "$ROOTMODE_COMFYUI_WORKFLOWS" | tr ',' '\n' | while IFS= read -r pair; do
                    model=$(printf '%s' "$pair" | cut -d= -f1 | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
                    file=$(printf '%s' "$pair" | cut -d= -f2- | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
                    if [ -n "$model" ] && [ -n "$file" ]; then
                        echo "[[backends.workflow_for]]"
                        echo "model = \"$model\""
                        echo "file = \"$file\""
                        echo
                    fi
                done
            fi

            if [ -n "${ROOTMODE_COMFYUI_WORKFLOW:-}" ]; then
                echo "workflow = \"${ROOTMODE_COMFYUI_WORKFLOW}\""
                echo
                echo "[backends.slots]"
                toml_slots "${ROOTMODE_COMFYUI_SLOTS:-prompt=6.inputs.text,seed=3.inputs.seed}"
            fi
            echo
        fi
    } > "$CONFIG"

    echo "wrote $CONFIG:"
    sed 's/^/  /' "$CONFIG"
fi

exec rootmode-worker "$@" --config "$CONFIG"
