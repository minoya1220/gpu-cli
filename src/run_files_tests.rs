use crate::process::{Cmd, RealRunner, Runner};
use crate::run_files::{self, prepare};
use crate::testing::{FakeRunner, TestEnv};
use std::fs;
use std::path::Path;

#[test]
fn git_upload_respects_ignores_edits_and_subdirectory_without_recursing_links() {
    let env = TestEnv::new();
    let root = env.root.join("project");
    fs::create_dir_all(root.join("sub")).unwrap();
    let runner = FakeRunner::new(|cmd| {
        assert_eq!(cmd.program(), "git");
        RealRunner.run(cmd) // local git only; no account/cloud path
    });
    let ctx = env.ctx().runner(&runner).build();
    let git = |args: &[&str]| {
        let mut argv = vec![
            "git".to_string(),
            "-C".into(),
            root.to_string_lossy().into_owned(),
        ];
        argv.extend(args.iter().map(|s| s.to_string()));
        assert_eq!(ctx.run(&Cmd::new(&argv)).unwrap().code, 0);
    };
    git(&["init", "-q"]);
    fs::write(root.join(".gitignore"), "ignored\n.env\n").unwrap();
    fs::write(root.join("tracked"), "old").unwrap();
    git(&["add", ".gitignore", "tracked"]);
    fs::write(root.join("tracked"), "new edit").unwrap();
    fs::write(root.join("sub/untracked file\nwith newline"), "untracked").unwrap();
    fs::write(root.join("ignored"), "not sent").unwrap();
    fs::write(root.join(".env"), "secret").unwrap();
    fs::create_dir(root.join("gpu-runs")).unwrap();
    fs::write(root.join("gpu-runs/old-output"), "not sent").unwrap();
    std::os::unix::fs::symlink("/no/such/target", root.join("link")).unwrap();
    let upload = prepare(&ctx, &root.join("sub"), 1024).unwrap();
    assert_eq!(upload.root, root.canonicalize().unwrap());
    assert_eq!(upload.relative, Path::new("sub"));
    let gzip = flate2::read::GzDecoder::new(fs::File::open(upload.archive.path()).unwrap());
    let mut archive = tar::Archive::new(gzip);
    let output = env.root.join("unpacked");
    archive.unpack(&output).unwrap();
    assert_eq!(fs::read(output.join("tracked")).unwrap(), b"new edit");
    assert!(output.join("sub/untracked file\nwith newline").exists());
    assert_eq!(
        fs::read_link(output.join("link")).unwrap(),
        Path::new("/no/such/target")
    );
    for name in ["ignored", ".env", "gpu-runs", ".git"] {
        assert!(!output.join(name).exists());
    }
    assert!(prepare(&ctx, &root, 1)
        .unwrap_err()
        .text()
        .contains("Largest:"));
    // gitlinks can be created without cloning anything or contacting a remote.
    git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "160000,1111111111111111111111111111111111111111,submodule",
    ]);
    assert!(prepare(&ctx, &root, 1024)
        .unwrap_err()
        .text()
        .contains("submodules"));
}

#[test]
fn outside_git_excludes_builtin_trees_and_warns_about_dotenv() {
    let env = TestEnv::new();
    let ctx = env
        .ctx()
        .runner(&FakeRunner::new(|cmd| {
            assert_eq!(cmd.program(), "git");
            crate::testing::status(128)
        }))
        .build();
    let root = env.root.join("files");
    fs::create_dir_all(root.join(".venv")).unwrap();
    fs::create_dir_all(root.join("gpu-runs")).unwrap();
    fs::write(root.join(".venv/not-sent"), "skip").unwrap();
    fs::write(root.join("gpu-runs/not-sent"), "skip").unwrap();
    fs::write(root.join(".env.local"), "sent with warning").unwrap();
    let upload = prepare(&ctx, &root, 100).unwrap();
    assert_eq!(upload.files, [Path::new(".env.local")]);
    assert!(crate::testing::err(&ctx).contains("WARNING: uploading .env.local"));
}

#[test]
fn results_keep_inner_symlinks_and_skip_escaping_links_without_aborting() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    let archive = env.root.join("results.tar");
    let mut tar = tar::Builder::new(fs::File::create(&archive).unwrap());
    let link = |tar: &mut tar::Builder<fs::File>, kind, name: &str, target: &str| {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o777);
        header.set_size(0);
        header.set_entry_type(kind);
        header.set_link_name(target).unwrap();
        header.set_cksum();
        tar.append_data(&mut header, name, &b""[..]).unwrap();
    };
    let file = |tar: &mut tar::Builder<fs::File>, name: &str, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(data.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, name, data).unwrap();
    };
    file(&mut tar, "outputs/epoch=3.ckpt", b"weights");
    link(
        &mut tar,
        tar::EntryType::Symlink,
        "outputs/last.ckpt",
        "epoch=3.ckpt",
    );
    link(
        &mut tar,
        tar::EntryType::Symlink,
        "outputs/sub/best",
        "../epoch=3.ckpt",
    );
    link(
        &mut tar,
        tar::EntryType::Symlink,
        "outputs/absolute",
        "/tmp",
    );
    link(
        &mut tar,
        tar::EntryType::Symlink,
        "outputs/escape",
        "../../secret",
    );
    link(
        &mut tar,
        tar::EntryType::Symlink,
        "outputs/sibling",
        "../project",
    );
    link(
        &mut tar,
        tar::EntryType::Link,
        "outputs/hard",
        "outputs/epoch=3.ckpt",
    );
    file(&mut tar, "outputs/escape/after.txt", b"after");
    tar.finish().unwrap();
    let dest = env.root.join("dest");
    fs::create_dir(&dest).unwrap();
    run_files::unpack(&ctx, &archive, &dest, Path::new("outputs")).unwrap();
    assert_eq!(
        fs::read(dest.join("outputs/last.ckpt")).unwrap(),
        b"weights"
    );
    assert_eq!(fs::read(dest.join("outputs/sub/best")).unwrap(), b"weights");
    assert_eq!(
        fs::read_link(dest.join("outputs/last.ckpt")).unwrap(),
        Path::new("epoch=3.ckpt")
    );
    for skipped in ["absolute", "sibling", "hard"] {
        assert!(fs::symlink_metadata(dest.join("outputs").join(skipped)).is_err());
    }
    assert!(!fs::symlink_metadata(dest.join("outputs/escape"))
        .unwrap()
        .is_symlink());
    assert_eq!(
        fs::read(dest.join("outputs/escape/after.txt")).unwrap(),
        b"after"
    );
    let warnings = crate::testing::err(&ctx);
    for name in ["absolute", "escape", "sibling", "hard"] {
        assert_eq!(
            warnings
                .lines()
                .filter(|l| l.starts_with(&format!("WARNING: skipping outputs/{name} ")))
                .count(),
            1,
            "{warnings}"
        );
    }
}

#[test]
fn results_with_unsafe_paths_are_still_refused() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    let archive = env.root.join("results.tar");
    let mut tar = tar::Builder::new(fs::File::create(&archive).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o600);
    header.set_size(1);
    header.set_cksum();
    tar.append_data(&mut header, "elsewhere/x", &b"x"[..])
        .unwrap();
    tar.finish().unwrap();
    fs::create_dir(env.root.join("dest")).unwrap();
    assert!(
        run_files::unpack(&ctx, &archive, &env.root.join("dest"), Path::new("outputs")).is_err()
    );
    assert!(!env.root.join("dest/elsewhere").exists());
}
