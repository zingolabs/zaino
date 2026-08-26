//! Declares a crate's metric names beside the `# HELP` text `zainod` registers.

/// Declare a crate's metrics: one line each, name and `# HELP` together.
///
/// Generates a `pub const` per metric plus the `COUNTERS` / `GAUGES` / `HISTOGRAMS`
/// slices `zainod` registers from, so a metric cannot exist without its help text
/// and the two cannot drift.
///
/// - A macro because a `fn` can neither declare a `const` nor bind a name
/// - Entries may interleave kinds, so metrics group by subsystem rather than by kind
/// - rustfmt does not reformat a macro body, which is what keeps one metric to one
///   line; mind the 100-col rule yourself in here
///
/// ```ignore
/// metric_names! {
///     gauge CHAIN_TIP_HEIGHT = "zaino.chain.tip_height" => "Latest chain tip height";
///     counter SYNC_TXS = "zaino.sync.transactions_total" => "Transactions ingested";
/// }
/// ```
#[macro_export]
macro_rules! metric_names {
    ($($body:tt)*) => { $crate::__metric_names_munch! { [][][] $($body)* } };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __metric_names_munch {
    ([$($c:tt)*][$($g:tt)*][$($h:tt)*]
     $(#[$a:meta])* counter $id:ident = $name:literal => $help:literal; $($rest:tt)*) => {
        $(#[$a])* pub const $id: &str = $name;
        $crate::__metric_names_munch! { [$($c)* ($id, $help),][$($g)*][$($h)*] $($rest)* }
    };
    ([$($c:tt)*][$($g:tt)*][$($h:tt)*]
     $(#[$a:meta])* gauge $id:ident = $name:literal => $help:literal; $($rest:tt)*) => {
        $(#[$a])* pub const $id: &str = $name;
        $crate::__metric_names_munch! { [$($c)*][$($g)* ($id, $help),][$($h)*] $($rest)* }
    };
    ([$($c:tt)*][$($g:tt)*][$($h:tt)*]
     $(#[$a:meta])* histogram $id:ident = $name:literal => $help:literal; $($rest:tt)*) => {
        $(#[$a])* pub const $id: &str = $name;
        $crate::__metric_names_munch! { [$($c)*][$($g)*][$($h)* ($id, $help),] $($rest)* }
    };
    ([$($c:tt)*][$($g:tt)*][$($h:tt)*]) => {
        /// Counters emitted here, with the `# HELP` `zainod` registers
        pub const COUNTERS: &[(&str, &str)] = &[$($c)*];
        /// Gauges emitted here, with the `# HELP` `zainod` registers
        pub const GAUGES: &[(&str, &str)] = &[$($g)*];
        /// Histograms emitted here; `zainod` owns their bucket ladders
        pub const HISTOGRAMS: &[(&str, &str)] = &[$($h)*];
    };
}
