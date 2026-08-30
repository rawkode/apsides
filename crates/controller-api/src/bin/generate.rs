use std::{env, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let check = env::args().skip(1).any(|argument| argument == "--check");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    match oxideloop_controller_api::write_or_check_crds(&root, check) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
