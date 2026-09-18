import ast
import asyncio
import base64
import contextvars
import inspect
import io
import json
import os
import pickle
import signal
import sys
import threading
import traceback
import types
import uuid

PROTOCOL = 1

_proto_fd = os.dup(1)
_proto = os.fdopen(_proto_fd, "w", encoding="utf-8", buffering=1)
_proto_lock = threading.Lock()

_current_cell: contextvars.ContextVar = contextvars.ContextVar("mush_cell", default=None)
_current_origin: contextvars.ContextVar = contextvars.ContextVar("mush_origin", default=None)
_loop: asyncio.AbstractEventLoop | None = None
_pending_replies: dict = {}
_running: dict = {"id": None, "task": None}
_main_thread = threading.main_thread().ident
_namespace: dict = {}
_cell_count = 0


def emit(event: dict) -> None:
    line = json.dumps(event, ensure_ascii=False, default=str)
    with _proto_lock:
        _proto.write(line + "\n")


class _TaggedWriter(io.TextIOBase):
    def __init__(self, stream: str):
        self._stream = stream
        self._buffer = ""

    def writable(self) -> bool:
        return True

    def write(self, text: str) -> int:
        text = str(text)
        self._buffer += text
        cut = self._buffer.rfind("\n")
        if cut >= 0:
            self._emit(self._buffer[: cut + 1])
            self._buffer = self._buffer[cut + 1 :]
        return len(text)

    def flush(self) -> None:
        if self._buffer:
            self._emit(self._buffer)
            self._buffer = ""

    def _emit(self, text: str) -> None:
        emit({"type": self._stream, "id": _current_cell.get(), "text": text})


def _pump(fd: int, stream: str) -> None:
    with os.fdopen(fd, "rb", buffering=0) as raw:
        while True:
            chunk = raw.read(4096)
            if not chunk:
                return
            emit({"type": stream, "id": None, "text": chunk.decode("utf-8", "replace")})


def _capture_fds() -> None:
    for fd, stream in ((1, "stdout"), (2, "stderr")):
        read_end, write_end = os.pipe()
        os.dup2(write_end, fd)
        os.close(write_end)
        threading.Thread(target=_pump, args=(read_end, stream), daemon=True).start()
    sys.stdout = _TaggedWriter("stdout")
    sys.stderr = _TaggedWriter("stderr")
    devnull = os.open(os.devnull, os.O_RDONLY)
    os.dup2(devnull, 0)
    os.close(devnull)


async def host_request(kind: str, payload=None):
    assert _loop is not None
    request_id = uuid.uuid4().hex
    future = _loop.create_future()
    _pending_replies[request_id] = future
    emit(
        {
            "type": "host_request",
            "id": request_id,
            "cell": _current_cell.get(),
            "kind": kind,
            "payload": payload if payload is not None else {},
        }
    )
    return await future


class HostError(RuntimeError):
    pass


class BashResult:
    def __init__(self, exit_code: int, output: str, timed_out: bool = False, killed: bool = False):
        self.exit_code = exit_code
        self.output = output
        self.timed_out = timed_out
        self.killed = killed

    @property
    def ok(self) -> bool:
        return self.exit_code == 0

    def __repr__(self) -> str:
        return f"BashResult(exit_code={self.exit_code}, output={self.output!r})"

    def __str__(self) -> str:
        return self.output


def _result_from(reply: dict) -> BashResult:
    end = reply.get("end") or {}
    return BashResult(
        int(reply.get("exit_code", -1)),
        str(reply.get("output", "")),
        timed_out=end.get("kind") == "timed_out",
        killed=end.get("kind") == "killed",
    )


