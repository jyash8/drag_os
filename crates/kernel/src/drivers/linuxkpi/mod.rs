//! LinuxKPI — Kernel Programming Interface compatibility shim.
//!
//! Provides Linux kernel abstractions needed to port GPU drivers
//! (Intel i915/xe, AMD amdgpu) without forking their source trees.

#![allow(unused)]

pub mod fence;
pub mod gem;
pub mod mutex;
pub mod workqueue;
