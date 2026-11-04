//! Worktree detection, status and removal against real git repositories in temp dirs; `create`
//! against the real omp when `OMPDECK_TEST_OMP=<omp path>` is set (skipped otherwise).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use deck_core::omp::Omp;
use deck_core::sessions::{SessionsRoot, scan};
use deck_core::worktree::{
    BranchState, Removal, Removed, Unsafe, Worktree, WorktreeError, add_existing, branch_state,
    check_branch_name, create, default_location, infer_base, park_blockers, remove, repo_info,
    resolve, status, switch_branch,
};

const DELETE: Removal = Removal::Merged {
    delete_branch: true,
};
const KEEP: Removal = Removal::Merged {
    delete_branch: false,
};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c"])
        .arg("commit.gpgsign=false")
        .args(args)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env(
            "GIT_CONFIG_SYSTEM",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("LC_ALL", "C")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn commit(dir: &Path, file: &str, body: &str) {
    fs::write(dir.join(file), body).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", file]);
}

struct Fixture {
    _tmp: tempfile::TempDir,
    /// Canonical temp root (git prints canonical paths).
    root: PathBuf,
    repo: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    // Windows canonical paths are verbatim (`\\?\C:\…`), which git can't create worktrees under;
    // git itself reports the plain drive form.
    let root = match root.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(plain) if !plain.starts_with("UNC\\") => PathBuf::from(plain),
        _ => root,
    };
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join(".gitignore"), "*.log\ntarget/\n").unwrap();
    commit(&repo, "a.txt", "a");
    Fixture {
        _tmp: tmp,
        root,
        repo,
    }
}

/// Linked worktree `name` on a new branch `br` next to the repo.
fn linked(f: &Fixture, name: &str, br: &str) -> Worktree {
    let path = f.root.join(name);
    git(
        &f.repo,
        &["worktree", "add", "-q", "-b", br, path.to_str().unwrap()],
    );
    resolve(&path).expect("linked worktree resolves")
}

#[test]
fn resolve_distinguishes_primary_linked_subdir_detached_and_non_repo() {
    let f = fixture();
    assert_eq!(resolve(&f.repo), None, "primary checkout");
    assert_eq!(resolve(&f.repo.join(".git")), None);

    let wt = linked(&f, "feat", "feature/x");
    assert_eq!(wt.path, f.root.join("feat"));
    assert_eq!(wt.primary, f.repo);
    assert_eq!(wt.branch.as_deref(), Some("feature/x"));

    let sub = wt.path.join("deep/er");
    fs::create_dir_all(&sub).unwrap();
    assert_eq!(
        resolve(&sub),
        Some(wt.clone()),
        "subdirectory of a worktree"
    );
    assert_eq!(
        resolve(&wt.path.join("not/yet/created")),
        Some(wt.clone()),
        "walks up over missing directories"
    );

    git(&wt.path, &["checkout", "-q", "--detach"]);
    let detached = resolve(&wt.path).unwrap();
    assert_eq!(detached.branch, None);
    assert_eq!(detached.primary, f.repo);

    let plain = f.root.join("plain");
    fs::create_dir(&plain).unwrap();
    assert_eq!(resolve(&plain), None, "not a repository");
}

#[test]
fn resolve_accepts_relative_gitdir_and_rejects_other_layouts() {
    let f = fixture();
    let wt = linked(&f, "rel", "rel-branch");
    // `git worktree add --relative-paths` style: the `.git` file holds a relative path.
    let rel = format!(
        "gitdir: ../repo/.git/worktrees/{}\n",
        wt.path.file_name().unwrap().to_string_lossy()
    );
    fs::write(wt.path.join(".git"), rel).unwrap();
    assert_eq!(resolve(&wt.path), Some(wt.clone()));

    // A `.git` file that is not a linked worktree (submodule-like: no commondir) → None.
    let sub = f.root.join("sub");
    fs::create_dir_all(sub.join("meta")).unwrap();
    fs::write(sub.join(".git"), "gitdir: meta\n").unwrap();
    assert_eq!(resolve(&sub), None);
    fs::write(sub.join(".git"), "garbage").unwrap();
    assert_eq!(resolve(&sub), None);
}

