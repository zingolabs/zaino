# Validator readiness is owned by the runtime, not by its source consumers

## Status

proposed (number provisional until merge; 0012/0013 in flight via #1461)

## Context

The source seam already separates a validator's *domain answer* from a
*transport failure* (`QueryError::Domain` vs `Fetch`), and ADR-0008's resilient
decorator — now `ValidatorClient` (#1459) — owns retrying transient transport
blips. #982 records that consumers must not define validator connection retries.

A second, distinct concern is unowned: validator **bootup/readiness** — whether
the validator process is up and serving at all. Today the source consumers
handle it ad hoc — chain-head runs its own backoff ladder, chain_index its
sync-loop retries, and `zaino-state` waits out warm-up with `while get_chain_tip
{ sleep }` loops. This conflates lifecycle readiness with transient resilience
and scatters it across consumers, which can then disagree about one external
fact. ADR-0011 already made chain-head fail fast on sustained startup
unavailability, but did not say who *above* it owns readiness.

## Decision

Source consumers (chain-head, mempool, chain_index, and future ones) **assume
the validator is already booted and responsive**. They do not poll for, wait
out, or retry validator readiness ad hoc. Two concerns, two owners:

- **Transient transport blips** → the resilient source port (`ValidatorClient`,
  ADR-0008 / #1459): a short retry ladder, transparent to consumers.
- **Validator readiness / liveness** → the overarching runtime (driving-port +
  runtime composition, #1418), which confirms the validator is up and responsive
  **before** invoking the components and gates their interaction on it.

Validator readiness is modeled as first-class status via a **dedicated validator
stateful component** that owns the validator's lifecycle status (the "validator"
member of the status orchestra); the runtime reads it to gate startup. Consumers
may observe that status but never drive or retry readiness themselves.

## Considered alternatives

- **Each component owns validator-readiness** (status quo): duplicates the logic,
  lets consumers disagree, and conflates lifecycle with transient resilience.
- **Fold readiness into the retry ladder** (retry warm-up as if transient): the
  blip-tuned ladder cannot wait out a multi-minute bootup, and warm-up is a
  lifecycle state, not a transport failure (cf. the `IN_WARMUP`-in-`is_retryable`
  conflation).
- **A readiness typestate on the shared client**: rejected for the same reasons
  #498 rejected typestate lifecycle on the shared status cell (fights
  Arc/async/multi-subscriber); readiness is better a status the runtime reads.

## Consequences

- Together with the OneShot→resilient port migration (#1485), consumers shed both
  their backoff ladders (transience) and their readiness handling (bootup) — they
  just query, assuming a live, serving validator.
- The `spawn_rpc`/`spawn_direct` warm-up loops move out of `zaino-state` into the
  runtime's readiness gate; the current asymmetry (`spawn_direct` has no gate)
  disappears.
- Requires the runtime (#1418) to own a validator-status component and sequence
  startup on it.

## Open questions (must be resolved to accept)

- Dedicated validator-status component, or a status cell on the existing
  `ZebraValidatorSource` (which already holds the validator, driven by its own
  runtime)?
- Is the gate a typestate handle (mirroring #1418's `NfsView`) or a runtime
  `await` before startup?
- What does "ready" mean — answering at all, serving a block, or caught up?
- Interaction with ADR-0011's component-level fail-fast: is sustained
  validator-down at boot a runtime-level fail-fast that mirrors it?

## Related

ADR-0008 (source ports / resilient decorator), ADR-0011 (chain-head separation /
fail-fast startup), ADR-0013 (block representation / passthrough). #1459
(`ValidatorClient`), #982 (retries not in consumers), #1418 (runtime /
driving-port), #1485 (OneShot→resilient migration).
