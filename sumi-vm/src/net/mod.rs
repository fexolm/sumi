//! Host-side networking: the userspace gateway that stands in for a TAP
//! device (see `docs/networking-design.md` Phase 2). No root, no IRQ — see
//! `gateway`'s module doc comment for the design.

pub mod gateway;
