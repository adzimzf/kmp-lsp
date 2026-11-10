"""How to start each server, from a clean or a kept state directory.

Every server gets its own state directory (caches, indexes, workspace
metadata) under `--state`. A *cold* run starts from an empty one; a *warm*
run reuses the one the previous run left. `GRADLE_USER_HOME` is shared and
never cleared: downloading dependencies is network time, not server time.
"""

import glob
import os
import platform
import shutil


def _base_environment(state, gradle_home):
    env = dict(os.environ)
    env["XDG_CACHE_HOME"] = os.path.join(state, "cache")
    env["XDG_CONFIG_HOME"] = os.path.join(state, "config")
    env["XDG_DATA_HOME"] = os.path.join(state, "data")
    if gradle_home:
        env["GRADLE_USER_HOME"] = gradle_home
    for directory in ("cache", "config", "data"):
        os.makedirs(os.path.join(state, directory), exist_ok=True)
    return env


def kmp_lsp(paths, state, gradle_home, options):
    env = _base_environment(state, gradle_home)
    sync = "auto" if options.get("dependency_sync", True) else "off"
    return {
        "command": [paths["kmp_lsp"]],
        "env": env,
        "initialization_options": {"indexingOptions": {"dependencySync": sync}},
        "settings": {},
        "languages": ["kotlin", "java"],
    }


def _jdtls_configuration_name():
    """jdtls ships one OSGi configuration per OS and CPU."""
    system = platform.system()
    arm = platform.machine().lower() in {"arm64", "aarch64"}
    name = {"Darwin": "config_mac", "Windows": "config_win"}.get(system, "config_linux")
    return name + "_arm" if arm and system != "Windows" else name


def jdtls(paths, state, gradle_home, options):
    env = _base_environment(state, gradle_home)
    home = paths["jdtls"]
    launcher = sorted(glob.glob(os.path.join(home, "plugins", "org.eclipse.equinox.launcher_*.jar")))[-1]
    # jdtls writes into its configuration directory: give each state its own copy.
    configuration = os.path.join(state, "jdtls-config")
    if not os.path.isdir(configuration):
        shutil.copytree(os.path.join(home, _jdtls_configuration_name()), configuration)
    settings = {"java": {"import": {"gradle": {"enabled": True, "wrapper": {"enabled": True}},
                                    "maven": {"enabled": True}},
                         "autobuild": {"enabled": True},
                         # Off by default in jdtls; the integration test asks both.
                         "signatureHelp": {"enabled": True},
                         # vscode-java's default; jdtls's own is none.
                         "inlayHints": {"parameterNames": {"enabled": "literals"}},
                         "format": {"onType": {"enabled": True}}}}
    return {
        "command": [
            paths.get("java", "java"),
            "-Declipse.application=org.eclipse.jdt.ls.core.id1",
            "-Dosgi.bundles.defaultStartLevel=4",
            "-Declipse.product=org.eclipse.jdt.ls.core.product",
            "-Dlog.level=ALL", "-Xmx2G",
            "--add-modules=ALL-SYSTEM",
            "--add-opens", "java.base/java.util=ALL-UNNAMED",
            "--add-opens", "java.base/java.lang=ALL-UNNAMED",
            "-jar", launcher,
            "-configuration", configuration,
            "-data", os.path.join(state, "jdtls-workspace"),
        ],
        "env": env,
        # Every real jdtls client (vscode-java, nvim-jdtls, Zed) enables this:
        # without it jdtls answers nothing for targets in compiled classes
        # (libraries, the JDK, and here moshi's own Kotlin core).
        "initialization_options": {"settings": settings,
                                   "extendedClientCapabilities": {"classFileContentsSupport": True}},
        "settings": settings,
        "languages": ["java"],
    }


def _kotlin_lsp_executable(home):
    """`bin/intellij-server` of an unpacked kotlin-lsp archive (Linux layout,
    or `Contents/` inside a macOS app bundle), else the older `kotlin-lsp.sh`."""
    candidates = [
        os.path.join(home, "bin", "intellij-server"),
        os.path.join(home, "Contents", "bin", "intellij-server"),
        *glob.glob(os.path.join(home, "*", "bin", "intellij-server")),
        *glob.glob(os.path.join(home, "*", "Contents", "bin", "intellij-server")),
        os.path.join(home, "kotlin-lsp.sh"),
    ]
    for candidate in candidates:
        if os.path.isfile(candidate):
            return candidate
    raise SystemExit(f"no bin/intellij-server under {home}: pass the unpacked kotlin-lsp directory")


def kotlin_lsp(paths, state, gradle_home, options):
    env = _base_environment(state, gradle_home)
    # Without these the server starts from a fresh /tmp/idea-system* every
    # time, so a "warm" run would be cold.
    env["IJ_JAVA_OPTIONS"] = (f"-Didea.system.path={os.path.join(state, 'idea-system')} "
                              f"-Didea.config.path={os.path.join(state, 'idea-config')}")
    return {
        "command": [_kotlin_lsp_executable(paths["kotlin_lsp"]), "--stdio"],
        "env": env,
        "initialization_options": {},
        "settings": {},
        "languages": ["kotlin", "java"],
    }


SERVERS = {"kmp-lsp": kmp_lsp, "jdtls": jdtls, "kotlin-lsp": kotlin_lsp}
