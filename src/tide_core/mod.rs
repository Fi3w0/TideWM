//! TideWM's compositor core.
//!
//! These modules own configuration, state, input dispatch, layouts, IPC, and
//! the Waves parser. They are re-exported at the crate root so existing module
//! paths stay stable while the source tree remains grouped by responsibility.

pub(crate) mod config;
pub(crate) mod input;
pub(crate) mod ipc;
pub(crate) mod layout;
pub(crate) mod state;
pub(crate) mod waves;
