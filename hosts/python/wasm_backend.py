"""A Python binding for libwasm_backend, on ctypes: no build step, no
dependencies.

    backend = Backend(
        "path/to/libwasm_backend.so",
        {"modules": {"greeter": {"path": "greeter.wasm"}}},
        host_commands={"now": lambda module, args: time.time()},
    )
    backend.invoke("greeter", "greet", {"name": "Python"})

The whole C interface is in crates/wasm-backend-ffi/include/wasm_backend.h;
this file wraps it in the shape Python expects: values instead of JSON text,
exceptions instead of status codes.
"""

import ctypes
import json

_EVENT_FN = ctypes.CFUNCTYPE(None, ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p)
_CALL_FN = ctypes.CFUNCTYPE(
    None, ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_void_p
)

# WASM_BACKEND_STATUS_*
OK, COMMAND, NOT_ALLOWED = 0, 7, 12


class BackendError(Exception):
    """A call failed. `status` is its WASM_BACKEND_STATUS_* code."""

    def __init__(self, status, message):
        super().__init__(message)
        self.status = status


class CommandError(BackendError):
    """A command ran and returned an error. `value` is what it returned."""

    def __init__(self, value):
        super().__init__(COMMAND, value if isinstance(value, str) else json.dumps(value))
        self.value = value


def _text(value):
    return None if value is None else value.encode()


def _json(value):
    return None if value is None else json.dumps(value).encode()


class Backend:
    """A running backend. `config` is the configuration as a dict.

    `host_commands` maps names to `f(module, args)`: the return value is the
    command's result, an exception its error. `on_event(event, payload)`
    receives every event. `load_gate(request)` returns nothing to let a
    module load, or the reason it may not.
    """

    def __init__(self, library, config=None, *, host_commands=None, on_event=None, load_gate=None):
        lib = self._lib = ctypes.CDLL(str(library))
        lib.wasm_backend_reply_ok.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        lib.wasm_backend_reply_err.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        lib.wasm_backend_string_free.argtypes = [ctypes.c_void_p]
        lib.wasm_backend_free.argtypes = [ctypes.c_void_p]
        self._backend = None
        # Callbacks must outlive the backend that calls them.
        self._callbacks = []

        builder = ctypes.c_void_p()
        self._call("builder_new", _json(config), ctypes.byref(builder))
        if on_event:
            callback = _EVENT_FN(lambda _, event, payload: on_event(event.decode(), json.loads(payload)))
            self._callbacks.append(callback)
            lib.wasm_backend_builder_on_event(builder, callback, None)
        for name, command in (host_commands or {}).items():
            callback = _CALL_FN(self._answer(lambda module, args, f=command: f(module, args)))
            self._callbacks.append(callback)
            self._call("builder_host_command", builder, _text(name), callback, None)
        if load_gate:

            def gate(_module, request):
                reason = load_gate(request)
                if reason:
                    raise PermissionError(reason)

            callback = _CALL_FN(self._answer(gate))
            self._callbacks.append(callback)
            lib.wasm_backend_builder_load_gate(builder, callback, None)
        backend = ctypes.c_void_p()
        self._call("builder_build", builder, ctypes.byref(backend))
        self._backend = backend

    def _answer(self, f):
        """Wrap `f(module, input)` as a C callback that replies with its result."""

        def callback(_user_data, module, input_json, reply):
            try:
                result = f(module.decode(), json.loads(input_json))
                self._lib.wasm_backend_reply_ok(reply, _json(result))
            except Exception as error:  # An exception must not cross into C.
                self._lib.wasm_backend_reply_err(reply, _text(str(error) or type(error).__name__))

        return callback

    def _call(self, function, *args, raw=False):
        """Call `wasm_backend_<function>(*args, &out)` and return what it left in `out`."""
        out = ctypes.c_void_p()
        f = getattr(self._lib, "wasm_backend_" + function)
        f.restype = ctypes.c_int32
        status = f(*args, ctypes.byref(out))
        text = None
        if out.value:
            text = ctypes.string_at(out.value).decode()
            self._lib.wasm_backend_string_free(out)
        if status == COMMAND:
            raise CommandError(json.loads(text))
        if status != OK:
            raise BackendError(status, text)
        return text if raw or text is None else json.loads(text)

    def invoke(self, module, command, args=None):
        """Call a module's command and return its result."""
        return self._call("invoke", self._backend, _text(module), _text(command), _json(args))

    def emit(self, event, payload=None):
        """Tell the modules listening to `event` that it happened."""
        self._call("emit", self._backend, _text(event), _json(payload))

    def modules(self):
        """Every loaded module."""
        return self._call("modules", self._backend)

    def inactive(self):
        """Every module that is known but not running, and why."""
        return self._call("inactive", self._backend)

    def load(self, name, config):
        return self._call("load", self._backend, _text(name), _json(config))

    def load_bytes(self, name, data, config=None):
        return self._call("load_bytes", self._backend, _text(name), data, ctypes.c_size_t(len(data)), _json(config))

    def unload(self, name):
        self._call("unload", self._backend, _text(name))

    def reload(self, name):
        return self._call("reload", self._backend, _text(name))

    def enable(self, name):
        return self._call("enable", self._backend, _text(name))

    def disable(self, name):
        self._call("disable", self._backend, _text(name))

    def state(self, module):
        return self._call("state", self._backend, _text(module))

    def clear_state(self, module):
        self._call("clear_state", self._backend, _text(module))

    def typescript(self, module):
        return self._call("typescript", self._backend, _text(module), raw=True)

    def close(self):
        if self._backend:
            self._lib.wasm_backend_free(self._backend)
            self._backend = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
