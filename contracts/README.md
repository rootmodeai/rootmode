# The money layer

Contracts are **Vyper 0.4.3** (`src/*.vy`). Foundry tests and the local
deploy script stay in Solidity and call them through `src/interfaces.sol`.
`vyper` must be on `PATH` (`pipx install vyper==0.4.3`).

Clients deposit USDC once. The wallet deposits, sets caps, and withdraws
**unlocked** funds. Work is paid from a per-worker lock the client's **app
key** signs — a forked client cannot skip that by editing JavaScript. The
**worker** submits the `settle` transaction (and pays its gas) with its own
Ethereum key, so it does not need the client online to *collect* — but the
amount is still authorised by the client's app-key ticket, which the contract
verifies. The caps a job is checked against live on the channel, not the account:
lowering the account's limits cannot block a settle for work already
delivered. They rise to the account's on any `reserve`, and a `close` lets
them start afresh from the account, lower included.

```
deposit → reserve (lock) → stream against prepaid slices → capture actual
                                                     ↘ if silent, worker signs settle
90% worker payout / 10% FeeVault
```

**`FeeVault`** is admin-only. Until the project token is live, withdraw
collected USDC. `setBuyToken(true)` turns on epoch buybacks of that token.
The deployer is admin.

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
`0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`.

```sh
BASE_RPC_URL=https://mainnet.base.org PRIVATE_KEY=0x… ./contracts/deploy-base.sh
```

That writes `deployments/base.json` and bakes the addresses into the next
desktop build (`apps/desktop/src-tauri/chain.base.json`). Point workers at
the same pot (`ROOTMODE_POT`, `ROOTMODE_RPC`, `ROOTMODE_PAY_KEY`). The
worker's pay key is a normal Ethereum key and needs ETH for gas.

`FEE_BPS = 1000` is a constant. Changing it is a new contract.
