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
_parked_interrupts: set = set()
_interrupt_next = False
_interrupt_lock = threading.Lock()
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
    def __init__(self, exit_code: int, output: str, duration: float = 0.0, timed_out: bool = False, killed: bool = False):
        self.exit_code = exit_code
        self.output = output
        self.duration = duration
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
        duration=float(reply.get("duration_ms", 0)) / 1000,
        timed_out=end.get("kind") == "timed_out",
        killed=end.get("kind") == "killed",
    )


class BashHandle:
    """A shell command started in the background. `await h` waits for it."""

    def __init__(self, command: str, timeout: float | None):
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
        reply = await host_request("bash.running", await self._job())
        return bool(reply.get("running"))

    async def output(self) -> str:
        reply = await host_request("bash.poll", await self._job())
        return str(reply.get("output", ""))

    async def tail(self, lines: int = 50) -> str:
        text = await self.output()
        return "\n".join(text.splitlines()[-lines:])

    async def kill(self) -> None:
        await host_request("bash.kill", await self._job())

    def __repr__(self) -> str:
        state = "pending" if self.job_id is None else self.job_id
        return f"BashHandle({self.command!r}, {state}, pid={self.pid})"


def bash(command: str, timeout: float | None = None) -> BashHandle:
    if not isinstance(command, str) or not command:
        raise TypeError("command must be a non-empty str")
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
        self.paused = reply.get("paused")

    @property
    def settled(self) -> bool:
        return self.status not in ("starting", "running")

    def __repr__(self) -> str:
        note = f", note={self.progress_note!r}" if self.progress_note else ""
        paused = ", paused" if self.paused else ""
        return f"ChildSummary(name={self.name!r}, status={self.status!r}, turns={self.turns}{note}{paused})"


def _child_target(target) -> str:
    if isinstance(target, (ChildHandle, ChildSummary)):
        return target.rlm_child_id
    return str(target)


def _child_targets(targets) -> list:
    if targets is None:
        return []
    if isinstance(targets, (str, ChildHandle, ChildSummary)):
        targets = [targets]
    return [_child_target(target) for target in targets]


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

    async def pause_subagents(self, targets=None, *, reason: str | None = None) -> list:
        payload = {"targets": _child_targets(targets)}
        if reason is not None:
            payload["reason"] = reason
        return [ChildSummary(entry) for entry in await host_request("rlm.pause_subagents", payload)]

    async def resume_subagents(self, targets=None) -> list:
        payload = {"targets": _child_targets(targets)}
        return [ChildSummary(entry) for entry in await host_request("rlm.resume_subagents", payload)]

    async def delete_subagent(self, target) -> ChildSummary:
        return ChildSummary(await host_request("rlm.delete_subagent", {"target": _child_target(target)}))

    async def progress_note(self, text: str) -> None:
        await host_request("rlm.progress_note", {"text": text})

    async def create_session(self, prompt: str, *, name: str | None = None, model: str | None = None) -> dict:
        payload = {"prompt": prompt}
        if name is not None:
            payload["name"] = name
        if model is not None:
            payload["model"] = model
        return await host_request("rlm.create_session", payload)

    async def note(self, text: str) -> str:
        return str((await host_request("rlm.note", {"text": text})).get("notes", ""))

    async def notes(self) -> str:
        return str((await host_request("rlm.notes", {})).get("notes", ""))

    async def clear_notes(self) -> None:
        await host_request("rlm.clear_notes", {})


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

    async def fail(self, error: str) -> dict:
        return await host_request("goal.fail", {"error": error})

    async def pause(self) -> dict:
        return await host_request("goal.pause", {})

    async def resume(self) -> dict:
        return await host_request("goal.resume", {})

    async def clear(self) -> dict:
        return await host_request("goal.clear", {})


class _Compact:
    async def run(self, hint: str | None = None) -> dict:
        payload = {}
        if hint is not None:
            payload["hint"] = hint
        return await host_request("compact.run", payload)


class _AgentObserve:
    async def list_agents(self) -> list:
        reply = await host_request("agent_observe.list_agents", {})
        return reply.get("agents", [])

    async def recent_messages(self, target, *, limit: int = 8, max_chars: int = 800) -> list:
        reply = await host_request(
            "agent_observe.recent_messages",
            {"target": _child_target(target), "limit": limit, "max_chars": max_chars},
        )
        return reply.get("messages", [])


