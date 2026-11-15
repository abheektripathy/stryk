# RFC 0001: Stryk options on Solana

- **Product:** a wallet-native app for selling European covered calls and cash-secured puts. Writers receive USDC upfront, and positions settle by physical delivery at expiry.
- **Stack:** Rust throughout.
  - Solana program: Quasar.
  - Browser app: Dioxus, compiled to WASM.
  - RFQ service, settlement worker and maker CLI: Rust.
  - Wallet signing: Wallet Standard.
- **First market:** SOL/USDC. The program moves WSOL and USDC (legacy SPL Token), and the Pyth SOL/USD price settles each expiry.
- **Status:** working end to end on localnet and devnet. See the [README](README.md) for how to run it.

## 1. Reference product

- The interaction model follows Rysk's direct "earn" flow:
  - pick an asset and strategy,
  - pick a strike that shows its APR,
  - enter an amount,
  - see the upfront premium and both expiry outcomes,
  - confirm in the wallet.
- Rysk runs on EVM with a WebSocket maker auction. Stryk keeps that shape and implements it with original Solana code.
- Out of scope: Rysk's pooled vault product, points and leaderboard.

## 2. Options math

- **Symbols:**
  - `Q`: size in SOL
  - `K`: strike in USDC per SOL
  - `S0`: spot at entry
  - `ST`: verified price at expiry
  - `P`: total premium in USDC
- **Covered call:**
  - Writer P&L against entry value is `P + Q × (min(ST, K) − S0)`.
  - Effective sale price if assigned is `K + P/Q`.
- **Cash-secured put:**
  - Writer P&L is `P − Q × max(K − ST, 0)`.
  - Effective buy price if assigned is `K − P/Q`.
- **Premium return and APR:**
  - Premium return is `P / collateral value`.
  - APR is `return × 31,536,000 / seconds to expiry`.
  - Collateral value is `Q × S0` for calls and the strike notional for puts.
  - APR describes premium only, so the UI shows premium and both outcomes alongside it.
- **Pricing:** makers set the premium. The protocol never prices options. A deterministic demo maker exists for testing.

| Outcome | Call writer | Call maker | Put writer | Put maker |
| --- | --- | --- | --- | --- |
| At fill | Locks `Q` WSOL, gets `P` | Locks `Q×K` USDC, pays `P` | Locks `Q×K` USDC, gets `P` | Locks `Q` WSOL, pays `P` |
| In the money | Gets `Q×K` USDC | Gets `Q` WSOL | Gets `Q` WSOL | Gets `Q×K` USDC |
| At or out of the money | Gets `Q` WSOL back | Gets USDC back | Gets USDC back | Gets WSOL back |
| No valid price by deadline | Gets `Q` WSOL back | Gets USDC back | Gets USDC back | Gets WSOL back |

**Worked example (call):**
- Sell 2 SOL at a $160 strike for 6 USDC, with 7 days to expiry and spot at $145.
- At a $130 fix, the writer gets 2 WSOL back and keeps the 6 USDC.
- At a $170 fix, the writer gets 320 USDC and the maker gets 2 WSOL.
- Premium return is 2.07%, which annualizes to 107.88% APR.

## 3. Scope of v1

- **Product:**
  - one SOL/USDC market and both strategies;
  - Friday 08:00 UTC expiries, plus ad hoc expiries on test markets;
  - full-size quotes from a single maker.
- **Makers prefund both legs.** A call bid needs strike notional plus premium in USDC; a put bid needs WSOL and premium. Assignment then never depends on the maker being online.
- **Fees and features:**
  - No protocol fees. Network fees and rent are shown separately.
  - No leverage, early close, rolling, tokenized positions, AMM, orderbook or bridges.
- **Trust:**
  - The RFQ service decides which bids reach users.
  - Makers set prices and hold their own keys.
  - The program enforces escrow and settlement, and the oracle sets the fix.
  - The token issuer, the cluster and the program upgrade authority remain dependencies.

## 4. On-chain accounts

| Account | Seeds | Holds |
| --- | --- | --- |
| `Market` | `market, base_mint, quote_mint, version` | Mints, deployment domain, admin, strike tick, size bounds, expiry schedule, oracle and timeout policy, pause flag |
| `Expiry` | `expiry, market, expires_at` | `Open`, `Fixed` (price and proof) or `NoFix` |
| `MakerLane` | `maker, market, maker, lane` | Maker owner and enabled flag. Its two token accounts hold free inventory. |
| `Capacity` | `cap, lane, expiry` | Fixed strike-notional budget and amount filled |
| `Position` | `position, market, writer, rfq_id` | Both parties, terms, locked amounts, premium, outcome. Owns two escrow token accounts. |

**Amount rules:**
- Strike notional is `ceil(Q × K / 10^decimals)`, computed with checked `u128` arithmetic.
- Amounts are fixed at fill, and settlement moves exactly those amounts.
- Zero values, off-grid strikes, out-of-bounds sizes and overflow are rejected.

**Account checks:**
- Exact mints, token program, PDAs and canonical token accounts.
- The two parties must be distinct, and the client never chooses who gets paid.

**Donations:** tokens or lamports sent directly to an escrow are swept back to the party that supplied that leg. They never change the payout.

## 5. Instructions

