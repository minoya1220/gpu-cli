use std::sync::Arc;

use gpu::config::defaults;
use gpu::ctx::{Clock, Console, Ctx, Env, Ui};
use gpu::expiry::parse_when;
use gpu::instances::{format_uptime, instance_label};
use gpu::launch::UpArgs;
use gpu::offers::{
    category_label, gpu_category, group_by_category, model_label, offer_label, offer_rows,
    valid_gpu_name, OfferSearch, Stats,
};
use gpu::prompt::{choose, choose_gpu, Choices};
use gpu::pyfmt::{fixed, float_repr, general, parse_float, percent, py_float, py_str, shell_quote};
use gpu::pyjson::{dumps, loads, Dict, Value};
use gpu::redact::{redact, redact_text};
use gpu::repos::{resolve_repo, Repo};
use gpu::startup::startup_script;
use gpu::vast::{request, RequestArgs, Vast};

fn table(name: &str) -> Value {
    let path = format!("{}/tests/golden/{name}.json", env!("CARGO_MANIFEST_DIR"));
    loads(&std::fs::read_to_string(&path).unwrap()).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn rows(name: &str) -> Vec<Dict> {
    match table(name) {
        Value::List(items) => items
            .into_iter()
            .map(|i| i.as_dict().unwrap().clone())
            .collect(),
        other => panic!("{name}: not a list: {other:?}"),
    }
}

fn s<'a>(row: &'a Dict, key: &str) -> &'a str {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{key} missing in {row:?}"))
}

fn opt_s<'a>(row: &'a Dict, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

fn dict(value: &Value) -> Dict {
    value.as_dict().cloned().unwrap_or_default()
}

struct NoClock;

impl Clock for NoClock {
    fn monotonic(&self) -> f64 {
        0.0
    }

    fn sleep(&self, _seconds: f64) {}

    fn now(&self) -> jiff::Timestamp {
        jiff::Timestamp::UNIX_EPOCH
    }
}

/// A context whose console answers prompts with `input`; nothing here may
/// run a process (the runner fails every call).
fn ctx(env: Env, input: Option<Vec<&str>>) -> Ctx {
    struct NoRunner;
    impl gpu::process::Runner for NoRunner {
        fn run(
            &self,
            cmd: &gpu::process::Cmd,
        ) -> Result<gpu::process::Output, gpu::process::RunError> {
            panic!("unexpected command: {:?}", cmd.argv)
        }
        fn exec(&self, argv: &[String]) -> String {
            panic!("unexpected exec: {argv:?}")
        }
    }
    Ctx::new(
        Arc::new(env),
        Arc::new(NoRunner),
        Arc::new(NoClock),
        Arc::new(Console::buffer(input, false)),
        Arc::new(Ui::scripted::<&[u8]>(&[])),
    )
}

#[test]
fn python_float_formatting() {
    for row in rows("floats") {
        let x = f64::from_bits(s(&row, "bits").parse().unwrap());
        assert_eq!(float_repr(x), s(&row, "repr"), "repr");
        assert_eq!(fixed(x, 3), s(&row, "fixed3"), "{x:?} .3f");
        assert_eq!(fixed(x, 1), s(&row, "fixed1"), "{x:?} .1f");
        assert_eq!(percent(x, 1), s(&row, "percent1"), "{x:?} .1%");
        assert_eq!(
            format!("{:>6}", percent(x, 1)),
            s(&row, "percent_width"),
            "{x:?} 6.1%"
        );
        assert_eq!(general(x), s(&row, "general"), "{x:?} g");
    }
}

#[test]
fn python_float_parsing() {
    for row in rows("float_parse") {
        let text = s(&row, "text");
        if !text.is_ascii() {
            continue;
        }
        match row.get("error") {
            Some(error) => {
                assert_eq!(parse_float(text), None, "{text:?}");
                assert_eq!(
                    py_float(&Value::str(text)).unwrap_err(),
                    error.as_str().unwrap()
                );
            }
            None => {
                let parsed = parse_float(text).unwrap_or_else(|| panic!("{text:?} should parse"));
                match row.get("value") {
                    _ if row.get("nan") == Some(&Value::Bool(true)) => {
                        assert!(parsed.is_nan(), "{text:?}")
                    }
                    Some(Value::Str(inf)) => assert_eq!(float_repr(parsed), *inf, "{text:?}"),
                    Some(value) => assert_eq!(parsed, py_float(value).unwrap(), "{text:?}"),
                    None => panic!("no value for {text:?}"),
                }
            }
        }
    }
}

