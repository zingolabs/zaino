//! Resolve `ZEBRA_VERSION` to the git ref the zebra source build checks out.
//!
//! `ZEBRA_VERSION` (from `.env.testing-artifacts`) is canonically the Docker
//! Hub image tag — bare, e.g. "6.0.0-rc.0" — while zebra's git release tags
//! carry a `v` prefix. Both container build entry points (`build-image.sh`,
//! `build-n-push-ci-image.yaml`) run this to derive the checkout ref they
//! pass as the `ZEBRA_GIT_REF` build-arg, so an unresolvable version fails
//! before the build instead of as a pathspec error after the zebra clone.
//!
//! Usage: `get-zebra-git-ref <ZEBRA_VERSION>` (falls back to the
//! `ZEBRA_VERSION` environment variable).

use workbench::{git, run, zebra_git_ref, zebra_ref_probes, ZEBRA_REPO_URL};

fn main() {
    run(
        "get-zebra-git-ref",
        || {
            let version = std::env::args()
                .nth(1)
                .or_else(|| std::env::var("ZEBRA_VERSION").ok())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    vec!["usage: get-zebra-git-ref <ZEBRA_VERSION> \
(or set the ZEBRA_VERSION environment variable)"
                        .to_string()]
                })?;
            let probes = zebra_ref_probes(&version);
            let mut args = vec!["ls-remote", ZEBRA_REPO_URL];
            args.extend(probes.iter().map(String::as_str));
            let output = git(&args)?;
            zebra_git_ref(&version, &output)
        },
        |git_ref| println!("{git_ref}"),
    )
}
