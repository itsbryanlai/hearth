#![cfg(unix)]
use hearth_core::{fsguard::FsGuard, snapshot};
use std::{fs, os::unix::fs::symlink, path::PathBuf, process::Command};

#[test]
fn fails_if_private_snapshots_change_the_source_index_or_lose_a_diff() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("project");
    let data = base.join("data");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&data).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&project)
            .status()
            .unwrap()
            .success()
    );
    let path = PathBuf::from("odd name\n.txt");
    fs::write(project.join(&path), b"before\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["add", "--"])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let index = fs::read(project.join(".git/index")).unwrap();
    let git_output = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
        out.stdout
    };
    let objects = git_output(&["count-objects", "-v"]);
    let refs = git_output(&["for-each-ref"]);
    let guard = FsGuard::new([project.clone(), data.clone()], temp.path().to_path_buf());
    symlink("old-target", project.join("link")).unwrap();
    let paths = [path.clone(), PathBuf::from("link")];
    let before = snapshot::capture(&guard, &data, &project, &paths).unwrap();
    fs::write(project.join(&path), b"after\n").unwrap();
    fs::remove_file(project.join("link")).unwrap();
    symlink("new-target", project.join("link")).unwrap();
    let after = snapshot::capture(&guard, &data, &project, &paths).unwrap();
    let diff = snapshot::diff(&guard, &data, &before, &after).unwrap();
    assert!(diff.windows(b"-before\n".len()).any(|s| s == b"-before\n"));
    assert!(diff.windows(b"+after\n".len()).any(|s| s == b"+after\n"));
    assert!(
        diff.windows(b"+new-target".len())
            .any(|s| s == b"+new-target")
    );
    assert_eq!(fs::read(project.join(".git/index")).unwrap(), index);
    assert_eq!(git_output(&["count-objects", "-v"]), objects);
    assert_eq!(git_output(&["for-each-ref"]), refs);
    symlink(&project, project.join("parent-link")).unwrap();
    assert!(snapshot::capture(&guard, &data, &project, &["parent-link/file".into()]).is_err());
}

#[test]
fn fails_if_private_repository_symlink_redirects_writes_to_another_allowed_root() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("project");
    let data = base.join("data");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&data).unwrap();
    fs::write(project.join("file"), b"contents").unwrap();
    symlink(&project, data.join("objects")).unwrap();
    let guard = FsGuard::new([project.clone(), data.clone()], temp.path().to_path_buf());
    assert!(snapshot::capture(&guard, &data, &project, &["file".into()]).is_err());
}

#[test]
fn fails_if_noncanonical_path_is_silently_omitted_from_the_tree() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("project");
    let data = base.join("data");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&data).unwrap();
    fs::create_dir(project.join("nested")).unwrap();
    fs::write(project.join("nested/file"), b"contents").unwrap();
    let guard = FsGuard::new([project.clone(), data.clone()], temp.path().to_path_buf());
    assert!(snapshot::capture(&guard, &data, &project, &["nested/./file".into()]).is_err());
    assert!(snapshot::capture(&guard, &data, &project, &["nested/file".into()]).is_ok());
}

#[test]
fn fails_if_aliased_roots_or_alternate_object_stores_are_accepted() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("project");
    let data = base.join("data");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&data).unwrap();
    fs::write(project.join("file"), b"contents").unwrap();
    symlink(&project, base.join("alias")).unwrap();
    let guard = FsGuard::new([project.clone(), data.clone()], base.clone());
    assert!(snapshot::capture(&guard, &data, &base.join("alias"), &["file".into()]).is_err());
    assert!(snapshot::capture(&guard, &data, &project, &["file".into()]).is_ok());
    fs::create_dir_all(data.join("objects/info")).unwrap();
    fs::write(data.join("objects/info/alternates"), b"somewhere").unwrap();
    assert!(snapshot::capture(&guard, &data, &project, &["file".into()]).is_err());
}

#[test]
fn fails_if_git_omits_a_requested_reserved_path() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("project");
    let data = base.join("data");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&data).unwrap();
    fs::create_dir(project.join(".git")).unwrap();
    fs::write(project.join(".git/file"), b"contents").unwrap();
    fs::write(project.join("file"), b"contents").unwrap();
    let guard = FsGuard::new([project.clone(), data.clone()], base);
    assert!(snapshot::capture(&guard, &data, &project, &[".git/file".into()]).is_err());
    assert!(snapshot::capture(&guard, &data, &project, &["file".into()]).is_ok());
}

