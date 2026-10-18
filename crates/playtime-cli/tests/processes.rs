#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_cli_with_real_loopback_http_contract() {
    let count = playtime_test_support::suite::run(
        std::path::Path::new(env!("CARGO_BIN_EXE_playtime")),
        None,
    )
    .await
    .unwrap();
    assert!(count >= 10);
}
