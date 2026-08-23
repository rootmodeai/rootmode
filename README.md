# rootmode

p2p inference.

you hold the key. you pick the peer. the answer is a hash.

no account. no cloud. nobody in the middle reading the prompt.

[rootmode.ai](https://rootmode.ai) · [manifesto](https://rootmode.ai/manifesto) · [protocol](docs/PROTOCOL.md)

```
apps/desktop     tauri client (chat, images, local gateway)
apps/web         the site
crates/          protocol, p2p, worker
contracts/       RootmodePot — deposit, lock, settle
deploy/          the site (caddy + collector)
docs/            read these
services/stats   explorer collector (optional, centralised, honest about it)
```

## run the app

rust (stable), node 18+. linux: [webkitgtk](https://tauri.app/start/prerequisites/).

```sh
cd apps/desktop
npm install
npm run app
```

mock worker is on by default. talk to it before you talk to a stranger.

```sh
cargo test --workspace
cd apps/desktop && npm run build
cd contracts && forge test
```

## run a worker

```sh
cp .env.example .env    # vLLM and/or ComfyUI
docker compose up -d --build
```

the volume is the identity. lose it, you are a different node.
operator guide: [`docs/WORKER.md`](docs/WORKER.md).

## priced jobs (local)

```sh
./contracts/local.sh
```

then **Wallet** in the app. the worker signs `settle` with `ROOTMODE_PAY_KEY`.
[`contracts/README.md`](contracts/README.md).

## release

tag `vX.Y.Z`. CI builds mac (signed/notarized if secrets exist), windows,
linux. the site **Download** button hits `/download` and redirects to the
installer for that OS. [`docs/RELEASE.md`](docs/RELEASE.md).

## docs

| | |
|---|---|
| [`docs/MANIFESTO.md`](docs/MANIFESTO.md) | why this exists |
| [`docs/PROTOCOL.md`](docs/PROTOCOL.md) | the wire (rust types are the truth) |
| [`docs/WORKER.md`](docs/WORKER.md) | GPU node, docker, payments |
| [`docs/NETWORK.md`](docs/NETWORK.md) | discovery and bootstrap |
| [`docs/GATEWAY.md`](docs/GATEWAY.md) | cursor / claude code / vs code |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | how the pieces fit |
| [`docs/RELEASE.md`](docs/RELEASE.md) | tagging, apple, download |

MIT. your keys, your peers.
