//! Builds and runs the hosts in `hosts/`, each written in another language
//! against the library and header this crate produces.
//!
//! A host whose toolchain is missing is skipped, unless
//! `WASM_CORE_REQUIRE_ALL_HOSTS` is set (as in CI).

#[path = "../../wasm-backend-core/tests/common/mod.rs"]
mod common;

use std::{path::PathBuf, process::Command};

/// The shared library cargo built for these tests, next to the test binary.
fn library() -> PathBuf {
    let name = if cfg!(windows) {
        "wasm_backend.dll"
    } else if cfg!(target_os = "macos") {
        "libwasm_backend.dylib"
    } else {
        "libwasm_backend.so"
    };
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join(name);
    assert!(path.exists(), "{} was not built", path.display());
    path
}

/// The first of `programs` that runs, or `None` (test skipped).
fn tool(programs: &[&str], probe: &str) -> Option<String> {
    let found = programs.iter().find(|p| {
        Command::new(p)
            .arg(probe)
            .output()
            .is_ok_and(|o| o.status.success())
    });
    if found.is_none() {
        let msg = format!("none of {programs:?} is installed");
        assert!(
            std::env::var_os("WASM_CORE_REQUIRE_ALL_HOSTS").is_none(),
            "{msg}"
        );
        eprintln!("{msg}; skipping");
    }
    found.map(|p| p.to_string())
}

/// Run a host and return what it printed.
fn run(command: &mut Command) -> String {
    let output = command.output().expect("failed to run the host");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert!(
        output.status.success(),
        "the host failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

fn hosts() -> PathBuf {
    common::repo().join("hosts")
}

#[cfg(unix)]
#[test]
fn c_host() {
    let compilers = [std::env::var("CC").unwrap_or_default(), "cc".to_string()];
    let compilers: Vec<&str> = compilers
        .iter()
        .map(String::as_str)
        .filter(|c| !c.is_empty())
        .collect();
    let Some(cc) = tool(&compilers, "--version") else {
        return;
    };
    let module = common::rust_component("component-guest");
    let library = library();
    let lib_dir = library.parent().unwrap();
    let out_dir = common::repo().join("target/hosts");
    std::fs::create_dir_all(&out_dir).unwrap();
    let binary = out_dir.join("c-host");
    let include = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
    let status = Command::new(cc)
        .args(["-std=c99", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&binary)
        .arg(hosts().join("c/host.c"))
        .arg("-I")
        .arg(include)
        .arg("-L")
        .arg(lib_dir)
        .arg("-lwasm_backend")
        .arg(format!("-Wl,-rpath,{}", lib_dir.display()))
        .status()
        .expect("failed to run the C compiler");
    assert!(status.success(), "failed to compile hosts/c/host.c");

    // C has no JSON library at hand, so the host prints results as they are.
    let printed = run(Command::new(&binary).arg(&module));
    assert_eq!(
        printed,
        "event wasm-backend://module-loaded\n\
         greet -> \"Hello, C!\"\n\
         fixture asked for the app's name\n\
         call-host -> \"\\\"C host\\\"\"\n\
         event test://hello [1,2]\n\
         describe -> error \"Old is too old\"\n\
         emitting other://x -> refused\n"
    );
}

#[test]
fn python_host() {
    let Some(python) = tool(&["python3", "python"], "--version") else {
        return;
    };
    let module = common::rust_component("component-guest");
    let printed = run(Command::new(python)
        .arg(hosts().join("python/host.py"))
        .arg(library())
        .arg(&module));
    // The module the load gate refused is reported, then listed.
    let (loading, rest) = printed
        .split_once("running: ")
        .expect("the host lists its modules");
    assert!(
        loading.contains("event wasm-backend://module-loaded\n"),
        "{printed}"
    );
    assert!(
        loading.contains("event wasm-backend://module-error\n"),
        "{printed}"
    );
    assert_eq!(
        rest,
        "['fixture']\n\
         not running: [('stranger', 'denied')]\n\
         greet -> Hello, Python!\n\
         fixture asked for the app's name\n\
         call-host -> \"Python host\"\n\
         event test://hello [1, 2]\n\
         describe -> error Old is too old\n\
         emitting other://x -> refused\n"
    );
}
