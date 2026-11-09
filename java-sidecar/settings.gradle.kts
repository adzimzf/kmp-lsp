rootProject.name = "kmp-jar-indexer"

// java-debug core + kmp-lsp providers: the Debug Adapter Protocol server
// behind `vscode.java.startDebugSession` (see src/debug/ in kmp-lsp).
include("debug-server")
