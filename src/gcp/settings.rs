//! The `gcp` section of the gpu config.
use super::id::component;
use crate::error::{Error, Result};
use crate::pyjson::{Dict, Value};

#[derive(Clone, Debug)]
pub struct Settings {
    pub project: String,
    pub zone: Option<String>,
    pub service_account: Option<String>,
    pub expiry: Option<Dict>,
}

fn optional(d: &Dict, key: &str) -> Result<Option<String>> {
    match d.get(key) {
        None => Ok(None),
        Some(Value::Str(s)) if !s.is_empty() && !s.chars().any(char::is_control) => {
            Ok(Some(s.clone()))
        }
        _ => Err(Error::msg(format!(
            "Set gcp.{key} to a nonempty string in gpu/config.json"
        ))
        .next("gpu up --help")),
    }
}
impl Settings {
    pub fn load(cfg: &Dict) -> Result<Self> {
        let d = cfg
            .get("gcp")
            .and_then(Value::as_dict)
            .ok_or_else(|| crate::setup::gcp_not_setup(None))?;
        let project = optional(d, "project")?
            .filter(|s| component(s))
            .ok_or_else(|| Error::msg("Set gcp.project in gpu/config.json").next("gpu setup"))?;
        let zone = optional(d, "zone")?;
        if zone.as_ref().is_some_and(|s| !component(s)) {
            return Err(Error::msg("Invalid gcp.zone").next("gpu up --help"));
        }
        let expiry = match d.get("expiry") {
            None => None,
            Some(Value::Dict(d)) => Some(d.clone()),
            _ => {
                return Err(Error::msg("Set gcp.expiry to an object in gpu/config.json")
                    .next("gpu up --help"))
            }
        };
        Ok(Self {
            project,
            zone,
            service_account: optional(d, "service_account")?,
            expiry,
        })
    }
}
