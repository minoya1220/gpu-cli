//! Account membership, not an accepted destroy request, authorizes lifecycle.
use crate::process::Output;
use crate::pyjson::Value;
use crate::testing::*;

#[test]
fn unknown_vast_ids_fail_consistently_without_mutating_or_connecting() {
    for inventory in ["[]", r#"[{"id":123,"actual_status":"running"}]"#] {
        for args in [
            vec!["down", "99999999", "--yes"],
            vec!["stop", "99999999"],
            vec!["expire", "99999999", "2h"],
            vec!["expire", "99999999", "show"],
            vec!["expire", "99999999", "cancel"],
            vec!["wait", "99999999"],
            vec!["exec", "99999999", "--", "true"],
            vec!["cp", "99999999", "./file", ":/workspace/file"],
        ] {
            let t = TestEnv::new();
            // A cached alias must not bypass the fresh account lookup.
            if args[0] != "down" {
                crate::ssh_config::update_host(
                    &t.env(),
                    &dict(r#"{"id":99999999,"ssh_host":"ssh1.vast.ai","ssh_port":22}"#),
                )
                .unwrap();
            }
            let runner = FakeRunner::new(move |cmd| {
                assert_eq!(strs(&cmd.argv), ["vastai", "show", "instances", "--raw"]);
                ok(inventory)
            });
            let ctx = t.ctx().runner(&runner).build();
            let mut json_args = args.clone();
            json_args.insert(0, "--json");
            assert_eq!(main(&ctx, &json_args), 16, "{args:?}: {}", err(&ctx));
            let printed = out(&ctx);
            assert_eq!(printed.lines().count(), 1);
            let object = dict(&printed);
            assert_eq!(object.get("ok"), Some(&Value::Bool(false)));
            let error = object.get("error").unwrap().as_dict().unwrap();
            assert_eq!(error.get("code"), Some(&Value::str("not_found")));
            assert_eq!(error.get("exit_code"), Some(&Value::Int(16)));
            let result = object.get("result").unwrap().as_dict().unwrap();
            assert_eq!(result.get("id"), Some(&Value::str("99999999")));
            assert!(result.get("state").is_none());
            assert!(err(&ctx).is_empty());
            assert_eq!(main(&ctx, &args), 16, "{args:?}");
            assert!(out(&ctx).is_empty());
            let error = err(&ctx);
            assert!(error.contains("not found in the account") && error.contains("Next: gpu ls"));
            assert_eq!(runner.calls().len(), 2);
            assert_eq!(t.home.join(".ssh/vst_config").exists(), args[0] != "down");
        }
    }
}

#[test]
fn failed_or_unreadable_inventory_is_not_proof_of_absence() {
    for mode in ["auth", "malformed"] {
        let t = TestEnv::new();
        let runner = FakeRunner::new(move |cmd| {
            assert_eq!(strs(&cmd.argv), ["vastai", "show", "instances", "--raw"]);
            if mode == "auth" {
                Ok(Output::failed(1, "invalid api key token=private"))
            } else {
                ok("unreadable inventory")
            }
        });
        let ctx = t.ctx().runner(&runner).build();
        assert_eq!(
            main(&ctx, &["down", "99999999", "--yes", "--json"]),
            if mode == "auth" { 13 } else { 1 }
        );
        let text = out(&ctx);
        assert!(
            !text.contains("not_found")
                && !text.contains("destroyed")
                && !text.contains("token=private")
        );
        assert_eq!(runner.calls().len(), 1);
    }
}

#[test]
fn down_of_a_recorded_but_absent_vast_instance_cleans_up_idempotently() {
    let t = TestEnv::new();
    crate::ssh_config::update_host(
        &t.env(),
        &dict(r#"{"id":99999999,"ssh_host":"ssh1.vast.ai","ssh_port":22}"#),
    )
    .unwrap();
    crate::ssh_config::update_host(
        &t.env(),
        &dict(r#"{"id":123,"ssh_host":"ssh1.vast.ai","ssh_port":22}"#),
    )
    .unwrap();
    let runner = FakeRunner::new(|cmd| {
        assert_eq!(strs(&cmd.argv), ["vastai", "show", "instances", "--raw"]);
        ok(r#"[{"id":123,"actual_status":"running"}]"#)
    });
    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(
        main(&ctx, &["--json", "down", "99999999", "--yes"]),
        0,
        "{}",
        err(&ctx)
    );
    let object = dict(&out(&ctx));
    assert_eq!(object.get("ok"), Some(&Value::Bool(true)));
    let result = object.get("result").unwrap().as_dict().unwrap();
    assert_eq!(result.get("state"), Some(&Value::str("already_absent")));
    assert!(!crate::ssh_config::has_host(&t.env(), "99999999").unwrap());
    assert!(crate::ssh_config::has_host(&t.env(), "123").unwrap());
    assert_eq!(main(&ctx, &["down", "99999999", "--yes"]), 16);
    assert_eq!(runner.calls().len(), 2);
}
