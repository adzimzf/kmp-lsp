"""How to start each server, from a clean or a kept state directory.

Every server gets its own state directory (caches, indexes, workspace
metadata) under `--state`. A *cold* run starts from an empty one; a *warm*
run reuses the one the previous run left. `GRADLE_USER_HOME` is shared and
never cleared: downloading dependencies is network time, not server time.
"""

import glob
import os
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
    sync = "on" if options.get("dependency_sync", True) else "off"
    return {
        "command": [paths["kmp_lsp"]],
        "env": env,
        "initialization_options": {"indexingOptions": {"dependencySync": sync}},
        "settings": {},
        "languages": ["kotlin", "java"],
    }


def jdtls(paths, state, gradle_home, options):
    env = _base_environment(state, gradle_home)
    home = paths["jdtls"]
    launcher = sorted(glob.glob(os.path.join(home, "plugins", "org.eclipse.equinox.launcher_*.jar")))[-1]
    # jdtls writes into its configuration directory: give each state its own copy.
    configuration = os.path.join(state, "jdtls-config")
    if not os.path.isdir(configuration):
        shutil.copytree(os.path.join(home, "config_linux"), configuration)
    settings = {"java": {"import": {"gradle": {"enabled": True, "wrapper": {"enabled": True}},
                                    "maven": {"enabled": True}},
                         "autobuild": {"enabled": True}}}
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
        "initialization_options": {"settings": settings},
        "settings": settings,
        "languages": ["java"],
    }


def kotlin_lsp(paths, state, gradle_home, options):
    env = _base_environment(state, gradle_home)
    return {
        "command": [os.path.join(paths["kotlin_lsp"], "bin", "intellij-server"), "--stdio"],
        "env": env,
        "initialization_options": {},
        "settings": {},
        "languages": ["kotlin", "java"],
    }


SERVERS = {"kmp-lsp": kmp_lsp, "jdtls": jdtls, "kotlin-lsp": kotlin_lsp}
