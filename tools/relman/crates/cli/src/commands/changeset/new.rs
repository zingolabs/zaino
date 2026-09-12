use clap::Args as ClapArgs;

use relman_core::ports::NewChangeset;
use relman_core::types::Description;

use crate::commands::changeset::ChangesetCommandError;
use crate::context::Ctx;

/// `relman changeset new [--empty <REASON>]`.
#[derive(ClapArgs)]
pub struct Args {
    /// Scaffold the no-op `[empty]` form with this reason, instead of an
    /// editable template.
    #[arg(long, value_name = "REASON")]
    empty: Option<String>,
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), ChangesetCommandError> {
    let req = match &args.empty {
        Some(reason) => NewChangeset::Empty {
            reason: Description::parse(reason)?,
        },
        None => NewChangeset::Template,
    };

    let slug = ctx.changesets.new(req)?;
    let path = ctx.changesets_dir.join(slug.file_name());
    println!("Created {}", path.display());
    Ok(())
}
