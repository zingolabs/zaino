---
status: proposed
date: 2024-07-30
builds-on: N/A
story: Clarify the reasons for Zaino's Docker entrypoint handling of optional path configurations involving the string "None", stemming from its Rust config parsing.
---

# ADR-001: Handling of Literal "None" String in Zaino Configuration

## Context and Problem Statement

Zaino's core configuration parsing logic (in `zaino/zainod/src/config.rs`) treats the literal string "None" within the `zindexer.toml` file as a special indicator for unset optional path configurations. This is distinct from Zebra and Zallet, whose configuration parsers primarily rely on standard Serde `Option<T>` deserialization (where a missing TOML key results in a Rust `None` value) without special string literal handling for paths.

This difference in Rust-level parsing necessitates a specific approach in `zaino/docker/entrypoint.sh` for generating `zindexer.toml` from environment variables. The entrypoint currently writes `field_name = "None"` for such paths if the corresponding environment variable is unset or explicitly set to "None", and includes checks like `if [[ "${path_val}" != "None" ]]` before creating directories. This contrasts with the simpler omission of lines for unset variables in Zebra and Zallet entrypoints.

The purpose of this ADR is to document the reasons for this specific behavior in Zaino, ensuring clarity on how its configuration parsing influences the Docker entrypoint script, and to consider the path forward.

## Priorities & Constraints

*   **Parser-Entrypoint Alignment:** The `zaino/docker/entrypoint.sh` script *must* generate a `zindexer.toml` that is correctly interpreted by `zainod`'s Rust configuration parser.
*   **Understanding Divergence:** The reasons for Zaino's configuration handling (Rust parser and consequently the entrypoint script) diverging from Zebra and Zallet should be clearly documented.
*   **Maintainability:** A clear understanding of the direct dependency of the entrypoint's logic on the Rust parser's specific behavior is crucial for future maintenance of both.
*   **Consistency (Desirable):** Where feasible and not detrimental, aligning configuration handling approaches across related projects (Zaino, Zebra, Zallet) can reduce developer cognitive load and simplify shared tooling or practices.

## Considered Options

1.  **Affirm Current System:** Acknowledge that Zaino's Rust config parser explicitly handles the string "None", and therefore the current `entrypoint.sh` behavior (writing "None" for unset optional paths and checking for this string before directory creation) is the correct and necessary adaptation to this parser logic. This implies accepting the current divergence from Zebra/Zallet.
2.  **Standardize Zaino's Rust Parser (and then Entrypoint):** Modify `zainod`'s Rust configuration parser (`zaino/zainod/src/config.rs`) to no longer treat the string "None" specially for optional paths. Instead, make it rely solely on standard Serde `Option<T>` behavior (where a missing TOML key implies Rust `None`). Subsequently, simplify the `entrypoint.sh` to omit lines for unset paths, aligning with Zebra and Zallet. This would improve consistency but requires changes to core Zaino parsing logic.

## Decision Outcome

**[To be decided]**

{Justification for the chosen option will be documented here once a decision is made.}

## Expected Consequences

*Consequences will depend on the chosen option.*

*If Option 1 is chosen:*
*   The `zaino/docker/entrypoint.sh` script will continue to handle optional path configurations differently from Zebra's and Zallet's entrypoints.
*   Developers working with Zaino's configuration and Docker setup must remain aware that the literal string "None" in `zindexer.toml` has a special meaning, driven by the Rust parser, and that the entrypoint script accommodates this.

*If Option 2 is chosen:*
*   Zaino's Rust configuration parsing (`zaino/zainod/src/config.rs`) would be refactored to remove the special parsing of the string "None" and rely on standard Serde `Option<T>` behavior for missing keys.
*   Zaino's `entrypoint.sh` could then be simplified to align with the approach used by Zebra and Zallet, omitting lines for unset optional path variables.
*   This would improve consistency across the projects but requires modifications to Zaino's core parsing logic.

## More Information

*   Zaino config parsing (current root cause of specific entrypoint behavior): `zaino/zainod/src/config.rs` (specifically the `parse_field_or_warn_and_default!` macro and `load_config` function).
*   Zaino entrypoint adaptation: `zaino/docker/entrypoint.sh`.
*   Zebra config parsing: Primarily uses Serde defaults. Example: `zebra/zebrad/src/config.rs` and `zebra/zebra-network/src/config.rs`.
*   Zallet config parsing: Primarily uses Serde defaults. Example: `wallet/zallet/src/config.rs`. 