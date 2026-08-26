# rootmode-worker

The binary you run on the box with the GPUs. It advertises what the node can
actually do, accepts jobs over RootmodeProtocol v1, and drives a local
inference server — vLLM for text, ComfyUI for images and video.

It holds no models of its own and executes nothing a client sends. A job picks
a **declared** workflow and fills **declared** parameters. That is the whole
surface.

## Run it with Docker

The quickest way onto a GPU box. The container needs **no GPU access** — it
talks to your vLLM or ComfyUI over HTTP; they keep the GPUs.

```sh
cp .env.example .env          # uncomment vLLM and/or ComfyUI
docker compose up -d --build
```

That is the whole thing on any Linux GPU box — Spark, A100, H100, or a
machine whose cards sit behind a remote OpenAI-compatible URL. The compose
file is the same everywhere; only `.env` names the endpoints on *this* box.
vLLM, ComfyUI, or both. Logs: `docker compose logs -f`. Stop:
`docker compose down`.

Or by hand, without Compose:

```sh
docker build -f docker/Dockerfile.worker -t rootmode-worker .

docker run -d --name rootmode-worker --restart unless-stopped \
  --network host \
  -v rootmode-worker:/var/lib/rootmode \
  -e ROOTMODE_VLLM=http://127.0.0.1:8000 \
  rootmode-worker
```

That is the whole thing. A client on the same network finds it automatically —
nothing to paste. For a client somewhere else, it logs an address to use:

```
INFO this node is its own entry point — give a client one of:
INFO   /ip4/192.168.1.50/tcp/4101/p2p/12D3KooWR3Tjk...
```

With no bootstrap address the worker answers DHT queries itself, so a client
pointed at it discovers what it serves. Set `ROOTMODE_BOOTSTRAP` in `.env` once
you have several nodes that need to find each other — see
[`docs/NETWORK.md`](NETWORK.md).

`--network host` is the simplest thing that works on Linux: the worker reaches
your inference server on `127.0.0.1`, and the address it advertises is the
host's own. With published ports instead, set `ROOTMODE_P2P_EXTERNAL` to the
address other peers should dial — otherwise they discover this node and then
cannot reach it.

**Mount the volume.** `/var/lib/rootmode` holds `worker.key` — and, once you charge, `pay.key` (an Ethereum key with your gas ETH on it) and `channels.json` (bills you have not yet collected). Losing the volume loses money, not just a peer id. Without it the
peer id changes every time the container is recreated, and every client that
pinned it stops recognising the node.

On each start the container writes a `worker.toml` from the environment and
prints it, so a change in `.env` takes effect on restart. Mount your own file
and set `ROOTMODE_CONFIG` to its path — then none of the variables apply.