class BashHandle:
    """A shell command started in the background. `await h` waits for it."""

    def __init__(self, command: str, timeout: int | None):
        self.command = command
        self.job_id: str | None = None
        self.pid: int | None = None
        payload = {"command": command}
        if timeout is not None:
            payload["timeout"] = timeout
        if _current_origin.get() is not None:
            payload["origin"] = _current_origin.get()
        self._started = asyncio.ensure_future(self._start(payload))

    async def _start(self, payload: dict) -> None:
        reply = await host_request("bash.start", payload)
        self.job_id = reply["job_id"]
        self.pid = reply.get("pid")

    async def _job(self) -> dict:
        await self._started
        return {"job_id": self.job_id}

    def __await__(self):
        return self.wait().__await__()

    async def wait(self) -> BashResult:
        return _result_from(await host_request("bash.wait", await self._job()))

    async def poll(self) -> BashResult | None:
        reply = await host_request("bash.poll", await self._job())
        return None if reply.get("running") else _result_from(reply)

    async def running(self) -> bool:
        reply = await host_request("bash.poll", await self._job())
        return bool(reply.get("running"))

    async def output(self) -> str:
        reply = await host_request("bash.poll", await self._job())
        return str(reply.get("output", ""))

    async def tail(self, lines: int = 20) -> str:
        text = await self.output()
        return "\n".join(text.splitlines()[-lines:])

    async def kill(self) -> None:
        await host_request("bash.kill", await self._job())

    def __repr__(self) -> str:
        state = "pending" if self.job_id is None else self.job_id
        return f"BashHandle({self.command!r}, {state}, pid={self.pid})"


def bash(command: str, timeout: int | None = None) -> BashHandle:
    return BashHandle(command, timeout)


class ChildHandle:
    def __init__(self, reply: dict):
        self.rlm_child_id = str(reply.get("rlm_child_id", ""))
        self.name = str(reply.get("name", ""))
        self.session_id = str(reply.get("session_id", ""))
        self.model = str(reply.get("model", ""))

    def __repr__(self) -> str:
        return f"ChildHandle(rlm_child_id={self.rlm_child_id!r}, name={self.name!r}, model={self.model!r})"


class ChildSummary:
    def __init__(self, reply: dict):
        self.id = str(reply.get("id", ""))
        self.rlm_child_id = self.id
        self.name = str(reply.get("name", ""))
        self.session_id = str(reply.get("session_id", ""))
        self.model = str(reply.get("model_id", ""))
        self.status = str(reply.get("status", ""))
        self.answer = reply.get("answer")
        self.error = reply.get("error")
        self.progress_note = reply.get("progress_note")
        self.turns = int(reply.get("turns", 0))
        self.usage = reply.get("usage") or {}
        self.cost = reply.get("cost") or {}

    @property
    def settled(self) -> bool:
        return self.status not in ("starting", "running")

    def __repr__(self) -> str:
        note = f", note={self.progress_note!r}" if self.progress_note else ""
        return f"ChildSummary(name={self.name!r}, status={self.status!r}, turns={self.turns}{note})"


def _child_target(target) -> str:
    if isinstance(target, (ChildHandle, ChildSummary)):
        return target.rlm_child_id
    return str(target)


class _Rlm:
    async def spawn(self, prompt: str, *, name: str, model: str | None = None, thinking: str | None = None) -> ChildHandle:
        payload = {"prompt": prompt, "name": name}
        if model is not None:
            payload["model"] = model
        if thinking is not None:
            payload["thinking"] = thinking
        return ChildHandle(await host_request("rlm.spawn", payload))

    async def list_subagents(self) -> list:
        return [ChildSummary(entry) for entry in await host_request("rlm.list_subagents", {})]

    async def collect(self, targets=None, *, timeout_ms: int = 0) -> list:
        if targets is None:
            targets = []
        elif isinstance(targets, (str, ChildHandle, ChildSummary)):
            targets = [targets]
        payload = {"targets": [_child_target(t) for t in targets], "timeout_ms": int(timeout_ms)}
        return [ChildSummary(entry) for entry in await host_request("rlm.collect", payload)]

    async def delete_subagent(self, target) -> ChildSummary:
        return ChildSummary(await host_request("rlm.delete_subagent", {"target": _child_target(target)}))

    async def progress_note(self, text: str) -> None:
        await host_request("rlm.progress_note", {"text": text})


