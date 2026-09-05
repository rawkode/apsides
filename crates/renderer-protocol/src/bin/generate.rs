use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let check = matches!(args.next().as_deref(), Some(value) if value == "--check");
    let root = if check {
        std::env::current_dir()?
    } else {
        args.next().map_or(std::env::current_dir()?, PathBuf::from)
    };
    renderer_protocol::generate::generate(&root, check)
}
