# The network

How a client and a worker find each other without either of them being told an
address by hand.

```
                    ┌──────────────────┐
                    │ rootmode-bootstrap│   entry point + relay
                    └────────┬─────────┘   holds no directory, sees no jobs
              join / publish │ join / look up
              ┌──────────────┴──────────────┐
              │                             │
    ┌─────────┴────────┐          ┌─────────┴────────┐
    │ rootmode-worker  │          │ desktop client   │
    │ "I do llm"       │◀─────────│ "who does llm?"  │
    └──────────────────┘  direct  └──────────────────┘
                          connection — jobs never touch the bootstrap node
```

## How a node joins with nothing configured

Every peer-to-peer network has the same chicken-and-egg problem: to find peers
you need a peer. The universal answer is to **ship entry points in the
binary** — IPFS does it, Ethereum does it, BitTorrent does it. rootmode does it
in `DEFAULT_BOOTSTRAP`, in `crates/rootmode-p2p/src/lib.rs`:

```rust
pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    "/dns4/bootstrap.rootmode.ai/tcp/4001",
];
```

The peer id (`/p2p/12D3Koo…`) is optional. Including it means libp2p refuses
to talk to anything else at that address, which is stronger; leaving it out
means the address is just a DNS name you can publish and re-key without
reissuing builds. Without it a hostile node in that position could show you a
biased view of who is on the network — it still cannot read your jobs, forge a
worker, or defeat a pinned peer, because every peer authenticates with its own
key and every result is checked against its hash.

With that filled in:

- `docker compose up` joins the network and starts receiving jobs.
  Nothing to configure.
- Opening the client joins the same network and lists what people are serving.
  Nothing to configure.

**This list is empty until you run bootstrap nodes and put their addresses in
it.** Until then there is no open network to join — only the local-network
discovery below. That is an operational step, not a code one: stand up one or
more of these on stable public addresses, then paste what they print into the
constant and rebuild.

`ROOTMODE_BOOTSTRAP` (comma-separated) overrides the list at runtime for both
binaries, which is how you test a private network without touching the source.

Nothing about this makes the network centralised. Entry points answer "who else
is here", relay for nodes behind NAT, and never see a job. Anyone can run one,
users can override the list, and once a node has met a few peers it does not
need them again.

## On your own network, nothing is configured

Start a worker. Open the client. The worker appears. That is the whole
procedure — no addresses, no bootstrap node, nothing pasted.

Both announce themselves over **mDNS**, the same mechanism that makes printers
and Chromecasts show up. The client reacts to the announcement the moment it
arrives rather than polling for it, so a worker that starts while the app is
open appears within a second.

This covers the common case completely: your machines, one network.

## Beyond your own network

mDNS stops at the LAN. To find peers further away you need either

- **an address** — a worker with no bootstrap configured is itself an entry
  point, so paste the address it logs into the client's settings, or
- **a bootstrap node** — worth running once several machines need to find each
  other without being told about each other, or when a worker is behind NAT
  and needs something to relay for it.

```
INFO this node is its own entry point — give a client one of:
INFO   /ip4/192.168.1.50/tcp/4101/p2p/12D3KooWR3Tjk...
```

## What the bootstrap node is

An ordinary peer with a stable address. You dial it once to join; it tells you
about peers near you, and from then on lookups are peer-to-peer. It is:

- **not a registry.** Advertisements are stored across the DHT by key, not
  handed to it for safekeeping. It holds the ones that happen to land near its
  node id, like any other participant.
- **not an authority.** It cannot approve, deny, or evict anyone.
- **not in the data path.** Once two peers have found each other they connect
  directly and the bootstrap node is not involved in the job at all.

It does one extra job: it runs a **relay**, so a worker that cannot accept an
inbound connection (behind NAT, no port forwarding) is still reachable. Even
then it only brokers the connection — the two peers then attempt a direct
upgrade (hole punching) and the traffic moves off the relay.

Run more than one. Any node can be a bootstrap node, and clients accept a list.
If every bootstrap node vanishes, running peers keep working; only *new* joins
are affected.

## Running one

With Docker:

```sh
docker build -f docker/Dockerfile.bootstrap -t rootmode-bootstrap .

docker run -d --name rootmode-bootstrap \
  -p 4001:4001 \
  -v rootmode-bootstrap:/var/lib/rootmode \
  rootmode-bootstrap \
    --listen /ip4/0.0.0.0/tcp/4001 \
    --key /var/lib/rootmode/bootstrap.key \
    --announce /dns4/bootstrap.example.com/tcp/4001

docker logs rootmode-bootstrap    # prints the address to hand out
```

