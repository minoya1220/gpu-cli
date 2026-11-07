use super::*;

#[test]
fn setup_json_reports_status_plan_applied_and_removed_results() {
    let (_env, ctx, _runner, _state) = fixture();
    assert_eq!(main(&ctx, &["setup", "--json"]), 0);
    let object = dict(&out(&ctx));
    let result = object.get("result").unwrap().as_dict().unwrap();
    assert_eq!(result.get("action"), Some(&Value::str("status")));
    assert_eq!(result.get("gcp_ready"), Some(&Value::Bool(false)));
    let mut flags = APPLY[..5].to_vec();
    flags.extend(["--dry-run", "--json"]);
    assert_eq!(main(&ctx, &flags), 0, "{}", err(&ctx));
    let printed = out(&ctx);
    assert_eq!(printed.lines().count(), 1);
    let object = dict(&printed);
    let result = object.get("result").unwrap().as_dict().unwrap();
    assert_eq!(result.get("dry_run"), Some(&Value::Bool(true)));
    assert_eq!(result.get("project"), Some(&Value::str(PROJECT)));
    assert_eq!(result.get("queue"), Some(&Value::str(RESOURCE)));
    let mut flags = APPLY.to_vec();
    flags.push("--json");
    assert_eq!(main(&ctx, &flags), 0, "{}", err(&ctx));
    let object = dict(&out(&ctx));
    assert_eq!(
        object
            .get("result")
            .unwrap()
            .as_dict()
            .unwrap()
            .get("ready"),
        Some(&Value::Bool(true))
    );
    let mut flags = REMOVE.to_vec();
    flags.push("--json");
    assert_eq!(main(&ctx, &flags), 0, "{}", err(&ctx));
    let object = dict(&out(&ctx));
    let result = object.get("result").unwrap().as_dict().unwrap();
    assert_eq!(result.get("ready"), Some(&Value::Bool(false)));
    assert_eq!(result.get("action"), Some(&Value::str("remove")));
}