class _AgentMessage:
    async def send(self, message: str, *, receiver_role: str = "parent", receiver_name: str | None = None, mode: str = "auto") -> dict:
        payload = {"message": message, "receiver_role": receiver_role, "mode": mode}
        if receiver_name is not None:
            payload["receiver_name"] = receiver_name
        if _current_origin.get() is not None:
            payload["origin"] = _current_origin.get()
        return await host_request("agent_message.send", payload)


class _Goal:
    async def get(self) -> dict:
        return await host_request("goal.get", {})

    async def create(self, objective: str, *, token_budget: int | None = None) -> dict:
        payload = {"objective": objective}
        if token_budget is not None:
            payload["token_budget"] = int(token_budget)
        return await host_request("goal.create", payload)

    async def complete(self, summary: str | None = None) -> dict:
        payload = {}
        if summary is not None:
            payload["summary"] = summary
        return await host_request("goal.complete", payload)

    async def pause(self) -> dict:
        return await host_request("goal.pause", {})

    async def resume(self) -> dict:
        return await host_request("goal.resume", {})

    async def clear(self) -> dict:
        return await host_request("goal.clear", {})


rlm = _Rlm()
agent_message = _AgentMessage()
goal = _Goal()


async def edit(path: str, old: str, new: str) -> str:
    payload = {"path": path, "old": old, "new": new}
    if _current_origin.get() is not None:
        payload["origin"] = _current_origin.get()
    reply = await host_request("edit", payload)
    return str(reply.get("summary", ""))


_ATTACHMENT_MIME = "application/vnd.mush.attachment+json"
_MAX_ATTACHMENT_BYTES = 20_000_000
_IMAGE_SIGNATURES = (
    ("image/png", b"\x89PNG\r\n\x1a\n"),
    ("image/jpeg", b"\xff\xd8\xff"),
    ("image/gif", b"GIF87a"),
    ("image/gif", b"GIF89a"),
)


def _image_mime(data: bytes) -> str | None:
    for mime, prefix in _IMAGE_SIGNATURES:
        if data.startswith(prefix):
            return mime
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    return None


def display(data: dict) -> None:
    if not isinstance(data, dict) or not data or not all(isinstance(k, str) for k in data):
        raise TypeError("display() takes a non-empty dict keyed by mime type")
    json.dumps(data, allow_nan=False)
    emit({"type": "display", "id": _current_cell.get(), "data": data})


def attach_image(*paths: str) -> str:
    if not paths:
        raise TypeError("attach_image() needs at least one path")
    attached = []
    for path in paths:
        resolved = os.path.expanduser(str(path))
        if not os.path.isfile(resolved):
            raise FileNotFoundError(f"{path} is not an existing file")
        size = os.path.getsize(resolved)
        if size > _MAX_ATTACHMENT_BYTES:
            raise ValueError(f"{path} is {size // 1_000_000}MB, images must be under {_MAX_ATTACHMENT_BYTES // 1_000_000}MB")
        with open(resolved, "rb") as handle:
            data = handle.read()
        mime = _image_mime(data)
        if mime is None:
            raise ValueError(f"{path} is not a png, jpeg, gif or webp image")
        display({_ATTACHMENT_MIME: {"mime_type": mime, "data": base64.b64encode(data).decode("ascii"), "path": resolved}})
        attached.append(resolved)
    return "attached " + ", ".join(attached)


def _install_preload() -> None:
    _namespace.update(
        {
            "__name__": "__main__",
            "__builtins__": __builtins__,
            "asyncio": asyncio,
            "bash": bash,
            "edit": edit,
            "display": display,
            "attach_image": attach_image,
            "host_request": host_request,
            "BashResult": BashResult,
            "BashHandle": BashHandle,
            "HostError": HostError,
            "rlm": rlm,
            "agent_message": agent_message,
            "goal": goal,
            "ChildHandle": ChildHandle,
            "ChildSummary": ChildSummary,
        }
    )


