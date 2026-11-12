use crate::testing::{dict, gcp_config, write_config, TestEnv};
#[test]
fn cloud_opt_in_is_snapshotted_once_per_command_not_once_per_context() {
    let env = TestEnv::new();
    write_config(&env, &gcp_config(None, None));
    let ctx = env.ctx().build().for_command(false);
    ctx.require_gcloud().unwrap();
    write_config(&env, &dict("{}"));
    ctx.require_gcloud().unwrap();
    let next = ctx.for_command(false);
    assert_eq!(next.require_gcloud().unwrap_err().exit_code(), 12);
    // Setup's explicitly scoped capability remains separate from cached state.
    next.for_gcp_setup().require_gcloud().unwrap();
}
