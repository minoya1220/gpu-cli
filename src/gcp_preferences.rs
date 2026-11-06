//! Launch preferences are local, project-scoped history, not setup resources.
use crate::{
    ctx::Ctx,
    gcp::{self, Settings},
};
fn key(project: &str) -> String {
    format!("gcp_zone:{project}")
}
pub(crate) fn last(ctx: &Ctx, project: &str) -> Option<String> {
    crate::history::recent_picks(&ctx.env, &key(project))
        .into_iter()
        .find(|z| gcp::component(z))
}
pub(crate) fn preview(ctx: &Ctx, settings: &Settings) -> Option<String> {
    last(ctx, &settings.project)
        .or_else(|| settings.zone.clone())
        .or_else(|| {
            crate::gcp_catalog::known(ctx, &settings.project, None)
                .first()
                .map(|z| z.name.clone())
        })
}
pub(crate) fn remember(ctx: &Ctx, project: &str, zone: &str) {
    if gcp::component(project) && gcp::component(zone) {
        let _ = crate::history::record_picks(&ctx.env, &[(&key(project), zone)]);
    }
}