class _Refine:
    async def status(self) -> dict:
        return await host_request("refine.status", {})

    async def run(self, instructions: str | None = None, global_: bool = False) -> dict:
        payload = {}
        if instructions is not None:
            payload["instructions"] = instructions
        if global_:
            payload["global"] = True
        return await host_request("refine.run", payload)


class _Harness:
    async def list(self, kind: str | None = None, *, global_: bool = False) -> list:
        payload = {"global": global_}
        if kind is not None:
            payload["kind"] = kind
        reply = await host_request("harness.list", payload)
        return reply.get("entries", [])

    async def get(self, kind: str, id: str, *, global_: bool = False) -> dict | None:
        reply = await host_request("harness.get", {"kind": kind, "id": id, "global": global_})
        return reply.get("entry")

    async def search(self, query: str, kind: str | None = None, limit: int = 10, *, global_: bool = False) -> list:
        payload = {"query": query, "limit": int(limit), "global": global_}
        if kind is not None:
            payload["kind"] = kind
        reply = await host_request("harness.search", payload)
        return reply.get("entries", [])

    async def overview(self, *, global_: bool = False) -> str:
        reply = await host_request("harness.overview", {"global": global_})
        return str(reply.get("overview", ""))

    async def create(
        self,
        kind: str,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        reference: dict | None = None,
        arguments: dict | None = None,
        metadata: dict | None = None,
        global_: bool = False,
    ) -> dict:
        payload = {
            "kind": kind,
            "title": title,
            "content": content,
            "path": path,
            "global": global_,
        }
        for key, value in (
            ("id", id),
            ("reference", reference),
            ("arguments", arguments),
            ("metadata", metadata),
        ):
            if value is not None:
                payload[key] = value
        reply = await host_request("harness.create", payload)
        return reply.get("entry", {})

    async def update(
        self,
        kind: str,
        id: str,
        title: str | None = None,
        content: str | None = None,
        *,
        path: str | None = None,
        reference: dict | None = None,
        arguments: dict | None = None,
        metadata: dict | None = None,
        global_: bool = False,
    ) -> dict:
        payload = {"kind": kind, "id": id, "global": global_}
        for key, value in (
            ("title", title),
            ("content", content),
            ("path", path),
            ("reference", reference),
            ("arguments", arguments),
            ("metadata", metadata),
        ):
            if value is not None:
                payload[key] = value
        reply = await host_request("harness.update", payload)
        return reply.get("entry", {})

    async def delete(self, kind: str, id: str, *, global_: bool = False) -> bool:
        reply = await host_request("harness.delete", {"kind": kind, "id": id, "global": global_})
        return bool(reply.get("deleted"))

    async def create_memory(self, title: str, content: str, *, id: str | None = None, path: str = "general", metadata: dict | None = None, global_: bool = False) -> dict:
        return await self.create("memory", title, content, id=id, path=path, metadata=metadata, global_=global_)

    async def update_memory(self, id: str, title: str | None = None, content: str | None = None, *, path: str | None = None, metadata: dict | None = None, global_: bool = False) -> dict:
        return await self.update("memory", id, title, content, path=path, metadata=metadata, global_=global_)

    async def delete_memory(self, id: str, *, global_: bool = False) -> bool:
        return await self.delete("memory", id, global_=global_)

    async def create_prompt_note(self, title: str, content: str, *, id: str | None = None, path: str = "policy", metadata: dict | None = None, global_: bool = False) -> dict:
        return await self.create("prompt", title, content, id=id, path=path, metadata=metadata, global_=global_)

    async def update_prompt_note(self, id: str, title: str | None = None, content: str | None = None, *, path: str | None = None, metadata: dict | None = None, global_: bool = False) -> dict:
        return await self.update("prompt", id, title, content, path=path, metadata=metadata, global_=global_)

    async def delete_prompt_note(self, id: str, *, global_: bool = False) -> bool:
        return await self.delete("prompt", id, global_=global_)

    async def create_skill(self, title: str, content: str, *, id: str | None = None, path: str = "general", reference: dict | None = None, arguments: dict | None = None, metadata: dict | None = None, global_: bool = False) -> dict:
        return await self.create("skill", title, content, id=id, path=path, reference=reference, arguments=arguments, metadata=metadata, global_=global_)

    async def update_skill(self, id: str, title: str | None = None, content: str | None = None, *, path: str | None = None, reference: dict | None = None, arguments: dict | None = None, metadata: dict | None = None, global_: bool = False) -> dict:
        return await self.update("skill", id, title, content, path=path, reference=reference, arguments=arguments, metadata=metadata, global_=global_)

    async def delete_skill(self, id: str, *, global_: bool = False) -> bool:
        return await self.delete("skill", id, global_=global_)

    async def create_subagent(self, title: str, content: str, *, id: str | None = None, path: str = "general", metadata: dict | None = None, global_: bool = False) -> dict:
        return await self.create("subagent", title, content, id=id, path=path, metadata=metadata, global_=global_)

    async def update_subagent(self, id: str, title: str | None = None, content: str | None = None, *, path: str | None = None, metadata: dict | None = None, global_: bool = False) -> dict:
        return await self.update("subagent", id, title, content, path=path, metadata=metadata, global_=global_)

    async def delete_subagent(self, id: str, *, global_: bool = False) -> bool:
        return await self.delete("subagent", id, global_=global_)

    async def record_refinement(self, trigger: str, changes, *, evidence: str = "", outcome: str = "", id: str | None = None, global_: bool = False) -> dict:
        payload = {"trigger": trigger, "changes": changes, "evidence": evidence, "outcome": outcome, "global": global_}
        if id is not None:
            payload["id"] = id
        reply = await host_request("harness.record_refinement", payload)
        return reply.get("refinement", {})


