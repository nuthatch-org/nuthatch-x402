# nuthatch-x402

`nuthatch-x402` is an optional x402 payment sidecar for a hosted Nuthatch nest.
It prices Nuthatch's bounded named-query surface, `GET /q/<name>`, then forwards
only after a facilitator has verified and settled the payment.

It does **not** put payment accounts, merchant wallets, or metering into the
Nuthatch binary. A local Nuthatch remains local and free to query.

```text
buyer or x402-aware MCP bridge
              |
       nuthatch-x402
  x402 headers, pricing, facilitator
              |
      Nuthatch GET /q/<name>
```

## Why named queries

Do not sell arbitrary `GET /sql`. Its cost and meaning are unconstrained.
Nuthatch's `queries.toml` already describes a typed, bounded public surface;
those names are the billable resources here.

## Current state

The first slice implements and tests the HTTP resource-server boundary:

- an unpaid request receives `402 Payment Required`, base64 JSON in the
  `PAYMENT-REQUIRED` header, and a JSON payment-requirements body;
- a request with `PAYMENT-SIGNATURE` reaches the Nuthatch backend only after
  the facilitator boundary accepts it;
- a rejected signature remains a `402`, never an accidental backend call.

The production binary deliberately uses `UnconfiguredFacilitator`, which
refuses every payment signature. It is safe to run but cannot collect payment
until a real x402 facilitator adapter is configured. No mock proof is accepted
outside tests.

The design starts with `exact`. A session-credit token is application credit,
not x402 batching. For repeated agent calls, x402 EVM `batch-settlement` is the
proper next scheme: the buyer funds a channel once, signs vouchers per call,
and the seller settles in batches.

## Run

```sh
cp x402.example.toml x402.toml
# Set provider.pay_to and point upstream at a running Nuthatch instance.
cargo test
cargo run -- x402.toml
curl -i 'http://127.0.0.1:8402/q/latest_transfers'
```

The final command returns `402` until a real facilitator adapter is configured.
It must not be exposed publicly until then.

## Configuration

`x402.toml` is operator policy, deliberately separate from a nest's
content-addressed inputs:

```toml
[provider]
upstream = "http://127.0.0.1:8288"
network = "eip155:84532"
asset = "0x036CbD53842c5426634e7929541eC2318f3dCF7e"
pay_to = "0xYourMerchantAddress"

[[query]]
name = "latest_transfers"
price_atomic = "2000"
description = "The latest indexed transfer events"
```

`network` is a CAIP-2 identifier. `asset` is the ERC-20 contract address.
Amounts are strings in atomic token units, matching the x402 wire format.

## Security boundary

A real facilitator adapter must verify and settle the payment payload against
the selected requirement, bind it to the resource, and reject replayed payment
identifiers. Never accept a client-supplied amount, infer settlement from a
transaction hash without verification, or place bearer credentials in URLs.

For an agent bridge, enforce merchant, asset, network, tool, and spend-limit
policy in code before its wallet signs. Model instructions are not a spending
policy.