// Fails if independent captures cannot share the app-owned object store.
#[test]
fn concurrent_captures_preserve_both_trees() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("project");
    let data = base.join("data");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&data).unwrap();
    fs::write(project.join("first"), "first contents\n").unwrap();
    fs::write(project.join("second"), "second contents\n").unwrap();
    let guard = FsGuard::new([project.clone(), data.clone()], base);
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let capture = |name: &'static str| {
            let guard = &guard;
            let data = &data;
            let project = &project;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                snapshot::capture(guard, data, project, &[name.into()]).unwrap()
            })
        };
        let first = capture("first");
        let second = capture("second");
        let diff = snapshot::diff(
            &guard,
            &data,
            &first.join().unwrap(),
            &second.join().unwrap(),
        )
        .unwrap();
        let diff = String::from_utf8(diff).unwrap();
        assert!(diff.contains("-first contents") && diff.contains("+second contents"));
    });
}

// Fails if an object larger than available space is copied or leaves a partial review.
#[test]
fn live_review_refuses_an_object_without_space_and_cleans_up() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("project");
    let data = base.join("data");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&data).unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(&project)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let commit = String::from_utf8(output.stdout).unwrap();
    let guard = FsGuard::new([project.clone(), data.clone()], base);
    let gitdir = project.join(".git");
    drop(
        snapshot::LiveReview::open(&guard, &data, &project, &gitdir, &gitdir, commit.trim())
            .unwrap(),
    );
    let objects = project.join(".git/objects");
    fs::create_dir_all(objects.join("aa")).unwrap();
    let space = rustix::fs::statvfs(&data).unwrap();
    let size = space
        .f_bavail
        .saturating_mul(space.f_frsize)
        .saturating_add(1);
    fs::File::create(objects.join("aa").join("a".repeat(38)))
        .unwrap()
        .set_len(size)
        .unwrap();
    let error =
        snapshot::LiveReview::open(&guard, &data, &project, &gitdir, &gitdir, commit.trim())
            .err()
            .unwrap();
    assert!(error.contains("insufficient space"));
    assert_eq!(fs::read_dir(&data).unwrap().count(), 0);
}

// Fails if recovery removes a live review, follows a symlink, or retains an abandoned review.
#[test]
fn recovery_preserves_leased_reviews_and_durable_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("project");
    let data = base.join("data");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&data).unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(&project)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let commit = String::from_utf8(output.stdout).unwrap();
    let guard = FsGuard::new([project.clone(), data.clone()], base.clone());
    let gitdir = project.join(".git");
    let review =
        snapshot::LiveReview::open(&guard, &data, &project, &gitdir, &gitdir, commit.trim())
            .unwrap();
    let live = fs::read_dir(&data).unwrap().next().unwrap().unwrap().path();
    let abandoned = data.join("live-review-ABC123");
    fs::create_dir(&abandoned).unwrap();
    fs::copy(
        live.join(".hearth-live-review"),
        abandoned.join(".hearth-live-review"),
    )
    .unwrap();
    fs::write(abandoned.join("private"), b"abandoned").unwrap();
    let lookalike = data.join("live-review-GHI789");
    fs::create_dir(&lookalike).unwrap();
    fs::write(lookalike.join("retained"), b"unmarked").unwrap();
    let durable = data.join("snapshots");
    fs::create_dir(&durable).unwrap();
    fs::write(durable.join("retained"), b"durable").unwrap();
    symlink(&durable, data.join("live-review-DEF456")).unwrap();
    snapshot::recover_live_reviews(&guard, &data).unwrap();
    assert!(!abandoned.exists());
    assert_eq!(fs::read(lookalike.join("retained")).unwrap(), b"unmarked");
    assert_eq!(fs::read(durable.join("retained")).unwrap(), b"durable");
    assert!(review.files().is_ok());
    assert!(
        fs::symlink_metadata(data.join("live-review-DEF456"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_dir(&data).unwrap().count(), 4);
}
