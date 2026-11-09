//! Java/Kotlin debugging through Microsoft's `java-debug`, the way jdtls
//! offers it — without jdtls.
//!
//! Editors drive `java-debug` (nvim-dap + nvim-jdtls, Zed, vscode-java-debug)
//! through a fixed set of `workspace/executeCommand` "delegate commands" that
//! jdtls loads from the `com.microsoft.java.debug.plugin` bundle. kmp-lsp
//! answers the same commands (`vscode.java.*`), so those editor setups work
//! unchanged when pointed at kmp-lsp:
//!
//! * `vscode.java.startDebugSession` starts `kmp-debug-server.jar` (java-debug
//!   core + kmp-lsp's providers, see `java-sidecar/debug-server`) and returns
//!   the port of its Debug Adapter Protocol socket.
//! * `vscode.java.resolveMainClass`, `resolveClasspath`,
//!   `resolveJavaExecutable` fill in launch configurations from the index.
//!
//! What jdtls answers *inside* a debug session through the JDT Java model —
//! which class holds the code on a line (breakpoints) and which file a class
//! came from (stack frames) — the debug server asks kmp-lsp over its stdio
//! control channel ([`server`]), answered from tree-sitter symbol data
//! ([`jvm_names`]).

pub(crate) mod commands;
pub(crate) mod jvm_names;
pub(crate) mod launch;
pub(crate) mod server;
