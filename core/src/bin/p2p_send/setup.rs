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

pub fn create(output: &Path, bind: SocketAddr, endpoint: SocketAddr) -> Result<(), String> {
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
    let (client_cert, client_key) =
        issue(Vec::new(), ExtendedKeyUsagePurpose::ClientAuth).map_err(certificate_error)?;

    let sender = output.join("sender");
    let receiver = output.join("receiver");
    fs::create_dir(output).map_err(write_error)?;
    fs::create_dir(&sender).map_err(write_error)?;
    fs::create_dir(&receiver).map_err(write_error)?;
    for (directory, cert, key) in [
        (&sender, &server_cert, &server_key),
        (&receiver, &client_cert, &client_key),
    ] {
        write_new(&directory.join("ca.der"), ca.der().as_ref()).map_err(write_error)?;
        write_new(&directory.join("certificate.der"), cert.der().as_ref()).map_err(write_error)?;
        write_new(&directory.join("private-key.der"), &key.serialize_der()).map_err(write_error)?;
    }
    write_new(&sender.join("event-key.bin"), &seed).map_err(write_error)?;

    let server = json!({
        "share": share, "generation": 1, "members": { (member.as_str()): "writer" },
        "bind": bind.to_string(), "client_pin": hex(&sha256(client_cert.der().as_ref())),
        "ca_der": "ca.der", "certificate_der": "certificate.der",
        "private_key_der": "private-key.der", "event_key": "event-key.bin",
    });
    let client = json!({
        "share": share, "generation": 1, "members": { (member.as_str()): "writer" },
        "address": endpoint.to_string(), "server_name": host,
        "server_pin": hex(&sha256(server_cert.der().as_ref())),
        "ca_der": "ca.der", "certificate_der": "certificate.der", "private_key_der": "private-key.der",
    });
    write_new(&sender.join("server.json"), server.to_string().as_bytes()).map_err(write_error)?;
    write_new(&receiver.join("direct.json"), client.to_string().as_bytes()).map_err(write_error)?;
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
