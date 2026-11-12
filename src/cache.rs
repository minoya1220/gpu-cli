//! Shared cache locations and best-effort, durable JSON cache writes.
use crate::{
    ctx::{Ctx, Env},
    pyjson::{self, Dict, Value},
};
use std::path::PathBuf;
pub(crate) fn dir(env: &Env) -> PathBuf {
    env.xdg("XDG_CACHE_HOME", ".cache").join("gpu")
}
pub(crate) fn read_json(ctx: &Ctx, name: &str, ttl: i64) -> Option<Dict> {
    let raw = std::fs::read_to_string(dir(&ctx.env).join(name)).ok()?;
    let value = pyjson::loads(&raw).ok()?;
    let data = value.as_dict()?;
    let Value::Int(at) = data.get("at")? else {
        return None;
    };
    let age = ctx.clock.now().as_second().checked_sub(*at)?;
    (0..ttl).contains(&age).then(|| data.clone())
}
pub(crate) fn write_json(ctx: &Ctx, name: &str, mut data: Dict) {
    data.insert("at", Value::Int(ctx.clock.now().as_second()));
    let bytes = pyjson::dumps(&Value::Dict(data), None).into_bytes();
    let _ = crate::atomic_file::update(&dir(&ctx.env).join(name), |_| Ok(bytes));
}
