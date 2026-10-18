#[tokio::main]
async fn main() {
    // sshd invokes only this root-owned forced command; it is never a shell or
    // argument interpreter. Reject even an attempted remote command.
    let invalid = std::env::args_os().len() != 1
        || std::env::var_os("SSH_ORIGINAL_COMMAND").is_some_and(|s| !s.is_empty());
    if invalid
        || playtime_backend_host::ssh::bridge(
            tokio::io::stdin(),
            tokio::io::stdout(),
            std::path::Path::new("/run/playtime-backend"),
        )
        .await
        .is_err()
    {
        std::process::exit(1);
    }
}
