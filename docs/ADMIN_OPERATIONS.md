# Administrative Operations

Administrative batches are bounded to 16 operations per contract call. Each
contract checks its governance or timelock identity once before dispatching the
batch, and a failed operation aborts the transaction. Batch envelopes and inner
operations are emitted by the Soroban contracts so indexers can reconstruct the
requested changes.

Token rescue is restricted to live excess: the contract reads the token balance
at execution time and subtracts the liability view for that token. The rescue
recipient must be the configured governance recipient. A rescue must be
investigated with the token contract, amount, transaction hash, and accounting
liability snapshot before it is submitted through governance.

Recurring treasury schedules reserve their complete budget when created. A
keeper can execute at most one due period per call. Cancellation and exhaustion
release the unspent reserve; a schedule never authorizes spending beyond its
reserved asset balance.