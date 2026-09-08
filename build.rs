use std::path::Path;

fn main() {
    // The dashboard HTML references styles.css, which is compiled from
    // dashboard/styles.scss by the `just css` recipe. Without it the server
    // builds but ships an unstyled dashboard, so fail the build early.
    let css = "dashboard/styles.css";
    if !Path::new(css).exists() {
        eprintln!("error: {css} is missing.");
        eprintln!("Generate it with: just css");
        eprintln!("  (or: grass dashboard/styles.scss dashboard/styles.css --style=compressed)");
        std::process::exit(1);
    }

    // rust-embed reads dashboard/ files at proc-macro expansion; cargo does
    // not know that, so without rerun-if-changed lines editing an embedded
    // file would silently ship a stale dashboard. Track every embedded file.
    println!("cargo:rerun-if-changed=dashboard/styles.scss");
    println!("cargo:rerun-if-changed=dashboard/styles.css");
    println!("cargo:rerun-if-changed=dashboard/index.html");
    println!("cargo:rerun-if-changed=dashboard/app.js");
}
