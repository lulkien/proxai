use std::path::Path;

fn main() {
    // The dashboard HTML references styles.css, which is compiled from
    // dashboard/styles.scss by the `just css` recipe. Without it the server
    // builds but ships an unstyled dashboard, so fail the build early.
    let css = "dashboard/styles.css";
    if !Path::new(css).exists() {
        eprintln!("error: {css} is missing.");
        eprintln!("Generate it with: just css");
        eprintln!(
            "  (or: sass dashboard/styles.scss dashboard/styles.css --no-source-map --style=compressed)"
        );
        std::process::exit(1);
    }

    println!("cargo:rerun-if-changed=dashboard/styles.scss");
    println!("cargo:rerun-if-changed=dashboard/styles.css");
}
