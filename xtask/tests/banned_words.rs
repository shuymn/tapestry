//! `cargo xtask banned-words` against temporary Git repositories.

use std::path::Path;
use std::process::{Command, Output};

const HEADER: &str = "# header\n";
const LIST: &str = "# header\nzorblax\n";

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.gitconfig"), "").unwrap();
        std::fs::create_dir(dir.path().join("repo")).unwrap();
        let repo = Repo { dir };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn root(&self) -> std::path::PathBuf {
        self.dir.path().join("repo")
    }

    /// A command isolated from the caller's Git configuration and hook
    /// environment.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(self.root())
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.dir.path().join("empty.gitconfig"))
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid");
        cmd
    }

    fn git(&self, args: &[&str]) -> Output {
        let out = self.command("git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        out
    }

    /// Runs git without requiring success; returns the exit code.
    fn git_code(&self, args: &[&str]) -> i32 {
        let out = self.command("git").args(args).output().unwrap();
        out.status.code().unwrap()
    }

    /// Whether a merge is in progress (`MERGE_HEAD` exists).
    fn merging(&self) -> bool {
        self.git_code(&["rev-parse", "-q", "--verify", "MERGE_HEAD"]) == 0
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.root().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn stage(&self, path: &str, contents: &str) {
        self.write(path, contents);
        self.git(&["add", "--", path]);
    }

    fn commit(&self, message: &str) {
        self.git(&["commit", "-q", "--no-verify", "-m", message]);
    }

    /// Runs `xtask banned-words <args>`; returns the exit code and stderr.
    fn check(&self, args: &[&str]) -> (i32, String) {
        let out = self
            .command(env!("CARGO_BIN_EXE_xtask"))
            .arg("banned-words")
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            !stderr.to_lowercase().contains("zorblax"),
            "term leaked: {stderr}"
        );
        (out.status.code().unwrap(), stderr)
    }
}

fn fixture_list(dir: &Path) -> String {
    let path = dir.join("fixture-list.txt");
    std::fs::write(&path, LIST).unwrap();
    path.to_str().unwrap().to_owned()
}

#[test]
fn staged_with_fixture_list() {
    let repo = Repo::new();
    let list = fixture_list(repo.dir.path());
    repo.stage("probe.md", "a Zorblax b\n");
    let (code, err) = repo.check(&["--staged", "--list", &list]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("probe.md:1:3: banned term #1 ("), "{err}");

    repo.git(&["rm", "-q", "--cached", "probe.md"]);
    repo.stage("clean.md", "nothing here\n");
    let (code, err) = repo.check(&["--staged", "--list", &list]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn list_is_read_from_the_index() {
    let repo = Repo::new();
    repo.stage("banned-words.txt", LIST);
    repo.stage("probe.md", "zorblax\n");
    // The working copy drops the term; the staged list still has it.
    repo.write("banned-words.txt", HEADER);
    let (code, err) = repo.check(&["--staged"]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("probe.md:1:1: banned term #1 (banned-words.txt:2)"),
        "{err}"
    );
    let (code, err) = repo.check(&["--all"]);
    assert_eq!(code, 1, "{err}");

    // The working copy adds the term; the staged list has none.
    repo.stage("banned-words.txt", HEADER);
    repo.write("banned-words.txt", LIST);
    for mode in ["--staged", "--all"] {
        let (code, err) = repo.check(&[mode]);
        assert_eq!(code, 2, "{err}");
        assert!(err.contains("banned-words.txt: list has no terms"), "{err}");
    }
}

#[test]
fn scanned_file_is_read_from_the_index() {
    let repo = Repo::new();
    repo.stage("banned-words.txt", LIST);
    repo.stage("probe.md", "clean\n");
    repo.write("probe.md", "zorblax\n");
    assert_eq!(repo.check(&["--staged"]).0, 0);
    assert_eq!(repo.check(&["--all"]).0, 0);

    repo.stage("probe.md", "zorblax\n");
    repo.write("probe.md", "clean\n");
    assert_eq!(repo.check(&["--staged"]).0, 1);
    assert_eq!(repo.check(&["--all"]).0, 1);
}

#[test]
fn staged_checks_only_changes_against_head_and_all_checks_everything() {
    let repo = Repo::new();
    repo.stage("banned-words.txt", LIST);
    repo.stage("old.md", "zorblax\n");
    repo.commit("init");
    repo.stage("new.md", "clean\n");
    assert_eq!(repo.check(&["--staged"]).0, 0);
    let (code, err) = repo.check(&["--all"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("old.md:1:1:"), "{err}");
}

#[test]
fn path_findings_are_redacted() {
    let repo = Repo::new();
    repo.stage("banned-words.txt", LIST);
    repo.stage("docs/newZorblaxPlayer.md", "clean\n");
    let (code, err) = repo.check(&["--staged"]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("docs/new<banned term #1>Player.md:0:0: banned term #1"),
        "{err}"
    );
}

#[test]
fn merge_resolution_adding_a_term_is_staged() {
    let repo = Repo::new();
    repo.stage("banned-words.txt", LIST);
    repo.stage("a.md", "base\n");
    repo.commit("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.stage("a.md", "side\n");
    repo.commit("side");
    repo.git(&["checkout", "-q", "main"]);
    repo.stage("a.md", "main\n");
    repo.commit("main");
    assert_eq!(repo.git_code(&["merge", "-q", "side"]), 1);
    assert!(repo.merging());
    let unmerged = repo.git(&["ls-files", "--unmerged", "--", "a.md"]).stdout;
    assert!(!unmerged.is_empty(), "a.md should be in conflict");
    repo.stage("a.md", "resolved zorblax\n");
    let (code, err) = repo.check(&["--staged"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("a.md:1:10:"), "{err}");
}

#[test]
fn missing_list_and_bad_usage_exit_2() {
    let repo = Repo::new();
    let (code, err) = repo.check(&["--all"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("banned-words.txt is not in the index"),
        "{err}"
    );
    assert_eq!(repo.check(&[]).0, 2);
    assert_eq!(repo.check(&["--staged", "--all"]).0, 2);
    assert_eq!(repo.check(&["--all", "--list"]).0, 2);
}

/// What `pre-merge-commit` sees: an automatic merge, before its commit.
#[test]
fn automatic_merge_bringing_a_term_is_staged() {
    let repo = Repo::new();
    repo.stage("banned-words.txt", LIST);
    repo.stage("a.md", "base\n");
    repo.commit("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.stage("b.md", "zorblax\n");
    repo.commit("side");
    repo.git(&["checkout", "-q", "main"]);
    repo.stage("a.md", "main\n");
    repo.commit("main");
    repo.git(&["merge", "-q", "--no-commit", "--no-ff", "side"]);
    assert!(repo.merging());
    let (code, err) = repo.check(&["--staged"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("b.md:1:1:"), "{err}");
}
