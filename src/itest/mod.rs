//! Integration cases written the way rust-analyzer writes its tests: the
//! sources are a string in the test, the cursor is a `$0` in them, and the
//! expected answer is a snapshot beside it.
//!
//! ```ignore
//! #[test]
//! fn definition() {
//!     check(
//!         Kind::Definition,
//!         r#"
//! //- /com/demo/Greeter.java
//! package com.demo;
//! class Greeter { String greet() { return ""; } }
//! //- /com/demo/Main.java
//! package com.demo;
//! class Main { void run(Greeter greeter) { greeter.gr$0eet(); } }
//! "#,
//!         expect![[r#"
//!             /com/demo/Greeter.java:2:24  class Greeter { String greet() { return ""; } }
//!         "#]],
//!     );
//! }
//! ```
//!
//! A case talks LSP to a whole server, not to one function, so it covers
//! the same ground as `bench/compare/itest.sh` — without a checkout, a build
//! tool, a JDK or a reference server. By default the server is this crate's
//! own, in the test process, and nothing is written to disk
//! ([`server`]). Each case is the smallest program that shows one rule, so a
//! new rule is a new ten-line test rather than a line someone has to find in
//! a real project.
//!
//! - `cargo test itest` runs the cases.
//! - `UPDATE_EXPECT=1 cargo test itest` rewrites the snapshots in place;
//!   read the diff before committing it.
//! - `KMP_ITEST_SERVER=jdtls KMP_ITEST_BIN=…/bin/jdtls cargo test itest`
//!   asks jdtls the same cases, and `KMP_ITEST_SERVER=kmp-lsp
//!   KMP_ITEST_BIN=…/kmp-lsp` a built executable ([`server::Target`]).
//!
//! Sources declare every type they use: a case must not depend on which JDK
//! the machine has.

mod cases;
mod fixture;
mod request;
mod server;

use expect_test::Expect;

use fixture::Fixture;
use request::Kind;
use server::{Session, Target};

/// Asks `kind` at the `$0` of `fixture` and compares the rendered answer
/// with `expect`.
#[track_caller]
fn check(kind: Kind, fixture: &str, expect: Expect) {
    let fixture = Fixture::parse(fixture);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let actual = runtime.block_on(async {
        let mut session = Session::open(&Target::from_environment(), &fixture).await;
        kind.ask(&mut session, &fixture).await
    });
    expect.assert_eq(&actual);
}