def _format_traceback(exc: BaseException) -> list[str]:
    frames = traceback.extract_tb(exc.__traceback__)
    kept = [frame for frame in frames if frame.filename != __file__]
    lines = traceback.format_list(kept)
    lines.extend(traceback.format_exception_only(type(exc), exc))
    return [line.rstrip("\n") for line in lines]


async def _run_cell(cell_id: str, code: str, origin: str | None) -> None:
    global _cell_count
    _cell_count += 1
    filename = f"<cell-{_cell_count}>"
    import linecache

    linecache.cache[filename] = (len(code), None, code.splitlines(True), filename)
    token = _current_cell.set(cell_id)
    origin_token = _current_origin.set(origin)
    ok = True
    try:
        tree = ast.parse(code, filename)
        trailing = None
        if tree.body and isinstance(tree.body[-1], ast.Expr):
            trailing = tree.body.pop()
        flags = ast.PyCF_ALLOW_TOP_LEVEL_AWAIT
        if tree.body:
            body = compile(tree, filename, "exec", flags=flags)
            outcome = eval(body, _namespace)
            if body.co_flags & inspect.CO_COROUTINE:
                await outcome
        if trailing is not None:
            expression = ast.Expression(trailing.value)
            ast.copy_location(expression, trailing)
            ast.fix_missing_locations(expression)
            expr_code = compile(expression, filename, "eval", flags=flags)
            value = eval(expr_code, _namespace)
            if expr_code.co_flags & inspect.CO_COROUTINE:
                value = await value
            if value is not None:
                _namespace["_"] = value
                emit({"type": "result", "id": cell_id, "text": repr(value)})
    except (KeyboardInterrupt, asyncio.CancelledError):
        ok = False
        emit(
            {
                "type": "error",
                "id": cell_id,
                "ename": "KeyboardInterrupt",
                "evalue": "interrupted",
                "traceback": [],
            }
        )
    except BaseException as exc:  # noqa: BLE001
        ok = False
        emit(
            {
                "type": "error",
                "id": cell_id,
                "ename": type(exc).__name__,
                "evalue": str(exc),
                "traceback": _format_traceback(exc),
            }
        )
    finally:
        sys.stdout.flush()
        sys.stderr.flush()
        _current_cell.reset(token)
        _current_origin.reset(origin_token)
        _running["id"] = None
        _running["task"] = None
        emit({"type": "done", "id": cell_id, "ok": ok})


def _snapshot(request: dict) -> None:
    cell_id = request["id"]
    max_bytes = int(request.get("max_variable_bytes") or 16 * 1024 * 1024)
    saved, skipped = [], []
    payload = {}
    for name, value in list(_namespace.items()):
        if name.startswith("_") or name in _PRELOADED or isinstance(value, types.ModuleType):
            continue
        try:
            blob = pickle.dumps(value)
        except Exception:  # noqa: BLE001
            skipped.append(name)
            continue
        if len(blob) > max_bytes:
            skipped.append(name)
            continue
        payload[name] = blob
        saved.append(name)
    try:
        with open(request["path"], "wb") as handle:
            pickle.dump(payload, handle)
    except OSError as exc:
        emit({"type": "done", "id": cell_id, "ok": False, "reason": str(exc)})
        return
    emit({"type": "done", "id": cell_id, "ok": True, "saved": saved, "skipped": skipped})


