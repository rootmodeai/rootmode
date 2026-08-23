# Architecture

```
 apps/desktop                                     crates/rootmode-worker
 ┌────────────────────────┐                       ┌────────────────────────┐
 │ React UI               │                       │ server                 │
 │   ↕ invoke / listen    │  RootmodeProtocol v1  │   ↓                    │
 │ Tauri shell            │ ◀───────────────────▶ │ backends → vLLM        │
 │   transports, storage  │   ws://  or  p2p://   │            ComfyUI     │
 │                        │                       │            OpenRouter │
 └───────────┬────────────┘                       └───────────┬────────────┘
             │           crates/rootmode-p2p                  │
             └─────────── discovery (DHT) ───────────────────-┘
                                  │
                          rootmode-bootstrap
                        entry point + NAT relay
                     (no directory, never sees a job)

              crates/rootmode-core — protocol, jobs, identity, payments
                       depended on by all of the above
```

The client and the worker are two peers speaking one protocol, not two layers
of one program. `rootmode-core` is the only thing they share, which is what
keeps the two sides of the wire honest.

## Crates and modules

### `crates/rootmode-core`

Transport-agnostic and dependency-light. Nothing in here knows about Tauri,
SQLite, HTTP, or GPUs. Both the client and the worker depend on it, which is
what keeps the two sides of the wire honest.

| module | responsibility |
|---|---|
| `protocol` | v1 message types, strict parsing, version checks |
| `job` | `JobPayload` (`llm` \| `image` \| `video`), bounds validation, summaries |
| `identity` | ed25519 keypair, `peer_id = hex(public key)`, sign/verify |
| `payments` | EIP-712 tickets for the pot (`ReserveTicket`, `SpendTicket`) |
| `tokens` | OpenAI tokenizer counts used for billing |
| `canonical` | canonical-JSON signing pre-image |
| `hash` | `sha256_hex` |
| `keyfile` | the one filesystem exception: `0600` key material, shared by client and worker |

### `crates/rootmode-p2p`

Discovery, shared by all three binaries. Also builds `rootmode-bootstrap`.

| module | responsibility |
|---|---|
| `node` | one libp2p swarm on a task, reached by message: provide, find, open |
| `ident` | rootmode peer id ↔ libp2p PeerId (the same ed25519 key, two encodings) |
| `framing` | newline-delimited JSON over a libp2p stream |

### `crates/rootmode-worker`

What an operator runs on a GPU box. Binary + library.

| module | responsibility |
|---|---|
| `config` | the operator's TOML: listen address, policy, backends, slots |
| `server` | listeners (ws + libp2p), announce, per-job tasks, concurrency permits |
| `p2p` | joining the network and publishing what the backends resolved |
| `backends` | `Backend` trait + registry/routing by kind and model |
| `backends::vllm` | OpenAI-compatible chat completions, streamed for progress |
| `backends::comfyui` | one API-format workflow, declared slots only |
| `backends::openrouter` | OpenAI-compatible proxy, catalogue rates × `markup` |
| `chain` | pot lock check; this node signs `settle` and sends it |

Its `examples/submit.rs` is a minimal standalone client, useful for checking a
node without the desktop app.

### `apps/desktop/src-tauri`

| module | responsibility |
|---|---|
| `commands` | the entire frontend surface — every `#[tauri::command]` |
| `state` | `AppState`: db handle, identity, settings |
| `store` | SQLite: peers, jobs, results, settings |
| `identity_store` | key file, `0600`, outside the repo |
| `net` | `Transport` trait + `WsTransport`, endpoint validation |
| `p2p` | `Libp2pTransport`, discovery, `p2p://` endpoints |
| `mock` | in-process worker (dev) implementing the same `Transport` |
| `jobs` | job lifecycle, event emission, prepaid spend on submit |
| `pot` | Wallet tab: deposit, on-chain lock, spend tickets |
| `gateway` | loopback OpenAI/Anthropic HTTP for editors |
| `results` | hash verification, writing image files |
| `error` | `AppError`, serialised to the frontend as a plain string |

## How a job flows

1. **UI** calls `submit_job(peerId, payload)`.
2. **`jobs::submit`** validates the payload, checks the peer advertises the
   capability, writes a `queued` row, emits `job:update`, and returns. The UI is
   never blocked past this point.
3. A tokio task builds a `JobSubmit`, signs it, and hands it to the peer's
   `Transport` along with a channel.
4. The transport streams `job.status` / `job.result` messages into the channel.
5. Each message updates SQLite and re-emits `job:update`. A `job.result` is
   verified (`sha256` of the actual bytes), written to disk if it is an image,
   stored, and emitted as `job:result`.
6. If the peer disconnects without a terminal status, the job is marked
   `failed` with the reason — no spinner outlives its connection.

At startup, `fail_orphaned_jobs` marks anything left `queued`/`running` by a
previous run as failed: the socket that owned it is gone, so it is not
resumable.

## How a job runs on the worker

1. A frame arrives; `ClientMessage::parse` version-checks it and drops unknown
   types.
2. `handle_submit` authorizes (signature policy, allowlist), re-validates the
   payload — the client's validation is not trusted — and routes to a backend
   by kind and requested model.
3. The job waits on a semaphore permit. That wait *is* the queue, and the
   client sees `queued` for exactly as long as it lasts.
4. The backend runs, reporting progress into a channel that becomes
   `job.status` frames.
