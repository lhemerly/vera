mod cli;

fn main() -> std::process::ExitCode {
    match cli::run(std::env::args_os().skip(1).collect()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err((code, message)) => {
            if !message.is_empty() {
                eprintln!("{message}");
            }
            std::process::ExitCode::from(code)
        }
    }
}