#[test]
fn python_shlex_quote() {
    for row in rows("shlex_quote") {
        assert_eq!(shell_quote(s(&row, "text")), s(&row, "quoted"));
    }
}

#[test]
fn python_json_dumps() {
    for row in rows("json_dumps") {
        let value = loads(s(&row, "doc")).unwrap();
        assert_eq!(
            dumps(&value, None),
            s(&row, "compact"),
            "{}",
            s(&row, "doc")
        );
        assert_eq!(
            dumps(&value, Some(1)),
            s(&row, "indent1"),
            "{}",
            s(&row, "doc")
        );
        assert_eq!(
            dumps(&value, Some(2)),
            s(&row, "indent2"),
            "{}",
            s(&row, "doc")
        );
    }
}

#[test]
fn python_json_errors() {
    for row in rows("json_errors") {
        let doc = s(&row, "doc");
        assert_eq!(loads(doc).err().as_deref(), opt_s(&row, "error"), "{doc:?}");
    }
}

#[test]
fn python_redaction() {
    let golden = table("redact");
    for row in golden.get("strings").unwrap().as_list().unwrap() {
        let row = dict(row);
        assert_eq!(redact_text(s(&row, "text")), s(&row, "redacted"));
    }
    for row in golden.get("values").unwrap().as_list().unwrap() {
        let row = dict(row);
        assert_eq!(
            redact(row.get("value").unwrap()),
            *row.get("redacted").unwrap()
        );
    }
}

#[test]
fn python_resolve_repo() {
    for row in rows("resolve_repo") {
        let repo = s(&row, "repo");
        match resolve_repo(repo, "minoya1220") {
            Ok((url, name)) => {
                let expected = row
                    .get("result")
                    .unwrap_or_else(|| panic!("{repo:?} should be rejected: {row:?}"));
                assert_eq!(
                    Value::List(vec![Value::Str(url), Value::Str(name)]),
                    *expected,
                    "{repo:?}"
                );
            }
            Err(err) => assert_eq!(Some(err.text()), opt_s(&row, "error"), "{repo:?}"),
        }
    }
}

#[test]
fn startup_script_contracts() {
    for row in rows("startup_scripts") {
        let cfg = dict(row.get("cfg").unwrap());
        let script = startup_script(
            &Repo::from_arg(s(&row, "repo")),
            s(&row, "mode"),
            &cfg,
            None,
        )
        .unwrap();
        assert_eq!(script, s(&row, "script"), "{row:?}");
    }
}

#[test]
fn python_create_requests() {
    let repo = Repo::from_arg("gpu-kernels");
    for row in rows("requests") {
        let offer = dict(row.get("offer").unwrap());
        let cfg = dict(row.get("cfg").unwrap());
        let mode = s(&row, "mode");
        let args = RequestArgs {
            repo: &repo,
            mode,
            interruptible: row.get("interruptible") == Some(&Value::Bool(true)),
            disk: match row.get("disk") {
                Some(Value::Int(disk)) => Some(*disk),
                _ => None,
            },
            deadline: None,
        };
        match (request(&offer, &args, &cfg), row.get("request")) {
            (Ok(mut actual), Some(expected)) => {
                let at = actual
                    .iter()
                    .position(|a| a.as_str() == Some("--onstart-cmd"))
                    .unwrap()
                    + 1;
                assert_eq!(
                    actual[at],
                    Value::Str(startup_script(&repo, mode, &cfg, None).unwrap())
                );
                actual[at] = Value::str("<startup_script>");
                assert_eq!(Value::List(actual), *expected, "{row:?}");
            }
            (Err(err), None) => assert_eq!(Some(err.text()), opt_s(&row, "error"), "{row:?}"),
            (actual, _) => panic!("{row:?}: {actual:?}"),
        }
    }
}

#[test]
fn python_offer_queries() {
    let vast = Vast::new(&ctx(Env::default(), None));
    for row in rows("offer_queries") {
        let number = |key: &str| match row.get(key) {
            Some(Value::Float(f)) => Some(*f),
            Some(Value::Int(i)) => Some(*i as f64),
            _ => None,
        };
        let args = UpArgs {
            interruptible: row.get("interruptible") == Some(&Value::Bool(true)),
            max_price: number("max_price"),
            yes: row.get("yes") == Some(&Value::Bool(true)),
            offer: number("offer").map(|o| o as i64),
            unverified: row.get("unverified") == Some(&Value::Bool(true)),
            reliability: number("reliability"),
            num_gpus: 2,
            disk: number("disk").map(|d| d as i64),
            ..UpArgs::default()
        };
        let search = OfferSearch::new(vast.clone(), &args, &defaults()).unwrap();
        assert_eq!(
            search.query(s(&row, "mode"), opt_s(&row, "gpu")),
            s(&row, "query"),
            "{row:?}"
        );
    }
}