**Mount the volume.** It holds `bootstrap.key`, and the peer id is derived from
it. Lose the key and the address everyone is configured with stops working.

Or from source:

```sh
cargo build --release -p rootmode-p2p     # builds rootmode-bootstrap
rootmode-bootstrap --listen /ip4/0.0.0.0/tcp/4001 \
                   --announce /dns4/bootstrap.example.com/tcp/4001
```

It prints the address to hand out:

```
peer id   12D3KooWEYBDUKv5KLXV2oL3TgLjU4HypJ9KVv7DPjUo5u5gHCCV
give this to workers and clients:
  /dns4/bootstrap.example.com/tcp/4001/p2p/12D3KooWEYBDUKv5KLXV2oL3TgLjU4HypJ9KVv7DPjUo5u5gHCCV
```

Add the `/p2p/<peer id>` suffix when you want the node verified on connect;
without it any address that answers on that host and port is accepted.

`--announce` is what gets printed for others to use. Without it the node
prints the addresses it bound to, which inside a container are the container's
own and no use to anyone outside.

Open the port. TCP, whatever you passed to `--listen`.

## Configuring a worker

```toml
[p2p]
enabled = true
bootstrap = ["/dns4/bootstrap.example.com/tcp/4001/p2p/12D3Koo..."]
listen = ["/ip4/0.0.0.0/tcp/4101"]
relay = true        # leave on if this box is behind NAT
dht_server = false  # turn on only with a public address
```

On start the worker joins, then publishes two kinds of record:

- `rootmode/cap/llm`, `rootmode/cap/image` — what it can do
- `rootmode/model/<id>` — each model it actually serves

Those come from the backends, so a worker advertises what its vLLM or ComfyUI
really has, not what the config wishes for. It republishes periodically, so a
node that stays up stays findable.

## Configuring a client

Settings → discovery → paste the bootstrap address, one per line. The client
then asks the network who serves `llm` and `image` every 90 seconds, and any
peer it finds appears on the peers screen marked **discovered**, alongside
peers you added by hand.

Discovery only gives you a **key**, never an address — the network resolves the
key to a route. That is why discovered endpoints look like
`p2p://<64 hex characters>` rather than a host and port.

## Identity is the same key everywhere

A rootmode peer id is a hex ed25519 public key. A libp2p PeerId is a multihash
of the same key, and because ed25519 keys are short, libp2p embeds the key
rather than hashing it. So the two are the same identity in two encodings:

```
peer_id   ef9084b8a01c18be4b75fb7e0b72b7b4d57b5be99dddf2c430e4db01a43ea8a7
libp2p    12D3KooWRwXRPuPux1G6SZaxcGobMHx4qbcZmXfjPGgcPQUbk8dC
```

This matters because it means **pinning still works over discovery**. Paste a
peer id into a peer's "public key" field and a connection to that peer is
checked twice: the libp2p connection is cryptographically bound to the key, and
the announce it sends must match. Either mismatch refuses the connection.

## What discovery does not give you

Anyone can advertise anything. A DHT record is a claim, not a credential — a
node can announce `llama-3.1-405b` and serve nonsense, or nothing. Discovery
gets you a list of candidates; it does not get you trust.

What helps today: pin the keys of peers you actually trust, and use the
worker's `allow_peers` on the other side. What is not built yet: signed result
receipts, reputation, and any notion of payment. Treat a discovered peer as a
stranger, because it is one.

## Troubleshooting

**Client finds nothing.** Check the worker logged `announcing [...] to the
network`, and that both point at the same bootstrap address including the same
`/p2p/` suffix. Publishing needs a moment after start — the worker waits for
the bootstrap connection before its first announce.

**Discovered but offline.** The client found the advertisement but cannot reach
the node. If the worker is behind NAT, set `relay = true` and make sure the
bootstrap node is reachable from both sides. In Docker with published ports
rather than `--network host`, the worker advertises its container address —
set `ROOTMODE_P2P_EXTERNAL` (or `[p2p] external`) to the address others should
actually dial.

**Discovered, online, but the wrong peer.** That is `mismatch`: the key you
pinned is not the key that answered. Do not send it a job.