#[test]
fn default_location_adds_collision_suffixes() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let primary = Path::new("/src/My Repo");
    let first = default_location(base, primary, "wt/2026-10-23");
    assert_eq!(first, base.join("wt-2026-10-23-My-Repo"));
    fs::create_dir(&first).unwrap();
    let second = default_location(base, primary, "wt/2026-10-23");
    assert_eq!(second, base.join("wt-2026-10-23-My-Repo-2"));
    fs::create_dir(&second).unwrap();
    assert_eq!(
        default_location(base, primary, "wt/2026-10-23"),
        base.join("wt-2026-10-23-My-Repo-3")
    );
    // A missing base is fine: nothing exists yet.
    assert_eq!(
        default_location(&base.join("nope"), primary, "x"),
        base.join("nope").join("x-My-Repo")
    );
}

#[test]
fn status_counts_untracked_but_not_ignored_and_tracks_merge_state() {
    let f = fixture();
    let wt = linked(&f, "st", "topic");

    let s = status(&wt, "main").unwrap();
    assert_eq!(s.branch.as_deref(), Some("topic"));
    assert!(
        !s.dirty && s.merged && s.ahead == 0,
        "fresh branch is merged: {s:?}"
    );

    // Ignored files (a file and a directory) must not count as changes.
    fs::write(wt.path.join("x.log"), "noise").unwrap();
    fs::create_dir(wt.path.join("target")).unwrap();
    fs::write(wt.path.join("target/o"), "bin").unwrap();
    assert!(!status(&wt, "main").unwrap().dirty, "ignored-only is clean");

    fs::write(wt.path.join("new.txt"), "n").unwrap();
    assert!(status(&wt, "main").unwrap().dirty, "untracked counts");
    fs::remove_file(wt.path.join("new.txt")).unwrap();

    fs::write(wt.path.join("a.txt"), "changed").unwrap();
    assert!(status(&wt, "main").unwrap().dirty, "unstaged counts");
    git(&wt.path, &["add", "a.txt"]);
    assert!(status(&wt, "main").unwrap().dirty, "staged counts");
    git(&wt.path, &["commit", "-q", "-m", "c1"]);
    commit(&wt.path, "b.txt", "b");

    let s = status(&wt, "main").unwrap();
    assert!(!s.dirty && !s.merged);
    assert_eq!(s.ahead, 2);

    git(&f.repo, &["merge", "-q", "topic"]);
    let s = status(&wt, "main").unwrap();
    assert!(s.merged && s.ahead == 0, "{s:?}");

    assert!(matches!(
        status(&wt, "no-such-branch"),
        Err(WorktreeError::Git { .. })
    ));
    assert!(matches!(
        status(&wt, "--all"),
        Err(WorktreeError::Git { .. })
    ));
}

#[test]
fn remove_refuses_unsafe_states_without_touching_anything() {
    let f = fixture();
    let wt = linked(&f, "r1", "dirty-one");
    fs::write(wt.path.join("wip.txt"), "wip").unwrap();
    match remove(&wt, "main", DELETE) {
        Err(WorktreeError::Unsafe(r)) => assert_eq!(r, vec![Unsafe::Dirty]),
        other => panic!("expected Unsafe(Dirty), got {other:?}"),
    }
    assert!(wt.path.join("wip.txt").exists());
    assert!(git(&f.repo, &["branch", "--list", "dirty-one"]).contains("dirty-one"));

    fs::remove_file(wt.path.join("wip.txt")).unwrap();
    commit(&wt.path, "work.txt", "w");
    fs::write(wt.path.join("wip2.txt"), "x").unwrap();
    match remove(&wt, "main", DELETE) {
        Err(WorktreeError::Unsafe(r)) => {
            assert_eq!(r, vec![Unsafe::Dirty, Unsafe::NotMerged { ahead: 1 }]);
        }
        other => panic!("expected Dirty + NotMerged, got {other:?}"),
    }
    fs::remove_file(wt.path.join("wip2.txt")).unwrap();
    assert!(matches!(
        remove(&wt, "main", DELETE),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::NotMerged { ahead: 1 }]
    ));
    assert!(wt.path.join("work.txt").exists());
    assert!(
        git(&f.repo, &["branch", "--list", "dirty-one"]).contains("dirty-one"),
        "unmerged branch survives"
    );

    // Detached with commits outside the base is refused too.
    git(&wt.path, &["checkout", "-q", "--detach"]);
    let det = resolve(&wt.path).unwrap();
    assert!(matches!(
        remove(&det, "main", DELETE),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::Detached]
    ));
    assert!(wt.path.exists());
}

