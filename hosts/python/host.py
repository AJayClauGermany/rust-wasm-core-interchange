"""A host in Python: loads a WIT module, gives it a host command and calls it.

    python host.py <libwasm_backend.so> <component_guest.wasm>

The module is guests/rust/component-guest.
"""

import sys

from wasm_backend import Backend, BackendError, CommandError


def on_event(event, payload):
    if event.startswith("wasm-backend://"):
        print("event", event)
    else:
        print("event", event, payload)


def app_name(module, _args):
    print(module, "asked for the app's name")
    return "Python host"


def load_gate(request):
    # Decide what runs before any of it is compiled.
    if request["name"] == "stranger":
        return "never seen before"


library, module = sys.argv[1:]
config = {
    "modules": {
        "fixture": {
            "path": module,
            # A module gets nothing it isn't granted here.
            "capabilities": {"hostCommands": ["app_name"], "events": {"emit": ["test://*"]}},
        },
        "stranger": {"path": module},
    },
    "limits": {"timeoutMs": 5000, "memoryLimitMb": 64},
}

with Backend(library, config, host_commands={"app_name": app_name}, on_event=on_event, load_gate=load_gate) as backend:
    print("running:", [m["name"] for m in backend.modules()])
    print("not running:", [(m["name"], m["reason"]) for m in backend.inactive()])

    print("greet ->", backend.invoke("fixture", "greet", {"name": "Python"}))
    print("call-host ->", backend.invoke("fixture", "call-host", {"command": "app_name", "args": "null"}))
    backend.invoke("fixture", "emit-event", {"name": "test://hello", "payload": "[1, 2]"})

    # An error the module returns is an exception carrying the value.
    try:
        backend.invoke("fixture", "describe", {"person": {"name": "Old", "ageYears": 200}})
    except CommandError as error:
        print("describe -> error", error.value)

    # What it was not granted is refused.
    try:
        backend.invoke("fixture", "emit-event", {"name": "other://x", "payload": "1"})
        print("emitting other://x -> allowed")
    except BackendError:
        print("emitting other://x -> refused")
