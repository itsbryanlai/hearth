//! The file-access guard. Every file read in this crate goes through
//! [`FsGuard::check`]: paths are canonicalized (so `..` and symlinks are
//! resolved first), must sit inside an allowed root, and must never touch the
//! agent credential and config locations in the home directory.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// A path that passed [`FsGuard::check`]: absolute, symlink-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPath(PathBuf);

impl CanonicalPath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Why a path was refused.
#[derive(Debug)]
pub enum Denied {
    /// The path is in the credential/config denylist (even inside a root).
    Denylist(PathBuf),
    /// The path is not inside any allowed root.
    OutsideRoots(PathBuf),
    /// The path could not be canonicalized (missing, unreadable).
    Unresolvable(PathBuf, io::ErrorKind),
}

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denylist(p) => write!(f, "{} is a protected location", p.display()),
            Self::OutsideRoots(p) => write!(f, "{} is outside the allowed folders", p.display()),
            Self::Unresolvable(p, k) => write!(f, "cannot resolve {}: {k}", p.display()),
        }
    }
}

impl std::error::Error for Denied {}

/// Error from the guarded read helpers.
#[derive(Debug)]
pub enum FsError {
    Denied(Denied),
    Io(io::Error),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied(d) => d.fmt(f),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for FsError {}

impl From<Denied> for FsError {
    fn from(d: Denied) -> Self {
        Self::Denied(d)
    }
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub struct FsGuard {
    roots: Vec<PathBuf>,
    /// Both the canonical and the literal home, so a denylist entry that does
    /// not exist on disk is still matched.
    homes: Vec<PathBuf>,
}

impl FsGuard {
    /// `roots`: project dirs, worktrees, the data dir. Roots that do not exist
    /// yet are kept as given. `home` is the user's home directory.
    pub fn new(roots: impl IntoIterator<Item = PathBuf>, home: PathBuf) -> Self {
        let roots = roots
            .into_iter()
            .map(|r| r.canonicalize().unwrap_or(r))
            .collect();
        let mut homes = vec![home.clone()];
        if let Ok(c) = home.canonicalize()
            && c != home
        {
            homes.push(c);
        }
        Self { roots, homes }
    }

    /// Allows one more root (a project the user added).
    pub fn add_root(&mut self, root: &Path) {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if !self.roots.contains(&root) {
            self.roots.push(root);
        }
    }

    /// The verdict without the debug panic. Prefer [`FsGuard::check`].
    pub fn try_check(&self, path: &Path) -> Result<CanonicalPath, Denied> {
        let canon = path
            .canonicalize()
            .map_err(|e| Denied::Unresolvable(path.to_path_buf(), e.kind()))?;
        if self.in_denylist(&canon) || self.in_denylist(path) {
            return Err(Denied::Denylist(canon));
        }
        if self.roots.iter().any(|r| canon.starts_with(r)) {
            Ok(CanonicalPath(canon))
        } else {
            Err(Denied::OutsideRoots(canon))
        }
    }

    /// Canonicalize and allow, or deny. A denylist hit panics under
    /// `debug_assertions` (it means a caller tried to reach a credential).
    pub fn check(&self, path: &Path) -> Result<CanonicalPath, Denied> {
        let verdict = self.try_check(path);
        if let Err(Denied::Denylist(p)) = &verdict {
            debug_assert!(false, "file-access guard: denylisted path {}", p.display());
        }
        verdict
    }

    pub fn read_to_string(&self, path: &Path) -> Result<String, FsError> {
        let p = self.check(path)?;
        Ok(std::fs::read_to_string(p.as_path())?)
    }

    /// Entries of a directory, as paths under the canonical directory.
    /// Symlinked entries are listed, not followed; reading one goes through
    /// `check` again.
    pub fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>, FsError> {
        let p = self.check(path)?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(p.as_path())? {
            out.push(entry?.path());
        }
        out.sort();
        Ok(out)
    }

    fn in_denylist(&self, path: &Path) -> bool {
        self.homes.iter().any(|home| {
            let Ok(rest) = path.strip_prefix(home) else {
                return false;
            };
            let mut it = rest.components().map(|c| c.as_os_str().to_string_lossy());
            let first = it.next();
            let second = it.next();
            match first.as_deref() {
                Some(n) if n.starts_with(".claude") => true,
                Some(".qwen" | ".codex" | ".ssh") => true,
                Some(".config") => second.as_deref() == Some("gh"),
                Some("Library") => second.as_deref() == Some("Keychains"),
                _ => false,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Env {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        project: PathBuf,
        guard: FsGuard,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let home = base.join("home");
        let project = base.join("project");
        for d in [".qwen", ".ssh", ".config/gh"] {
            fs::create_dir_all(home.join(d)).unwrap();
            fs::write(home.join(d).join("secret"), "s").unwrap();
        }
        fs::write(home.join(".claude.json"), "s").unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("ok.txt"), "hello").unwrap();
        fs::write(base.join("outside.txt"), "x").unwrap();
        // Symlink inside the project that points into ~/.qwen.
        std::os::unix::fs::symlink(home.join(".qwen"), project.join("link")).unwrap();
        // A project root that is the home dir itself would contain the denylist.
        let guard = FsGuard::new([project.clone(), home.clone()], home.clone());
        Env {
            _tmp: tmp,
            home,
            project,
            guard,
        }
    }

    // Fails if allowed files are refused, files outside the roots or `..`
    // escapes are let through, or any denylist entry (including via symlink
    // or inside an allowed root) is let through.
    #[test]
    fn verdicts() {
        let e = env();
        let outside = e.project.join("../outside.txt");
        let cases: Vec<(&str, PathBuf, &str)> = vec![
            ("allowed file", e.project.join("ok.txt"), "ok"),
            (
                "outside roots",
                e.project.parent().unwrap().join("outside.txt"),
                "outside",
            ),
            ("dotdot escape", outside, "outside"),
            (
                "symlink into ~/.qwen",
                e.project.join("link/secret"),
                "deny",
            ),
            ("~/.claude.json", e.home.join(".claude.json"), "deny"),
            ("~/.ssh in allowed root", e.home.join(".ssh/secret"), "deny"),
            (
                "~/.config/gh in allowed root",
                e.home.join(".config/gh/secret"),
                "deny",
            ),
        ];
        for (name, path, want) in cases {
            let got = match e.guard.try_check(&path) {
                Ok(_) => "ok",
                Err(Denied::OutsideRoots(_)) => "outside",
                Err(Denied::Denylist(_)) => "deny",
                Err(Denied::Unresolvable(..)) => "unresolvable",
            };
            assert_eq!(got, want, "{name}");
        }
    }

    // Fails if the read helper bypasses the guard.
    #[test]
    fn read_helpers_go_through_check() {
        let e = env();
        assert_eq!(
            e.guard.read_to_string(&e.project.join("ok.txt")).unwrap(),
            "hello"
        );
        assert!(matches!(
            e.guard.read_to_string(&e.project.join("../outside.txt")),
            Err(FsError::Denied(Denied::OutsideRoots(_)))
        ));
        assert_eq!(e.guard.read_dir(&e.project).unwrap().len(), 2);
    }

    // Fails if a denylist hit does not panic under debug_assertions.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "denylisted")]
    fn denylist_hit_panics_in_debug() {
        let e = env();
        let _ = e.guard.check(&e.home.join(".claude.json"));
    }
}
