//! Submission: pure `Job` policy (`policy.rs`) + its async runtime (`driver.rs`)

mod driver;
mod policy;

pub(crate) use driver::submit;
pub(crate) use policy::Ended;
pub use policy::SubmitPolicy;
