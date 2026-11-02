//! Built-in defaults and `config.json`.

use std::path::PathBuf;

use crate::ctx::Env;
use crate::error::{os_error, Error, Result};
use crate::pyjson::{self, Dict, Value};

pub const CONTAINER_ENV: &str = r#"-p 1111:1111 -p 6006:6006 -p 8080:8080 -p 8384:8384 -e OPEN_BUTTON_PORT=1111 -e OPEN_BUTTON_TOKEN=1 -e JUPYTER_DIR=/ -e DATA_DIRECTORY=/workspace/ -e PORTAL_CONFIG="localhost:1111:11111:/:Instance Portal|localhost:8080:18080:/:Jupyter|localhost:8080:8080:/terminals/1:Jupyter Terminal|localhost:8384:18384:/:Syncthing|localhost:6006:16006:/:Tensorboard""#;

/// The defaults every config starts from, in Python's key order.
pub fn defaults() -> Dict {
    let mut cfg = Dict::new();
    cfg.insert("owner", Value::str("minoya1220"));
    cfg.insert("git_name", Value::str("minoya1220"));
    cfg.insert("git_email", Value::str("minoya1220@gmail.com"));
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

/// `cfg[key]`, or a missing-field error.
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
