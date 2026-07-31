use std::path::Path;

fn main() {
    let scss = Path::new("styles.scss");
    let css = Path::new("styles.css");

    println!("cargo:rerun-if-changed={}", scss.display());

    let content = std::fs::read_to_string(scss).expect("read styles.scss");
    match grass::from_string(content, &grass::Options::default()) {
        Ok(output) => {
            std::fs::write(css, output).expect("write styles.css");
            println!("cargo:warning=SCSS compiled -> {}", css.display());
        }
        Err(e) => {
            // If grass fails (e.g. complex SCSS), try external sass
            println!("cargo:warning=grass failed: {e}, trying sass CLI");
            if let Ok(_) = std::process::Command::new("sass")
                .args([
                    scss.to_str().unwrap(),
                    css.to_str().unwrap(),
                    "--no-source-map",
                ])
                .status()
            {
                println!("cargo:warning=SCSS compiled via sass CLI");
            } else {
                panic!("SCSS compilation failed and sass CLI not available");
            }
        }
    }
}
