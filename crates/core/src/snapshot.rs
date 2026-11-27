//! Private Git trees built exclusively from guarded bytes.
use crate::fsguard::FsGuard;
#[cfg(unix)]
use crate::fsguard::SnapshotEntry;
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct PrivateGit {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    objects: PathBuf,
    index: PathBuf,
}

fn checked_dir(guard: &FsGuard, path: &Path) -> Result<PathBuf, String> {
    let checked = guard.try_check(path).map_err(|e| e.to_string())?;
    if checked.as_path().as_os_str() != path.as_os_str() || !path.is_dir() {
        return Err("snapshot directory is not its canonical path".into());
    }
    Ok(path.to_owned())
}

impl PrivateGit {
    fn open(guard: &FsGuard, data: &Path, create: bool) -> Result<Self, String> {
        let data = checked_dir(guard, data)?;
        let objects = data.join("objects");
        if create {
            match std::fs::create_dir(&objects) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        checked_dir(guard, &objects)?;
        match std::fs::symlink_metadata(objects.join("info")) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                checked_dir(guard, &objects.join("info"))?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            _ => return Err("invalid snapshot object metadata directory".into()),
        }
        match std::fs::symlink_metadata(objects.join("info/alternates")) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            _ => return Err("snapshot object alternates are unsupported".into()),
        }
        let temp = tempfile::Builder::new()
            .prefix("snapshot-git-")
            .tempdir_in(&data)
            .map_err(|e| e.to_string())?;
        let repo = temp.path().join("git");
        let status = Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "init.templateDir=", "init", "--bare", "--quiet"])
            .arg(&repo)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("cannot initialize private Git metadata".into());
        }
        let index = temp.path().join("index");
        Ok(Self {
            _temp: temp,
            repo,
            objects,
            index,
        })
    }

    fn git(&self, args: &[OsString], input: &mut dyn Read) -> Result<Vec<u8>, String> {
        let mut cmd = Command::new("git");
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OBJECT_DIRECTORY", &self.objects)
            .env("GIT_INDEX_FILE", &self.index)
            .arg("--git-dir")
            .arg(&self.repo)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        io::copy(input, &mut child.stdin.take().ok_or("missing Git stdin")?)
            .map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        Ok(out.stdout)
    }
}

/// Captures exactly the supplied relative paths; selection belongs to the caller.
/// An unsafe or missing entry makes the whole boundary unavailable. Trees remain
/// in app-owned storage; retention and cleanup are the caller's responsibility.
#[cfg(unix)]
pub fn capture(
    guard: &FsGuard,
    data: &Path,
    root: &Path,
    paths: &[PathBuf],
) -> Result<String, String> {
    use std::os::unix::ffi::OsStrExt;
    checked_dir(guard, root)?;
    let git = PrivateGit::open(guard, data, true)?;
    let mut entries = Vec::new();
    for path in paths {
        let entry = guard
            .read_snapshot_entry(root, path)
            .map_err(|e| e.to_string())?;
        let (mode, mut input): (&[u8], Box<dyn Read>) = match entry {
            SnapshotEntry::File(file, executable) => (
                if executable { b"100755 " } else { b"100644 " },
                Box::new(file),
            ),
            SnapshotEntry::Symlink(target) => (b"120000 ", Box::new(io::Cursor::new(target))),
        };
        let oid = git.git(
            &[
                "hash-object".into(),
                "-w".into(),
                "--stdin".into(),
                "--no-filters".into(),
            ],
            &mut input,
        )?;
        entries.extend_from_slice(mode);
        entries.extend_from_slice(oid.trim_ascii_end());
        entries.push(b'\t');
        entries.extend_from_slice(path.as_os_str().as_bytes());
        entries.push(0);
    }
    git.git(
        &["update-index".into(), "-z".into(), "--index-info".into()],
        &mut &entries[..],
    )?;
    let listed = git.git(&["ls-files".into(), "-z".into()], &mut &[][..])?;
    let mut actual: Vec<&[u8]> = listed
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    let mut requested: Vec<&[u8]> = paths.iter().map(|p| p.as_os_str().as_bytes()).collect();
    actual.sort_unstable();
    requested.sort_unstable();
    requested.dedup();
    if actual != requested {
        return Err("Git could not capture every requested path".into());
    }
    let tree = git.git(&["write-tree".into()], &mut &[][..])?;
    Ok(String::from_utf8(tree)
        .map_err(|e| e.to_string())?
        .trim()
        .to_owned())
}

/// Reads a diff between retained trees without external drivers or text conversion.
pub fn diff(guard: &FsGuard, data: &Path, before: &str, after: &str) -> Result<Vec<u8>, String> {
    if ![before, after]
        .iter()
        .all(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("invalid tree ID".into());
    }
    let git = PrivateGit::open(guard, data, false)?;
    git.git(
        &[
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            before.into(),
            after.into(),
            "--".into(),
        ],
        &mut &[][..],
    )
}

/// Selects checkout paths using a private copy of its ordinary index.
#[cfg(unix)]
pub fn select(
    guard: &FsGuard,
    data: &Path,
    root: &Path,
    gitdir: &Path,
    common: &Path,
) -> Result<Vec<PathBuf>, String> {
    use std::collections::BTreeSet;
    use std::os::unix::ffi::OsStringExt;

    checked_dir(guard, root)?;
    if !guard.allows_snapshot_traversal(root) {
        return Err("snapshot checkout contains protected paths".into());
    }
    checked_dir(guard, gitdir)?;
    let git = PrivateGit::open(guard, data, true)?;
    match guard.read_snapshot_entry(gitdir, Path::new("index")) {
        Ok(SnapshotEntry::File(mut source, _)) => {
            let mut target = std::fs::File::create(&git.index).map_err(|e| e.to_string())?;
            io::copy(&mut source, &mut target).map_err(|e| e.to_string())?;
        }
        Err(crate::fsguard::FsError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {}
        _ => return Err("invalid Git index".into()),
    }
    let exclude = git._temp.path().join("exclude");
    let has_exclude = match guard.read_snapshot_entry(common, Path::new("info/exclude")) {
        Ok(SnapshotEntry::File(mut source, _)) => {
            let mut target = std::fs::File::create(&exclude).map_err(|e| e.to_string())?;
            io::copy(&mut source, &mut target).map_err(|e| e.to_string())?;
            true
        }
        Err(crate::fsguard::FsError::Io(e)) if e.kind() == io::ErrorKind::NotFound => false,
        _ => return Err("invalid Git exclude file".into()),
    };
    let list = |options: &[&str], include_exclude: bool| -> Result<BTreeSet<Vec<u8>>, String> {
        let mut args = vec![
            "-C".into(),
            root.as_os_str().to_owned(),
            "--work-tree".into(),
            root.as_os_str().to_owned(),
            "ls-files".into(),
            "-z".into(),
        ];
        args.extend(options.iter().map(OsString::from));
        if include_exclude && has_exclude {
            args.push("--exclude-from".into());
            args.push(exclude.as_os_str().to_owned());
        }
        Ok(git
            .git(&args, &mut &[][..])?
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(<[u8]>::to_vec)
            .collect())
    };
    let mut paths = list(
        &["--cached", "--others", "--exclude-per-directory=.gitignore"],
        true,
    )?;
    for deleted in list(&["--deleted"], false)? {
        paths.remove(&deleted);
    }
    Ok(paths
        .into_iter()
        .map(|p| PathBuf::from(OsString::from_vec(p)))
        .collect())
}
