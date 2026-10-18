use playtime_backend_guard::{
    capability, random_id,
    transport::{read_json, write_json},
};
use playtime_backend_host::{ssh, wire::*};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};

#[tokio::test]
async fn bridge_preserves_split_frames_and_has_no_endpoint_or_command_forwarding() {
    let root = std::env::temp_dir().join(format!("pt-bridge-{}", random_id().unwrap().as_str()));
    playtime_local::PrivateDirectory::open(&root, true).unwrap();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    let (input, mut input_peer) = tokio::io::duplex(64);
    let (output, mut output_peer) = tokio::io::duplex(64);
    let copy = root.clone();
    let task = tokio::spawn(async move { ssh::bridge(input, output, &copy).await });
    let nonce = random_id().unwrap();
    let request = ControlRequest {
        version: VERSION,
        nonce: nonce.clone(),
        credential: capability().unwrap(),
        command: Command::Query {},
    };
    let encoded = serde_json::to_vec(&request).unwrap();
    let writer = tokio::spawn(async move {
        input_peer.write_u8(ssh::CONTROL).await.unwrap();
        let bytes = [(encoded.len() as u32).to_be_bytes().as_slice(), &encoded].concat();
        for byte in bytes {
            input_peer.write_u8(byte).await.unwrap();
            tokio::task::yield_now().await;
        }
        input_peer
    });
    let (mut socket, _) = listener.accept().await.unwrap();
    let received: ControlRequest = read_json(&mut socket).await.unwrap();
    assert_eq!(received.nonce, nonce);
    let input_peer = writer.await.unwrap();
    write_json(
        &mut socket,
        &ControlReply {
            version: VERSION,
            nonce: nonce.clone(),
            outcome: Outcome::Rejected {
                failure: Failure::Busy,
            },
        },
    )
    .await
    .unwrap();
    let reply: ControlReply = read_json(&mut output_peer).await.unwrap();
    assert_eq!(reply.nonce, nonce);
    drop(input_peer);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(
        socket.read(&mut byte).await.unwrap(),
        0,
        "SSH input loss closes owner socket"
    );
    let (input, mut peer) = tokio::io::duplex(16);
    peer.write_u8(255).await.unwrap();
    assert!(ssh::bridge(input, tokio::io::sink(), &root).await.is_err());
    drop(listener);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_ssh_uses_only_pinned_fixed_argv_without_remote_shell_commands() {
    let root = std::env::temp_dir().join(format!("pt-ssh-{}", random_id().unwrap().as_str()));
    playtime_local::PrivateDirectory::open(&root, true).unwrap();
    let key = root.join("identity; touch NEVER");
    let hosts = root.join("known_hosts");
    playtime_local::replace_private(&key, b"synthetic-key-not-a-live-credential").unwrap();
    playtime_local::replace_private(
        &hosts,
        b"oldman-bird.local ssh-ed25519 AAAAFixturePublicKeyOnly\n",
    )
    .unwrap();
    let client = ssh::Client::new(key.clone(), hosts, capability().unwrap()).unwrap();
    let argv = client.argv();
    assert_eq!(argv.last().unwrap(), "playtime-bridge@oldman-bird.local");
    for option in [
        "-oStrictHostKeyChecking=yes",
        "-oBatchMode=yes",
        "-oClearAllForwardings=yes",
        "-oPermitLocalCommand=no",
        "-oControlPath=none",
        "-oIdentityAgent=none",
    ] {
        assert!(argv.iter().any(|a| a == option));
    }
    assert!(argv.contains(&key.into_os_string()));
    assert!(!root.join("NEVER").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn malformed_private_lengths_and_unknown_wire_versions_fail_explicitly() {
    let (mut a, mut b) = tokio::io::duplex(8);
    a.write_u32(CONTROL_BYTES as u32 + 1).await.unwrap();
    assert_eq!(
        read_json::<_, ControlReply>(&mut b).await.unwrap_err(),
        Failure::Limit
    );
    let nonce = random_id().unwrap();
    assert_eq!(
        ssh::checked(
            &nonce,
            ControlReply {
                version: 2,
                nonce: nonce.clone(),
                outcome: Outcome::Rejected {
                    failure: Failure::Busy
                }
            }
        ),
        Err(Failure::Unauthorized)
    );
    assert_eq!(
        ssh::checked(
            &nonce,
            ControlReply {
                version: VERSION,
                nonce: random_id().unwrap(),
                outcome: Outcome::Rejected {
                    failure: Failure::Busy
                }
            }
        ),
        Err(Failure::Unauthorized)
    );
}