def _restore(request: dict) -> None:
    cell_id = request["id"]
    try:
        with open(request["path"], "rb") as handle:
            payload = pickle.load(handle)
    except FileNotFoundError:
        emit({"type": "done", "id": cell_id, "ok": True, "restored": [], "failed": [], "reason": "snapshot not found"})
        return
    except Exception as exc:  # noqa: BLE001
        emit({"type": "done", "id": cell_id, "ok": False, "reason": str(exc)})
        return
    restored, failed = [], []
    for name, blob in payload.items():
        try:
            _namespace[name] = pickle.loads(blob)
            restored.append(name)
        except Exception:  # noqa: BLE001
            failed.append(name)
    emit({"type": "done", "id": cell_id, "ok": True, "restored": restored, "failed": failed})


def _list_names(request: dict) -> None:
    names = sorted(
        name
        for name, value in _namespace.items()
        if not name.startswith("_") and name not in _PRELOADED and not isinstance(value, types.ModuleType)
    )
    emit({"type": "done", "id": request["id"], "ok": True, "names": names})


def _on_sigint(signum, frame) -> None:
    task = _running["task"]
    if task is not None and asyncio.current_task(_loop) is task:
        raise KeyboardInterrupt


def _cancel_cell(cell_id: str) -> None:
    task = _running["task"]
    if task is not None and _running["id"] == cell_id and not task.done():
        task.cancel()


def _reader(queue: asyncio.Queue) -> None:
    assert _loop is not None
    for line in _stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError as exc:
            emit({"type": "error", "id": None, "ename": "ProtocolError", "evalue": str(exc), "traceback": []})
            continue
        kind = request.get("type")
        if kind == "interrupt":
            cell_id = _running["id"]
            if cell_id is not None:
                _loop.call_soon_threadsafe(_cancel_cell, cell_id)
                signal.pthread_kill(_main_thread, signal.SIGINT)
            continue
        if kind == "host_reply":
            future = _pending_replies.pop(request.get("id"), None)
            if future is None:
                continue
            if request.get("ok"):
                _loop.call_soon_threadsafe(_resolve, future, request.get("value"))
            else:
                _loop.call_soon_threadsafe(_reject, future, HostError(str(request.get("error", "host request failed"))))
            continue
        _loop.call_soon_threadsafe(queue.put_nowait, request)
        if kind == "shutdown":
            return
    _loop.call_soon_threadsafe(queue.put_nowait, {"type": "shutdown"})


def _resolve(future, value) -> None:
    if not future.done():
        future.set_result(value)


def _reject(future, error) -> None:
    if not future.done():
        future.set_exception(error)


async def _serve() -> None:
    global _loop
    _loop = asyncio.get_running_loop()
    queue: asyncio.Queue = asyncio.Queue()
    threading.Thread(target=_reader, args=(queue,), daemon=True).start()
    emit({"type": "ready", "protocol": PROTOCOL, "python": sys.version.split()[0]})
    while True:
        request = await queue.get()
        kind = request.get("type")
        if kind == "shutdown":
            for future in _pending_replies.values():
                _reject(future, HostError("kernel shutting down"))
            return
        if kind == "execute":
            task = asyncio.ensure_future(
                _run_cell(request["id"], request.get("code", ""), request.get("origin"))
            )
            _running["id"] = request["id"]
            _running["task"] = task
            try:
                await task
            except asyncio.CancelledError:
                pass
        elif kind == "snapshot":
            _snapshot(request)
        elif kind == "restore":
            _restore(request)
        elif kind == "list_names":
            _list_names(request)
        else:
            emit(
                {
                    "type": "error",
                    "id": request.get("id"),
                    "ename": "ProtocolError",
                    "evalue": f"unknown request type {kind!r}",
                    "traceback": [],
                }
            )
            if request.get("id") is not None:
                emit({"type": "done", "id": request["id"], "ok": False})


_stdin = os.fdopen(os.dup(0), "r", encoding="utf-8")
_PRELOADED = frozenset()


def main() -> None:
    global _PRELOADED
    _install_preload()
    _PRELOADED = frozenset(_namespace)
    _capture_fds()
    signal.signal(signal.SIGINT, _on_sigint)
    asyncio.run(_serve())


if __name__ == "__main__":
    main()
