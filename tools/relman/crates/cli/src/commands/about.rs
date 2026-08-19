use clap::Args as ClapArgs;

use crate::context::Ctx;
use crate::format;

#[derive(ClapArgs)]
pub struct Args {}

pub fn run(_args: &Args, ctx: &Ctx) {
    let report = ctx.about.report();
    println!("{}", format::about(&report));
}
