//! Test fixture: a WIT guest built for `wasm32-unknown-unknown`. The build
//! is a core module carrying its component type, which the host wraps into
//! a component when it loads it. There is no WASI underneath: the
//! `tauri:backend/host` interface is everything the module can reach.

use std::cell::Cell;

wit_bindgen::generate!({ world: "bare", generate_all });

use tauri::backend::host::{self, LogLevel};

struct Bare;

thread_local! {
    static COUNT: Cell<u32> = const { Cell::new(0) };
}

impl Guest for Bare {
    fn greet(name: String) -> String {
        host::log(LogLevel::Info, &format!("greeting {name}"));
        format!("Hello, {name}!")
    }

    fn count() -> u32 {
        COUNT.with(|c| {
            c.set(c.get() + 1);
            c.get()
        })
    }

    fn call_host(command: String, args: String) -> Result<String, String> {
        host::invoke(&command, &args)
    }

    fn crash() {
        panic!("crash requested");
    }
}

export!(Bare);