5. The result is hashed by the backend and sent, followed by a terminal
   `done`. A failure at any point becomes `failed` with the reason.

Each job is its own task writing through one shared socket writer, so a long
render never blocks another job's status updates on the same connection.

### Why backends see so little

A backend receives a `JobPayload` and nothing else — no peer id, no raw
message, no connection. It is the last place a hostile prompt could do damage,
so it gets the least context. The ComfyUI adapter takes this furthest: the
operator declares a fixed graph and a map of `field → node input`, and a job
supplies values for those slots only. There is no code path from a job to a new
node, a different checkpoint, or a filesystem path.

## Two pipes, one protocol

`net::Transport` is the seam:

```rust
async fn run_job(
    &self,
    submit: JobSubmit,
    sink: UnboundedSender<WorkerMessage>,
    stop: Arc<Notify>,
    replies: UnboundedReceiver<ClientMessage>,
) -> Result<()>;
```

`replies` carries `job.pay` on the same socket after an invoice.

`jobs::transport_for` picks an implementation from the peer's endpoint scheme:
`ws://` for an address someone typed, `p2p://` for a peer that was discovered,
`mock://` for the in-process dev worker. All three carry identical messages, so
the job manager, the store and the entire UI are unchanged by which one runs —
they only ever see `WorkerMessage`s. It is also why the mock worker is honest:
it is not a special case inside the job manager, it is a `Transport` like any
other.

On the worker side the equivalent seam is `Worker::announce()`, which builds
the `peer.announce` record from what the backends actually resolved. It is both
sent on connect and published into the DHT — one description of the node, two
audiences. Everything from `handle_submit` down never learns which pipe a job
arrived on.

## How discovery works

1. The worker joins through a bootstrap node and publishes provider records:
   `rootmode/cap/llm`, `rootmode/cap/image`, and `rootmode/model/<id>` for each
   model its backends actually resolved.
2. The client joins the same way and asks the DHT who provides `llm` / `image`.
   It gets back **peer ids** — keys, not addresses.
3. Each is upserted into the same peers table as manually added peers, marked
   `discovered`, then probed: the client dials it directly and reads the
   `peer.announce` off the connection. That is the authoritative answer; a DHT
   record is only a claim by whoever wrote it.
4. Jobs go straight to the peer. The bootstrap node is not in the path.

The one address a client is ever configured with is the bootstrap node's;
everything else is looked up. See [`NETWORK.md`](NETWORK.md).

## Data locations

| what | where |
|---|---|
| secret key | `<app data>/identity.key` (`0600`) |
| database | `<app data>/rootmode.sqlite` |
| image results | download dir setting, default `<downloads>/rootmode` |

`<app data>` is `~/Library/Application Support/ai.rootmode.desktop` on macOS,
`~/.local/share/ai.rootmode.desktop` on Linux, `%APPDATA%\ai.rootmode.desktop`
on Windows. None of it is inside the repo.

## Security posture

- **No shell.** The Tauri capability set is the window, IPC to our own commands,
  and `reveal_item_in_dir`. There is no `shell`, no `fs`, no `http` plugin, so
  the frontend cannot execute or fetch anything on its own.
- **Model output is never code.** Results are stored and rendered as text or
  image bytes. Nothing parses them as commands, and no job field reaches
  `Command::new` — there is no `Command::new` in the codebase.
- **Peer input is validated at the boundary.** Strict deserialisation, version
  checks, a frame size cap, and unknown message types dropped.
- **Results are content-addressed.** The peer's claimed `sha256` is recomputed
  from the bytes; a mismatch fails the job.
- **Outbound connections are user-configured only.** `ws`/`wss` with a host,
  nothing else, and only to endpoints on the peers screen.
- **File reads are mediated.** `read_result_image` and `reveal_result` accept a
  `job_id`, not a path; only files recorded in the results table are reachable.

On the worker, which is the side facing strangers:

- **Fixed workflows only.** A job selects a declared workflow and fills
  declared slots. A client cannot send a ComfyUI graph, add a node, change a
  checkpoint path, or reach an input the operator did not list. Slot paths are
  verified against the workflow at startup.
- **The client's validation is not trusted.** Payload bounds are re-checked on
  arrival; the client-side check exists to give fast feedback, not to protect
  the worker.
- **Identity is enforced, not assumed.** A signature that is present but
  invalid is always rejected. `require_signature` and `allow_peers` let an
  operator go from open to closed; an allowlist implies signature checking,
  because otherwise `from` is just a claim.
- **Concurrency is bounded globally.** One semaphore across all connections, so
  a client cannot open twenty sockets to get twenty GPU slots.

## Payments

Priced jobs lock USDC in `RootmodePot` before the GPU runs. The client's
**app key** signs 1M-token slices on submit so the worker can stream; after
the job it captures the actual bill. The **worker** then signs `settle` with
its Ethereum pay key (`ROOTMODE_PAY_KEY`) and submits the raw transaction.
Withdrawal cannot take a lock. Close cannot take `earned`. The wallet only
deposits, sets limits, and withdraws unlocked funds. 90% to the worker
payout address, 10% to FeeVault. See
[`../contracts/README.md`](../contracts/README.md).

## Deferred

Gossip, a bid step, signed announce records, and IPFS pinning. New `type`
values are ignored by v1 clients. Deliberately *not* planned: a broker every
job must pass through.
