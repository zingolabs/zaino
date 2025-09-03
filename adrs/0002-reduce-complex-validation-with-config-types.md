# 2. reduce complex validation with config types

Date: 2025-09-03

## Status

In Discussion

## Context

We have complex multi-step config validation, that is difficult to parse and may leave some
configurations without validation.

## Decision

Use a more comprehensive type that requires a valid state for instantiation.

## Consequences

What becomes easier or more difficult to do and any risks introduced by the change that will need to be mitigated.
