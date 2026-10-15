# Observability: metrics

How Zaino measures itself, and the rules that keep the measuring from becoming
a cost or a leak. [`logging.md`](logging.md) covers log configuration; this
covers metrics. Each emitting crate's `usage.md` lists its own series and links
here for the rules.

## Principles

- **Opt-in.** Metrics compile in only with a crate's non-default `prometheus`
  feature, enabled from `zainod`'s. A build without it carries no metrics code.
  At runtime the exporter starts only when `metrics_endpoint`
  (`ZAINO_METRICS_ENDPOINT`) is set.
- **Nothing identifying.** A label value comes from a fixed table compiled into
  the binary: a method or port name, a status or outcome, a placement. Never a
  client address, a transparent address, a txid, a block hash or height, or any
  other request argument. A value outside the table is reported as `unknown`,
  never as itself.
- **Bounded cardinality.** Because every label comes from a fixed table, the
  number of series is fixed at compile time however clients behave.

## Layers

Each layer answers a question the others cannot. Together they decompose one
slow call into where its time went.

| Layer | Question | Hook | Prefix | Status |
|---|---|---|---|---|
| Wire | What did the client get: status, time to first message, duration, messages, bytes, hang-ups? | `zaino-lightserve` tower layer around the gRPC service; close-out when the response body drops | `zaino.grpc.*` | built |
| Read | Which read, under which placement, with which outcome, and how long? | every `EngineSnapshot` port method in `zaino-core`; each owns a static `Read` | `zaino.read.*` | built |
| Source port | How long did a trip to the validator take, retries included, and how did it end? | `ValidatorClient::with_retry`, the funnel every resilient port goes through; each generated port method owns a static `Port` | `zaino.source.*` | built |
| Transport | What did each JSON-RPC call cost on the wire? | the `zaino-rpc` client | `zaino.rpc.outbound.*` | built |

A read that calls other reads (`block_header` reads the block, `block_deltas`
composes several) records itself and each read it makes, so sum durations per
read, not across reads. Streams are timed at the wire, not as reads: a stream's
duration is set by its consumer's pace.

A wire call minus the reads behind it is encoding and network. A passthrough
read minus its source trips is our own overhead. A source trip with more than
one attempt is retries, not the validator.

## Conventions

- **One `emit` module per emitting crate.** It holds the metric names as
  constants, `describe_metrics()` for the `# HELP` text, `METRIC_BUCKETS` for
  the histogram edges, and one function per measurement. Call sites never use
  the `metrics` macros directly.
- **Explicit buckets.** `zainod` installs every crate's `METRIC_BUCKETS`.
  Without them a histogram is exported as a summary, which cannot be aggregated
  across instances. Edges span what the measurement ranges over: a cached read
  is microseconds, a cold one tens of milliseconds, a full-chain stream minutes.
- **Names are dotted at the emit site** (`zaino.grpc.requests_total`) and
  `_`-joined once scraped (`zaino_grpc_requests_total`).

## Hot-path budget

Instrumentation must not show in the throughput or CPU it is measuring. The
rules that keep it there:

1. **Instrument at the unit of work:** a request, a read, a validator trip, a
   sync batch. Never inside a loop over notes, receives, transactions or
   blocks.
2. **Resolve handles once.** A `metrics::counter!(name, labels)` call looks up
   the registry each time (a hash lookup, plus an allocation for a dynamic
   label). Take each series' handle once, keep it in a `OnceLock` or a static
   table indexed by label slot, and only increment or record on the hot path.
3. **Never format per call.** Label values are `&'static str` from the fixed
   tables.
4. **No per-request logs or spans on the serving path.** Formatting, the
   subscriber and an attached exporter (OTLP) cost microseconds and may lock;
   log per window or per failure class, not per request.

### Assumptions behind the budget

Estimates unless marked measured; recheck them when the hot path changes.

| Assumption | Value | Basis |
|---|---|---|
| Clock read (`Instant::now`) | ~20–30 ns | estimate (vDSO) |
| Increment or record on a resolved handle | a few to a few tens of ns | estimate (atomic) |
| Wire layer per call | under 1 µs: two clock reads, about six records, a walk of the 5-byte message prefixes | estimate |
| Server time per `GetTreeState` | ~0.29 ms mean, 94% under 0.5 ms | measured, `zaino.grpc.first_message_seconds`, 32 clients, 2026-10-15 |
| Wire layer as a share of that call | ~0.3% | derived from the two rows above |
| Shared-counter contention | negligible below ~10⁶ updates/s across cores; we run ~10⁵ | estimate |
| Exporter memory | bounded: the recorder's upkeep folds buffered histogram samples into buckets | property of `metrics-exporter-prometheus` installed with `install()` |

## Checking that it holds

1. **A/B, same commit, same load.** Build with and without `prometheus`, run
   the same scenario, and compare **server CPU-seconds per request** (from the
   container's cgroup `cpu.stat`) as well as throughput. CPU per request moves
   before latency does. A difference beyond run-to-run noise is a regression.
2. **Flamegraph under load.** `metrics`, `tracing` and `Instant` frames should
   total well under 1% of samples.
3. **Cost per record.** A micro-benchmark of each emit function keeps an
   unresolved handle or a formatted label from slipping in later.
4. **Scaling.** If throughput stops scaling with cores while CPU is spare, look
   for false sharing on hot counters (`perf c2c`).

## Open

- **The exporter runs on the serving runtime.** A scrape under overload
  competes with the requests it is measuring. Serving `/metrics` from its own
  thread and runtime removes that.
- **Tier detail.** The read layer labels a read `local`, `passthrough` or
  `composed`; which local tier (finalised store or chain-head window) answered
  is not yet recorded.
