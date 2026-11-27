#![forbid(unsafe_code)]
//! Bluetooth Classic host for the Q2 (docs/bluetooth.md). No I/O: the caller
//! moves bytes between `host::Host` and the controller's UART.
pub mod sbc;
pub mod codec;
pub mod h5;
pub mod resample;
pub mod rtl;
pub mod avrcp;
pub mod sdp;
pub mod host;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
#[cfg(test)]
mod stack_tests;
