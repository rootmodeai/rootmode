# The money layer

Contracts are **Vyper 0.4.3** (`src/*.vy`). Foundry tests and the local
deploy script stay in Solidity and call them through `src/interfaces.sol`.
`vyper` must be on `PATH` (`pipx install vyper==0.4.3`).

Clients deposit USDC once. The wallet deposits, sets caps, and withdraws
**unlocked** funds. Work is paid from a per-worker lock the client's **app
key** signs — a forked client cannot skip that by editing JavaScript. The
**worker** signs `settle` with its own Ethereum key so collection does not
depend on the client.

```
deposit → reserve (lock) → stream against prepaid slices → capture actual
                                                     ↘ if silent, worker signs settle
90% worker payout / 10% FeeVault
```

Nothing about a job reaches the chain: no prompt, no answer, no model name.

**`RootmodePot`** is what the desktop uses. Spend tickets are cumulative.
`close` returns `reserved − earned`; billed work stays for `collect`.

**`RootmodeChannels`** is the earlier session-auth design. Digest parity
tests still pin it; new work goes to the pot.

## Local chain

```sh
./contracts/local.sh
```

Anvil 31337, mock USDC, FeeVault, RootmodePot. Writes
`~/.rootmode/local-chain.json`. Then: MetaMask → Localhost 8545 → import the
printed key → in the app, **Wallet** → deposit.

## Tests

```sh
cd contracts && forge test
```

`ForkE2E` hits live Base USDC for the channels contract.

Production settlement is Base (`8453`), USDC
`0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`. Point the desktop chain
config and the worker (`ROOTMODE_POT`, `ROOTMODE_RPC`, `ROOTMODE_PAY_KEY`)
at a deployed `RootmodePot` / `FeeVault`. The worker's pay key is a normal
Ethereum private key and needs ETH for gas.

`FEE_BPS = 1000` is a constant. Changing it is a new contract.
