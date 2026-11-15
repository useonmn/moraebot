//! moraebot core: a messenger-agnostic gateway in front of omp.

pub mod config;
pub mod init;
pub mod adapter;
pub mod testing;
pub mod store;
pub mod omp;
pub mod access;
pub mod dispatch;
pub mod logging;
pub mod launchd;
pub mod discord;
pub mod gateway;
