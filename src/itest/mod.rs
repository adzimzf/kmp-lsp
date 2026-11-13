//! Integration cases written the way rust-analyzer writes its tests: the
//! sources are a fixture with a `$0` cursor, and the expected answer is
//! written beside it — here in the same file, under a `//= expected` line.
//!
//! ```text
//! src/itest/cases/java/definition/method_on_a_parameter.case
//!
//! //- /com/demo/Greeter.java
//! package com.demo;
//! class Greeter { String greet() { return ""; } }
//! //- /com/demo/Main.java
//! package com.demo;
//! class Main { void run(Greeter greeter) { greeter.gr$0eet(); } }
//! //= expected
//! /com/demo/Greeter.java:2:24  class Greeter { String greet() { return ""; } }
//! ```
//!
//! A case talks LSP to a whole server, not to one function, so it covers
//! the same ground as `bench/compare/itest.sh` — without a checkout, a
//! build tool or a JDK. The server is this crate's own, in the test
//! process, and nothing is written to disk ([`server`]). Each case is the
//! smallest program that shows one rule, so a new rule is a new case file
//! and no Rust at all: `cases/<language>/<request>/<name>.case`, where
//! `<language>` is `java` or `kotlin` and `<request>` is the request asked
//! ([`cases`] lists them). One `#[test]` runs a whole directory.
//!
//! A case starts a server of its own, so the suites are off unless asked
//! for: without `INTEGRATION=1` each one prints why it did nothing and
//! passes, and `cargo test` stays a unit-test run.
//!
//! - `INTEGRATION=1 cargo test itest` runs every case.
//! - `INTEGRATION=1 cargo test itest::cases::kotlin::hover` runs one
//!   directory.
//! - `UPDATE_EXPECT=1 cargo test itest` writes the answers into the case
//!   files (it implies `INTEGRATION=1`); read the diff before committing
//!   it. A new case can be committed with nothing under its `//= expected`
//!   line and filled in this way.
//!
//! Sources declare every type they use: a case must not depend on which JDK
//! or Kotlin stdlib the machine has.

mod cases;
mod fixture;
mod request;
mod server;
