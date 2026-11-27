//! Generates the commands from the proto (D52), and names the commit for `version`.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let set = ironflowx_proto::wire::FILE_DESCRIPTOR_SET;
    if let Err(e) = ironflowx_codegen::generate(set, &out) {
        println!("cargo::error={e}");
    }
    println!("cargo::rerun-if-changed=build.rs");
    // A checkout writes HEAD, and a commit the branch it names, if any. Cargo takes a file that is
    // not there, as a packed branch's is not, for a changed one.
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    for name in ["HEAD"].into_iter().chain(branch.as_deref()) {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", name]) {
            println!("cargo::rerun-if-changed={path}");
        }
    }
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    println!("cargo::rustc-env=IRONFLOWX_COMMIT={commit}");
}

/// The line git prints; `None` outside a checkout.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let line = String::from_utf8(out.stdout).ok()?;
    out.status.success().then(|| line.trim().to_owned())
}
