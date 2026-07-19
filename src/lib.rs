//! SpritEXAI Pay — core engine library surface.
//!
//! AI-native, open-core payment orchestration for MFS ecosystems (bKash, Nagad).
//! Authored and maintained by Mohammad Sijan / SpritexAI.
//!
//! The engine is deliberately dependency-light so it runs comfortably on a modest
//! VPS: SQLite (WAL) by default, Redis only when horizontal scaling is needed.

pub mod charge;
pub mod config;
pub mod crypto;
pub mod db;
pub mod device;
pub mod gateway;
pub mod http;
pub mod ledger;
pub mod reconcile;
pub mod sms;
pub mod webhook;
