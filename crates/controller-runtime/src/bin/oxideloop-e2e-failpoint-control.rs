//! E2E-only private UDS client. It is not built without `test-failpoints`.

use std::{
    io::{self, BufRead, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::ExitCode,
};

fn usage() -> &'static str {
    "usage: oxideloop-e2e-failpoint-control --socket SOCKET --point AfterEventBeforeStatus|AfterStatusBeforeAck|AfterPrepareBeforeDispatchAuthorization|AfterStateFinalizerBeforeRootFinalizer"
}

fn main() -> ExitCode {
    let mut values = std::env::args().skip(1);
    let mut socket = None;
    let mut point = None;
    while let Some(flag) = values.next() {
        if flag == "--help" || flag == "-h" {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        let Some(value) = values.next() else {
            eprintln!("{flag} requires a value\n{}", usage());
            return ExitCode::from(2);
        };
        match flag.as_str() {
            "--socket" => socket = Some(PathBuf::from(value)),
            "--point" => point = Some(value),
            _ => {
                eprintln!("unknown argument {flag}\n{}", usage());
                return ExitCode::from(2);
            }
        }
    }
    let (Some(socket), Some(point)) = (socket, point) else {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    };
    if !socket.is_absolute()
        || !matches!(
            point.as_str(),
            "AfterEventBeforeStatus"
                | "AfterStatusBeforeAck"
                | "AfterPrepareBeforeDispatchAuthorization"
                | "AfterStateFinalizerBeforeRootFinalizer"
        )
    {
        eprintln!("invalid socket or point\n{}", usage());
        return ExitCode::from(2);
    }
    match run(&socket, &point) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("failpoint control failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn run(socket: &PathBuf, point: &str) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(point.as_bytes())?;
    stream.write_all(b"\n")?;
    if !matches!(
        point,
        "AfterPrepareBeforeDispatchAuthorization" | "AfterStateFinalizerBeforeRootFinalizer"
    ) {
        let mut armed = [0_u8; 6];
        stream.read_exact(&mut armed)?;
        if armed != *b"ARMED\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected arm response",
            ));
        }
        println!("ARMED");
        return Ok(());
    }
    let mut armed = [0_u8; 6];
    stream.read_exact(&mut armed)?;
    if armed != *b"ARMED\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected arm response",
        ));
    }
    println!("ARMED");
    io::stdout().flush()?;
    let mut reached = [0_u8; 8];
    stream.read_exact(&mut reached)?;
    if reached != *b"REACHED\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected barrier response",
        ));
    }
    // Scripts can run this with a held stdin pipe: `REACHED` is the explicit
    // observable state, and writing RELEASE to that pipe is the only release
    // capability. No network listener or sidecar is involved.
    println!("REACHED");
    io::stdout().flush()?;
    let mut release = String::new();
    io::stdin().lock().read_line(&mut release)?;
    if release != "RELEASE\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected RELEASE on stdin",
        ));
    }
    stream.write_all(b"RELEASE\n")?;
    println!("RELEASED");
    Ok(())
}
