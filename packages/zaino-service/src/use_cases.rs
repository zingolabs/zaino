//! Use cases: what a consumer of Zaino demands, as types.
//!
//! A use case is a marker type naming one public consumer shape — a
//! lightwalletd-compatible server, a node-RPC/explorer server, a wallet
//! embedding Zaino as a library — with its demand beside it as a **service**
//! trait (`<U>Service`): one of the [`read_sets`](crate::read_sets) over a
//! pin, plus the controls that use case needs. The `required` set of the
//! availability model, made first-class.
//!
//! A service is blanket-implemented: a type *is* it exactly when it has the
//! constituent capabilities. So one engine satisfies every service it has the
//! parts for, and each serving adapter depends only on the one it speaks.
//!
//! [`Serves<U>`] carries a use case's demand as a bound a generic wiring can
//! name without naming the service trait — Rust cannot make a trait an
//! associated item, and this is the indirection that stands in for one. One
//! blanket impl per use case forwards to its service.
//!
//! Nothing here says *how* a use case is served. Which provider answers each
//! capability and which indexes are built are choices an implementation makes
//! per deployment, below this crate; a second implementation of the same use
//! case may make them differently and still be `Serves<U>`.

mod full_wallet;
mod light_wallet;
mod node_rpc;

pub use full_wallet::{FullWallet, FullWalletService};
pub use light_wallet::{LightWallet, LightWalletService};
pub use node_rpc::{NodeRpc, NodeRpcService};

/// A public consumer shape, as a marker type.
pub trait UseCase: Send + Sync + 'static {
    /// The name a deployment is selected by, and its serving component's name.
    const NAME: &'static str;
}

/// The demand of use case `U`, as a bound.
///
/// `S: Serves<U>` holds exactly when `S` has `U`'s service bundle. A wiring
/// generic over the use case requires it once, where the engine is composed,
/// and a demand the engine cannot meet fails there rather than at a request.
pub trait Serves<U: UseCase> {}
