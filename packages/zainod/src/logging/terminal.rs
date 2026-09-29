//! Terminal log line:
//! `INFO  [09-25|12:00:30.002] ZainoSync:           Syncing blocks                height=1,730,091 bps=2,853`
//!
//! - Level tag, `MM-DD|HH:MM:SS.mmm` UTC, nearest [`COMPONENT`] span, message padded to
//!   [`MESSAGE_WIDTH`] when fields follow
//! - Fields `key=value`: integers ≥ 1,000 grouped, `%` fields as displayed, a value with a space
//!   or `=` quoted, a 64-hex hash shortened
//! - Enclosing spans' fields follow the event's own

use std::fmt::{self, Write as _};

use time::{macros::format_description, OffsetDateTime};
use tracing::{
    field::{Field, Visit},
    span::{Attributes, Id},
    Event, Level, Subscriber,
};
use tracing_subscriber::{
    fmt::{format::Writer, FmtContext, FormatEvent, FormatFields, FormattedFields},
    layer::Context,
    registry::LookupSpan,
    Layer,
};

/// Span field naming the component
pub(super) const COMPONENT: &str = "component";

/// Longest component (`TransparentAddrIdx:`) + 2
const COMPONENT_WIDTH: usize = 21;

const MESSAGE_WIDTH: usize = 30;

const GROUP_FROM: u64 = 1_000;

/// Hash shown as its first + last this many hex digits
const HASH_ENDS: usize = 8;

const TIME: &[time::format_description::FormatItem<'static>] =
    format_description!("[month]-[day]|[hour]:[minute]:[second].[subsecond digits:3]");

const RESET: &str = "\x1b[0m";

pub(super) struct Terminal {
    pub(super) color: bool,
    pub(super) location: bool,
}

impl<S, N> FormatEvent<S, N> for Terminal
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let mut fields = Fields::default();
        event.record(&mut fields);
        if self.location {
            if let (Some(file), Some(line)) = (meta.file(), meta.line()) {
                fields.pairs.push(("at", format!("{file}:{line}")));
            }
        }

        let (tag, ink) = level(*meta.level());
        let now = OffsetDateTime::now_utc().format(TIME).map_err(|_| fmt::Error)?;
        let spans = span_fields(ctx);
        let paint = |text: &str| match self.color {
            true => format!("{ink}{text}{RESET}"),
            false => text.to_owned(),
        };
        let component = component(ctx).map(|name| format!("{name}:")).unwrap_or_default();

        write!(writer, "{} [{now}] {component:<COMPONENT_WIDTH$}", paint(tag))?;
        match fields.pairs.is_empty() && spans.is_empty() {
            true => write!(writer, "{}", fields.message)?,
            false => write!(writer, "{:<MESSAGE_WIDTH$}", fields.message)?,
        }
        for (key, value) in &fields.pairs {
            write!(writer, " {}={value}", paint(key))?;
        }
        if !spans.is_empty() {
            write!(writer, " {spans}")?;
        }
        writeln!(writer)
    }
}

/// 5-wide tag + level colour
fn level(level: Level) -> (&'static str, &'static str) {
    match level {
        Level::ERROR => ("ERROR", "\x1b[31m"),
        Level::WARN => ("WARN ", "\x1b[33m"),
        Level::INFO => ("INFO ", "\x1b[32m"),
        Level::DEBUG => ("DEBUG", "\x1b[36m"),
        Level::TRACE => ("TRACE", "\x1b[34m"),
    }
}

/// Span's [`COMPONENT`] value, stored at creation by [`Components`]
struct Component(String);

/// Records each span's [`COMPONENT`] field for [`Terminal`]
pub(super) struct Components;

impl<S> Layer<S> for Components
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        let Some(name) = fields.component else {
            return;
        };
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(Component(name));
        }
    }
}

/// Innermost enclosing component
fn component<S, N>(ctx: &FmtContext<'_, S, N>) -> Option<String>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    ctx.event_scope()?
        .find_map(|span| span.extensions().get::<Component>().map(|found| found.0.clone()))
}

/// Outermost span first, each as its formatter recorded it
fn span_fields<S, N>(ctx: &FmtContext<'_, S, N>) -> String
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    let Some(scope) = ctx.event_scope() else {
        return String::new();
    };
    let mut out = String::new();
    for span in scope.from_root() {
        let extensions = span.extensions();
        if let Some(fields) = extensions.get::<FormattedFields<N>>() {
            if !fields.is_empty() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(fields);
            }
        }
    }
    out
}

/// Span fields as `key=value` pairs
pub(super) struct Logfmt;

