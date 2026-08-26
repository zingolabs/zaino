use clap::Args as ClapArgs;

use relman_core::ports::About;

use crate::format;

#[derive(ClapArgs)]
pub struct Args {}

/// Print relman's version and clock; needs no manifest, ledger, or repository.
pub fn run<A: About>(_args: &Args, about: &A) {
    let report = about.report();
    println!("{}", format::about(&report));
}
