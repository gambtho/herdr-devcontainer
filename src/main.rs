use herdr_devcontainer::error::Error;
use herdr_devcontainer::{open, pane};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result: Result<(), Error> = match args.first().map(String::as_str) {
        Some("pane") => pane::run_pane(args.iter().any(|a| a == "--shell")),
        Some("stop") => {
            let result = herdr_devcontainer::stop::run_stop();
            if result.is_ok() {
                hold();
            }
            result
        }
        Some("open") => open::run_open(args.get(1).map(String::as_str)),
        _ => {
            eprintln!("usage: herdr-devc <pane [--shell] | stop | open <entrypoint>>");
            std::process::exit(2);
        }
    };
    if let Err(err) = result {
        eprintln!("error: {err}");
        if let Some(hint) = err.hint() {
            eprintln!("hint: {hint}");
        }
        hold();
        std::process::exit(1);
    }
}

/// Herdr may close the pane the moment its command exits; keep the message
/// on screen until the user acknowledges it.
fn hold() {
    eprintln!("press Enter to close");
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
}
