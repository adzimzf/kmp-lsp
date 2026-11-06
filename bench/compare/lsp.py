"""A minimal, instrumented LSP client over stdio (standard library only)."""

import json
import os
import queue
import subprocess
import threading
import time


class LspError(Exception):
    pass


class RequestTimeout(Exception):
    pass


def process_tree_rss_kib(root_pid: int) -> int:
    """Resident memory of `root_pid` and all its descendants, in KiB."""
    children = {}
    rss = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as handle:
                fields = handle.read().rsplit(")", 1)[1].split()
            parent = int(fields[1])
            with open(f"/proc/{entry}/status") as handle:
                for line in handle:
                    if line.startswith("VmRSS:"):
                        rss[int(entry)] = int(line.split()[1])
                        break
            children.setdefault(parent, []).append(int(entry))
        except (OSError, IndexError, ValueError):
            continue
    total, stack = 0, [root_pid]
    while stack:
        pid = stack.pop()
        total += rss.get(pid, 0)
        stack.extend(children.get(pid, []))
    return total


class LanguageServer:
    """One server process. Answers the server's own requests (configuration,
    capability registration, progress tokens) so every server can run
    unattended, records `$/progress`, and samples memory."""

    def __init__(self, command, cwd, env, stderr_path, settings=None):
        self.started = time.monotonic()
        self.process = subprocess.Popen(
            command, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=open(stderr_path, "wb"), start_new_session=True)
        self.settings = settings or {}
        self.next_id = 0
        self.pending = {}
        self.lock = threading.Lock()
        self.active_progress = set()
        self.progress_events = []
        self.diagnostics = {}
        self.peak_rss_kib = 0
        self.closed = threading.Event()
        threading.Thread(target=self._read_loop, daemon=True).start()
        threading.Thread(target=self._sample_memory, daemon=True).start()

    # ── transport ──────────────────────────────────────────────────────────
    def _write(self, message):
        body = json.dumps(message).encode()
        with self.lock:
            self.process.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
            self.process.stdin.flush()

    def _read_message(self):
        headers = {}
        while True:
            line = self.process.stdout.readline()
            if not line:
                return None
            line = line.decode("ascii", "replace").strip()
            if not line:
                break
            if ":" in line:
                key, value = line.split(":", 1)
                headers[key.strip().lower()] = value.strip()
        length = int(headers.get("content-length", 0))
        return json.loads(self.process.stdout.read(length))

    def _read_loop(self):
        while True:
            try:
                message = self._read_message()
            except (ValueError, OSError):
                message = None
            if message is None:
                self.closed.set()
                for waiter in list(self.pending.values()):
                    waiter.put({"error": {"message": "server exited"}})
                return
            if "method" in message and "id" in message:
                self._answer_server_request(message)
            elif "method" in message:
                self._notification(message)
            elif "id" in message:
                waiter = self.pending.pop(message["id"], None)
                if waiter:
                    waiter.put(message)

    def _answer_server_request(self, message):
        method = message["method"]
        result = None
        if method == "workspace/configuration":
            result = [self._setting(item.get("section")) for item in message["params"].get("items", [])]
        elif method == "workspace/applyEdit":
            result = {"applied": False}
        elif method == "workspace/workspaceFolders":
            result = None
        self._write({"jsonrpc": "2.0", "id": message["id"], "result": result})

    def _setting(self, section):
        if not section:
            return self.settings
        value = self.settings
        for part in section.split("."):
            value = value.get(part) if isinstance(value, dict) else None
        return value

    def _notification(self, message):
        method, params = message["method"], message.get("params") or {}
        if method == "$/progress":
            token, kind = str(params.get("token")), (params.get("value") or {}).get("kind")
            now = time.monotonic() - self.started
            if kind == "begin":
                self.active_progress.add(token)
            elif kind == "end":
                self.active_progress.discard(token)
            self.progress_events.append((now, token, kind, (params.get("value") or {}).get("title")))
        elif method == "textDocument/publishDiagnostics":
            self.diagnostics[params.get("uri")] = params.get("diagnostics", [])

    def _sample_memory(self):
        while not self.closed.is_set() and self.process.poll() is None:
            self.peak_rss_kib = max(self.peak_rss_kib, process_tree_rss_kib(self.process.pid))
            time.sleep(0.5)

    # ── API ────────────────────────────────────────────────────────────────
    def request(self, method, params, timeout=60.0):
        """(result, seconds). Raises LspError / RequestTimeout."""
        self.next_id += 1
        request_id = self.next_id
        waiter = queue.Queue()
        self.pending[request_id] = waiter
        begin = time.monotonic()
        self._write({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        try:
            response = waiter.get(timeout=timeout)
        except queue.Empty:
            self.pending.pop(request_id, None)
            self._write({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": request_id}})
            raise RequestTimeout(method)
        elapsed = time.monotonic() - begin
        if "error" in response:
            raise LspError(response["error"].get("message", str(response["error"])))
        return response.get("result"), elapsed

    def notify(self, method, params):
        self._write({"jsonrpc": "2.0", "method": method, "params": params})

    def rss_kib(self):
        return process_tree_rss_kib(self.process.pid)

    def stop(self):
        try:
            self.request("shutdown", None, timeout=20)
            self.notify("exit", None)
            self.process.wait(timeout=20)
        except Exception:
            pass
        # Build tools the server started (Gradle daemons) share its session.
        try:
            os.killpg(self.process.pid, 9)
        except OSError:
            pass
        if self.process.poll() is None:
            self.process.wait()
        self.closed.set()
