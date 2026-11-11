//! RelayCheck core library: capture, verify, handoff, import, and the local
//! HTTP service. See tasks/RELAYCHECK-SPEC.md (v1.2) for the frozen contract.

pub mod canonical;
pub mod errors;
pub mod ids;
pub mod model;
pub mod store;
pub mod pathsafe;
pub mod gitx;
pub mod blobstore;
pub mod capture;
pub mod diffx;
pub mod secrets;
pub mod checks;
pub mod handoff;
pub mod zipx;
pub mod verify_handoff;
pub mod service;
pub mod opsman;
pub mod http;
pub mod ui;
