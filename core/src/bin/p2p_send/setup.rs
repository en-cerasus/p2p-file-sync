use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::Path,
};

use p2p_sync_core::{auth::PeerSigningKey, sha256};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};

pub fn create(
    output: &Path,
    bind: SocketAddr,
    endpoint: SocketAddr,
    receivers: usize,
    writers: u16,
) -> Result<(), String> {
    if !(1..=32).contains(&receivers) {
        return Err("Generate between 1 and 32 receiver configurations.".into());
    }
    if !(1..=8).contains(&writers)
        || bind
            .port()
            .checked_add(writers + receivers as u16 - 1)
            .is_none()
    {
        return Err("Generate between 1 and 8 writers with enough consecutive ports.".into());
    }
    if bind.port() == 0
        || bind.port() != endpoint.port()
        || endpoint.ip().is_unspecified()
        || endpoint.ip().is_multicast()
    {
        return Err("Use matching nonzero ports and a specific peer address.".into());
    }
    let write_error = |error: io::Error| format!("Could not write setup files: {error}");
    let certificate_error =
        |error: rcgen::Error| format!("Could not issue test certificates: {error}");
    let mut share = [0; 16];
    getrandom::fill(&mut share).map_err(|error| error.to_string())?;
    let mut members = serde_json::Map::new();
    let share = hex(&share);

    let now = OffsetDateTime::now_utc();
    let mut ca_params = CertificateParams::default();
    ca_params.not_before = now - Duration::minutes(5);
    ca_params.not_after = now + Duration::days(30);
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let ca =
        CertifiedIssuer::self_signed(ca_params, KeyPair::generate().map_err(certificate_error)?)
            .map_err(certificate_error)?;
    let issue = |names, usages| {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(names)?;
        params.not_before = now - Duration::minutes(5);
        params.not_after = now + Duration::days(30);
        params.extended_key_usages = usages;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let certificate = params.signed_by(&key, &ca)?;
        Ok::<_, rcgen::Error>((certificate, key))
    };
    let host = endpoint.ip().to_string();
    fs::create_dir(output).map_err(write_error)?;
    let mut servers = Vec::new();
    let mut endpoints = Vec::new();
    let mut client_pins = Vec::new();
    for index in 0..writers {
        let directory = output.join(if index == 0 {
            "sender".into()
        } else {
            format!("writer-{}", index + 1)
        });
        fs::create_dir(&directory).map_err(write_error)?;
        let (certificate, key) = issue(
            vec![host.clone()],
            vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ],
        )
        .map_err(certificate_error)?;
        write_new(&directory.join("ca.der"), ca.der().as_ref()).map_err(write_error)?;
        write_new(
            &directory.join("certificate.der"),
            certificate.der().as_ref(),
        )
        .map_err(write_error)?;
        write_new(&directory.join("private-key.der"), &key.serialize_der()).map_err(write_error)?;
        let pin = hex(&sha256(certificate.der().as_ref()));
        let mut address = endpoint;
        address.set_port(endpoint.port() + index);
        endpoints.push(json!({
            "address": address.to_string(), "server_name": host, "server_pin": pin,
        }));
        let mut listen = bind;
        listen.set_port(bind.port() + index);
        servers.push((directory.clone(), listen, pin.clone()));
        client_pins.push(pin);
        write_event_key(&directory, "writer", &mut members, write_error)?;
    }

    let mut clients = Vec::with_capacity(receivers);
    let mut reader_endpoints = Vec::with_capacity(receivers);
    for index in 1..=receivers {
        let (client_cert, client_key) = issue(
            vec![host.clone()],
            vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ],
        )
        .map_err(certificate_error)?;
        let receiver = if receivers == 1 {
            output.join("receiver")
        } else {
            output.join(format!("receiver-{index}"))
        };
        fs::create_dir(&receiver).map_err(write_error)?;
        write_new(&receiver.join("ca.der"), ca.der().as_ref()).map_err(write_error)?;
        write_new(
            &receiver.join("certificate.der"),
            client_cert.der().as_ref(),
        )
        .map_err(write_error)?;
        write_new(
            &receiver.join("private-key.der"),
            &client_key.serialize_der(),
        )
        .map_err(write_error)?;
        let pin = hex(&sha256(client_cert.der().as_ref()));
        client_pins.push(pin.clone());
        write_event_key(&receiver, "reader", &mut members, write_error)?;
        let mut listen = bind;
        listen.set_port(bind.port() + writers + index as u16 - 1);
        let mut address = endpoint;
        address.set_port(endpoint.port() + writers + index as u16 - 1);
        reader_endpoints.push(json!({
            "address": address.to_string(), "server_name": host, "server_pin": pin,
        }));
        clients.push((receiver, listen, pin));
    }
    let direct_config = |peers: &[serde_json::Value], reader: bool| {
        let first = &peers[0];
        let mut client = json!({
            "share": share, "generation": 1, "members": members,
            "address": first["address"], "server_name": first["server_name"],
            "server_pin": first["server_pin"],
            "ca_der": "ca.der", "certificate_der": "certificate.der", "private_key_der": "private-key.der",
        });
        if reader {
            client["event_key"] = "event-key.bin".into();
        }
        if peers.len() > 1 {
            client["fallback_peers"] = json!(&peers[1..]);
        }
        client
    };
    for (index, (receiver, listen, pin)) in clients.into_iter().enumerate() {
        let peers: Vec<_> = endpoints
            .iter()
            .chain(
                reader_endpoints
                    .iter()
                    .enumerate()
                    .filter_map(|(other, peer)| (other != index).then_some(peer)),
            )
            .take(9)
            .cloned()
            .collect();
        let client = direct_config(&peers, true);
        write_new(&receiver.join("direct.json"), client.to_string().as_bytes())
            .map_err(write_error)?;
        let pins: Vec<_> = client_pins.iter().filter(|peer| **peer != pin).collect();
        let server = json!({
            "share": share, "generation": 1, "members": members,
            "bind": listen.to_string(), "client_pins": pins,
            "ca_der": "ca.der", "certificate_der": "certificate.der",
            "private_key_der": "private-key.der", "event_key": "event-key.bin",
        });
        write_new(&receiver.join("server.json"), server.to_string().as_bytes())
            .map_err(write_error)?;
    }
    for (index, (directory, listen, pin)) in servers.into_iter().enumerate() {
        let pins: Vec<_> = client_pins.iter().filter(|peer| **peer != pin).collect();
        let server = json!({
            "share": share, "generation": 1, "members": members,
            "bind": listen.to_string(), "client_pins": pins,
            "ca_der": "ca.der", "certificate_der": "certificate.der",
            "private_key_der": "private-key.der", "event_key": "event-key.bin",
        });
        write_new(
            &directory.join("server.json"),
            server.to_string().as_bytes(),
        )
        .map_err(write_error)?;
        {
            let other_peers: Vec<_> = endpoints
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .map(|(_, endpoint)| endpoint)
                .chain(reader_endpoints.iter())
                .take(9)
                .cloned()
                .collect();
            let client = direct_config(&other_peers, false);
            write_new(
                &directory.join("direct.json"),
                client.to_string().as_bytes(),
            )
            .map_err(write_error)?;
        }
    }
    Ok(())
}

fn write_event_key(
    directory: &Path,
    role: &str,
    members: &mut serde_json::Map<String, serde_json::Value>,
    write_error: impl Fn(io::Error) -> String,
) -> Result<(), String> {
    let mut seed = [0; 32];
    getrandom::fill(&mut seed).map_err(|error| error.to_string())?;
    let key = PeerSigningKey::from_bytes(&seed);
    members.insert(hex(key.verifying_key().as_bytes()), role.into());
    write_new(&directory.join("event-key.bin"), &seed).map_err(write_error)
}

fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
