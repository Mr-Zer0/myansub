// Guards against docs/config.example drifting from the Config struct.
// Loading the example through the normal file layer must succeed, which means
// every key it documents is known (deny_unknown_fields) and in range.

use myansub::config::ConfigSources;
use std::path::PathBuf;

fn config_example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("config.example")
}

#[test]
fn config_example_loads_cleanly() {
    let path = config_example_path();
    assert!(
        path.exists(),
        "docs/config.example should exist at {path:?}"
    );

    let sources = ConfigSources {
        project_file: Some(path),
        ..Default::default()
    };
    let config = myansub::config::load(&sources)
        .expect("docs/config.example must load through config::load");

    // The example documents the defaults, so a few sentinel values should match.
    assert_eq!(config.translation.batch_size, 80);
    assert_eq!(config.llm.service_tier, "flex");
    assert_eq!(config.cache.provider, "auto");
}
