//! USB support — host controller drivers.
//!
//! Currently: minimal polled xHCI for HID boot-protocol keyboard/mouse
//! (QEMU `qemu-xhci -device usb-kbd -device usb-mouse`).
//!
//! ponytail: polled event ring (no MSI/MSI-X), boot protocol only,
//! port detect at init (no hotplug). Add descriptor-driven HID parsing,
//! hub support and MSI when a consumer needs them.

pub mod xhci;