#[test]
fn remove_deletes_clean_merged_worktree_and_its_branch_only() {
    let f = fixture();
    let wt = linked(&f, "done", "finished");
    let other = linked(&f, "keep", "keeper");
    commit(&wt.path, "f.txt", "f");
    git(&f.repo, &["merge", "-q", "finished"]);
    // Ignored leftovers do not block removal... git refuses them without --force, so use none.
    let done = remove(&wt, "main", DELETE).unwrap();
    assert_eq!(
        done,
        Removed {
            branch_deleted: true,
            branch_note: None
        }
    );
    assert!(!wt.path.exists());
    assert!(git(&f.repo, &["branch", "--list", "finished"]).is_empty());
    assert!(other.path.exists(), "sibling worktree untouched");
    assert!(git(&f.repo, &["branch", "--list", "keeper"]).contains("keeper"));

    // Missing directory → Unsafe::Missing, nothing run.
    assert!(matches!(
        remove(&wt, "main", DELETE),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::Missing]
    ));

    // Without delete_branch the branch stays.
    let w2 = linked(&f, "done2", "finished2");
    assert!(!remove(&w2, "main", KEEP).unwrap().branch_deleted);
    assert!(git(&f.repo, &["branch", "--list", "finished2"]).contains("finished2"));
}

/// Two commits on `br` in a fresh linked worktree, squash-merged into `main` as one commit.
fn squash_merged(f: &Fixture, name: &str, br: &str) -> Worktree {
    let wt = linked(f, name, br);
    commit(&wt.path, "s1.txt", "one");
    commit(&wt.path, "s2.txt", "two");
    git(&f.repo, &["merge", "--squash", "-q", br]);
    git(&f.repo, &["commit", "-q", "-m", "squash"]);
    wt
}

#[test]
fn squash_merged_branch_counts_as_merged_and_remove_deletes_it() {
    let f = fixture();
    let wt = squash_merged(&f, "sq", "squashed");
    let s = status(&wt, "main").unwrap();
    assert!(s.merged && !s.dirty, "{s:?}");
    assert_eq!(s.ahead, 2, "ahead still counts commits not in base");

    // Unrelated later work on main does not undo it.
    commit(&f.repo, "later.txt", "later");
    assert!(status(&wt, "main").unwrap().merged);

    let done = remove(&wt, "main", DELETE).unwrap();
    assert!(done.branch_deleted, "{done:?}");
    assert!(!wt.path.exists());
    assert!(git(&f.repo, &["branch", "--list", "squashed"]).is_empty());
}

#[test]
fn squash_that_was_reverted_or_extended_is_not_merged() {
    let f = fixture();
    let wt = squash_merged(&f, "rv", "reverted");
    git(&f.repo, &["revert", "--no-edit", "HEAD"]);
    assert!(!status(&wt, "main").unwrap().merged, "reverted squash");
    assert!(matches!(
        remove(&wt, "main", DELETE),
        Err(WorktreeError::Unsafe(r)) if matches!(r.as_slice(), [Unsafe::NotMerged { .. }])
    ));
    assert!(wt.path.exists());

    let f = fixture();
    let wt = squash_merged(&f, "ex", "extended");
    commit(&wt.path, "s3.txt", "three");
    assert!(!status(&wt, "main").unwrap().merged, "commit after squash");
}

