use std::{env, path::Path, process::ExitCode, thread, time::Duration};

use p2p_sync_core::{host::Sender, replica::EventId};

enum PollSource {
    Folder(std::path::PathBuf),
    Peer(std::path::PathBuf),
}

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
    if mode == Some("setup") && (4..=6).contains(&args.len()) {
        let address = |index: usize| {
            args[index]
                .to_str()
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| "Specify an IP address and port.".to_owned())
        };
        let output = Path::new(&args[1]);
        let receivers = match args.get(4) {
            Some(value) => value
                .to_str()
                .and_then(|count| count.parse().ok())
                .ok_or_else(|| "Specify the number of receiver configurations.".to_owned())?,
            None => 1,
        };
        let writers = match args.get(5) {
            Some(value) => value
                .to_str()
                .and_then(|count| count.parse().ok())
                .ok_or_else(|| "Specify the number of writer configurations.".to_owned())?,
            None => 1,
        };
        setup::create(output, address(2)?, address(3)?, receivers, writers)?;
        println!("Test connection files created in {}", output.display());
        return Ok(());
    }
    if !matches!(
        mode,
        Some(
            "publish"
                | "publish-tree"
                | "refresh-tree"
                | "update"
                | "delete"
                | "move"
                | "conflicts"
                | "resolve"
                | "catch-up"
                | "prepare-generation"
                | "serve"
                | "serve-tree"
                | "serve-peer"
        )
    ) || (mode == Some("publish") && args.len() < 4)
        || (matches!(mode, Some("publish-tree" | "refresh-tree")) && args.len() != 4)
        || (matches!(mode, Some("update" | "move")) && args.len() != 5)
        || (mode == Some("delete") && args.len() != 4)
        || (matches!(mode, Some("serve-tree" | "serve-peer")) && args.len() != 5)
        || (mode == Some("serve") && args.len() != 3)
        || (mode == Some("conflicts") && args.len() != 3)
        || (mode == Some("resolve") && args.len() < 5)
        || (mode == Some("catch-up") && args.len() != 4)
        || (mode == Some("prepare-generation") && args.len() != 6)
    {
        return Err(
            "Usage: p2p-send setup <new-dir> <bind-ip:port> <peer-ip:port> [receivers] [writers]\n       p2p-send publish <server.json> <state-dir> <file> [file ...]\n       p2p-send publish-tree <server.json> <state-dir> <folder>\n       p2p-send refresh-tree <server.json> <state-dir> <folder>\n       p2p-send update <server.json> <state-dir> <shared/path> <source>\n       p2p-send delete <server.json> <state-dir> <shared/path>\n       p2p-send move <server.json> <state-dir> <old/path> <new/path>\n       p2p-send conflicts <server.json> <state-dir>\n       p2p-send catch-up <server.json> <state-dir> <direct.json>\n       p2p-send prepare-generation <server.json> <state-dir> <next-server.json> <administrator-seed-file> <new-dir>\n       p2p-send resolve <server.json> <state-dir> <chosen-id> <other-id> [other-id ...]\n       p2p-send serve <server.json> <state-dir>\n       p2p-send serve-peer <server.json> <state-dir> <direct.json> <interval-seconds>\n       p2p-send serve-tree <server.json> <state-dir> <folder> <interval-seconds>"
                .to_string(),
        );
    }

    let mut sender = Sender::open(Path::new(&args[1]), Path::new(&args[2]))
        .map_err(|error| format!("Could not open sender configuration: {error:?}"))?;
    if mode == Some("prepare-generation") {
        let count = sender
            .prepare_generation(
                Path::new(&args[3]),
                Path::new(&args[4]),
                Path::new(&args[5]),
            )
            .map_err(|error| format!("Could not prepare migration: {error:?}"))?;
        println!("Prepared migration with {count} verified file blob(s).");
        return Ok(());
    }
    if mode == Some("catch-up") {
        let added = sender
            .catch_up(Path::new(&args[3]))
            .map_err(|error| format!("Could not synchronize serving cache: {error:?}"))?;
        println!("Imported {added} event(s).");
        return Ok(());
    }
    if mode == Some("conflicts") {
        let conflicts = sender
            .conflicts()
            .map_err(|error| format!("Could not list conflicts: {error:?}"))?;
        for heads in conflicts {
            println!("Conflict");
            for head in heads {
                let peer: String = head
                    .id
                    .peer
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                let description = head
                    .change
                    .entry()
                    .map_or("(deleted)", |entry| entry.name.as_str());
                println!("  {peer}:{}  {description}", head.id.sequence);
            }
        }
        return Ok(());
    }
    if mode == Some("resolve") {
        let choice = parse_event_id(&args[3])?;
        let reviewed = args[3..]
            .iter()
            .map(|value| parse_event_id(value))
            .collect::<Result<_, _>>()?;
        let event = sender
            .resolve_conflict(choice, reviewed)
            .map_err(|error| format!("Could not resolve conflict: {error:?}"))?;
        println!("Recorded resolution (event {})", event.sequence);
        return Ok(());
    }
    if matches!(mode, Some("update" | "delete" | "move")) {
        let shared = args[3].to_str().ok_or("Shared path must be UTF-8.")?;
        let event = if mode == Some("move") {
            let destination = args[4].to_str().ok_or("Destination must be UTF-8.")?;
            sender.relocate(shared, destination)
        } else {
            sender.revise(shared, args.get(4).map(Path::new))
        }
        .map_err(|error| format!("Could not record shared-file change: {error:?}"))?;
        println!("Recorded change for {shared} (event {})", event.sequence);
        return Ok(());
    }
    if matches!(mode, Some("publish-tree" | "refresh-tree")) {
        let root = Path::new(&args[3]);
        let summary = if mode == Some("refresh-tree") {
            sender.refresh_tree(root)
        } else {
            sender.publish_tree(root)
        }
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

    let polling = if matches!(mode, Some("serve-tree" | "serve-peer")) {
        let seconds = args[4]
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|seconds| (1..=86_400).contains(seconds))
            .ok_or("Use an interval from 1 to 86400 seconds.")?;
        let source = if mode == Some("serve-tree") {
            let root = sender
                .validate_source_root(Path::new(&args[3]))
                .map_err(|error| format!("Invalid source directory: {error:?}"))?;
            sender
                .refresh_tree(&root)
                .map_err(|error| format!("Initial scan failed: {error:?}"))?;
            PollSource::Folder(root)
        } else {
            PollSource::Peer(Path::new(&args[3]).to_owned())
        };
        let worker = Sender::open(Path::new(&args[1]), Path::new(&args[2]))
            .map_err(|error| format!("Could not open scan worker: {error:?}"))?;
        Some((worker, source, Duration::from_secs(seconds)))
    } else {
        None
    };

    let listener = sender
        .bind()
        .map_err(|error| format!("Could not bind listener: {error:?}"))?;
    if let Some((mut worker, source, interval)) = polling {
        thread::Builder::new()
            .name("share-refresh".into())
            .spawn(move || {
                if matches!(source, PollSource::Folder(_)) {
                    thread::sleep(interval);
                }
                loop {
                    let result = match &source {
                        PollSource::Folder(root) => worker
                            .validate_source_root(root)
                            .and_then(|root| worker.refresh_tree(&root))
                            .map(|_| ()),
                        PollSource::Peer(config) => worker.catch_up(config).map(|_| ()),
                    };
                    if let Err(error) = result {
                        eprintln!("Share refresh failed: {error:?}");
                    }
                    thread::sleep(interval);
                }
            })
            .map_err(|error| error.to_string())?;
    }
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

fn parse_event_id(value: &std::ffi::OsStr) -> Result<EventId, String> {
    let error = || "Specify a candidate as <64-hex-peer-id>:<sequence>.".to_owned();
    let (peer, sequence) = value
        .to_str()
        .and_then(|text| text.split_once(':'))
        .ok_or_else(error)?;
    if peer.len() != 64 || !peer.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(error());
    }
    let mut decoded = [0; 32];
    for (index, byte) in decoded.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&peer[index * 2..index * 2 + 2], 16).map_err(|_| error())?;
    }
    let sequence = sequence
        .parse::<u64>()
        .ok()
        .filter(|&value| value != 0)
        .ok_or_else(error)?;
    Ok(EventId {
        peer: decoded,
        sequence,
    })
}
