# Debugging (Java and Kotlin/JVM)

kmp-lsp speaks the same debugger protocol jdtls does, so editor setups built
for jdtls + Microsoft's [java-debug](https://github.com/microsoft/java-debug)
(nvim-dap with nvim-jdtls, Zed's Java extension, …) work against kmp-lsp.

## How jdtls does it, and which parts kmp-lsp replaces

java-debug comes in two halves:

| java-debug module | What it is | Depends on |
|---|---|---|
| `com.microsoft.java.debug.core` | The Debug Adapter Protocol implementation: launch/attach, breakpoints, stepping, threads, stack frames, variables, exception breakpoints, console output — on top of JDI | JDI, gson, rxjava, commons-lang3/io. **No Eclipse.** |
| `com.microsoft.java.debug.plugin` | An OSGi bundle jdtls loads (`initializationOptions.bundles`). Hosts the core inside the jdtls JVM and answers its questions from the JDT Java model | jdtls (JDT core, JDT launching, Eclipse JDI, the Eclipse build) |

The plugin is what ties java-debug to jdtls. It contributes two things:

**1. `workspace/executeCommand` delegate commands** (`plugin.xml`), which the
editor calls on the language server:

| Command | jdtls answers it with | kmp-lsp |
|---|---|---|
| `vscode.java.startDebugSession` | Starts a DAP `ServerSocket` inside jdtls, returns its port | Starts `kmp-debug-server.jar` (core + kmp-lsp providers), returns its port |
| `vscode.java.resolveMainClass` | JDT search for `main` methods | Index search: Java `static main`, Kotlin top-level `main` (file facade `FooKt` / `@file:JvmName`), `@JvmStatic main` in an `object` / companion |
| `vscode.java.resolveClasspath` | `JavaRuntime.computeUnresolvedRuntimeClasspath` of the Eclipse project | The module's compiled-class directories, then every other module's, then the jars of the last [dependency sync](features.md#dependency-sync) and configured `jarPaths` |
| `vscode.java.resolveJavaExecutable` | The project's `IVMInstall` | `jdkHome` setting / `workspace.json`, `JAVA_HOME`, `java` on the `PATH`, the login shell's JDK |
| `vscode.java.updateDebugSettings` | `DebugSettings.updateSettings` in-process | Forwarded to the debug server (replayed after a restart) |
| `vscode.java.validateLaunchConfig`, `checkProjectSettings`, `isOnClasspath`, `fetchPlatformSettings`, `inferLaunchCommandLength`, `resolveSourceUri` | Project model checks | Answered (permissive defaults / index lookup) |
| `vscode.java.buildWorkspace` | Eclipse incremental build | **Not offered** — build with Gradle/Maven before launching |
| `resolveElementAtSelection`, `resolveInlineVariables`, `resolveBuildFiles`, `resolveClassFilters`, `fetchUsageData` | JDT AST / project model | Not offered |

**2. The providers the core asks during a session** (`JdtProviderContextFactory`):

| Provider | Used for | jdtls implementation | kmp-lsp implementation |
|---|---|---|---|
| `ISourceLookUpProvider` | Breakpoints: which JVM class holds the code on a line. Stack frames: which file a class came from | JDT AST of the compilation unit; JDT search | Asks kmp-lsp over the debug server's stdio; answered from the tree-sitter index (`src/debug/jvm_names.rs`), Kotlin included |
| `IVirtualMachineManagerProvider` | Launching/attaching the debuggee | Eclipse JDI + `AdvancedLaunchingConnector` (cwd, env) | The JDK's JDI + `KmpLaunchingConnector` (cwd, env) |
| `IEvaluationProvider` | Watch/console expressions, conditional breakpoints, logpoints, `toString()` in the Variables view | JDT compiler + Eclipse JDI evaluation engine | Variable and field paths (`user.address.city`, `items.length`), method invocation through JDI. No compiler: conditional breakpoints and logpoints are not supported |
| `IHotCodeReplaceProvider` | Redefining classes after a save | Eclipse build delta | Not supported (no-op) |
| `ICompletionsProvider` | Debug-console completion | JDT code assist | Not supported (empty) |

So everything a debugger session runs on — the DAP server and JDI — is
java-debug core, unchanged. What kmp-lsp provides is the jdtls part: the
delegate commands and the project knowledge behind the providers.

## Setup

1. Build the debug server and put it next to `kmp-lsp` (or point
   `KMP_LSP_DEBUG_SERVER_JAR` at it):

   ```sh
   cd java-sidecar
   ./gradlew :debug-server:shadowJar
   cp debug-server/build/libs/kmp-debug-server.jar "$(dirname "$(command -v kmp-lsp)")"
   ```

   It runs on the user's JDK (`java -jar`, JDK 17+ with the `jdk.jdi` module —
   any full JDK). The JVM it runs on is found the same way as
   `resolveJavaExecutable`.

2. Compile the project (`./gradlew classes`, `mvn compile`). kmp-lsp doesn't
   build; the debuggee runs from `build/classes/…` / `target/classes`.

3. For library jars on the classpath, keep [dependency sync](features.md#dependency-sync)
   on (the default) or list them in `workspace.json` `jarPaths`.

### Neovim (nvim-dap)

Neovim 0.11+ (`client:request_sync`):

```lua
local dap = require("dap")

local function execute(command, arguments)
  local client = vim.lsp.get_clients({ name = "kmp-lsp" })[1]
  assert(client, "kmp-lsp is not running")
  local response = client:request_sync("workspace/executeCommand",
    { command = command, arguments = arguments }, 60000)
  assert(response and not response.err, response and response.err and response.err.message)
  return response.result
end

dap.adapters.java = function(callback)
  local port = execute("vscode.java.startDebugSession", {})
  callback({ type = "server", host = "127.0.0.1", port = port })
end

-- One launch configuration per main class, as nvim-jdtls'
-- setup_dap_main_class_configs() builds them from jdtls.
local function main_class_configurations()
  local configurations = {}
  for _, main in ipairs(execute("vscode.java.resolveMainClass", {})) do
    local arguments = { main.mainClass, main.projectName }
    local paths = execute("vscode.java.resolveClasspath", arguments)
    table.insert(configurations, {
      type = "java",
      request = "launch",
      name = "Launch " .. main.mainClass,
      mainClass = main.mainClass,
      projectName = main.projectName,
      modulePaths = paths[1],
      classPaths = paths[2],
      javaExec = execute("vscode.java.resolveJavaExecutable", arguments),
      cwd = vim.fn.getcwd(),
    })
  end
  return configurations
end

vim.api.nvim_create_user_command("KmpDebugConfigurations", function()
  dap.configurations.kotlin = main_class_configurations()
  dap.configurations.java = dap.configurations.kotlin
end, {})
```

Run `:KmpDebugConfigurations` once the index is ready (and after adding a
`main`), then `:lua require("dap").continue()`. nvim-jdtls'
`setup_dap_main_class_configs()` issues the same commands and works when its
requests are routed to kmp-lsp.

### Attach

Start the program with
`-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5005` and use an
`attach` configuration (`hostName`, `port`). Breakpoints and source mapping
work the same as for launch.

## How breakpoints are placed in Kotlin

java-debug installs a breakpoint on a class name and on `ClassName$*` as
classes load, and searches nested types of classes already loaded. kmp-lsp
therefore answers with the **outermost** class at the line:

- inside a top-level `class`/`object`/`interface`/`enum` → that class
  (`com.example.Greeter`); nested classes, companions, anonymous classes and
  pre-`invokedynamic` lambdas (`Greeter$greet$1`) are reached through `$*`;
- outside every class in a `.kt` file → the file facade (`MainKt`, or the
  `@file:JvmName`);
- outside every class in a `.java` file → no class; the breakpoint stays
  unverified.

Stack frames are mapped back by the class file's `SourceFile` attribute plus
its package (`com/example/Main.kt`), matched against indexed files by file
name *and* `package` declaration, so Kotlin files that don't live in their
package directory are found.

## Limitations

- No conditional breakpoints, logpoints or arbitrary expression evaluation —
  only variable/field paths (`a.b.c`, `array.length`). These need a compiler.
- No hot code replace and no debug-console completion.
- No build before launch: compile first.
- Breakpoints inside the body of a Kotlin `inline` function stop only when the
  function's own class runs that line, not at inlined call sites (that needs
  the SMAP `KotlinDebug` stratum).
- The classpath is per workspace, not per Gradle source set: every module's
  main output plus every synced dependency jar. Pass `classPaths` in the
  launch configuration when that's not right.
- Libraries' sources in stack frames: only indexed workspace files are mapped;
  set `sourcePaths` in the launch configuration for others.
