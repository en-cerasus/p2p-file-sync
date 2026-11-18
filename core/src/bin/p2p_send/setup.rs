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
) -> Result<(), String> {
    if !(1..=32).contains(&receivers) {
        return Err("Generate between 1 and 32 receiver configurations.".into());
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
    let mut seed = [0; 32];
    getrandom::fill(&mut seed).map_err(|error| error.to_string())?;
    let signer = PeerSigningKey::from_bytes(&seed);
    let member = hex(signer.verifying_key().as_bytes());
    let mut members = serde_json::Map::new();
    members.insert(member, json!("writer"));
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
    let issue = |names, usage| {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(names)?;
        params.not_before = now - Duration::minutes(5);
        params.not_after = now + Duration::days(30);
        params.extended_key_usages = vec![usage];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let certificate = params.signed_by(&key, &ca)?;
        Ok::<_, rcgen::Error>((certificate, key))
    };
    let host = endpoint.ip().to_string();
    let (server_cert, server_key) = issue(vec![host.clone()], ExtendedKeyUsagePurpose::ServerAuth)
        .map_err(certificate_error)?;
    let sender = output.join("sender");
    fs::create_dir(output).map_err(write_error)?;
    fs::create_dir(&sender).map_err(write_error)?;

    write_new(&sender.join("ca.der"), ca.der().as_ref()).map_err(write_error)?;
    write_new(&sender.join("certificate.der"), server_cert.der().as_ref()).map_err(write_error)?;
    write_new(&sender.join("private-key.der"), &server_key.serialize_der()).map_err(write_error)?;
    write_new(&sender.join("event-key.bin"), &seed).map_err(write_error)?;
    let server_pin = hex(&sha256(server_cert.der().as_ref()));
    let mut client_pins = Vec::with_capacity(receivers);

    let mut clients = Vec::with_capacity(receivers);
    for index in 1..=receivers {
        let (client_cert, client_key) =
            issue(Vec::new(), ExtendedKeyUsagePurpose::ClientAuth).map_err(certificate_error)?;
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
        client_pins.push(hex(&sha256(client_cert.der().as_ref())));
        let mut reader_seed = [0; 32];
        getrandom::fill(&mut reader_seed).map_err(|error| error.to_string())?;
        let reader = PeerSigningKey::from_bytes(&reader_seed);
        members.insert(hex(reader.verifying_key().as_bytes()), json!("reader"));
        write_new(&receiver.join("event-key.bin"), &reader_seed).map_err(write_error)?;
        clients.push(receiver);
    }
    for receiver in clients {
        let client = json!({
            "share": share, "generation": 1, "members": members,
            "address": endpoint.to_string(), "server_name": host,
            "server_pin": server_pin,
            "ca_der": "ca.der", "certificate_der": "certificate.der", "private_key_der": "private-key.der",
            "event_key": "event-key.bin",
        });
        write_new(&receiver.join("direct.json"), client.to_string().as_bytes())
            .map_err(write_error)?;
    }

    let server = json!({
        "share": share, "generation": 1, "members": members,
        "bind": bind.to_string(), "client_pins": client_pins,
        "ca_der": "ca.der", "certificate_der": "certificate.der",
        "private_key_der": "private-key.der", "event_key": "event-key.bin",
    });
    write_new(&sender.join("server.json"), server.to_string().as_bytes()).map_err(write_error)?;
    Ok(())
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
