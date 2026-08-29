use std::{env, fs, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("compiler-spike: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    if args.next().as_deref() != Some("transform") {
        return Err(usage());
    }

    let input = PathBuf::from(args.next().ok_or_else(usage)?);
    let mut jsx_import_source = None;
    let mut output = None;

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--jsx-import-source" => jsx_import_source = args.next(),
            "--out" => output = args.next().map(PathBuf::from),
            _ => return Err(format!("unknown argument {flag:?}\n{}", usage())),
        }
    }

    let jsx_import_source = jsx_import_source.ok_or_else(usage)?;
    let output = output.ok_or_else(usage)?;
    let source = fs::read_to_string(&input)
        .map_err(|error| format!("failed to read {}: {error}", input.display()))?;
    let transformed = compiler_spike::transform_tsx(&input, &source, &jsx_import_source)
        .map_err(|error| error.to_string())?;

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    fs::write(&output, transformed)
        .map_err(|error| format!("failed to write {}: {error}", output.display()))?;
    Ok(())
}

fn usage() -> String {
    "usage: compiler-spike transform <input.tsx> --jsx-import-source <package> --out <output.mjs>"
        .to_string()
}
