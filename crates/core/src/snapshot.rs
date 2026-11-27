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
        if !out.status.success()
            && !(args.iter().any(|arg| arg == "--no-index") && out.status.code() == Some(1))
        {
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

/// A changed path in two retained trees. The ID encodes the exact path bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffFile {
    pub id: String,
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
}

/// The meaning of a displayed unified diff row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffRowKind {
    Context,
    Delete,
    Add,
    Notice,
}

/// A display row from a unified diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffRow {
    pub kind: DiffRowKind,
    pub text: String,
    pub old_line: Option<u64>,
    pub new_line: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    pub header: String,
    pub rows: Vec<DiffRow>,
    pub split_rows: Vec<SplitRow>,
}

/// Indices into a hunk's rows for aligned side-by-side display.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitRow {
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub notice: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub binary: bool,
    pub hunks: Vec<DiffHunk>,
}

fn valid_trees(before: &str, after: &str) -> Result<(), String> {
    if [before, after]
        .iter()
        .all(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        Ok(())
    } else {
        Err("invalid tree ID".into())
    }
}

fn path_id(path: &[u8]) -> String {
    path.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct RawDiffFile {
    path: Vec<u8>,
    old_path: Option<Vec<u8>>,
    status: String,
    old_blob: String,
    new_blob: String,
}

fn raw_files(git: &PrivateGit, before: &str, after: &str) -> Result<Vec<RawDiffFile>, String> {
    valid_trees(before, after)?;
    let output = git.git(
        &[
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--raw".into(),
            "--no-abbrev".into(),
            "-z".into(),
            "-M".into(),
            before.into(),
            after.into(),
            "--".into(),
        ],
        &mut &[][..],
    )?;
    let mut fields = output.split(|b| *b == 0).filter(|p| !p.is_empty());
    let mut files = Vec::new();
    while let Some(header) = fields.next() {
        let header = std::str::from_utf8(header).map_err(|e| e.to_string())?;
        let mut parts = header.split_whitespace();
        parts.next().ok_or("missing old mode")?;
        parts.next().ok_or("missing new mode")?;
        let old_blob = parts.next().ok_or("missing old blob")?.to_owned();
        let new_blob = parts.next().ok_or("missing new blob")?.to_owned();
        let status = parts.next().ok_or("missing status")?.as_bytes();
        let first = fields.next().ok_or("missing diff path")?;
        let renamed = status.starts_with(b"R");
        let path = if renamed {
            fields.next().ok_or("missing rename path")?
        } else {
            first
        };
        files.push(RawDiffFile {
            path: path.to_vec(),
            old_path: renamed.then(|| first.to_vec()),
            status: String::from_utf8_lossy(&status[..1]).into_owned(),
            old_blob,
            new_blob,
        });
    }
    Ok(files)
}

/// Lists changed files without interpreting Git's path bytes as UTF-8.
pub fn files(
    guard: &FsGuard,
    data: &Path,
    before: &str,
    after: &str,
) -> Result<Vec<DiffFile>, String> {
    let git = PrivateGit::open(guard, data, false)?;
    Ok(raw_files(&git, before, after)?
        .into_iter()
        .map(|file| DiffFile {
            id: path_id(&file.path),
            path: String::from_utf8_lossy(&file.path).into_owned(),
            old_path: file
                .old_path
                .map(|path| String::from_utf8_lossy(&path).into_owned()),
            status: file.status,
        })
        .collect())
}

/// Reads one previously listed path and parses its unified hunks.
#[cfg(unix)]
pub fn file_diff(
    guard: &FsGuard,
    data: &Path,
    before: &str,
    after: &str,
    id: &str,
) -> Result<FileDiff, String> {
    let git = PrivateGit::open(guard, data, false)?;
    let file = raw_files(&git, before, after)?
        .into_iter()
        .find(|file| path_id(&file.path) == id)
        .ok_or("unknown diff file")?;
    let blob = |id: &str| -> Result<Vec<u8>, String> {
        if id.bytes().all(|byte| byte == b'0') {
            Ok(Vec::new())
        } else {
            git.git(&["cat-file".into(), "blob".into(), id.into()], &mut &[][..])
        }
    };
    let old_path = git._temp.path().join("old");
    let new_path = git._temp.path().join("new");
    std::fs::write(&old_path, blob(&file.old_blob)?).map_err(|e| e.to_string())?;
    std::fs::write(&new_path, blob(&file.new_blob)?).map_err(|e| e.to_string())?;
    let patch = git.git(
        &[
            "diff".into(),
            "--no-index".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--no-color".into(),
            "--".into(),
            old_path.into_os_string(),
            new_path.into_os_string(),
        ],
        &mut &[][..],
    )?;
    let mut result = FileDiff {
        binary: false,
        hunks: Vec::new(),
    };
    let mut old = 0;
    let mut new = 0;
    for line in patch.split(|b| *b == b'\n') {
        if line.starts_with(b"Binary files ") || line == b"GIT binary patch" {
            result.binary = true;
        }
        if line.starts_with(b"@@ ") {
            let header = String::from_utf8_lossy(line).into_owned();
            let mut parts = header.split_whitespace();
            parts.next();
            old = parts
                .next()
                .and_then(|s| s.strip_prefix('-'))
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.parse().ok())
                .ok_or("invalid hunk header")?;
            new = parts
                .next()
                .and_then(|s| s.strip_prefix('+'))
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.parse().ok())
                .ok_or("invalid hunk header")?;
            result.hunks.push(DiffHunk {
                header,
                rows: Vec::new(),
                split_rows: Vec::new(),
            });
        } else if let Some(hunk) = result.hunks.last_mut() {
            let Some((&prefix, text)) = line.split_first() else {
                continue;
            };
            let (kind, old_line, new_line) = match prefix {
                b' ' => {
                    let pair = (Some(old), Some(new));
                    old += 1;
                    new += 1;
                    (DiffRowKind::Context, pair.0, pair.1)
                }
                b'-' => {
                    let n = old;
                    old += 1;
                    (DiffRowKind::Delete, Some(n), None)
                }
                b'+' => {
                    let n = new;
                    new += 1;
                    (DiffRowKind::Add, None, Some(n))
                }
                b'\\' => (DiffRowKind::Notice, None, None),
                _ => continue,
            };
            hunk.rows.push(DiffRow {
                kind,
                text: String::from_utf8_lossy(text).into_owned(),
                old_line,
                new_line,
            });
        }
    }
    for hunk in &mut result.hunks {
        let mut index = 0;
        while index < hunk.rows.len() {
            match hunk.rows[index].kind {
                DiffRowKind::Context => {
                    hunk.split_rows.push(SplitRow {
                        left: Some(index),
                        right: Some(index),
                        notice: None,
                    });
                    index += 1;
                }
                DiffRowKind::Notice => {
                    hunk.split_rows.push(SplitRow {
                        left: None,
                        right: None,
                        notice: Some(index),
                    });
                    index += 1;
                }
                DiffRowKind::Delete | DiffRowKind::Add => {
                    let mut left = Vec::new();
                    let mut right = Vec::new();
                    while index < hunk.rows.len() {
                        match hunk.rows[index].kind {
                            DiffRowKind::Delete => left.push(index),
                            DiffRowKind::Add => right.push(index),
                            _ => break,
                        }
                        index += 1;
                    }
                    for offset in 0..left.len().max(right.len()) {
                        hunk.split_rows.push(SplitRow {
                            left: left.get(offset).copied(),
                            right: right.get(offset).copied(),
                            notice: None,
                        });
                    }
                }
            }
        }
    }
    Ok(result)
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
