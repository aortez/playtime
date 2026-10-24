//! Authenticated loopback transport and independent lifecycle coordination.
//! HTTP execution and durable authority are supplied by an application adapter;
//! the included HTTP and worker fixtures are explicitly scripted.

pub mod application;
pub mod auth;
pub mod catalog;
pub mod daemon;
pub mod fixture;
pub mod http;
pub mod lifecycle;

pub mod models;

pub mod registrations;
