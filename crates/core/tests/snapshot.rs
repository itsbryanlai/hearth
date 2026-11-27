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
    let before = snapshot::capture(&guard, &data, &project, std::slice::from_ref(&path)).unwrap();
    fs::write(project.join(&path), b"after\n").unwrap();
    let after = snapshot::capture(&guard, &data, &project, std::slice::from_ref(&path)).unwrap();
    let diff = snapshot::diff(&guard, &data, &before, &after).unwrap();
    assert!(diff.windows(b"-before\n".len()).any(|s| s == b"-before\n"));
    assert!(diff.windows(b"+after\n".len()).any(|s| s == b"+after\n"));
    assert_eq!(fs::read(project.join(".git/index")).unwrap(), index);
    assert_eq!(git_output(&["count-objects", "-v"]), objects);
    assert_eq!(git_output(&["for-each-ref"]), refs);
    symlink(project.join(&path), project.join("link")).unwrap();
    assert!(snapshot::capture(&guard, &data, &project, &["link".into()]).is_err());
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
