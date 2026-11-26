use crate::instances::{display_ids, render_table};
use crate::testing::*;

#[test]
fn one_table_aligns_both_providers_and_formats_prices_status_and_ids() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    let items = dicts(
        r#"[
        {"id":30306428,"provider":"vast","gpu_name":"RTX 4090","actual_status":"created","dph_total":0.10188888888888889,"duration":300},
        {"id":"gcp:test-project:us-west1-c:gpu-1794014955-uwhcp7","provider":"gcp","gpu_name":"v5litepod-1","actual_status":"DELETING","dph_total":0.359,"duration":420}
    ]"#,
    );
    render_table(&ctx, &items, false).unwrap();
    assert_eq!(
        out(&ctx),
        concat!(
            "ID          Provider  Type      Status    Price      Uptime  Owner\n",
            "30306428    vast      RTX 4090  created   $0.102/hr  5m\n",
            "gpu-uwhcp7  gcp       v5e-1     deleting  $0.359/hr  7m\n",
        )
    );
    render_table(&ctx, &items, true).unwrap();
    assert!(out(&ctx).contains("gcp:test-project:us-west1-c:gpu-1794014955-uwhcp7"));
}

#[test]
fn colliding_short_names_fall_back_to_unique_node_names_or_full_ids() {
    for (input, expected) in [
        (
            r#"[{"id":"gcp:p1:z1:gpu-100-same","provider":"gcp"},{"id":"gcp:p1:z2:gpu-200-same","provider":"gcp"}]"#,
            vec!["gpu-100-same", "gpu-200-same"],
        ),
        (
            r#"[{"id":"gcp:p1:z1:gpu-100-same","provider":"gcp"},{"id":"gcp:p1:z2:gpu-100-same","provider":"gcp"}]"#,
            vec!["gcp:p1:z1:gpu-100-same", "gcp:p1:z2:gpu-100-same"],
        ),
        (
            r#"[{"id":"gcp:p1:z1:gpu-100-same","provider":"gcp"},{"id":"gcp:p1:z2:gpu-same","provider":"gcp"}]"#,
            vec!["gpu-100-same", "gcp:p1:z2:gpu-same"],
        ),
        (
            r#"[{"id":"gcp:p1:z1:custom-name","provider":"gcp"},{"id":123}]"#,
            vec!["custom-name", "123"],
        ),
        (
            r#"[{"id":"gcp:p1:z1:123","provider":"gcp"},{"id":123}]"#,
            vec!["gcp:p1:z1:123", "123"],
        ),
    ] {
        assert_eq!(display_ids(&dicts(input), false).unwrap(), expected);
    }
}

#[test]
fn malformed_prices_and_terminal_controls_do_not_break_the_table() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    for price in ["NaN", "Infinity", "-1", "true", "null", "{}"] {
        let item = dict(&format!(
            r#"{{"id":1,"gpu_name":"bad\nname token=do-not-print","actual_status":"READY\u001b","dph_total":{price}}}"#
        ));
        render_table(&ctx, &[item], false).unwrap();
        let output = out(&ctx);
        assert_eq!(output.lines().count(), 2, "{output:?}");
        assert!(output.contains("bad name"));
        assert!(output.contains("ready"));
        assert!(!output.contains('\x1b'));
        assert!(!output.contains("do-not-print"));
        assert!(!output.contains("/hr"));
    }
}