#[test]
fn python_ls_json_output() {
    for row in rows("ls") {
        let raw = loads(s(&row, "raw")).unwrap();
        assert_eq!(dumps(&redact(&raw), Some(1)), s(&row, "json"));
    }
}

#[test]
fn python_uptime_and_labels() {
    for row in rows("uptime") {
        assert_eq!(
            format_uptime(row.get("value")),
            s(&row, "uptime"),
            "{row:?}"
        );
    }
    for row in rows("instance_labels") {
        assert_eq!(
            instance_label(&dict(row.get("item").unwrap())).unwrap(),
            s(&row, "label"),
            "{row:?}"
        );
    }
    let golden = table("offer_labels");
    let offers: Vec<Dict> = golden
        .get("offers")
        .unwrap()
        .as_list()
        .unwrap()
        .iter()
        .map(dict)
        .collect();
    let strings = |key: &str| -> Vec<String> {
        golden
            .get(key)
            .unwrap()
            .as_list()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    let labels: Vec<String> = offers.iter().map(|o| offer_label(o).unwrap()).collect();
    assert_eq!(labels, strings("labels"));
    assert_eq!(offer_rows(&offers).unwrap(), strings("rows"));
    for (offer, expected) in offers
        .iter()
        .zip(golden.get("rows_each").unwrap().as_list().unwrap())
    {
        let expected: Vec<String> = expected.as_list().unwrap().iter().map(py_str).collect();
        assert_eq!(offer_rows(std::slice::from_ref(offer)).unwrap(), expected);
    }
}

#[test]
fn python_gpu_models() {
    let golden = table("gpu_models");
    for (model, category) in golden.get("categories").unwrap().as_dict().unwrap().iter() {
        if model == "\u{df}" {
            continue;
        }
        assert_eq!(gpu_category(model), category.as_str().unwrap(), "{model}");
    }
    let models: Vec<String> = include_str!("../assets/gpu_models.txt")
        .lines()
        .map(String::from)
        .collect();
    let groups: Vec<Value> = group_by_category(&models)
        .into_iter()
        .map(|(name, members)| {
            Value::List(vec![
                Value::Str(name),
                Value::List(members.into_iter().map(Value::Str).collect()),
            ])
        })
        .collect();
    assert_eq!(Value::List(groups), *golden.get("groups").unwrap());
    let stats: Stats = [
        ("B200", 1.735, 1),
        ("RTX 4090", 0.3, 12),
        ("H100 SXM", 2.0, 3),
        ("Tesla T4", 0.0625, 1),
        ("A100 PCIE", f64::INFINITY, 2),
    ]
    .into_iter()
    .map(|(m, p, c)| (m.to_string(), (p, c)))
    .collect();
    for pair in golden.get("model_labels").unwrap().as_list().unwrap() {
        let pair = pair.as_list().unwrap();
        assert_eq!(
            model_label(pair[0].as_str().unwrap(), &stats),
            pair[1].as_str().unwrap()
        );
    }
    let groups = group_by_category(&models);
    for (pair, (name, members)) in golden
        .get("category_labels")
        .unwrap()
        .as_list()
        .unwrap()
        .iter()
        .zip(&groups)
    {
        let pair = pair.as_list().unwrap();
        assert_eq!(pair[0].as_str().unwrap(), name);
        assert_eq!(
            category_label(name, members, &stats),
            pair[1].as_str().unwrap()
        );
    }
    for pair in golden.get("valid_names").unwrap().as_list().unwrap() {
        let pair = pair.as_list().unwrap();
        let name = pair[0].as_str().unwrap();
        if !name.is_ascii() && name.chars().any(|c| c.is_numeric() && !c.is_ascii_digit()) {
            continue;
        }
        assert_eq!(
            valid_gpu_name(name),
            pair[1] == Value::Bool(true),
            "{name:?}"
        );
    }
}

fn python_internal(message: &str) -> bool {
    [
        "Invalid datetime",
        "UTC offset must be between",
        "year ",
        "date value out of range",
    ]
    .iter()
    .any(|prefix| message.starts_with(prefix))
}

#[test]
fn python_parse_when() {
    let mut checked = 0;
    let mut rejected = 0;
    let mut failures = Vec::new();
    for row in rows("parse_when") {
        let zone = match s(&row, "zone") {
            "UTC" => jiff::tz::TimeZone::UTC,
            name => jiff::tz::TimeZone::get(name).unwrap(),
        };
        let now: jiff::Timestamp = s(&row, "now").parse().unwrap();
        let when = s(&row, "when");
        let actual = parse_when(when, now, &zone);
        let ok = match (&actual, opt_s(&row, "result"), opt_s(&row, "error")) {
            (result, Some(expected), _) if expected.starts_with("9999-12-31") => {
                rejected += 1;
                result.is_err()
            }
            (Ok(value), Some(expected), _) => {
                *value == expected.parse::<jiff::Timestamp>().unwrap()
            }
            (Err(message), None, Some(expected)) => {
                message == expected || (python_internal(expected) && python_internal(message))
            }
            _ => false,
        };
        if !ok {
            failures.push(format!(
                "{} {} {:?}: python {:?} {:?}, rust {actual:?}",
                s(&row, "zone"),
                s(&row, "now"),
                when,
                opt_s(&row, "result"),
                opt_s(&row, "error")
            ));
        }
        checked += 1;
    }
    assert!(
        failures.is_empty(),
        "{} of {checked} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(checked > 1000);
    assert!(rejected > 0);
}

#[test]
fn python_line_prompts() {
    let golden = table("line_prompts");
    let dir = tempfile::tempdir().unwrap();
    let run = |history: Option<&Value>,
               answers: &Value,
               f: &dyn Fn(&Ctx) -> gpu::error::Result<String>|
     -> (Result<String, String>, String) {
        let state = tempfile::tempdir_in(dir.path()).unwrap();
        let env = Env::default()
            .with("HOME", dir.path().to_string_lossy())
            .with("XDG_STATE_HOME", state.path().to_string_lossy());
        if let Some(history) = history.filter(|h| **h != Value::Null) {
            let path = state.path().join("gpu");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("history.json"), dumps(history, None)).unwrap();
        }
        let answers: Vec<&str> = answers
            .as_list()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let ctx = ctx(env, Some(answers));
        let result = f(&ctx).map_err(|e| e.text().to_string());
        (result, ctx.console.take_out())
    };
    for case in golden.get("choose").unwrap().as_list().unwrap() {
        let case = dict(case);
        let options: Vec<String> = case
            .get("options")
            .unwrap()
            .as_list()
            .unwrap()
            .iter()
            .map(py_str)
            .collect();
        let prompt = s(&case, "prompt").to_string();
        let step = opt_s(&case, "step").map(String::from);
        let (result, stdout) = run(case.get("history"), case.get("answers").unwrap(), &|ctx| {
            choose(
                ctx,
                &Choices::strings(options.clone()),
                &prompt,
                step.as_deref(),
                None,
            )
        });
        assert_eq!(stdout, s(&case, "stdout"), "{case:?}");
        match result {
            Ok(value) => assert_eq!(Some(value.as_str()), opt_s(&case, "result"), "{case:?}"),
            Err(error) => assert_eq!(Some(error.as_str()), opt_s(&case, "error"), "{case:?}"),
        }
    }
    let gpu = golden.get("choose_gpu").unwrap();
    let models: Vec<String> = gpu
        .get("models")
        .unwrap()
        .as_list()
        .unwrap()
        .iter()
        .map(py_str)
        .collect();
    let stats: Stats = gpu
        .get("stats")
        .unwrap()
        .as_dict()
        .unwrap()
        .iter()
        .map(|(k, v)| {
            let pair = v.as_list().unwrap();
            (
                k.clone(),
                (
                    py_float(&pair[0]).unwrap(),
                    py_float(&pair[1]).unwrap() as usize,
                ),
            )
        })
        .collect();
    for case in gpu.get("cases").unwrap().as_list().unwrap() {
        let case = dict(case);
        let (result, stdout) = run(case.get("history"), case.get("answers").unwrap(), &|ctx| {
            choose_gpu(ctx, &models, &stats)
        });
        assert_eq!(stdout, s(&case, "stdout"), "{case:?}");
        match result {
            Ok(value) => assert_eq!(Some(value.as_str()), opt_s(&case, "result"), "{case:?}"),
            Err(error) => assert_eq!(Some(error.as_str()), opt_s(&case, "error"), "{case:?}"),
        }
    }
}