| variable | default | |
|---|---|---|
| `ROOTMODE_VLLM` | — | OpenAI-compatible endpoint (comma-separated if you have more than one) |
| `ROOTMODE_VLLM_PRICE` | — | what you charge per million tokens, for every model that server reports |
| `ROOTMODE_VLLM_PRICES` | — | optional per-id overrides: `id=0.40,id=0.10` |
| `ROOTMODE_COMFYUI` | — | ComfyUI endpoint (images, and video models if they are installed) |
| `ROOTMODE_COMFYUI_PRICE` | — | what you charge per image, for every checkpoint |
| `ROOTMODE_COMFYUI_PRICES` | — | optional per-id overrides: `sdxl=0.02,krea2-turbo=0.08` |
| `ROOTMODE_CURRENCY` | `USD` | currency for the prices above |
| `ROOTMODE_PAYOUT` | the node's settle key | where the 90% USDC is sent when a job settles. Unset, earnings accrue to the pay key on the volume (logged on every start) — name a wallet of your own |
| `ROOTMODE_POT` | built in | RootmodePot address. A node with a price settles on the network's pot; set this only to point at another deployment |
| `ROOTMODE_CHAIN_ID` | `8453` | settlement chain (Base) |
| `ROOTMODE_RPC` | built in | JSON-RPC URL for lock checks and transactions. The node fails over to other public Base endpoints when one is rate-limited |
| `ROOTMODE_PAY_KEY` | — | Ethereum private key (secp256k1, 32-byte hex) this node signs `reserve`/`settle` with. Omit it and one is minted on the volume (`pay.key`). **Needs ETH on Base for gas** — see *Before you charge* |
| `ROOTMODE_PAY_SENDER` | — | only for a node whose RPC holds an unlocked account (`eth_sendTransaction`). On a public RPC this fails with `unknown account`; use a key instead |
| `ROOTMODE_BOOTSTRAP` | built in | comma-separated bootstrap multiaddrs; the network's own are compiled in |
| `ROOTMODE_P2P_EXTERNAL` | — | address to advertise, if not what it binds |
| `ROOTMODE_LABEL` | hostname | shown to clients |
| `ROOTMODE_COUNTRY` | — | ISO 3166-1 alpha-2 (`DE`, `GB`), shown beside the label |
| `ROOTMODE_STATS_URL` | `https://rootmode.ai/report` | where usage goes; `""` reports nothing |
| `ROOTMODE_STATS_INTERVAL` | `300` | seconds between reports |
| `ROOTMODE_LISTEN` | `0.0.0.0:9944` | websocket address |
| `ROOTMODE_MAX_CONCURRENT` | `2` | jobs at once |
| `ROOTMODE_REFRESH_SECS` | `60` | how often to re-read what the backends serve; `0` disables |
| `ROOTMODE_REQUIRE_SIGNATURE` | `true` | refuse unsigned submissions |
| `ROOTMODE_ALLOW_PEERS` | — | comma-separated client peer ids |
| `ROOTMODE_RELAY` | `true` | ask for a relay slot (needed behind NAT) |
| `ROOTMODE_DHT_SERVER` | `false` | answer DHT queries for others. Set `true` on a publicly reachable node — otherwise its records live only in the bootstrap's memory |
| `ROOTMODE_VLLM_API_KEY`, `ROOTMODE_VLLM_MODELS` | — | |
| `ROOTMODE_COMFYUI_WORKFLOW`, `ROOTMODE_COMFYUI_CHECKPOINT`, `ROOTMODE_COMFYUI_SLOTS` | see below | |
| `ROOTMODE_COMFYUI_WORKFLOWS` | — | `model=/path/graph.json,…` — one graph per model |

For ComfyUI, mount your API-format workflow and point at it:

```sh
  -e ROOTMODE_COMFYUI=http://127.0.0.1:8188 \
  -e ROOTMODE_COMFYUI_CHECKPOINT=sdxl-base-1.0 \
  -e ROOTMODE_COMFYUI_WORKFLOW=/etc/rootmode/workflows/mine.json \
  -e ROOTMODE_COMFYUI_SLOTS='prompt=6.inputs.text,seed=3.inputs.seed' \
  -v ./mine.json:/etc/rootmode/workflows/mine.json:ro \
```

The image ships `sdxl_txt2img.json` and defaults the slots to match it.

Health: the container reports unhealthy only when **no** configured backend
is answering. A box with vLLM up and ComfyUI off is still a worker.


## Before you charge

Serving is free to run. **Charging is not**: every paid job ends with this
node posting a transaction on Base — the client's lock (`reserve`) and,
batched, its own `settle` — and each one costs gas, about $0.0014. So a
charging node needs an Ethereum key with ETH on it, and it needs it **before
the first paid job**, or that job is served and never collected.

1. Set a price (`ROOTMODE_VLLM_PRICE` / `ROOTMODE_COMFYUI_PRICE`) and
   `ROOTMODE_PAYOUT` (where your USDC goes) and start the container. The
   pot, the chain and an RPC are built in; nothing else to look up.
2. On first start it mints `pay.key` on the volume and logs the address:
   ```
   INFO settle signer 0x1fdc…0ba2
   ```
   Send that address a little ETH on Base. `0.002 ETH` (a few dollars) is
   roughly 3,500 transactions; it is a hot key for gas only, never for
   revenue — revenue goes to `ROOTMODE_PAYOUT`, which can be a cold wallet.
3. The node refuses to start charging while that address has no ETH, and
   warns at start when it is running low. Watch `docker compose logs` for
   `pay key … has no ETH` / `is low on ETH`. When the key runs dry, paid
   work still gets done but its tickets expire uncollected an hour later.

Set no price and none of this applies: the node serves free and never
touches the chain. A price is the switch — with one, the node settles on the
network's pot whether or not you name it.
## Build from source

```sh
cargo build --release -p rootmode-worker
# target/release/rootmode-worker
```

## Quickstart

```sh
rootmode-worker init                 # writes worker.toml
$EDITOR worker.toml                  # point it at your inference server
rootmode-worker check                # config + backend reachability
rootmode-worker run
```

