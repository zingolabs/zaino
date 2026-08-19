//! One module per subcommand. Each exposes a clap `Args` and a
//! `run(&Args, &Ctx)` that calls the application through driving ports.

pub mod about;
pub mod bump;
pub mod changelog;
pub mod changeset;
pub mod versions;