class _RlmHeartbeat:
    async def create(self, prompt: str, *, interval: str = "5m", label: str | None = None) -> dict:
        payload = {"prompt": prompt, "interval": interval}
        if label is not None:
            payload["label"] = label
        return await host_request("rlm_heartbeat.create", payload)

    async def list(self) -> list:
        return await host_request("rlm_heartbeat.list", {})

    async def update(self, id: str, *, status: str) -> dict:
        return await host_request("rlm_heartbeat.update", {"id": id, "status": status})


harness = _Harness()
_Rlm.harness = harness
rlm = _Rlm()
agent_message = _AgentMessage()
goal = _Goal()
rlm_heartbeat = _RlmHeartbeat()
compact = _Compact()
refine = _Refine()
agent_observe = _AgentObserve()


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
            "rlm_heartbeat": rlm_heartbeat,
            "compact": compact,
            "refine": refine,
            "harness": harness,
            "agent_observe": agent_observe,
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
        if _take_parked_interrupt(cell_id):
            raise KeyboardInterrupt
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
        with _interrupt_lock:
            _running["id"] = None
            _running["task"] = None
            _parked_interrupts.discard(cell_id)
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


def _interrupt(cell_id) -> None:
    """Interrupt a cell, parking the request when that cell has not started.

    A bare interrupt hits the running cell, or the next one to start. An
    interrupt naming a cell only ever hits that cell. Runs on the reader
    thread: the signal has to reach a loop stuck in synchronous code.
    """
    global _interrupt_next
    with _interrupt_lock:
        running = _running["id"]
        if cell_id is None:
            if running is None:
                _interrupt_next = True
                return
            cell_id = running
        elif cell_id != running:
            _parked_interrupts.add(cell_id)
            return
    _deliver_interrupt(cell_id)


def _deliver_interrupt(cell_id: str) -> None:
    assert _loop is not None
    _loop.call_soon_threadsafe(_cancel_cell, cell_id)
    signal.pthread_kill(_main_thread, signal.SIGINT)


def _take_parked_interrupt(cell_id: str) -> bool:
    global _interrupt_next
    with _interrupt_lock:
        if cell_id in _parked_interrupts:
            _parked_interrupts.discard(cell_id)
            return True
        if _interrupt_next:
            _interrupt_next = False
            return True
        return False


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
            _interrupt(request.get("id"))
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
            with _interrupt_lock:
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
