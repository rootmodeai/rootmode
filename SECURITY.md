# Security

rootmode moves money: a client deposits USDC into the pot contract on Base,
workers are paid from it, and the desktop app holds the key that signs
those payments. Bugs here can cost people real funds, so please report
them privately first.

## Reporting

Use GitHub's private vulnerability reporting on this repository
(**Security → Report a vulnerability**), or email
echelonresearch@protonmail.com. Include what you found, how to reproduce
it, and what you think the impact is. You will hear back within three
days.

Please do not open a public issue for anything that could be exploited —
the contracts are live and immutable, and a disclosed bug is one that can
be used before it can be fixed.

## Scope

- `contracts/` — the Vyper pot and fee vault deployed on Base
- `crates/rootmode-worker` — payment authorisation, settlement, the bill
- `apps/desktop` — the wallet, the app key, the local endpoint token
- `crates/rootmode-p2p` — peer identity and job signatures

Things that are out of scope: the seed fleet's choice of upstream
provider, rate limits on public RPC endpoints, and anything requiring a
compromised machine.

## What happens next

A confirmed report gets a fix, a note in the release, and credit if you
want it. Contract bugs may need a new deployment and a migration; those
take longer and we will say so.
