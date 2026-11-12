//! Canonical SSH argv options. Explicit runtime deadlines remain caller-owned.
use crate::process::Cmd;
pub(crate) const CONNECT_TIMEOUT: u32 = 10;
fn timeout_option() -> String {
    format!("ConnectTimeout={CONNECT_TIMEOUT}")
}
pub(crate) fn connect_options() -> Vec<String> {
    vec!["-o".into(), timeout_option()]
}
pub(crate) fn config_timeout() -> String {
    format!("  ConnectTimeout {CONNECT_TIMEOUT}\n")
}
pub(crate) fn options(keep_alive: bool) -> Vec<String> {
    let mut options = vec!["BatchMode=yes".to_string(), timeout_option()];
    if keep_alive {
        options.extend(["ServerAliveInterval=15", "ServerAliveCountMax=4"].map(String::from));
    }
    options
        .into_iter()
        .flat_map(|option| ["-o".into(), option])
        .collect()
}
pub(crate) fn command(prefix: &[&str], keep_alive: bool, alias: &str, remote: &str) -> Cmd {
    let mut cmd = Cmd::new(prefix);
    cmd.argv.extend(options(keep_alive));
    cmd.argv.extend([alias.into(), remote.into()]);
    cmd
}
pub(crate) fn gcloud_options(batch: bool) -> Vec<String> {
    (if batch {
        options(false)
    } else {
        connect_options()
    })
    .as_chunks::<2>()
    .0
    .iter()
    .map(|pair| format!("--ssh-flag=-o{}", pair[1]))
    .collect()
}
