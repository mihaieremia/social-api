use std::ffi::OsString;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("crate directory should have a workspace root")
        .to_path_buf()
}

fn compose_project_name() -> String {
    std::env::var("SOCIAL_API_TEST_COMPOSE_PROJECT")
        .or_else(|_| std::env::var("COMPOSE_PROJECT_NAME"))
        .unwrap_or_else(|_| "social-api".to_string())
}

pub fn compose_args(extra: &[&str]) -> Vec<OsString> {
    let root = repo_root();
    let mut args = vec![
        OsString::from("compose"),
        OsString::from("-p"),
        OsString::from(compose_project_name()),
        OsString::from("-f"),
        root.join("docker-compose.yml").into_os_string(),
        OsString::from("-f"),
        root.join("docker-compose.test.yml").into_os_string(),
    ];
    args.extend(extra.iter().map(OsString::from));
    args
}
