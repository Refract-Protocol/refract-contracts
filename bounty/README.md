# refract-bounty

Governance-controlled bug bounty escrow with commit-reveal disclosure
submission (issue #251).

## For reporters: submitting a disclosure

Everything you send on-chain is public. Don't post vulnerability details in
plaintext. Commit to a hash first, and reveal the details only when the payout
is being adjudicated.

1. **Pick a bounty.** `get_bounty(bounty_id)` shows the reward and the
   deadline. Commitments are accepted until that deadline.
2. **Build the commitment locally.** Use a fresh 32-byte random nonce for
   every commitment, and never reuse one.

   ```text
   commitment = sha256(
       b"refract.bounty.commit.v1"
    || u32_be(len(payload)) || payload
    || nonce                                  # 32 bytes
    || xdr(ScVal::Address(your_address))
    || u64_be(bounty_id)
   )
   ```

   Python (`stellar-sdk`):

   ```python
   import hashlib, os, struct
   from stellar_sdk import Address

   def commitment(payload: bytes, nonce: bytes, reporter: str, bounty_id: int) -> bytes:
       assert len(nonce) == 32
       pre = b"refract.bounty.commit.v1"
       pre += struct.pack(">I", len(payload)) + payload
       pre += nonce
       pre += Address(reporter).to_xdr_sc_val().to_xdr_bytes()
       pre += struct.pack(">Q", bounty_id)
       return hashlib.sha256(pre).digest()

   nonce = os.urandom(32)  # keep this secret until you reveal
   ```

   JavaScript (`@stellar/stellar-sdk`): the address part is
   `new Address(reporter).toScVal().toXDR()`.

   Don't compute the commitment by simulating a contract call against a public
   RPC node. Simulation sends your payload to that node's operator.
3. **Commit.** Call `commit(reporter, bounty_id, commitment, expiry)`.
   `expiry` is your reveal deadline. It must be in the future and no later
   than the bounty deadline. If governance has set a commit stake
   (`commit_stake()`), that amount is transferred from you now and returned
   when you reveal or when the commitment expires.
4. **Reveal.** Call `reveal(reporter, bounty_id, payload, nonce, locator)`
   no later than `expiry`. The contract recomputes the hash. A wrong payload,
   nonce, address or bounty id is rejected with `CommitmentNotFound`. What
   gets stored depends on `reveal_policy()`:
   - `StorePayload`: the payload itself, at most 1024 bytes. Pass
     `locator = None`.
   - `HashWithLocator`: only `sha256(payload)` plus your `locator` (for
     example a URL to an encrypted report), at most 256 bytes. The payload
     still appears in the reveal transaction's arguments.
5. **Get paid.** Governance pays revealed commitments with
   `award_commitment`. A commitment that was never revealed cannot be paid.

If you miss your deadline, anyone can call `expire_commitment(reporter,
commitment)` to close it and return your stake. After that, the commitment
can't be revealed.

### Why the hash includes your address and the bounty id

If someone copies your commitment from the ledger and commits it under their
own address, they can't reveal it, because the hash is bound to you. Their
copy doesn't block your entry either: commitments are stored per
`(reporter, commitment)`. Including the bounty id means a commitment can't be
replayed against a different bounty.

## For governance

| Entrypoint | Who | Effect |
|---|---|---|
| `create_bounty(caller, reward, deadline)` | governance | Moves `reward` from the free pool into the bounty's escrow |
| `award_commitment(caller, reporter, commitment, amount)` | governance | Pays a `Revealed` commitment; the unpaid part of the reward goes back to the pool |
| `expire_bounty(bounty_id)` | anyone | After `deadline + 30 days`, returns an unpaid reward to the pool |
| `set_commit_stake(caller, stake)` | governance | Stake required on `commit` (default 0) |
| `set_reveal_policy(caller, policy)` | governance | `StorePayload` (default) or `HashWithLocator` |
| `award(caller, researcher, amount, report_hash)` | governance | Legacy off-chain flow; rejects hashes registered through `commit` |

Token accounting: the contract's token balance always equals
`balance()` + the rewards of `Open` bounties + the stakes of `Committed`
commitments.
