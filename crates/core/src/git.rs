//! Local Git worktree operations. Commands never use a shell or a remote.
use crate::fsguard::FsGuard;
use hearth_protocol::ThreadId;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
}

fn git(cwd: &Path, home: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn check_root(path: &Path, home: &Path) -> Result<PathBuf, String> {
    let guard = FsGuard::new([path.to_path_buf()], home.to_path_buf());
    Ok(guard
        .try_check(path)
        .map_err(|e| e.to_string())?
        .as_path()
        .to_path_buf())
}

pub fn project_root(project: &Path, home: &Path) -> Result<PathBuf, String> {
    let project = check_root(project, home)?;
    let guard = FsGuard::new([project.clone()], home.to_path_buf());
    if !guard.git_metadata(&project, &guard)? {
        return Err("This project has no Git metadata.".into());
    }
    let root = git(&project, home, &["rev-parse", "--show-toplevel"])?;
    if guard
        .try_check(Path::new(&root))
        .map_err(|e| e.to_string())?
        .as_path()
        != project
    {
        return Err("The project must be the root of its Git repository.".into());
    }
    let common = git(&project, home, &["rev-parse", "--git-common-dir"])?;
    guard
        .try_check(&project.join(common))
        .map_err(|_| "Git common metadata is outside the project".to_owned())?;
    Ok(project)
}

pub fn base(project: &Path, home: &Path) -> Result<String, String> {
    let project = project_root(project, home)?;
    git(&project, home, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    Ok(git(
        &project,
        home,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .unwrap_or_else(|_| "HEAD".into()))
}

fn parent(project: &Path) -> Result<PathBuf, String> {
    let sibling = project.parent().ok_or("Project has no parent directory")?;
    let name = project.file_name().ok_or("Project has no name")?;
    Ok(sibling.join(".hearth-worktrees").join(name))
}

pub fn create(project: &Path, home: &Path, requested_base: &str) -> Result<Worktree, String> {
    create_in(project, home, requested_base, None)
}

pub fn create_in(
    project: &Path,
    home: &Path,
    requested_base: &str,
    selected: Option<&Path>,
) -> Result<Worktree, String> {
    let project = project_root(project, home)?;
    let revision = format!("{requested_base}^{{commit}}");
    let base = git(
        &project,
        home,
        &["rev-parse", "--verify", "--end-of-options", &revision],
    )?;
    let default_parent = parent(&project)?;
    let top = selected.unwrap_or(default_parent.parent().ok_or("Invalid destination")?);
    let parent = top.join(project.file_name().ok_or("Project has no name")?);
    let guard = FsGuard::new([project.clone()], home.to_path_buf());
    for dir in [top, parent.as_path()] {
        guard.worktree_component(&project, top, dir, selected.is_none() || dir != top, false)?;
    }
    let slug = ThreadId::new().0;
    let branch = format!("hearth/{slug}");
    let path = parent.join(&slug);
    guard.worktree_component(&project, top, &path, false, true)?;
    let path_text = path.to_str().ok_or("Worktree path is not valid UTF-8")?;
    git(
        &project,
        home,
        &["worktree", "add", "-b", &branch, path_text, &base],
    )
    .map_err(|e| {
        format!(
            "{e}; check for a partially created worktree at {} or branch {branch}",
            path.display()
        )
    })?;
    let path = validate_in(&project, home, &path, selected).map_err(|e| {
        format!(
            "{e}; worktree may need manual cleanup at {} on {branch}",
            path.display()
        )
    })?;
    Ok(Worktree { path, branch, base })
}

pub fn validate(project: &Path, home: &Path, path: &Path) -> Result<PathBuf, String> {
    validate_in(project, home, path, None)
}

pub fn validate_in(
    project: &Path,
    home: &Path,
    path: &Path,
    selected: Option<&Path>,
) -> Result<PathBuf, String> {
    let project = project_root(project, home)?;
    let default_parent = parent(&project)?;
    let top = selected.unwrap_or(default_parent.parent().ok_or("Invalid destination")?);
    let parent = top.join(project.file_name().ok_or("Project has no name")?);
    let guard = FsGuard::new([project.clone()], home.to_path_buf());
    for dir in [top, parent.as_path(), path] {
        guard.worktree_component(&project, top, dir, false, false)?;
    }
    let canonical = check_root(path, home)?;
    let expected_parent = check_root(&parent, home)?;
    if canonical.parent() != Some(expected_parent.as_path()) || path != canonical {
        return Err("Worktree is outside its expected directory".into());
    }
    let worktree_guard = FsGuard::new([canonical.clone()], home.to_path_buf());
    if !worktree_guard.git_metadata(&canonical, &guard)? {
        return Err("Worktree Git metadata is missing.".into());
    }
    let common = |cwd: &Path| -> Result<PathBuf, String> {
        let value = git(cwd, home, &["rev-parse", "--git-common-dir"])?;
        Ok(guard
            .try_check(&cwd.join(value))
            .map_err(|_| "Git common metadata is outside the project".to_owned())?
            .as_path()
            .to_path_buf())
    };
    if common(&project)? != common(&canonical)?
        || git(&canonical, home, &["rev-parse", "--show-toplevel"])? != canonical.to_string_lossy()
    {
        return Err("Worktree does not belong to this project".into());
    }
    Ok(canonical)
}