`run` prints the node's peer id and the endpoint to add in the client:

```
INFO rootmode worker v0.1.0 peer_id=ef9084b8a01c18be4b75fb7e0b72b7b4d57b5be99dddf2c430e4db01a43ea8a7
INFO listening on ws://0.0.0.0:9944
INFO caps: [llm]  max_concurrent: 2
INFO   meta-llama/Llama-3.1-8B-Instruct (llm)
```

Add `ws://<that-host>:9944` on the client's **peers** screen. Paste the peer id
into the "peer public key" field to pin it — then a different key on that
address is refused rather than silently accepted.

To check a node without the desktop client:

```sh
cargo run -p rootmode-worker --example submit -- ws://127.0.0.1:9944 "what is a peer?"
```

## Joining the network

Without this the worker still works — clients just have to be given its
`ws://` address by hand. With it, the worker announces what it serves and
clients discover it.

```toml
[p2p]
enabled = true
# empty = the entry points compiled into the build
bootstrap = []
listen = ["/ip4/0.0.0.0/tcp/4101"]
relay = true        # leave on if this box is behind NAT
dht_server = false  # turn on only if this host has a public address
```

On start you should see:

```
INFO joined the network peer=12D3KooWRwXRPuPux1G6SZaxcGobMHx4qbcZmXfjPGgcPQUbk8dC
INFO   p2p /ip4/192.168.10.37/tcp/4101
INFO announcing [llm] and 1 model(s) to the network
```

What gets published comes from the backends — the models your vLLM actually
has — not from the config. Details, and how to run a bootstrap node:
[`docs/NETWORK.md`](NETWORK.md).

## LLM inference (vLLM)

Any OpenAI-compatible server works: vLLM, SGLang, llama.cpp's server, TGI's
OpenAI shim.

```sh
vllm serve meta-llama/Llama-3.1-8B-Instruct --host 127.0.0.1 --port 8000
```

Set `country` in `[worker]` to have clients show where the box is:

```toml
[worker]
label   = "hetzner-a6000"
country = "DE"          # ISO 3166-1 alpha-2. Declared, never geolocated —
                        # leave it out and clients show nothing.
```

```toml
[[backends]]
kind = "vllm"
endpoint = "http://127.0.0.1:8000"
# api_key = "..."         # only if you started vLLM with --api-key
# models = []             # empty = ask the server via /v1/models
price = 0.15              # per million tokens, every model. unset = free
# [backends.prices]
# "meta-llama/Llama-3.1-8B-Instruct" = 0.40
```

