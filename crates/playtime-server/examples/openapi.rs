fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&playtime_server::http::openapi()).unwrap()
    );
}
