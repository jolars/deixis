use serde_json::{Value as JsonValue, json};

#[test]
fn registry_metadata_matches_the_shipped_stdio_package() {
    let metadata: JsonValue =
        serde_json::from_str(include_str!("../server.json"))
            .expect("server.json should contain valid JSON");

    assert_eq!(metadata["name"], "io.github.jolars/deixis");
    assert_eq!(metadata["version"], env!("CARGO_PKG_VERSION"));
    assert!(metadata.get("remotes").is_none());

    let packages = metadata["packages"]
        .as_array()
        .expect("packages should be an array");
    assert_eq!(packages.len(), 1);

    let package = &packages[0];
    assert_eq!(package["registryType"], "cargo");
    assert_eq!(package["identifier"], "deixis");
    assert_eq!(package["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(package["transport"], json!({ "type": "stdio" }));

    let argument_names: Vec<_> = package["packageArguments"]
        .as_array()
        .expect("packageArguments should be an array")
        .iter()
        .map(|argument| argument["name"].as_str())
        .collect();
    assert_eq!(argument_names, [Some("--root"), Some("--config")]);
}
