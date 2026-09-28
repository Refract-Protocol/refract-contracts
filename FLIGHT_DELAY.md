# FlightDelay Coverage — Feed Convention

> This document describes the on-chain oracle feed naming convention and
> contract-level support for `CoverageType::FlightDelay`. The off-chain
> relayer that actually fetches flight-status data from e.g. FlightAware or
> OAG is part of `refract-backend` and is out of scope here.

---

## Feed ID naming convention

Pattern: `FLIGHT_<CARRIER><NUMBER>_<YYYYMMDD>`

| Component | Description |
|-----------|-------------|
| `FLIGHT_` | Literal prefix identifying this as a flight feed |
| `<CARRIER>` | IATA airline code (2 letters), e.g. `DL`, `AA`, `UA` |
| `<NUMBER>` | Flight number (1–4 digits), no leading zeros, e.g. `420`, `100` |
| `_<YYYYMMDD>` | Scheduled operating date in UTC, e.g. `_20261201` |

**Examples:**
- `FLIGHT_DL420_20261201` — Delta flight 420 on 2026-12-01
- `FLIGHT_AA100_20261215` — American Airlines flight 100 on 2026-12-15
- `FLIGHT_UA33_20270101` — United flight 33 on 2027-01-01

The date suffix is **required** because the same flight number repeats every
day. Without it, a stale reading from a previous day could accidentally
trigger (or suppress) a claim for today's flight.

---

## Value convention

Oracle readings for flight feeds store the **actual delay in minutes** as an
unscaled `i128` integer. Positive values mean late; `0` means on-time or
arrived early.

| Value | Meaning |
|-------|---------|
| `0` | On-time or early |
| `90` | 90 minutes late |
| `i128::MAX` | **Cancelled** (see below) |

Unlike price/percentage feeds, flight-delay values are **not** multiplied by
`SCALE` (1e7). The pool's trigger comparison is `data.value > trigger_threshold`
where `trigger_threshold` is stored directly in minutes (e.g. `120` for 2
hours).

---

## Cancellation handling

A cancelled flight is submitted with the canonical sentinel value
`FLIGHT_CANCELLED_SENTINEL = i128::MAX` (defined in `oracle/src/lib.rs`).

**Rationale:** Cancellation is semantically distinct from "infinite delay"
but must map to a single numeric value so the pool's existing `data.value >
trigger_threshold` comparison correctly triggers a claim without any schema
change. Since `trigger_threshold` is measured in minutes and `i128::MAX`
(≈ 1.7 × 10³⁸) is guaranteed to exceed any realistic threshold, this sentinel
always triggers.

Relayer bots should:
1. Submit `FLIGHT_CANCELLED_SENTINEL` as soon as a cancellation is confirmed
   by the data provider.
2. Not submit further updates for a cancelled flight (the `StaleSubmission`
   guard in `submit` prevents older readings from overwriting it).

---

## Contract support

### `RefractOracle::register_flight_feed`

```
register_flight_feed(feed_id: Symbol, scheduled_departure: u64) → Result<(), OracleError>
```

Admin-only. Stores `FlightMeta { scheduled_departure }` for the given feed.
The metadata is used by the relayer bot to know when to start polling:
readings submitted well before `scheduled_departure` are unlikely to be
meaningful.

### `RefractOracle::get_flight_meta`

```
get_flight_meta(feed_id: Symbol) → Option<FlightMeta>
```

Returns the flight metadata, or `None` if the feed has not been registered.

### `RefractOracle::submit` (normal path)

The regular `submit` entrypoint accepts flight-delay values like any other
feed. The deviation check (`MaxDeviationBps`) applies but may be set wide (or
disabled via `set_max_deviation_bps(feed_id, 0)`) for flight feeds since a
sudden jump from 0 to `i128::MAX` (cancellation) would otherwise be rejected.

### `RefractOracle::submit_override` (admin override)

Use `submit_override` to accept a cancellation sentinel or any other large
jump that would trip the per-feed deviation cap.

---

## End-to-end claim flow

1. Admin calls `register_flight_feed("FLIGHT_DL420_20261201", departure_ts)`.
2. Policy holder calls `pool.buy_policy` with:
   - `coverage_type = CoverageType::FlightDelay`
   - `trigger_threshold = 120` (minutes)
   - `feed_id` is set by convention matching the oracle feed
3. Relayer bot monitors flight status; once departure data is available,
   calls `oracle.submit(relayer, "FLIGHT_DL420_20261201", delay_minutes, ts, source)`.
4. Anyone calls `pool.process_claim(policy_id)`.  The pool reads the oracle
   reading and evaluates `delay_minutes > 120`.  If true, the claim pays out.

---

## Out of scope

- Off-chain integration with actual flight data APIs (FlightAware, OAG, etc.)
  — this is the responsibility of the `refract-backend` relayer bot.
- N-of-M confirmation for flight claims — only standard (single) or
  dual-oracle (if `dual_confirmation_threshold` is set) confirmation applies.