#[test]
fn work_merged_only_into_the_remote_upstream_counts_as_merged() {
    let f = fixture();
    let origin = f.root.join("origin.git");
    git(
        &f.root,
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    git(
        &f.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
    let stale = git(&f.repo, &["rev-parse", "HEAD"]);

    let wt = squash_merged(&f, "up", "upstreamed");
    git(&f.repo, &["push", "-q", "origin", "main"]);
    // Local main and the remote-tracking ref are behind; only the remote knows the squash.
    git(&f.repo, &["reset", "-q", "--hard", &stale]);
    git(&f.repo, &["update-ref", "refs/remotes/origin/main", &stale]);

    let s = status(&wt, "main").unwrap();
    assert!(s.merged, "{s:?}");
    assert!(remove(&wt, "main", DELETE).unwrap().branch_deleted);
}

#[test]
fn branch_names_starting_with_a_dash_are_never_taken_for_options() {
    let f = fixture();
    let path = f.root.join("dash");
    git(
        &f.repo,
        &["worktree", "add", "-q", "--detach", path.to_str().unwrap()],
    );
    git(&f.repo, &["update-ref", "refs/heads/-dash", "main"]);
    git(&path, &["symbolic-ref", "HEAD", "refs/heads/-dash"]);
    let wt = resolve(&path).unwrap();
    assert_eq!(wt.branch.as_deref(), Some("-dash"));
    assert_eq!(
        status(&wt, "main").unwrap().branch.as_deref(),
        Some("-dash")
    );
    remove(&wt, "main", DELETE).unwrap();
    assert!(!path.exists());
    assert!(git(&f.repo, &["for-each-ref", "refs/heads/-dash"]).is_empty());
}

#[test]
fn create_rejects_option_like_base_and_relative_dest_before_running_omp() {
    let f = fixture();
    let omp = Omp::at("/nonexistent/omp");
    let dest = f.root.join("never");
    assert!(matches!(
        create(&omp, &f.repo, "b", "--detach", &dest),
        Err(WorktreeError::Git { .. })
    ));
    assert!(matches!(
        create(&omp, &f.repo, "b", "main", Path::new("rel/dest")),
        Err(WorktreeError::Io(_))
    ));
}

#[test]
fn remove_deletes_the_merged_branch_when_base_is_not_checked_out_in_the_primary() {
    let f = fixture();
    // The primary stays on `main`, but the work is merged into `release`, which nobody has
    // checked out: `git branch -d` would call the branch unmerged, our base check does not.
    git(&f.repo, &["branch", "release"]);
    let wt = linked(&f, "rel", "feat-rel");
    commit(&wt.path, "r.txt", "r");
    let rel = f.root.join("rel-wt");
    git(
        &f.repo,
        &["worktree", "add", "-q", rel.to_str().unwrap(), "release"],
    );
    git(&rel, &["merge", "-q", "feat-rel"]);
    git(&f.repo, &["worktree", "remove", rel.to_str().unwrap()]);
    assert_eq!(git(&f.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");

    let done = remove(&wt, "release", DELETE).unwrap();
    assert!(
        done.branch_deleted && done.branch_note.is_none(),
        "{done:?}"
    );
    assert!(!wt.path.exists());
    assert!(git(&f.repo, &["branch", "--list", "feat-rel"]).is_empty());
    assert!(git(&f.repo, &["branch", "--list", "release"]).contains("release"));
}

#[test]
fn infer_base_prefers_primary_branch_then_origin_head() {
    let f = fixture();
    let wt = linked(&f, "ib", "ib-branch");
    assert_eq!(infer_base(&wt).as_deref(), Some("main"));

    git(&f.repo, &["checkout", "-q", "-b", "dev"]);
    assert_eq!(infer_base(&wt).as_deref(), Some("dev"));

    // Primary detached, no origin → nothing to infer; with origin/HEAD → that target.
    git(&f.repo, &["checkout", "-q", "--detach"]);
    assert_eq!(infer_base(&wt), None);
    git(&f.repo, &["update-ref", "refs/remotes/origin/main", "main"]);
    git(
        &f.repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    assert_eq!(infer_base(&wt).as_deref(), Some("origin/main"));
}

#[test]
fn repo_info_lists_branches_and_reports_non_repos() {
    let f = fixture();
    let wt = linked(&f, "ri", "zeta");
    git(&f.repo, &["branch", "alpha"]);
    git(&f.repo, &["update-ref", "refs/remotes/origin/main", "main"]);
    git(
        &f.repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );

    let info = repo_info(&f.repo.join("")).unwrap();
    assert_eq!(info.root, f.repo);
    assert_eq!(info.primary, f.repo);
    assert_eq!(info.current_branch.as_deref(), Some("main"));
    assert!(info.has_commits);
    assert_eq!(info.branches, ["alpha", "main", "zeta", "origin/main"]);

    let w = repo_info(&wt.path).unwrap();
    assert_eq!(w.root, wt.path);
    assert_eq!(w.primary, f.repo, "linked worktree reports its primary");
    assert_eq!(w.current_branch.as_deref(), Some("zeta"));

    let empty = f.root.join("empty");
    fs::create_dir(&empty).unwrap();
    git(&empty, &["init", "-q", "-b", "trunk"]);
    let e = repo_info(&empty).unwrap();
    assert!(!e.has_commits && e.branches.is_empty());
    assert_eq!(e.current_branch.as_deref(), Some("trunk"));

    let plain = f.root.join("plain");
    fs::create_dir(&plain).unwrap();
    assert!(matches!(repo_info(&plain), Err(WorktreeError::NotARepo)));
    assert!(matches!(
        repo_info(&f.root.join("missing")),
        Err(WorktreeError::NotARepo)
    ));
}

#[test]
fn check_branch_name_rejects_invalid_and_existing() {
    let f = fixture();
    check_branch_name(&f.repo, "wt/20261023-1010").unwrap();
    for bad in ["", "a b", "a..b", "-x", "x.lock", "@{-1}", "a~1", "/x"] {
        assert!(
            matches!(
                check_branch_name(&f.repo, bad),
                Err(WorktreeError::Git { .. })
            ),
            "{bad:?} must be rejected"
        );
    }
    let err = check_branch_name(&f.repo, "main").unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[test]
fn scan_groups_carry_the_worktree_of_their_cwd() {
    let f = fixture();
    let wt = linked(&f, "sess", "sess-branch");
    let store = tempfile::tempdir().unwrap();
    for (bucket, cwd, id) in [("wt", &wt.path, "w1"), ("main", &f.repo, "m1")] {
        let dir = store.path().join(bucket);
        fs::create_dir_all(&dir).unwrap();
        let header = serde_json::json!({
            "type": "session", "version": 3, "id": id,
            "timestamp": "2026-01-01T00:00:00.000Z", "cwd": cwd,
        });
        let prompt = serde_json::json!({
            "type": "message", "id": "m",
            "message": {"role": "user", "content": "hello"},
        });
        fs::write(
            dir.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl")),
            format!("{header}\n{prompt}\n"),
        )
        .unwrap();
    }
    let groups = scan(&SessionsRoot::at(store.path()));
    assert_eq!(groups.len(), 2);
    let by = |cwd: &Path| groups.iter().find(|g| g.cwd == cwd).unwrap();
    assert_eq!(by(&wt.path).worktree.as_ref(), Some(&wt));
    assert_eq!(by(&f.repo).worktree, None);
}

fn omp_under_test() -> Option<PathBuf> {
    let p = std::env::var_os("OMPDECK_TEST_OMP").map(PathBuf::from)?;
    if p.as_os_str().is_empty() {
        return None;
    }
    Some(p)
}

/// Real omp bound to a sandbox HOME / XDG / `PI_*` environment under the fixture root.
fn sandboxed_omp(f: &Fixture, omp_bin: PathBuf) -> (Omp, PathBuf) {
    let sandbox = f.root.join("home");
    fs::create_dir_all(sandbox.join("rt")).unwrap();
    // Natives are extracted into `<config root>/natives`; reuse the real ones read-only.
    if let Some(real) = dirs::home_dir().map(|h| h.join(".omp").join("natives"))
        && real.exists()
    {
        fs::create_dir_all(sandbox.join(".omp")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, sandbox.join(".omp").join("natives")).unwrap();
    }
    let omp = Omp::at(omp_bin)
        .with_env("HOME", &sandbox)
        .with_env("USERPROFILE", &sandbox)
        .with_env("XDG_CONFIG_HOME", sandbox.join("cfg"))
        .with_env("XDG_DATA_HOME", sandbox.join("data"))
        .with_env("XDG_STATE_HOME", sandbox.join("state"))
        .with_env("XDG_CACHE_HOME", sandbox.join("cache"))
        .with_env("PI_CODING_AGENT_DIR", sandbox.join("agent"))
        .with_env("PI_CONFIG_DIR", ".omp")
        .with_env("OMP_WORKTREE_DIR", sandbox.join("wt"));
    (omp, sandbox)
}

/// `create` through the real omp in a sandboxed HOME / XDG / `PI_*` environment: new branch at
/// `dest` from `base_ref`, ignored files NOT copied. The sandbox keeps omp away from `~/.omp`.
#[test]
fn create_makes_a_clean_worktree_on_a_new_branch() {
    let Some(omp_bin) = omp_under_test() else {
        eprintln!("skipped: OMPDECK_TEST_OMP not set");
        return;
    };
    let f = fixture();
    let (omp, sandbox) = sandboxed_omp(&f, omp_bin);

    git(&f.repo, &["branch", "base-x"]);
    fs::write(f.repo.join(".env"), "SECRET=1\n").unwrap();
    fs::write(f.repo.join(".gitignore"), "*.log\ntarget/\n.env\n").unwrap();
    commit(&f.repo, "base-only.txt", "later");
    assert!(git(&f.repo, &["status", "--porcelain"]).is_empty());

    let dest = sandbox.join("wt").join("made");
    create(&omp, &f.repo, "new/branch", "base-x", &dest).unwrap();

    let wt = resolve(&dest).expect("dest is a linked worktree");
    assert_eq!(wt.primary, f.repo);
    assert_eq!(wt.branch.as_deref(), Some("new/branch"));
    assert!(dest.join("a.txt").exists());
    assert!(
        !dest.join("base-only.txt").exists(),
        "created from base_ref, not from HEAD"
    );
    assert!(!dest.join(".env").exists(), "ignored files are not copied");
    assert!(
        sandbox.join("data/omp-deck/worktree-overlay.yml").exists() || !cfg!(target_os = "linux"),
        "overlay is written under the configured (sandbox) data dir"
    );

    // A second create onto the same branch fails with omp's message.
    let err = create(
        &omp,
        &f.repo,
        "new/branch",
        "base-x",
        &sandbox.join("wt/again"),
    )
    .unwrap_err();
    assert!(
        matches!(err, WorktreeError::Omp { ref stderr } if !stderr.is_empty()),
        "{err:?}"
    );
}

/// Real omp, `add_existing`: checks the parked branch out at `dest` without creating a branch.
#[test]
fn add_existing_with_real_omp_checks_out_the_existing_branch() {
    let Some(omp_bin) = omp_under_test() else {
        eprintln!("skipped: OMPDECK_TEST_OMP not set");
        return;
    };
    let f = fixture();
    let (omp, sandbox) = sandboxed_omp(&f, omp_bin);
    let parked = linked(&f, "p", "parked/real");
    commit(&parked.path, "p.txt", "parked work");
    git(
        &f.repo,
        &["worktree", "remove", parked.path.to_str().unwrap()],
    );

    let dest = sandbox.join("wt").join("again");
    add_existing(&omp, &f.repo, "parked/real", &dest).unwrap();
    let wt = resolve(&dest).expect("dest is a linked worktree");
    assert_eq!(wt.branch.as_deref(), Some("parked/real"));
    assert!(
        dest.join("p.txt").exists(),
        "the branch's commits are checked out"
    );
    assert_eq!(
        git(
            &f.repo,
            &["for-each-ref", "refs/heads", "--format=%(refname:short)"]
        ),
        "main\nparked/real"
    );

    // Checked out elsewhere → git's refusal is surfaced.
    let err = add_existing(&omp, &f.repo, "parked/real", &sandbox.join("wt/third")).unwrap_err();
    assert!(
        matches!(err, WorktreeError::Omp { ref stderr } if !stderr.is_empty()),
        "{err:?}"
    );
}

/// `origin` bare repo with `main` pushed and set as upstream.
fn with_origin(f: &Fixture) {
    let origin = f.root.join("origin.git");
    git(
        &f.root,
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    git(
        &f.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
}

#[test]
fn park_removes_a_clean_pushed_unmerged_worktree_and_keeps_its_branch() {
    let f = fixture();
    with_origin(&f);
    let wt = linked(&f, "pk", "wt/pk");
    commit(&wt.path, "pk.txt", "work");
    git(&wt.path, &["push", "-q", "-u", "origin", "wt/pk"]);

    let s = status(&wt, "main").unwrap();
    assert!(s.pushed && !s.merged && !s.dirty, "{s:?}");
    assert_eq!(s.upstream.as_deref(), Some("origin/wt/pk"));
    assert!(park_blockers(&s).is_empty());
    // The merged path still refuses: parking is not a way around NotMerged.
    assert!(matches!(
        remove(&wt, "main", DELETE),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::NotMerged { ahead: 1 }]
    ));

    let done = remove(&wt, "main", Removal::Park).unwrap();
    assert_eq!(
        done,
        Removed {
            branch_deleted: false,
            branch_note: None
        }
    );
    assert!(!wt.path.exists());
    assert!(git(&f.repo, &["branch", "--list", "wt/pk"]).contains("wt/pk"));
}

#[test]
fn park_uses_the_remote_branch_of_the_same_name_when_no_upstream_is_configured() {
    let f = fixture();
    with_origin(&f);
    let wt = linked(&f, "np", "wt/np");
    commit(&wt.path, "np.txt", "work");
    git(&wt.path, &["push", "-q", "origin", "wt/np"]);
    let s = status(&wt, "main").unwrap();
    assert!(s.pushed, "{s:?}");
    assert_eq!(s.upstream.as_deref(), Some("origin/wt/np"));
    commit(&wt.path, "np2.txt", "more");
    assert!(!status(&wt, "main").unwrap().pushed);
    git(&wt.path, &["push", "-q", "origin", "wt/np"]);
    remove(&wt, "main", Removal::Park).unwrap();
}

#[test]
fn park_is_refused_for_unpushed_commits_missing_upstream_dirty_and_detached() {
    let f = fixture();
    with_origin(&f);

    // Pushed, then one more local commit.
    let wt = linked(&f, "up", "wt/up");
    commit(&wt.path, "1.txt", "1");
    git(&wt.path, &["push", "-q", "-u", "origin", "wt/up"]);
    commit(&wt.path, "2.txt", "2");
    let s = status(&wt, "main").unwrap();
    assert!(
        !s.pushed && s.upstream.as_deref() == Some("origin/wt/up"),
        "{s:?}"
    );
    assert_eq!(park_blockers(&s).len(), 1);
    assert!(matches!(
        remove(&wt, "main", Removal::Park),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::NotPushed]
    ));
    assert!(wt.path.exists());

    // Never pushed: no upstream at all.
    let np = linked(&f, "nu", "wt/nu");
    commit(&np.path, "n.txt", "n");
    let s = status(&np, "main").unwrap();
    assert!(!s.pushed && s.upstream.is_none(), "{s:?}");
    assert!(matches!(
        remove(&np, "main", Removal::Park),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::NotPushed]
    ));

    // Pushed but dirty.
    let dirty = linked(&f, "dy", "wt/dy");
    commit(&dirty.path, "d.txt", "d");
    git(&dirty.path, &["push", "-q", "-u", "origin", "wt/dy"]);
    fs::write(dirty.path.join("wip.txt"), "wip").unwrap();
    assert!(matches!(
        remove(&dirty, "main", Removal::Park),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::Dirty]
    ));
    assert!(dirty.path.join("wip.txt").exists());

    // Detached.
    git(&np.path, &["checkout", "-q", "--detach"]);
    let det = resolve(&np.path).unwrap();
    assert!(matches!(
        remove(&det, "main", Removal::Park),
        Err(WorktreeError::Unsafe(r)) if r == vec![Unsafe::Detached]
    ));
    assert!(np.path.exists());
}

#[test]
fn switch_branch_refuses_a_dirty_checkout_and_switches_a_clean_one() {
    let f = fixture();
    git(&f.repo, &["branch", "parked"]);
    fs::write(f.repo.join("wip.txt"), "wip").unwrap();
    assert!(matches!(
        switch_branch(&f.repo, "parked"),
        Err(WorktreeError::DirtyCheckout(d)) if d == f.repo
    ));
    assert_eq!(git(&f.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");

    fs::remove_file(f.repo.join("wip.txt")).unwrap();
    switch_branch(&f.repo, "parked").unwrap();
    assert_eq!(
        git(&f.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "parked"
    );

    // A branch that does not exist is not guessed from a remote or created.
    assert!(matches!(
        switch_branch(&f.repo, "nope"),
        Err(WorktreeError::Git { .. })
    ));
    assert!(matches!(
        switch_branch(&f.repo, "--detach"),
        Err(WorktreeError::Git { .. })
    ));
    assert_eq!(
        git(&f.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "parked"
    );
}

#[test]
fn branch_state_reports_existence_checkout_and_squash_merge() {
    let f = fixture();
    let wt = squash_merged(&f, "bs", "bs-branch");
    assert_eq!(
        branch_state(&f.repo, "bs-branch", "main").unwrap(),
        BranchState {
            exists: true,
            checked_out: Some(wt.path.clone()),
            merged: true
        }
    );

    git(&f.repo, &["worktree", "remove", wt.path.to_str().unwrap()]);
    assert_eq!(
        branch_state(&f.repo, "bs-branch", "main").unwrap(),
        BranchState {
            exists: true,
            checked_out: None,
            merged: true
        }
    );

    // Unmerged work, checked out in the primary.
    git(&f.repo, &["checkout", "-q", "-b", "open"]);
    commit(&f.repo, "o.txt", "o");
    let s = branch_state(&f.repo, "open", "main").unwrap();
    assert_eq!(
        (s.exists, s.merged, s.checked_out),
        (true, false, Some(f.repo.clone()))
    );

    git(&f.repo, &["checkout", "-q", "main"]);
    git(&f.repo, &["branch", "-D", "bs-branch"]);
    assert_eq!(
        branch_state(&f.repo, "bs-branch", "main").unwrap(),
        BranchState {
            exists: false,
            checked_out: None,
            merged: false
        }
    );
}

/// A fake omp that records its arguments and `PI_CONFIG_FILES`, then adds the worktree itself.
#[cfg(unix)]
#[test]
fn add_existing_runs_omp_worktree_add_without_dash_b_and_checks_the_branch_out() {
    // The script reaches its executed inode through `install` (another process), so no
    // sibling test's fork can inherit a write fd to it (exec would fail with ETXTBSY).
    let f = fixture();
    let parked = linked(&f, "pa", "wt/parked");
    commit(&parked.path, "pa.txt", "work");
    git(
        &f.repo,
        &["worktree", "remove", parked.path.to_str().unwrap()],
    );

    let log = f.root.join("args.log");
    let fake = f.root.join("omp");
    let stage = f.root.join("omp.src");
    fs::write(
        &stage,
        format!(
            "#!/bin/sh\necho \"$@\" > '{log}'\necho \"$PI_CONFIG_FILES\" >> '{log}'\nexec git -C \"$5\" worktree add -q \"$6\" \"$7\"\n",
            log = log.display()
        ),
    )
    .unwrap();
    let installed = Command::new("install")
        .args(["-m", "755"])
        .arg(&stage)
        .arg(&fake)
        .status()
        .unwrap();
    assert!(installed.success());
    let omp = Omp::at(&fake).with_env("XDG_DATA_HOME", f.root.join("data"));

    let dest = f.root.join("again");
    add_existing(&omp, &f.repo, "wt/parked", &dest).unwrap();
    let wt = resolve(&dest).expect("linked worktree");
    assert_eq!(wt.branch.as_deref(), Some("wt/parked"));
    assert!(dest.join("pa.txt").exists());

    let recorded = fs::read_to_string(&log).unwrap();
    let mut lines = recorded.lines();
    assert_eq!(
        lines.next().unwrap(),
        format!(
            "worktree add -q -C {} {} wt/parked",
            f.repo.display(),
            dest.display()
        )
    );
    assert!(lines.next().unwrap().ends_with("worktree-overlay.yml"));

    // Option-like branch and relative destination never reach omp.
    assert!(matches!(
        add_existing(&omp, &f.repo, "--detach", &f.root.join("x")),
        Err(WorktreeError::Git { .. })
    ));
    assert!(matches!(
        add_existing(&omp, &f.repo, "wt/parked", Path::new("rel")),
        Err(WorktreeError::Io(_))
    ));
}
