use std::sync::Arc;

use relman_core::ports::About;

/// The driving ports the CLI needs, injected by the binary's composition
/// root. Add a field per port as relman grows (`Changesets`, `Versions`, …).
pub struct Ctx {
    pub about: Arc<dyn About>,
}