| Instruction | Who can call | Effect |
| --- | --- | --- |
| `initialize_market`, `initialize_expiry`, `configure_lane`, `initialize_capacity` | Admin | Publish terms and caps |
| `set_market_paused`, `set_lane_enabled` | Admin | Stop new fills only. Settlement and withdrawals keep working. |
| `deposit`, `withdraw` | Maker | Move free inventory in and out of the lane |
| `fill` | Writer, with a maker-signed quote | Check terms, time, cap and balances. Escrow both legs, pay the premium and record the position, all in one transaction. |
| `fix_expiry` | Anyone | Record the single verified price for an expiry |
| `mark_no_fix` | Anyone, after the deadline | Mark the expiry as `NoFix` |
| `settle_position` | Anyone | Pay out from the recorded outcome and close the escrows |

**Timing:**
- A fill is accepted only while the quote is valid (at most 45 seconds) and before the fill cutoff (2 minutes before expiry).

**Assignment:**
- A call assigns when `ST > K`; a put assigns when `ST < K`.
- At exactly the strike, both legs are returned.

**Replay:**
- One RFQ ID maps to one position address. A second fill for the same RFQ fails, even after settlement.

## 6. Expiry price

- **Source:** a fully verified Pyth SOL/USD update from the pinned receiver. It must be the first observation at or after expiry, within 30 seconds, and its confidence must be within the market limit (100 bps).
- **Deadline:** anyone can post the fix for 24 hours after expiry. After that, both legs return to their owners and the writer keeps the premium. There is no admin price override.
- **Live spot vs. settlement price:**
  - The live spot is only for display and quoting. The service stops quoting when it goes stale.
  - The settlement price is always the verified on-chain fix.

## 7. RFQ and makers

- **Request:**
  - The browser sends market, expiry, strategy, strike, size and wallet.
  - The service assigns a random 128-bit RFQ ID and runs a two-second auction.
- **Maker connection:**
  - Makers connect over WebSocket and authenticate with a single-use challenge.
  - They need an enabled lane to bid.
- **Signed quote:** the maker signs the SHA-256 hash of a fixed 349-byte encoding covering:
  - deployment domain, program and accounts;
  - RFQ ID, both parties, both mints;
  - strategy, strike, size, premium and expiry time.
- **On-chain check:** the fill must directly follow an Ed25519 verification instruction. The program rebuilds the hash from the fill's own accounts and arguments.
- **Selection:**
  - The highest valid premium wins. Ties go to the first arrival.
  - Winning does not reserve funds. The fill rechecks everything on chain.
- **Indicative prices:** makers also publish short-lived rates for the strike grid. They power the APR previews, and the UI labels them as estimates.

## 8. Web app

- **Earn:** a strategy tab and an asset list showing cap used and max/min APR.
- **Ticket:**
  - Selectors for asset, strategy and expiry, plus live spot.
  - A strike picker showing APR and distance from spot.
  - An amount field with MAX and the wallet balance.
  - A "Now" box with APR, premium and a status log.
  - Both expiry outcomes, and one button that quotes and signs.
  - Side panels with trade details and the cap.
- **Dashboard:** total premium, a 30-day income chart, and open and settled positions.
- **Position page:** full receipt, settlement status, and a settle button when settlement is available.
- **Transaction safety:**
  - The browser rebuilds and checks the exact transaction before asking the wallet.
  - It saves the pending signature before sending, and on reload recovers status from the chain.
- **Look:** dark "Platinum" style. Classic Mac OS window chrome, IBM Plex Mono and Sans, Silkscreen for labels, a lavender accent.
- **Layouts tested:** 1440, 768 and 390 px wide.

## 9. Service

```text
Browser ── HTTPS ── Rust service ── WebSocket ── Maker CLI
   │                     │
 Wallet                  ├── Postgres (finalized positions)
   │                     ├── Solana RPC (reads, settlement worker)
   └── sign + send ───── Solana program ── SPL Token
                         └── Pyth receiver (verified price)
```

- **HTTP API:**
  - `/v1/markets` and `/v1/markets/{id}/expiries`
  - `/v1/rfqs` and `/v1/rfqs/{id}`
  - `/v1/positions?owner=` and `/v1/positions/{address}`
  - `/v1/income/{owner}`
- **Indexer:** reads finalized program transactions into Postgres and saves its checkpoint with each batch. It can rebuild from chain state.
- **Worker:** posts fixes, marks `NoFix` after the deadline, and settles positions in bounded batches. It's safe to restart at any point.
- **Deployment:** one container image serving the API and the web app. RPC and database secrets stay server-side.

## 10. Invariants

- A successful fill escrows both legs and pays the premium exactly once. A failed fill changes nothing except network fees.
- Every position settles once, to its two named parties, with its original amounts.
- The admin cannot move escrow, change existing terms, set a price or block settlement.
- Fills from different maker lanes can run in parallel. Only a lane's own accounts and its cap are shared writes.

## 11. Open decisions

- **Capital efficiency:** measure the cost of fully prefunded makers before considering an exercise window or netting.
- **Oracle policy:** confirm the 30-second window and 24-hour deadline hold up at real expiries.
- **Limits:** tune the fill cutoff, quote lifetime, confidence limit and caps against real usage.
- **More assets (JUP, PEPE):** each needs its own price feed, decimals and market. This is a program and service change.
- **Before mainnet:**
  - external program review,
  - custody plan for the admin and upgrade keys,
  - oracle checks at real expiries,
  - maker liquidity commitments.
