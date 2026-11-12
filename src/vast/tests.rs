use super::*;
use crate::testing::{self, FakeRunner, TestEnv};

#[test]
fn unreadable_instances_reply_has_retry_hint_without_raw_output() {
    for output in ["", "Please log in: secret-token", "{broken secret-token"] {
        let env = TestEnv::new();
        let runner = FakeRunner::new(move |cmd| {
            assert_eq!(cmd.argv, ["vastai", "show", "instances", "--raw"]);
            testing::ok(output)
        });
        let ctx = env.ctx().runner(&runner).build();
        let expected =
            "Vast returned no readable data; the provider may be temporarily unavailable. Retry: gpu ls";
        assert_eq!(list_instances(&ctx, None).unwrap_err().text(), expected);
        for json in [false, true] {
            assert_eq!(
                crate::provider::ls(
                    &ctx,
                    &Dict::new(),
                    json,
                    false,
                    Some(crate::provider::Provider::Vast),
                    false
                )
                .unwrap_err()
                .text(),
                expected
            );
            assert!(testing::out(&ctx).is_empty());
        }
        assert_eq!(
            Vast::new(&ctx)
                .json(&["show", "instances"])
                .unwrap_err()
                .text(),
            expected
        );
    }
}
