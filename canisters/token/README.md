# The token's ICRC ledger

The copy of the token that exchanges and wallets trade lives on DFINITY's
standard ICRC ledger, which implements ICRC-1, ICRC-2, and ICRC-3. It is
deployed as published, not built here, so its code is the code exchanges and
indexers already support.

| File | Source | SHA-256 |
| --- | --- | --- |
| `ic-icrc1-ledger.wasm.gz` | [ledger-suite-icrc-2026-03-09](https://github.com/dfinity/ic/releases/tag/ledger-suite-icrc-2026-03-09) | `354dd6ecfdc72b5409805b31dea22c9db11df6e14095a5a68924eb63535e6d8a` |
| `ledger.did` | the same release | `02a42599e3d2036a19dcfa53db04ad5a5271b0bd78ac1a5986b3dd361f33a99c` |

The release notes say how to rebuild the module from the `dfinity/ic`
repository and compare its hash.

## How it is installed

- The control canister is the ledger's minting account, and the only one.
  Withdrawing tokens mints copies; depositing copies burns them by sending
  them to the minting account.
- There are no initial balances: every copy is minted against a token the
  control plane moves to its bridge pool, so the ledger's total supply is
  always what that pool holds.
- Name, symbol, decimals, and fee come from `decomp_core::token`
  (`TOKEN_NAME`, `TOKEN_SYMBOL`, `TOKEN_DECIMALS`, `ICRC_TRANSFER_FEE`).
- ICRC-2 (approvals) is on, so exchanges and other canisters can move copies
  on a holder's approval.

`decomp-replica` installs it beside the control canister on a local replica
and passes its principal to the control canister.
