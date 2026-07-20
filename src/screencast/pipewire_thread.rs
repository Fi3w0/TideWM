//! The PipeWire side of a screencast session: owns a PipeWire main loop
//! on its own OS thread (PipeWire needs to own its own event loop, not
//! calloop's), negotiates the SPA buffer format, and turns each rendered
//! frame into a DMA-BUF queued on the stream. Not implemented yet.
//!
//! See AGENT.md's "Screencasting" section before writing this:
//!
//! - Reuse `capture.rs`'s existing render-and-readback path (the same
//!   `render_output`-equivalent every other capture consumer already
//!   goes through, so windows/layer-shell/lock surfaces/tab strips all
//!   stack identically here too) rather than a second, parallel render
//!   path. Only the destination changes -- a DMA-BUF via the GL
//!   renderer's export path, instead of the CPU readback + SHM copy
//!   `capture.rs` does today.
//! - `niri/src/screencasting/pw_utils.rs` is the reference for the shape
//!   (format negotiation, buffer allocation, cursor-meta packing, frame
//!   timing), not something to paste from: it's ~1600 lines of low-level
//!   `pw_*`/`spa_*` FFI, and unfamiliar unsafe code copied from it is
//!   exactly the class of GPU-memory-safety bug this project's "no
//!   unsafe without justification" rule exists to catch. Understand each
//!   call against the actual pinned `pipewire`/`libspa` crate docs before
//!   writing it, the same standard every other protocol in this codebase
//!   was held to.
//! - Verification for this file specifically needs real OBS Studio
//!   against the real udev/DRM backend with a moving-content window, not
//!   a nested winit session -- see AGENT.md's verification-plan
//!   subsection for exactly why (black frames, format-mismatch
//!   corruption, and frame-timing stutter are all failure modes a nested
//!   session can't surface).
