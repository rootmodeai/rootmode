# rootmode.ai

Static site. No build step. Public paths are `/`, `/worker`, `/explorer`,
`/protocol`, `/discovery`, `/brand` — no `.html` in the URL. Press colours
live at `/brand`. Wire format for `/protocol` is
[`docs/PROTOCOL.md`](../../docs/PROTOCOL.md); if they disagree, the Rust
types in `crates/rootmode-core` win.

```sh
cd apps/web
npm start          # http://127.0.0.1:4173
```

Waitlist POSTs to `/api/waitlist` and are appended to `data/waitlist.json`
(gitignored). On a host without that route the form still succeeds and keeps
a copy in `localStorage` under `rootmode.waitlist`.

Drop the directory on Cloudflare Pages, Netlify, or any static host. Point
`rootmode.ai` at it. To collect signups in production, keep this preview
server behind the domain or replace `/api/waitlist` with the form provider
you actually use.