impl<'writer> FormatFields<'writer> for Logfmt {
    fn format_fields<R: tracing_subscriber::field::RecordFields>(
        &self,
        mut writer: Writer<'writer>,
        record: R,
    ) -> fmt::Result {
        let mut fields = Fields::default();
        record.record(&mut fields);
        let mut first = true;
        for (key, value) in &fields.pairs {
            if !first {
                write!(writer, " ")?;
            }
            write!(writer, "{key}={value}")?;
            first = false;
        }
        Ok(())
    }
}

/// One event's message, [`COMPONENT`] and formatted `key=value` pairs
#[derive(Default)]
struct Fields {
    message: String,
    component: Option<String>,
    pairs: Vec<(&'static str, String)>,
}

impl Fields {
    fn push(&mut self, field: &Field, value: String) {
        match field.name() {
            "message" => self.message = value,
            COMPONENT => self.component = Some(value),
            name => self.pairs.push((name, quoted(shortened(value)))),
        }
    }
}

impl Visit for Fields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, grouped(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        let sign = if value < 0 { "-" } else { "" };
        self.push(field, format!("{sign}{}", grouped(value.unsigned_abs())));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_owned());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let mut chain = value.to_string();
        let mut source = value.source();
        while let Some(cause) = source {
            let _ = write!(chain, ": {cause}");
            source = cause.source();
        }
        self.push(field, chain);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.push(field, format!("{value:?}"));
    }
}

/// `1,730,091` from [`GROUP_FROM`] up
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    if value < GROUP_FROM {
        return digits;
    }
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// 64 hex digits → `00000000…1a76bf89`
fn shortened(value: String) -> String {
    match value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        true => format!("{}…{}", &value[..HASH_ENDS], &value[64 - HASH_ENDS..]),
        false => value,
    }
}

/// Empty or holding a space, `=` or `"` → Rust-escaped and quoted
fn quoted(value: String) -> String {
    match value.is_empty() || value.contains([' ', '=', '"', '\n', '\t']) {
        true => format!("{value:?}"),
        false => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use tracing_subscriber::{fmt::MakeWriter, layer::SubscriberExt};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Tag, timestamp shape, innermost component, padded message, grouped integers vs `%`
    /// fields, shortened hashes, quoting, error chains, span context, bare message
    #[test]
    fn lines_follow_the_layout_under_their_component() {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::registry().with(Components).with(
            tracing_subscriber::fmt::layer()
                .fmt_fields(Logfmt)
                .event_format(Terminal { color: false, location: false })
                .with_writer(captured.clone()),
        );
        let io = std::io::Error::other("disk gone");
        let hash = "000000000022c9338155930761987061f1c65cad85b6ee60037a0c221a76bf89";
        tracing::subscriber::with_default(subscriber, || {
            let daemon = tracing::error_span!("component", component = "Zainod");
            let sync = tracing::error_span!(parent: &daemon, "component", component = "ZainoSync");
            sync.in_scope(|| {
                tracing::info!(
                    height = %1_730_091u32,
                    blocks = 1_812u64,
                    delta = -250_000i64,
                    rows = 999u64,
                    "Syncing blocks"
                );
                tracing::warn!(reason = "queue full", ratio = 0.5, %hash, "Commit waited");
            });
            let poll = tracing::info_span!(parent: &daemon, "poll", endpoint = "10.0.0.1:8232");
            poll.in_scope(|| tracing::error!(error = &io as &dyn std::error::Error, "Poll failed"));
            tracing::info!("Shutting down");
        });

        let text =
            String::from_utf8(captured.0.lock().expect("capture lock").clone()).expect("utf-8 log");
        let lines: Vec<&str> = text.lines().collect();
        let body = |line: &str| line.split_once("] ").expect("timestamp closes").1.to_owned();
        let stamp: String =
            lines[0][7..25].chars().map(|c| if c.is_ascii_digit() { '0' } else { c }).collect();
        let levels: Vec<&str> = lines.iter().map(|l| &l[..5]).collect();
        assert_eq!(stamp, "00-00|00:00:00.000", "{}", lines[0]);
        assert_eq!(levels, ["INFO ", "WARN ", "ERROR", "INFO "]);
        let expected = [
            format!(
                "{:<21}{:<30} height=1730091 blocks=1,812 delta=-250,000 rows=999",
                "ZainoSync:", "Syncing blocks"
            ),
            format!(
                "{:<21}{:<30} reason=\"queue full\" ratio=0.5 hash=00000000…1a76bf89",
                "ZainoSync:", "Commit waited"
            ),
            format!(
                "{:<21}{:<30} error=\"disk gone\" endpoint=10.0.0.1:8232",
                "Zainod:", "Poll failed"
            ),
            format!("{:<21}Shutting down", ""),
        ];
        assert_eq!(lines.iter().map(|l| body(l)).collect::<Vec<_>>(), expected);
    }
}
