//! `mesh-client` is what apps link: it must not pull in the WASM runtime, the services
//! crate, the gateway, or the HTTP server stack (sub-spec 8a §1.1 criterion 4). Dev-dependencies
//! (the in-process test hosts) are not part of what apps link, hence `-e normal`.

use std::process::Command;

fn tree(extra: &[&str]) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "tree",
            "-p",
            "mesh-client",
            "-e",
            "normal",
            "--prefix",
            "none",
        ])
        .args(extra)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn starting_with<'a>(tree: &'a str, banned: &[&str]) -> Vec<&'a str> {
    tree.lines()
        .filter(|l| banned.iter().any(|b| l.starts_with(b)))
        .collect()
}

#[test]
fn no_runtime_deps() {
    let all = tree(&[]);
    assert!(all.lines().any(|l| l.starts_with("mesh-client")), "{all}");
    // Nowhere in the tree.
    let bad = starting_with(
        &all,
        &["wasmtime", "mesh-runtime", "mesh-services", "mesh-gateway"],
    );
    assert!(bad.is_empty(), "forbidden dependencies: {bad:?}");
    // iroh itself pulls hyper in (its HTTP client), so for the server stack only the
    // crate's own dependencies are checked.
    let direct = tree(&["--depth", "1"]);
    let bad = starting_with(&direct, &["hyper"]);
    assert!(bad.is_empty(), "forbidden direct dependencies: {bad:?}");
}
