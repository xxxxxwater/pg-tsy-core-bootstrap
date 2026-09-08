# Strategy and position state machine

Signals do not submit orders directly. A deterministic state machine converts a valid signal into an `OrderIntent`; risk still has final authority before execution.

```text
                 +------------------+
                 |       Flat       |
                 +---------+--------+
                           |
              entry signal + risk eligible
                 +---------+---------+
                 |                   |
                 v                   v
          EnteringLong         EnteringShort
                 |                   |
              fill                 fill
                 |                   |
                 v                   v
               Long                Short
                 |                   |
             exit signal         exit signal
                 |                   |
                 v                   v
           ExitingLong          ExitingShort
                 |                   |
              fill                 fill
                 +---------+---------+
                           v
                          Flat
```

At any exposure-increasing point, operational uncertainty can move the strategy to `SafeHold` or `Unknown`. Neither state may create new exposure.

## Invariants

- Entry intents use `ExposureEffect::Increase`.
- Exit intents use `ExposureEffect::ReduceOnly`.
- One strategy state machine owns one strategy/asset/venue tuple.
- A strategy never adopts a manual position based on symbol/side alone.
- Duplicate signals are ignored by signal id.
- An open/pending intent blocks a second entry/exit intent.
- `Unknown` requires reconciliation before returning to a tradable state.
- `SafeHold` blocks new entries; an externally initiated emergency/reduce-only path may still flatten owned positions.

The state machine creates *intent*, not venue orders. `pg-risk` and `pg-execution` remain separate gates.
