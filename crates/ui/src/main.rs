fn main() {
    let mut start_hidden = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--hidden" | "--tray" => start_hidden = true,
            "--version" => {
                println!("parla-ui {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-h" | "--help" => {
                println!(
                    "usage: parla-ui [--hidden]\n\n  --hidden   start with the main window closed (tray and overlay only)"
                );
                return;
            }
            other => {
                eprintln!("parla-ui: unknown option {other}");
                std::process::exit(2);
            }
        }
    }
    std::process::exit(parla_ui::host::run(start_hidden));
}
