//! SpritEXAI Pay — core engine library surface.
//!
//! AI-native, open-core payment orchestration for MFS ecosystems (bKash, Nagad).
//! Authored and maintained by Mohammad Sijan / SpritexAI.
//!
//! The engine is deliberately dependency-light so it runs comfortably on a modest
//! VPS: SQLite (WAL) by default, Redis only when horizontal scaling is needed.

pub mod activity;
pub mod ai;
pub mod auth;
pub mod charge;
pub mod checkout;
pub mod checkout_page;
pub mod config;
pub mod crypto;
pub mod customer;
pub mod db;
pub mod device;
pub mod domain;
pub mod fraud;
pub mod gateway;
pub mod http;
pub mod invoice;
pub mod ledger;
pub mod merchant;
pub mod payment_link;
pub mod reconcile;
pub mod settings;
pub mod sms;
pub mod webhook;
