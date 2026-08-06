// The Python "kernel forkserver": a long-lived template process that pays the
// ~1.2s IPython/ipykernel/rlm import cost once, then forks a ready-to-run kernel
// per request in ~ms. Children inherit the imported module objects via
// copy-on-write, bypassing the (slow, virtiofs-backed) per-file import path.
//
// Embedded as a string rather than shipped as a package asset so it can never be
// missing from a release layout (see the built-in-skills packaging gap). Run via
// `python -c <this> <control-socket-path> [<history-bound>]`.
//
// Protocol (newline-delimited JSON over the unix socket, forkserver is the client):
//   -> { "id": <n>, "connectionPath": "<abs path>" }   spawn request from Node
//   -> { "id": <n>, "kill": <fork-id>, "signal": "TERM"|"KILL" }  kill a forked child
//   -> { "id": <n>, "alive": <fork-id> }               liveness query for a child
//   <- { "type": "ready" }                             once, after imports finish
//   <- { "id": <n>, "pid": <pid> }                     fork succeeded
//   <- { "id": <n>, "error": "<message>" }             fork failed
//   <- { "id": <n>, "outcome": "signaled"|"already-exited"|"unknown-pid" }  kill reply
//   <- { "id": <n>, "alive": true|false }              alive reply
//   <- { "type": "exit", "id": <fork-id> }             child exit event
//
// Kill/alive are keyed by the never-reused fork request id, so a handle can only
// name the one child incarnation it forked (a recycled pid can't alias), and the
// forkserver signals only its own un-reaped children — POSIX-race-free.
export const FORK_SERVER_SCRIPT = String.raw`
import gc
import json
import os
import signal
import socket
import sys
import threading
import time

# fork id -> [pid, alive]; _pid_to_id holds un-reaped, un-evicted children only.
# The request loop and one reaper thread serialize these through _children_lock.
_children = {}
_pid_to_id = {}
# FIFO history bound; kill/alive for an evicted id fail closed.
_history_bound = 4096
_control = None
_write_lock = threading.Lock()
_children_lock = threading.Lock()
_children_changed = threading.Condition(_children_lock)


def _send(message):
    payload = json.dumps(message).encode() + b"\n"
    with _write_lock:
        _control.sendall(payload)


def _watch_children():
    # Block in waitpid whenever at least one child is live. The condition only
    # handles the no-child interval; child exit itself is the wake event.
    while True:
        with _children_changed:
            _children_changed.wait_for(lambda: any(entry[1] for entry in _children.values()))
        try:
            pid, _status = os.waitpid(-1, 0)
        except ChildProcessError:
            continue
        child_id = None
        with _children_lock:
            child_id = _pid_to_id.pop(pid, None)
            if child_id is not None:
                _children[child_id][1] = False
        if child_id is not None:
            _send({"type": "exit", "id": child_id})


def _watch_parent(original_ppid):
    # A SIGKILLed owner can't close our socket while a forked child still holds the
    # fd; poll ppid so the forkserver dies with its owner regardless.
    while True:
        if os.getppid() != original_ppid:
            os._exit(1)
        time.sleep(1.0)


def _import_template():
    # Everything a kernel touches at import time. Paid once; shared COW by children.
    import IPython  # noqa: F401
    import ipykernel  # noqa: F401
    import ipykernel.kernelapp  # noqa: F401
    import jupyter_client  # noqa: F401
    import nest_asyncio  # noqa: F401
    try:
        import rlm  # noqa: F401
    except Exception:
        # rlm may not import cleanly outside a live kernel namespace; the Node-side
        # bootstrap cell wires it up per-child regardless. Preloading is a best-effort
        # speedup, not a correctness requirement.
        pass


def _run_child(connection_path, cwd, env):
    # We are the forked child; become the ipykernel server on the given connection.
    from ipykernel.kernelapp import IPKernelApp

    # Drop the inherited SIGCHLD reaper so it can't interfere with ipykernel's own
    # child/signal handling.
    signal.signal(signal.SIGCHLD, signal.SIG_DFL)

    # cwd/env are per-kernel and applied here (not at template import), so all
    # kernels can share one warm template regardless of their working dir / env.
    if env:
        os.environ.update(env)
    if cwd:
        # Don't swallow a bad cwd: direct spawn fails fast on ENOENT, so match that
        # (the OSError propagates, the child exits non-zero, Node falls back).
        os.chdir(cwd)

    # Drop any singleton the template happened to build so the child owns a fresh
    # instance (and, critically, a jupyter_client Session created in *this* pid;
    # a Session inherited from the template silently drops messages via check_pid).
    IPKernelApp.clear_instance()
    # Watch our real parent (the forkserver): ipykernel distrusts a handle that
    # differs from getppid(), and its pid-1 fallback breaks under subreapers.
    app = IPKernelApp.instance(
        connection_file=connection_path,
        parent_handle=os.getppid(),
    )
    # initialize() binds the 5 ZMQ ports, writes the resolved ports back into
    # connection.json, and starts the heartbeat thread + ioloop — all post-fork,
    # so no thread/loop/socket is ever inherited across the fork boundary.
    app.initialize([])
    app.start()


def _serve(control_path):
    global _control
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(control_path)
    _control = sock
    control_fd = sock.fileno()

    # Compare against the original ppid (not ==1) so this holds under subreapers.
    # Threads aren't inherited across fork, so children never carry this watcher.
    threading.Thread(target=_watch_parent, args=(os.getppid(),), daemon=True).start()

    # Reap child exits independently of the request loop.
    threading.Thread(target=_watch_children, daemon=True).start()

    _import_template()
    # Freeze the heap so the cyclic GC doesn't write to (and thus COW-copy) the
    # shared module pages, keeping memory genuinely shared across children.
    gc.freeze()

    f = sock.makefile("rb", buffering=0)
    _send({"type": "ready"})

    while True:
        line = f.readline()
        if not line:
            break
        try:
            req = json.loads(line)
        except ValueError:
            continue
        req_id = req.get("id")

        kill_id = req.get("kill")
        if kill_id is not None:
            # The registry lock serializes check+kill with the only reaper.
            with _children_lock:
                entry = _children.get(kill_id)
                if entry is None:
                    outcome = "unknown-pid"
                elif not entry[1]:
                    outcome = "already-exited"
                else:
                    sig = signal.SIGKILL if req.get("signal") == "KILL" else signal.SIGTERM
                    try:
                        os.kill(entry[0], sig)
                        outcome = "signaled"
                    except ProcessLookupError:
                        outcome = "already-exited"
            _send({"id": req_id, "outcome": outcome})
            continue

        alive_id = req.get("alive")
        if alive_id is not None:
            with _children_lock:
                entry = _children.get(alive_id)
            _send({"id": req_id, "alive": bool(entry and entry[1])})
            continue

        connection_path = req.get("connectionPath")
        cwd = req.get("cwd")
        env = req.get("env")

        # Hold the registry lock across fork+bookkeeping so the reaper cannot
        # observe a fast exit before the id-to-pid relationship exists.
        with _children_lock:
            try:
                pid = os.fork()
            except OSError as exc:
                _send({"id": req_id, "error": str(exc)})
                continue

            if pid != 0:
                _children[req_id] = [pid, True]
                _pid_to_id[pid] = req_id
                _children_changed.notify()
                # FIFO-evict only exited entries: dropping a live child would make its
                # liveness read false and its kill unroutable. Live entries are bounded
                # by real kernels.
                if len(_children) > _history_bound:
                    for evicted_id in [i for i, e in _children.items() if not e[1]]:
                        if len(_children) <= _history_bound:
                            break
                        _children.pop(evicted_id)

        if pid == 0:
            # Shed every inherited fd tied to the control channel, then run.
            try:
                sock.close()
                f.close()
            except Exception:
                pass
            try:
                os.close(control_fd)
            except OSError:
                pass
            try:
                _run_child(connection_path, cwd, env)
            except BaseException as exc:  # never return to the accept loop
                sys.stderr.write("forked kernel failed: %r\n" % (exc,))
                os._exit(1)
            os._exit(0)

        # Parent: stay pristine (no loop/ZMQ ever) so the next fork is clean.
        _send({"id": req_id, "pid": pid})


if __name__ == "__main__":
    if len(sys.argv) > 2:
        _history_bound = int(sys.argv[2])
    _serve(sys.argv[1])
`;
