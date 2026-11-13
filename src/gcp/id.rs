//! TPU node identity: project, zone and name, and its gcloud resource arguments.
use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct TpuId {
    pub project: String,
    pub zone: String,
    pub name: String,
}

pub(crate) fn resource_parts<'a>(
    resource: &'a str,
    kind: &str,
) -> Option<(&'a str, &'a str, &'a str)> {
    let parts = resource.split('/').collect::<Vec<_>>();
    if parts.len() != 6
        || parts[0] != "projects"
        || parts[2] != "locations"
        || parts[4] != kind
        || ![parts[1], parts[3], parts[5]].into_iter().all(component)
    {
        return None;
    }
    Some((parts[1], parts[3], parts[5]))
}
pub fn component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl TpuId {
    pub fn parse(id: &str) -> Result<Self> {
        let parts: Vec<_> = id.split(':').collect();
        if parts.len() != 4 || parts[0] != "gcp" || !parts[1..].iter().all(|s| component(s)) {
            return Err(Error::msg("Use a fully qualified TPU ID").next("gpu ls --provider gcp"));
        }
        Ok(Self {
            project: parts[1].into(),
            zone: parts[2].into(),
            name: parts[3].into(),
        })
    }
    pub(crate) fn from_resource(resource: &str) -> Option<Self> {
        let (project, zone, name) = resource_parts(resource, "nodes")?;
        Some(Self {
            project: project.into(),
            zone: zone.into(),
            name: name.into(),
        })
    }
    pub fn cli(&self) -> String {
        format!("gcp:{}:{}:{}", self.project, self.zone, self.name)
    }
    /// gpu up names include a timestamp; the random suffix is enough for a
    /// compact candidate. Inventory must still prove that candidate is unique.
    pub(crate) fn short_name(&self) -> String {
        if let Some((epoch, suffix)) = self
            .name
            .strip_prefix("gpu-")
            .and_then(|s| s.split_once('-'))
        {
            if !epoch.is_empty() && epoch.bytes().all(|b| b.is_ascii_digit()) && component(suffix) {
                return format!("gpu-{suffix}");
            }
        }
        self.name.clone()
    }
    pub(crate) fn matches_short(&self, value: &str) -> bool {
        value == self.name || value == self.short_name()
    }
    pub fn resource(&self) -> String {
        format!(
            "projects/{}/locations/{}/nodes/{}",
            self.project, self.zone, self.name
        )
    }
    pub fn url(&self) -> String {
        format!("https://tpu.googleapis.com/v2/{}", self.resource())
    }
    pub fn args(&self, action: &str) -> Vec<String> {
        vec![
            "gcloud".into(),
            "compute".into(),
            "tpus".into(),
            "tpu-vm".into(),
            action.into(),
            self.name.clone(),
            format!("--project={}", self.project),
            format!("--zone={}", self.zone),
            "--quiet".into(),
        ]
    }
}
