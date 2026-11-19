//! Dev-only: rewrites the store schema of a stopped node, to check the incompatible-store
//! refusal by hand (`docs/manual-checks/sub-spec-8b.md` §4). Never use it on data you need
//! without a backup.
//!
//!   cargo run -p mesh-store --features test-hooks --example set_schema -- <store.redb | data dir> <schema>
//!
//! A data directory means its `store.redb`. The file must exist (redb would otherwise create
//! an empty one), and no process may have it open: a running meshd holds the store's lock,
//! and the rewrite is then refused.

use std::path::PathBuf;
use std::process::ExitCode;

use mesh_store::{SCHEMA_VERSION, StoreError, test_hooks::set_schema};

const USAGE: &str = "usage: set_schema <store.redb | data dir> <schema>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path, schema] = args.as_slice() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let Ok(schema) = schema.parse::<u64>() else {
        eprintln!("{USAGE}\nnot a schema number: {schema}");
        return ExitCode::from(2);
    };
    let mut path = PathBuf::from(path);
    if path.is_dir() {
        path.push("store.redb");
    }
    if !path.is_file() {
        eprintln!("no store at {}", path.display());
        return ExitCode::FAILURE;
    }
    match set_schema(&path, schema) {
        Ok(()) => {
            println!(
                "{}: store schema set to {schema} (this build reads {SCHEMA_VERSION})",
                path.display()
            );
            ExitCode::SUCCESS
        }
        // `StoreError::Io` carries redb's DatabaseAlreadyOpen as this text (error.rs).
        Err(StoreError::Io(e)) if e == "database already open" => {
            eprintln!(
                "{} is open in another process (a running meshd?); stop it first",
                path.display()
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}
