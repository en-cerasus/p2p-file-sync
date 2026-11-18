use std::{env, path::Path, process::ExitCode};

use p2p_sync_core::host::Sender;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let mode = args.first().and_then(|arg| arg.to_str());
    if !matches!(mode, Some("publish" | "serve"))
        || (mode == Some("publish") && args.len() < 4)
        || (mode == Some("serve") && args.len() != 3)
    {
        return Err(
            "Usage: p2p-send publish <server.json> <state-dir> <file> [file ...]\n       p2p-send serve <server.json> <state-dir>"
                .to_string(),
        );
    }

    let mut sender = Sender::open(Path::new(&args[1]), Path::new(&args[2]))
        .map_err(|error| format!("Could not open sender configuration: {error:?}"))?;
    if mode == Some("publish") {
        for source in &args[3..] {
            let event = sender.publish(Path::new(source)).map_err(|error| {
                format!(
                    "Could not publish {}: {error:?}",
                    Path::new(source).display()
                )
            })?;
            println!(
                "Registered {} (event {})",
                Path::new(source).display(),
                event.sequence
            );
        }
        return Ok(());
    }

    let listener = sender
        .bind()
        .map_err(|error| format!("Could not bind listener: {error:?}"))?;
    println!(
        "Serving on {}",
        listener.local_addr().map_err(|error| error.to_string())?
    );
    for socket in listener.incoming() {
        let socket = socket.map_err(|error| error.to_string())?;
        if let Err(error) = sender.serve(socket) {
            eprintln!("Session ended: {error:?}");
        }
    }
    Ok(())
}