In Docker that is one line in `.env`: `ROOTMODE_VLLM_PRICE=0.15`. Leave it
unset and the models are advertised as free. Clients pick the cheapest
provider for the model they want. A price without a pot is only a sticker —
see [Payment](#payment).

Leave `models` empty and the worker asks `/v1/models` and advertises everything
it finds — whatever you have loaded is what the network sees. It re-asks every
`refresh_secs`, so a model loaded later becomes servable without a restart. Set
`models` explicitly to expose only some of what is loaded.

Generation is streamed, so `job.status` progress in the client reflects real
tokens rather than a fake ramp.

**Multi-node clusters.** One worker fronts one endpoint. If your DGX Spark
cluster already presents a single OpenAI-compatible endpoint (a vLLM router, or
a load balancer over several `vllm serve` processes), run **one** worker
pointing at it and set `max_concurrent` to what the cluster can hold. If each
node serves separately, run a worker per node; each gets its own identity and
appears as its own peer.

## Image generation (ComfyUI)

A client sends a model and a prompt. Nothing else. Sampler steps, guidance,
size, scheduler and the shape of the graph are whatever you saved in the
workflow you exported — a client cannot know what suits a checkpoint it has
never seen, so it is not asked to. The worker reports what it used in the
result, so the numbers are visible afterwards without being dictated.

Two slots are fillable: `prompt` (the client's) and `seed` (the worker's, so
repeated prompts vary). Leave `seed` undeclared to render the same picture
every time.

```sh
python main.py --listen 127.0.0.1 --port 8188
```

### Serving whatever is installed

With **no `workflow` set**, the worker asks ComfyUI what checkpoints the box
has and advertises **every one of them**, running each through a standard
text-to-image graph it builds itself. A client picks one by name in
`checkpoint_id`; naming nothing gets the first, or whatever `checkpoint_id` in
the config names. Drop another `.safetensors` into
`ComfyUI/models/checkpoints` and it is servable within `refresh_secs` — no
restart.

```toml
[[backends]]
kind = "comfyui"
endpoint = "http://127.0.0.1:8188"
price = 0.02              # per image, every checkpoint. unset = free
[backends.prices]
"flux1-dev" = 0.05
"krea2-turbo" = 0.08
```

In Docker:

```
ROOTMODE_COMFYUI_PRICE=0.02
ROOTMODE_COMFYUI_PRICES=flux1-dev=0.05,krea2-turbo=0.08
```

The default covers anything you do not name. Keys match the advertised id
(the checkpoint filename without the extension), case-insensitively, and by
prefix — `krea2` prices `krea2-turbo`.

That is the whole configuration for the ordinary case.

**Checkpoints of different shapes are handled for you.** A file in
`models/checkpoints` may be an all-in-one — model, text encoder and VAE
together — or only diffusion weights, with the encoders installed separately.
Nothing ComfyUI exposes says which, so the worker finds out: it runs the
ordinary graph, and if the encoder comes back empty (*"clip input is invalid:
None"*) it builds the other shape — `UNETLoader` + `DualCLIPLoader` +
`VAELoader`, picking the encoders and VAE you have installed — and runs that
instead. The answer is remembered per checkpoint, so a box pays one wasted
attempt for a model once, not once per job.

If nothing suitable is installed you get a sentence saying so — *"needs a text
encoder and none is installed; put one in ComfyUI/models/text_encoders"* —
rather than a failed render.

Guidance is also lowered to 1.0 for checkpoints whose names look distilled
(Flux, SD3, turbo, lightning), because the value that suits SDXL burns those to
noise.

A pipeline with a real shape of its own — LoRAs, ControlNet, upscalers, video —
still wants a workflow, below.

### A different pipeline per model

One graph cannot serve every checkpoint. An all-in-one SDXL file carries its
own text encoder; a Flux or Krea-style model wants `CLIPLoader` nodes feeding
it and fails with *"clip input is invalid: None"* in the standard graph. So
name a workflow per model:

```toml
[[backends.workflow_for]]
model = "krea2-turbo"
file  = "/etc/rootmode/workflows/krea2.json"

[[backends.workflow_for]]
model = "lustify-v7"
file  = "/etc/rootmode/workflows/lustify.json"
```

A client picking `krea2-turbo` gets that graph; picking anything else on the
box gets the built-in one with that checkpoint loaded. Both are advertised
together, and a model with its own workflow is advertised **once** — offering
it through the built-in graph as well would let a client choose the pipeline
that cannot run it.

Slots default to the standard positions and can be set per workflow when your
node ids differ:

```toml
[[backends.workflow_for]]
model = "krea2-turbo"
file  = "/etc/rootmode/workflows/krea2.json"
slots = { prompt = "16.inputs.text", seed = "25.inputs.noise_seed" }
```

In Docker: `-e ROOTMODE_COMFYUI_WORKFLOWS="krea2-turbo=/etc/rootmode/workflows/krea2.json,lustify-v7=/etc/rootmode/workflows/lustify.json"`.

### Serving one pipeline you built

1. Build the workflow you want to serve in the ComfyUI web UI.
2. **Save (API Format)** — the API export, not the regular save. The regular
   format is the editor's graph and will not load here.
3. Point the config at it and declare which node inputs a client may fill.

```toml
[[backends]]
kind = "comfyui"
endpoint = "http://127.0.0.1:8188"
workflow = "workflows/sdxl_txt2img.json"
checkpoint_id = "sdxl-base-1.0"

[backends.slots]
prompt = "6.inputs.text"
seed   = "3.inputs.seed"
```

A slot path is `<node id>.inputs.<field>` from the API-format JSON. Open the
file and read the node ids off it — they are the object keys.

Slots are checked against the workflow **at startup**, so a typo is a refusal
to boot rather than a job that fails at 3am. `workflows/sdxl_txt2img.json` in
this repo is a working example matching the slots above.

A workflow loads the checkpoint it names, so a node configured this way
advertises **one** model — the other checkpoints on the box are not reachable
through that graph, and `checkpoint_id` on a job is ignored.

Only two fields may be declared:

```
prompt  seed
```

Anything else is a config error. This is the "workers only run fixed workflow
types" rule from the product spec, enforced rather than documented: a client
cannot send a graph, add a node, change `ckpt_name`, or reach an input you did
not list. A prompt that contains JSON lands in the text slot **as a string**.

Progress comes from ComfyUI's own websocket when available, so the client's
progress bar tracks real sampler steps.

## Access control

Signing is optional in v1 and the defaults are deliberately permissive so a
first run works. Tighten before exposing a port:

```toml
[worker]
require_signature = true
allow_peers = ["<client peer id>", "<another>"]
```

- **`require_signature`** — submissions must carry a valid ed25519 signature
  over the canonical JSON. A signature that is present but wrong is always
  rejected, even with this off; forging an identity is never tolerated.
- **`allow_peers`** — only these client peer ids may submit. Setting it implies
  `require_signature`, since you cannot enforce "these peers only" without
  verifying who is speaking. Clients find their id on the settings screen.

`max_concurrent` bounds jobs across *all* connections, because the GPU is
shared whether or not the clients know about each other. Beyond that the queue
is honest: a client sees `queued` until a slot frees.

## Identity

The node's ed25519 key lives at `identity_file` (default `worker.key` next to
the config), written `0600`. **The peer id is the public key** — there is
nothing to register.

Back the key up. Replacing it makes the node a stranger to every client that
pinned it.

```sh
rootmode-worker id      # print the peer id, creating the key if absent
```

## Running as a service

```ini
# /etc/systemd/system/rootmode-worker.service
[Unit]
Description=rootmode worker
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/rootmode-worker run --config /etc/rootmode/worker.toml
User=rootmode
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=rootmode_worker=info
# The worker needs no privileges beyond reaching its backend.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/etc/rootmode

[Install]
WantedBy=multi-user.target
```

`RUST_LOG=rootmode_worker=debug` adds per-frame detail when a client is
misbehaving.

## Troubleshooting

**"the model spent all N tokens reasoning and never got to an answer"** — a
reasoning model (DeepSeek-R1 and similar, when vLLM runs with a reasoning
parser) thinks before it writes. Raise `max_tokens` in the client; the
thinking counts against the same budget as the answer. Reasoning tokens are
not returned as the result — the result is the answer, and its sha256 covers
only that. The amount of thinking is reported as `reasoning_chars` in the
result meta.

**"advertising no models — is the backend running?"** — the worker started
before vLLM did. It still accepts jobs and forwards them; it just could not
list models at boot. Restart it once the backend is up to populate the
advertisement, or set `models` explicitly.

**Client shows `offline`** — check the port is reachable from the client host
(`nc -z <host> 9944`). The worker binds `0.0.0.0` by default; a firewall or a
`listen` set to `127.0.0.1` is the usual cause.

**Client shows `mismatch`** — the endpoint announced a different key than the
client pinned. Either the worker's `identity_file` changed, or you are not
talking to the node you think you are.

**`model 'X' is not served here`** — the client asked for a model this node
does not advertise. The error lists what is available. The worker will not
quietly substitute different weights, because that would make the result hash
a lie.

## What this is not, yet

Reputation. Discovery gets clients to you; it does not tell them whether you
are honest, and it does not tell you whether they are. Use `allow_peers` if
you only want to serve people you know.

## Images from OpenRouter

An `openrouter` backend can also serve pictures: name a model whose output
is images (`google/gemini-3.1-flash-image`, `openai/gpt-5-image-mini`, …)
in its `models` list and the node advertises it as an **image** model. The
per-picture price is the catalogue's output-image rate, times a dear-case
2,000 image tokens, times `markup` — that is what the client locks. The
bill is what the picture actually cost plus the markup, never more than
the advertised price. A starting image (`from_image`) is passed along for
edits.

Video works the same way through OpenRouter's asynchronous video API: name
a model from `GET /api/v1/videos/models` (`google/veo-3.1-lite`,
`kwaivgi/kling-v3.0-std`, `bytedance/seedance-2.0-fast`, …) and the node
advertises it as a **video** model. Every clip from a seed node has one
shape — 5 seconds (or the nearest the model offers), 720p, 16:9, silent —
because the request carries only a prompt and an optional first frame, and
one shape is one price: the catalogue's per-second rate for that shape,
times the seconds, times `markup`. The node submits the job, polls until it
completes (up to 20 minutes), downloads the mp4, and bills the reported cost
plus markup, never above the advertised price.

## Payment

A priced job is prepaid in **1 million token slices** (at the dearest of
input / output / cache-write, clipped to the channel's per-job cap — the
client's limit, taken from its account when the channel is opened). Before
the GPU starts, the node checks that the prompt and answer ceiling fit that
lock, pricing the prompt at the input rate and the answer at the output
rate. If a reply runs long, this node asks for the next slice mid-stream
(`top_up`); the app signs it with no UI. After the job it invoices the
actual bill: `uncached prompt × input + cached × cache + completion × output`
(completion includes reasoning), and never less than what the work actually
cost upstream times the markup, when the backend reports that cost
(OpenRouter does). If the invoice signature does not arrive — the client
stopped the reply, or went away — the prepaid slice is kept instead.

Collection is this node's job, not the client's. It checks the on-chain lock
before work, posts the client's signed `reserve` when the lock is short, and
**settles** with its own key (`eth_sendRawTransaction`). Settles are batched
— once a channel holds about $0.05 unsettled, at half the per-job cap at the
latest, and always before a ticket's one-hour deadline — so gas stays under a
few percent of what is collected. 90% goes to `ROOTMODE_PAYOUT`, 10% to the
network fee vault.

Every paid job logs one line you can reconcile against your provider's
statement:

```
INFO billed job_id=… model="kimi-k3" billed_micros=27234 upstream_cost_usd=0.02231 prompt=47909 cached=44928 completion=71 reasoning=0
```

Two Ethereum keys, different jobs:

| | |
|---|---|
| **Payout** (`ROOTMODE_PAYOUT`) | Where the 90% USDC is sent. Can be a cold wallet. |
| **Pay key** (`ROOTMODE_PAY_KEY`) | The Ethereum private key (secp256k1, 32-byte hex — the same shape MetaMask exports) this process uses to sign `settle`. It needs a little ETH for gas. It does not have to be the payout address. |

Keep a shared hex key in `.env` only when one process will settle. A fleet
uses `payments.key_file` (Docker: `/var/lib/rootmode/pay.key` on each
volume): generated on first start, one address per container so nonces do
not collide. `ROOTMODE_PAYOUT` can still be one treasury. Prefer a thin hot
key; it needs a little ETH for gas. The generated `worker.toml` (printed on
start) never contains the hex.

```toml
[worker]
payout_address = "0x…"    # unset: earnings accrue to the settle key on the volume

[payments]
# contract, chain_id and rpc default to the network's live pot on Base —
# the binary carries the deploy record. Name them only for another
# deployment (a local Anvil, a testnet).
# contract = "0x…"
# chain_id = 8453
# rpc      = "https://mainnet.base.org"
# sender is derived from ROOTMODE_PAY_KEY when that is set
```

In Docker: a price and `ROOTMODE_PAYOUT` in `.env` (`ROOTMODE_PAY_KEY` if
you would rather bring a key than have one minted). Set no price and the
node serves without charging.

If `ROOTMODE_PAY_KEY` is unset and `ROOTMODE_PAY_SENDER` is set, the RPC is
asked to sign (`eth_sendTransaction`). That only works against your own node
with that account unlocked; a public RPC answers `unknown account` and no
job ever settles. Prefer the key.

## Reporting usage

**On by default.** Every `interval_secs` a worker posts a signed account of
what *it* served to the network's collector, which is what fills in the
[explorer](https://rootmode.ai/explorer). An explorer with nothing in it
makes a working network look dead.

```toml
[stats]
url           = "https://rootmode.ai/report"   # the default
interval_secs = 300
```

Turning it off is one line, and costs you nothing else — a silent node serves
jobs exactly the same and stays a full member of the network:

```toml
[stats]
url = ""
```

In Docker: `-e ROOTMODE_STATS_URL=""`.

What goes in the report: request and image counts, token totals counted with
the OpenAI tokenizer (raised to the inference server's figure when that is
higher, so an under-report cannot shrink the bill), cache hits the server
reported, what you charged, your label, your models, your declared country.
What does not, because it never reaches the worker's counters at all: prompts,
answers, and the peer id of whoever asked.

Reports are signed with the node's own key, so the collector can tell your
numbers from an invented node's, and the collector reads the country from the
connection unless you declared one. Nothing about this is required — a node
that reports nothing is a full member of the network, and the explorer's
figures are a floor rather than a census because of it.

Run your own collector: [`services/stats`](../services/stats/README.md).
