//! Host mode switches that are read from the environment (`CLAW_CONFIG_DIR`, `CLAW_SESSIONS_DIR`).
//! Its own test binary, so setting these variables can't affect other tests.
use std::fs;

use runtime::{ConfigLoader, Session, SessionStore};

#[test]
fn config_dir_and_sessions_dir_take_over() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    let config = root.path().join("config");
    let sessions = root.path().join("sessions");
    fs::create_dir_all(cwd.join(".claw")).unwrap();
    fs::create_dir_all(&config).unwrap();
    // A config the agent could have written into its workdir must be ignored.
    fs::write(
        cwd.join(".claw.json"),
        r#"{"mcpServers":{"evil":{"command":"/bin/false"}}}"#,
    )
    .unwrap();
    fs::write(
        cwd.join(".claw").join("settings.local.json"),
        r#"{"hooks":{"PreToolUse":["/bin/false"]}}"#,
    )
    .unwrap();
    fs::write(
        config.join("settings.json"),
        r#"{"mcpServers":{"tools":{"command":"/usr/local/bin/bridge"}}}"#,
    )
    .unwrap();

    std::env::set_var("CLAW_CONFIG_DIR", &config);
    std::env::set_var("CLAW_SESSIONS_DIR", &sessions);

    let loader = ConfigLoader::default_for(&cwd);
    assert_eq!(loader.config_home(), config.as_path());
    let entries = loader.discover();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, config.join("settings.json"));
    let runtime_config = loader.load().unwrap();
    assert!(runtime_config.mcp().get("tools").is_some());
    assert!(runtime_config.mcp().get("evil").is_none());

    let store = SessionStore::from_cwd(&cwd).unwrap();
    assert_eq!(store.sessions_dir(), sessions.as_path());
    let handle = store.create_handle("s1");
    assert_eq!(handle.path, sessions.join("s1.jsonl"));

    // A session saved from another workdir resumes by absolute path (no workspace-root check).
    fs::create_dir_all(&sessions).unwrap();
    let session = Session::new()
        .with_workspace_root(root.path().join("elsewhere"))
        .with_persistence_path(handle.path.clone());
    session.save_to_path(&handle.path).unwrap();
    let resumed = store
        .load_session_loose(handle.path.to_str().unwrap())
        .unwrap();
    assert_eq!(resumed.session.session_id, session.session_id);

    std::env::remove_var("CLAW_CONFIG_DIR");
    std::env::remove_var("CLAW_SESSIONS_DIR");
    assert_ne!(
        SessionStore::from_cwd(&cwd).unwrap().sessions_dir(),
        sessions.as_path()
    );
    assert!(ConfigLoader::default_for(&cwd).discover().len() > 1);
}
