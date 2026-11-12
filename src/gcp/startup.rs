//! The TPU VM startup script: workspace, git identity and repository clone.
use crate::error::Result;
use crate::pyjson::{Dict, Value};

pub(super) fn startup(repo: &crate::repos::Repo, cfg: &Dict) -> Result<String> {
    let owner = crate::pyfmt::py_str(crate::config::get(cfg, "owner")?);
    let (url, name) = crate::repos::resolve_repo(repo.as_arg(), &owner)?;
    let mut script = format!(
        "#!/bin/bash\n{}\nchmod 1777 /workspace\n",
        crate::vast::startup::SETUP_START
    );
    for (key, field) in [("git_name", "name"), ("git_email", "email")] {
        if let Some(value) = cfg
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            script.push_str(&format!(
                "git config --global user.{field} {}\n",
                crate::pyfmt::shell_quote(value)
            ));
        }
    }
    if !url.is_empty() {
        let dir = format!("/workspace/{name}");
        script.push_str(&format!(
            "if [ ! -e {} ]; then\n  git clone -- {} {}\n  chmod -R a+rwX {}\nfi\n",
            crate::pyfmt::shell_quote(&dir),
            crate::pyfmt::shell_quote(&url),
            crate::pyfmt::shell_quote(&dir),
            crate::pyfmt::shell_quote(&dir)
        ));
    }
    script.push_str(crate::vast::startup::SETUP_DONE);
    script.push('\n');
    Ok(script)
}
