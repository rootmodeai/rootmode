# Bitcoin and Monero deposits

The app people download never holds an inventory key, a BTCPay secret, or
a Monero wallet. It only **asks how they want to pay**, connects their
**USDC wallet on Base**, and talks to an operator desk:

```
https://desk.rootmode.ai
```

(`ROOTMODE_DESK_URL` overrides that, for a local desk.)

That desk is a **private** repository. This repo only defines the HTTP
the fund page already calls.

## Status

`GET /status`

```json
{ "enabled": true, "btc": true, "xmr": true, "spread_bps": 1000, "min_usd": 10, "max_usd": 200, "btc_usd": 81245.1, "xmr_usd": 142.3 }
```

`btc_usd` / `xmr_usd` are Kraken spots when the desk can fetch them. The fund page uses them to show how much coin to send as the USDC amount changes.

## Quote

`POST /quote`

```json
{ "rail": "btc", "usd": 50, "address": "0x…" }
```

`rail` is `btc` or `xmr`. `address` is the USDC wallet on Base that
receives the credit.

Response (and `GET /quote/:id` after that):

```json
{
  "id": "uuid",
  "rail": "btc",
  "credit_usd": 50,
  "pay_amount": "0.00068000 BTC",
  "pay_address": "bc1q…",
  "pay_uri": "bitcoin:bc1q…?amount=0.00068000",
  "checkout_url": "https://btcpay…/i/…",
  "status": "pending"
}
```

`status` is `pending` → `paid` → `credited` (or `expired` / `failed`).
When `credited`, USDC has been sent to `address`. The app then deposits
into the pot as it does after a card buy.

The fund page shows a QR of `pay_uri`, the address, and `pay_amount`.
Changing the USDC amount requests a new quote (new address / amount).
