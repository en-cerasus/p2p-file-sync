use std::{env, path::Path, process::ExitCode};

use p2p_sync_core::host::Sender;

#[path = "p2p_send/setup.rs"]
mod setup;

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
    if mode == Some("setup") && args.len() == 4 {
        let address = |index: usize| {
            args[index]
                .to_str()
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| "Specify an IP address and port.".to_owned())
        };
        let output = Path::new(&args[1]);
        setup::create(output, address(2)?, address(3)?)?;
        println!("Test connection files created in {}", output.display());
        return Ok(());
    }
    if !matches!(mode, Some("publish" | "publish-tree" | "serve"))
        || (mode == Some("publish") && args.len() < 4)
        || (mode == Some("publish-tree") && args.len() != 4)
        || (mode == Some("serve") && args.len() != 3)
    {
        return Err(
            "Usage: p2p-send setup <new-dir> <bind-ip:port> <peer-ip:port>\n       p2p-send publish <server.json> <state-dir> <file> [file ...]\n       p2p-send publish-tree <server.json> <state-dir> <folder>\n       p2p-send serve <server.json> <state-dir>"
                .to_string(),
        );
    }

    let mut sender = Sender::open(Path::new(&args[1]), Path::new(&args[2]))
        .map_err(|error| format!("Could not open sender configuration: {error:?}"))?;
    if mode == Some("publish-tree") {
        let summary = sender
            .publish_tree(Path::new(&args[3]))
            .map_err(|error| format!("Could not publish folder: {error:?}"))?;
        println!(
            "Registered {} file(s) and {} folder(s); skipped {} link or special entry(s).",
            summary.files, summary.directories, summary.skipped
        );
        return Ok(());
    }
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
