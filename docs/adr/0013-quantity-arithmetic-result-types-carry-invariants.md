# Domain quantity types carry invariants on results, not on operators

## Status

proposed

## Context and decision

`Zatoshis` is a validated amount: at most the money supply. It is summed in
more than one place, and the sums do not share an invariant. A sum of balances
that exist at one moment cannot exceed the supply; a sum of movements — outputs
paid to an address, inputs it spent — counts the same coins each time they move
and is not bounded by the supply at all. The element type and the `+` sign are
identical in both cases; only the *meaning of the total* differs.

The earlier code expressed this with a single amount type, a supply cap applied
to every sum, and a bare signed type whose constructor validated nothing. Two
faults followed from putting the bound in the wrong place. A legitimate movement
total past the supply was rejected as if corrupt, because the cap sat on the
operator rather than on the result. And a signed value off the wire could be any
integer, because the type's "a balance change" claim was made in prose while its
only constructor enforced nothing.

The correction is a doctrine about primitive quantity types, illustrated here on
`Zatoshis` and meant to generalise:

1. **A quantity is not always closed under its own operation.** Two supply-sized
   amounts can sum past the supply, so summing amounts as flow cannot honestly
   return the same type; that result is a *different* type. We do not give
   `Zatoshis` an unconditional addition that pretends otherwise. Whether a sum
   is closed is decided by the meaning of the total, not by the `+` sign — the
   type family below has one sum that is.

2. **The invariant belongs to the result type, chosen by provenance — not to the
   operator or the element.** The same `Zatoshis` values summed as flow yield an
   unbounded accumulator; summed as coexisting balances they yield a
   supply-bounded total. The caller, who knows which the values are, picks the
   landing type, and that choice is where the bound is declared and enforced —
   once, in the type, not re-derived at each call site.

3. **A signed zatoshi value is its own type, bounded by ±supply.** A single
   movement is one amount; a change in a balance is a difference of two. A
   balance moves within `[0, supply]`, so its change lives in `[-supply, supply]`,
   and a single amount cannot exceed the supply either — so both are the same
   quantity, a *signed* zatoshi value, genuinely distinct from an (unsigned)
   amount. It is its own type, `SignedZatoshis`. "Difference" is then an
   *operation* on flow sums (`ZatoshisFlowSum::net`), not the type: a delta is
   a signed value obtained by subtracting, a movement one obtained by parsing.
   The type names what it is; the provenance names how it was obtained.

4. **No unchecked door.** A result type's guarantee comes from the absence of any
   constructor that skips its check, not from having a single constructor. Two
   validated provenances are expected: a value *derived* inside the domain (the
   difference of two flow totals) and a value *parsed at a boundary* (a field
   read off the wire or disk). Deserialisation is a real, unavoidable second
   door — it simply has to validate like the first. That boundary parser is the
   same external-input validation step this codebase already names at every wire
   and persistence boundary (ADR-0007, ADR-0008), one layer further down, at the
   primitive.

5. **Operations that relate several types are relations, not methods of one
   type.** Summing lands `Zatoshis` in an accumulator; the `net` relation
   subtracts a spent flow from a received one and lands the result in a signed
   value. These belong together in an arithmetic module beside the types, which
   is also where the allowed operations — the algebra — are written down as the
   specification. A new summation site inherits that algebra instead of
   reinventing a raw wide integer. `net` also shows that an operation can carry a
   precondition, not just a bound: its ±supply result holds only because the two
   sums are the received and spent flow of one balance, so their difference is a
   balance change. A generic subtraction of unrelated flow sums is not
   supply-bounded and is deliberately not offered — no `impl Sub` — so the bound
   is never claimed where it does not hold.

6. **Name by intent, not representation.** A checked constructor is `try_new`,
   not `try_from_i64`: the input width is incidental and would date the name; the
   intent — construct, checked — does not.

## The type family

`zaino-primitives::types::zatoshis`:

- `Zatoshis` — an amount held. `0 ..= supply`.
- `ZatoshisFlowSum` — an accumulation of movements. Bounded only by machine
  representability; deliberately not by the supply.
- `SignedZatoshis` — a signed value: a directional movement, or the difference
  of two totals. `-supply ..= supply`.

`ZatoshisFlowSum` earns a distinct type by carrying a new invariant.
`SignedZatoshis` earns one by being a different quantity. A sum of coexisting
balances earns neither: balances that coexist at one moment cannot total more
than the coins that exist, so the total is itself in `[0, supply]` and
`Zatoshis` is closed under that sum. A distinct type is warranted only when a
result escapes the element's invariant; this one does not. What the algebra
gains is its second accumulate as an *operation* — `Zatoshis::sum_balances`, a
supply-capped checked fold landing back in `Zatoshis`. Under its coexistence
contract a total past the supply is not a large number but evidence that the
operands overlap or double-count, so the fold refuses it.

## Considered options

- **Keep the supply cap on every sum.** Rejected: it sits on the operator, so it
  rejects legitimate movement totals as corruption. The bound is a property of
  certain results, not of addition.

- **One signed amount type with an infallible constructor, validated only where
  convenient.** Rejected: the invariant lives in prose, and any caller can
  construct an out-of-range value. A type that claims a bound must have no door
  that skips it.

- **A bare wide integer for the running total.** Rejected: it is an unnamed
  quantity carrying its rules in a comment — the same failure the doctrine
  removes one level up.

## Consequences

- The bound is enforced at result construction, so a movement total past the
  supply is a valid answer and only an impossible *balance change* is refused.

- A boundary parser stays, but validates; there is no infallible constructor on
  a bounded type.

- The next primitive quantity with more than one summation context has a worked
  pattern to copy: non-closure, result-typed invariants, a checked boundary
  door, and an arithmetic module that holds the algebra.

## Related

- ADR-0007 — block persistence is a row-set boundary.
- ADR-0008 — source ports over domain primitives: named conversions at every
  boundary, no type serving two roles. This ADR applies the same doctrine to
  arithmetic within a primitive.
