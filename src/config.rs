//! Built-in defaults and `config.json`.

use std::path::PathBuf;

use crate::ctx::Env;
use crate::error::{os_error, Error, Result};
use crate::pyjson::{self, Dict, Value};

pub const CONTAINER_ENV: &str = concat!(
    "-p 1111:1111 -p 6006:6006 -p 8080:8080 -p 8384:8384 ",
    "-e OPEN_BUTTON_PORT=1111 -e OPEN_BUTTON_TOKEN=1 -e JUPYTER_DIR=/ -e DATA_DIRECTORY=/workspace/ ",
    r#"-e PORTAL_CONFIG="localhost:1111:11111:/:Instance Portal|localhost:8080:18080:/:Jupyter|"#,
    r#"localhost:8080:8080:/terminals/1:Jupyter Terminal|localhost:8384:18384:/:Syncthing|"#,
    r#"localhost:6006:16006:/:Tensorboard""#,
);

pub fn defaults() -> Dict {
    let mut cfg = Dict::new();
    cfg.insert("owner", Value::str(""));
    cfg.insert("git_name", Value::str(""));
    cfg.insert("git_email", Value::str(""));
    cfg.insert("disk", Value::Int(20));
    cfg.insert("vm_disk", Value::Int(130));
    cfg.insert("reliability", Value::Float(0.95));
    cfg.insert(
        "container_image",
        Value::str("vastai/base-image:@vastai-automatic-tag"),
    );
    cfg.insert("vm_image", Value::str("docker.io/vastai/kvm"));
    cfg
}

pub fn config_path(env: &Env) -> PathBuf {
    env.xdg("XDG_CONFIG_HOME", ".config")
        .join("gpu")
        .join("config.json")
}

/// Defaults with `config.json` merged over them (a shallow merge).
pub fn load_config(env: &Env) -> Result<Dict> {
    let path = config_path(env);
    let mut cfg = defaults();
    if !path.exists() {
        return Ok(cfg);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| os_error(&e, &path))?;
    match pyjson::loads(&text)? {
        Value::Dict(data) => {
            for (key, value) in data.iter() {
                cfg.insert(key.clone(), value.clone());
            }
            Ok(cfg)
        }
        _ => Err(Error::msg(format!(
            "Config {} must contain a JSON object",
            path.display()
        ))),
    }
}

/// Infer only missing identity values; explicit config always wins. No account
/// name or email is shipped with the binary. Missing tools/auth are harmless.
pub(crate) fn infer_identity(ctx: &crate::ctx::Ctx, cfg: &mut Dict) {
    for (key, args) in [
        ("owner", vec!["gh", "api", "user", "--jq", ".login"]),
        ("git_name", vec!["git", "config", "--get", "user.name"]),
        ("git_email", vec!["git", "config", "--get", "user.email"]),
    ] {
        if !cfg
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(str::is_empty)
            || crate::process::which(&ctx.env, args[0]).is_none()
        {
            continue;
        }
        if let Ok(value) = crate::process::run(ctx, &args, Some(10.0)) {
            if !value.is_empty() && !value.chars().any(char::is_control) {
                cfg.insert(key, Value::str(&value));
            }
        }
    }
}

pub fn get<'a>(cfg: &'a Dict, key: &str) -> Result<&'a Value> {
    cfg.get(key).ok_or_else(|| Error::key(key))
}

/// `cfg.get("editor", {}).get("nvim_config")`, unchecked until Neovim uses it.
pub fn nvim_config(cfg: &Dict) -> Result<Option<Value>> {
    match cfg.get("editor") {
        None => Ok(None),
        Some(Value::Dict(editor)) => Ok(editor.get("nvim_config").cloned()),
        Some(_) => Err(Error::msg(
            "Config 'editor' must be a JSON object, such as {\"nvim_config\": \"~/.config/nvim\"}.",
        )),
    }
}

/// The Neovim config source as `launch_editor` reads it: falsy means none.
pub fn nvim_config_text(value: Option<&Value>) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s.clone())),
        Some(other) if !crate::pyfmt::truthy(other) => Ok(None),
        Some(_) => Err(Error::msg(
            "Config 'editor.nvim_config' must be a git URL or directory path string.",
        )),
    }
}

/// Locked read/modify/replace, preserving keys outside the GCP setup journal.
/// Call before cloud mutations so unwritable state fails before resource changes.
pub(crate) fn save_gcp(env: &Env, gcp: Option<Dict>) -> Result<()> {
    let path = config_path(env);
    crate::atomic_file::update(&path, |bytes| {
        let original = if bytes.is_empty() && !path.exists() {
            Dict::new()
        } else {
            let text = std::str::from_utf8(bytes).map_err(|_| {
                os_error(
                    &std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "stream did not contain valid UTF-8",
                    ),
                    &path,
                )
            })?;
            pyjson::loads(text)?
                .as_dict()
                .cloned()
                .ok_or_else(|| Error::msg("Config must be an object"))?
        };
        let mut config: Dict = original
            .iter()
            .filter(|(key, _)| key.as_str() != "gcp")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if let Some(gcp) = gcp {
            config.insert("gcp", Value::Dict(gcp));
        }
        Ok((pyjson::dumps(&Value::Dict(config), Some(2)) + "\n").into_bytes())
    })
}
