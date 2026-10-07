//! Sink metrics

const SINK_QUEUE_BYTES: &str = "zaino.sink.queue_bytes";

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    metrics::describe_gauge!(
        SINK_QUEUE_BYTES,
        "Bytes queued for one subscriber, not yet popped, by sink and subscriber; at its budget \
         = that subscriber is holding back the publisher"
    );
}

/// Bytes one subscriber's queue holds: + on push, − on pop (exact permit counts, no sampling)
#[derive(Clone)]
pub(crate) struct QueueBytes {
    gauge: metrics::Gauge,
}

impl QueueBytes {
    pub(crate) fn new(sink: &'static str, subscriber: &'static str) -> Self {
        Self {
            gauge: metrics::gauge!(SINK_QUEUE_BYTES, "sink" => sink, "subscriber" => subscriber),
        }
    }

    pub(crate) fn pushed(&self, bytes: u32) {
        self.gauge.increment(f64::from(bytes));
    }

    pub(crate) fn popped(&self, bytes: usize) {
        self.gauge.decrement(bytes as f64);
    }
}
