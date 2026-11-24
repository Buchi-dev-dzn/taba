//! Who is calling: the caller's scope (flag, environment or git worktree)
//! and agent name. See design v0.2, section 3, "Owner identity".

use crate::error::{Next, Result, TabaError};
use crate::state::validate_ident;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeSource {
    Flag,
    Env,
    Git,
}

impl ScopeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeSource::Flag => "flag",
            ScopeSource::Env => "env",
            ScopeSource::Git => "git",
        }
    }
}

/// The resolved identity of the process calling taba.
#[derive(Debug, Clone)]
pub struct Caller {
    pub scope: Option<String>,
    pub source: Option<ScopeSource>,
    /// Root of the git worktree containing `cwd`, if any (also when the
    /// scope itself came from a flag or the environment).
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
    /// `--agent`, else `$TABA_AGENT`, else `human`.
    pub agent: String,
    pub cwd: PathBuf,
}

impl Caller {
    /// The caller's scope, or `E_NO_SCOPE`.
    pub fn require_scope(&self) -> Result<String> {
        self.scope.clone().ok_or_else(no_scope)
    }

    /// Whether `--agent` / `$TABA_AGENT` was set.
    pub fn is_human(&self) -> bool {
        self.agent == "human"
    }
}

pub fn no_scope() -> TabaError {
    TabaError::new(
        "E_NO_SCOPE",
        "no scope: pass --scope, set TABA_SCOPE, or run inside a git worktree",
    )
    .next(Next::just("pass --scope SCOPE"))
    .next(Next::just(
        "cd into the git worktree you are working in (its folder is the scope)",
    ))
}

/// Resolves the caller: `--scope` > `$TABA_SCOPE` > git worktree of the
/// current directory > none.
pub fn resolve(scope_flag: Option<String>, agent_flag: Option<String>) -> Result<Caller> {
    let cwd = std::env::current_dir()
        .and_then(|d| d.canonicalize())
        .map_err(|e| TabaError::state(format!("cannot resolve current directory: {e}")))?;
    let agent = resolve_agent(agent_flag)?;
    let worktree = find_worktree(&cwd);
    let branch = worktree.as_deref().and_then(read_branch);

    let env_scope = std::env::var("TABA_SCOPE").ok().filter(|s| !s.is_empty());
    let (scope, source) = if let Some(s) = scope_flag {
        (Some(s), Some(ScopeSource::Flag))
    } else if let Some(s) = env_scope {
        (Some(s), Some(ScopeSource::Env))
    } else if let Some(root) = &worktree {
        (Some(derive_scope(root)), Some(ScopeSource::Git))
    } else {
        (None, None)
    };
    if let Some(s) = &scope {
        check_ident("scope", s)?;
    }
    Ok(Caller {
        scope,
        source,
        worktree,
        branch,
        agent,
        cwd,
    })
}

pub fn check_ident(kind: &str, s: &str) -> Result<()> {
    validate_ident(kind, s).map_err(|m| TabaError::new("E_INVALID_IDENT", m))
}

fn resolve_agent(flag: Option<String>) -> Result<String> {
    let agent = flag
        .or_else(|| std::env::var("TABA_AGENT").ok())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "human".to_string());
    let ok = agent.len() <= 128 && agent.chars().all(|c| c.is_ascii_graphic());
    if !ok {
        return Err(TabaError::new(
            "E_INVALID_IDENT",
            format!("invalid agent {agent:?}: use at most 128 printable characters, no spaces"),
        ));
    }
    Ok(agent)
}

/// Nearest ancestor of `dir` (itself included) that contains a `.git` file
/// or directory. Does not run `git`.
pub fn find_worktree(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Current branch of the worktree at `root`, or `None` when detached or
/// unreadable. Follows the `gitdir:` pointer of linked worktrees.
pub fn read_branch(root: &Path) -> Option<String> {
    let dotgit = root.join(".git");
    let gitdir = if dotgit.is_dir() {
        dotgit
    } else {
        let text = fs::read_to_string(&dotgit).ok()?;
        let p = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        if p.is_absolute() { p } else { root.join(p) }
    };
    let head = fs::read_to_string(gitdir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

/// `<dirname>-<hash4>`: a slug of the worktree folder's name plus the first
/// four hex digits of a 64-bit FNV-1a hash of its canonical path.
pub fn derive_scope(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    slug = slug.trim_start_matches('.').to_string();
    if slug.is_empty() {
        slug = "worktree".to_string();
    }
    slug.truncate(59); // ASCII only, so byte truncation is safe; 59 + 5 = 64
    let hash = fnv1a64(root.as_os_str().as_encoded_bytes());
    format!("{slug}-{}", &format!("{hash:016x}")[..4])
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_scope_is_valid_and_stable() {
        let s = derive_scope(Path::new("/Users/me/wt/login-fix"));
        assert!(s.starts_with("login-fix-"), "{s}");
        assert_eq!(s.len(), "login-fix-".len() + 4);
        assert_eq!(s, derive_scope(Path::new("/Users/me/wt/login-fix")));
        assert_ne!(s, derive_scope(Path::new("/Users/me/other/login-fix")));
        assert!(validate_ident("scope", &s).is_ok());

        let odd = derive_scope(Path::new("/x/.My Repo (copy)"));
        assert!(odd.starts_with("My-Repo--copy--"), "{odd}");
        assert!(validate_ident("scope", &odd).is_ok());

        let long = derive_scope(&PathBuf::from(format!("/x/{}", "a".repeat(200))));
        assert_eq!(long.len(), 64);
        assert!(validate_ident("scope", &long).is_ok());
    }
}
