use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use super::*;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    // The library's own git runs (restore, add, …) must see LF-only checkouts whatever the
    // machine's `core.autocrlf` is (windows-latest sets it to true system-wide).
    git(dir.path(), &["config", "core.autocrlf", "false"]);
    dir
}

fn write(dir: &Path, name: &str, text: &str) {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(p, text).expect("write");
}

fn commit_all(dir: &Path) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "c"]);
}

fn listed(dir: &Path) -> Changes {
    list(dir).expect("list").expect("inside a work tree")
}

fn find<'a>(c: &'a Changes, path: &str) -> &'a ChangedFile {
    c.files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path} not listed in {:?}", c.files))
}

#[test]
fn lists_every_kind_of_change() {
    let dir = init();
    let d = dir.path();
    for n in ["mod.txt", "staged.txt", "both.txt", "gone.txt"] {
        write(d, n, "one\ntwo\nthree\n");
    }
    write(
        d,
        "old-name.txt",
        "a rename needs enough shared content\nline 2\nline 3\nline 4\n",
    );
    commit_all(d);

    write(d, "mod.txt", "one\nTWO\nthree\n");
    write(d, "staged.txt", "one\nstaged\nthree\n");
    git(d, &["add", "staged.txt"]);
    write(d, "both.txt", "one\nindex\nthree\n");
    git(d, &["add", "both.txt"]);
    write(d, "both.txt", "one\nindex\nworktree\n");
    fs::remove_file(d.join("gone.txt")).expect("rm");
    write(d, "added.txt", "new\n");
    git(d, &["add", "added.txt"]);
    git(d, &["mv", "old-name.txt", "new-name.txt"]);
    write(d, "plain.txt", "untracked\n");
    write(d, "dir/inner/x.txt", "x\n");

    let c = listed(d);
    assert_eq!(c.branch.as_deref(), Some("main"));
    assert_eq!(
        c.root.canonicalize().expect("root"),
        d.canonicalize().expect("d")
    );

    let m = find(&c, "mod.txt");
    assert_eq!((m.staged, m.unstaged), (None, Some(Change::Modified)));
    let s = find(&c, "staged.txt");
    assert_eq!((s.staged, s.unstaged), (Some(Change::Modified), None));
    let b = find(&c, "both.txt");
    assert_eq!(
        (b.staged, b.unstaged),
        (Some(Change::Modified), Some(Change::Modified))
    );
    let g = find(&c, "gone.txt");
    assert_eq!((g.staged, g.unstaged), (None, Some(Change::Deleted)));
    let a = find(&c, "added.txt");
    assert_eq!((a.staged, a.unstaged), (Some(Change::Added), None));
    let r = find(&c, "new-name.txt");
    assert_eq!(r.staged, Some(Change::Renamed));
    assert_eq!(r.orig_path.as_deref(), Some("old-name.txt"));
    assert!(!c.files.iter().any(|f| f.path == "old-name.txt"));
    let u = find(&c, "plain.txt");
    assert!(u.untracked && u.staged.is_none() && !u.is_dir);
    let dd = find(&c, "dir/");
    assert!(dd.untracked && dd.is_dir);
    assert!(c.files.iter().all(|f| !f.conflicted));
}

#[test]
fn reports_a_real_merge_conflict() {
    let dir = init();
    let d = dir.path();
    write(d, "f.txt", "base\n");
    commit_all(d);
    git(d, &["checkout", "-q", "-b", "other"]);
    write(d, "f.txt", "other\n");
    commit_all(d);
    git(d, &["checkout", "-q", "main"]);
    write(d, "f.txt", "main\n");
    commit_all(d);
    let merge = Command::new("git")
        .arg("-C")
        .arg(d)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "merge",
            "other",
        ])
        .output()
        .expect("git");
    assert!(!merge.status.success(), "the merge must conflict");

    let c = listed(d);
    let f = find(&c, "f.txt");
    assert!(f.conflicted);
    assert_eq!((f.staged, f.unstaged), (None, None));
    assert!(matches!(discard(d, &[f]), Err(ChangesError::Conflicted)));
    assert!(
        fs::read_to_string(d.join("f.txt"))
            .expect("read")
            .contains("<<<<<<<")
    );
    // Staging marks it resolved.
    write(d, "f.txt", "resolved\n");
    stage(d, &[f]).expect("stage");
    assert!(!find(&listed(d), "f.txt").conflicted);
}

#[test]
fn awkward_paths_are_exact() {
    let dir = init();
    let d = dir.path();
    write(d, "keep.txt", "k\n");
    commit_all(d);
    let mut names = vec![
        "with space.txt",
        "ünï-cödé.txt",
        "-dash.txt",
        "multi word/ü ö.txt",
    ];
    if cfg!(unix) {
        names.push("star*.txt");
        names.push("br[a]ck.txt");
    }
    for n in &names {
        write(d, n, "x\n");
    }
    let c = listed(d);
    for n in names {
        let top = n.split('/').next().expect("segment");
        let expect = if top == n {
            n.to_owned()
        } else {
            format!("{top}/")
        };
        let f = find(&c, &expect);
        assert_eq!(f.os_path, OsString::from(&expect));
        assert!(f.untracked);
    }
}

#[test]
fn outside_a_work_tree_is_none_and_subdirs_resolve_the_root() {
    let plain = tempfile::tempdir().expect("tempdir");
    assert!(list(plain.path()).expect("list").is_none());
    let dir = init();
    write(dir.path(), "sub/a.txt", "a\n");
    let c = list(&dir.path().join("sub")).expect("list").expect("repo");
    assert_eq!(
        c.root.canonicalize().expect("root"),
        dir.path().canonicalize().expect("d")
    );
    assert_eq!(c.files.len(), 1);
    assert_eq!(c.files[0].path, "sub/");
}

#[test]
fn unborn_head_lists_staged_additions() {
    let dir = init();
    let d = dir.path();
    write(d, "a.txt", "a\n");
    git(d, &["add", "a.txt"]);
    let c = listed(d);
    assert_eq!(find(&c, "a.txt").staged, Some(Change::Added));
}

#[test]
fn staged_and_unstaged_diffs_are_separate() {
    let dir = init();
    let d = dir.path();
    write(d, "f.txt", "one\ntwo\nthree\n");
    commit_all(d);
    write(d, "f.txt", "one\nindex\nthree\nfour\n");
    git(d, &["add", "f.txt"]);
    write(d, "f.txt", "one\nindex\nthree\nfour\nworktree\n");
    let c = listed(d);
    let f = find(&c, "f.txt");

    let staged = diff(d, f, DiffSide::Staged).expect("staged");
    assert!(
        staged.text.starts_with("diff --git a/f.txt b/f.txt"),
        "{}",
        staged.text
    );
    assert!(staged.text.contains("-two") && staged.text.contains("+index"));
    assert!(!staged.text.contains("worktree"));
    assert_eq!((staged.added, staged.removed), (2, 1));
    assert!(!staged.binary && !staged.truncated);

    let unstaged = diff(d, f, DiffSide::Unstaged).expect("unstaged");
    assert!(unstaged.text.contains("+worktree") && !unstaged.text.contains("+index"));
    assert_eq!((unstaged.added, unstaged.removed), (1, 0));
}

#[test]
fn staged_rename_diff_pairs_both_names() {
    let dir = init();
    let d = dir.path();
    write(d, "old.txt", "alpha\nbeta\ngamma\ndelta\nepsilon\n");
    commit_all(d);
    git(d, &["mv", "old.txt", "new.txt"]);
    let c = listed(d);
    let f = find(&c, "new.txt");
    let diff = diff(d, f, DiffSide::Staged).expect("diff");
    assert!(diff.text.contains("rename from old.txt") && diff.text.contains("rename to new.txt"));
    assert_eq!((diff.added, diff.removed), (0, 0));
}

#[cfg(unix)]
#[test]
fn mode_only_change_has_empty_text_and_is_not_binary() {
    use std::os::unix::fs::PermissionsExt;
    let dir = init();
    let d = dir.path();
    write(d, "run.sh", "echo\n");
    commit_all(d);
    fs::set_permissions(d.join("run.sh"), fs::Permissions::from_mode(0o755)).expect("chmod");
    git(d, &["config", "core.fileMode", "true"]);
    let c = listed(d);
    let f = find(&c, "run.sh");
    let diff = diff(d, f, DiffSide::Unstaged).expect("diff");
    assert!(!diff.binary);
    assert_eq!((diff.added, diff.removed), (0, 0));
    assert!(diff.text.contains("old mode 100644"));
}

#[test]
fn binary_tracked_and_untracked_files_are_flagged() {
    let dir = init();
    let d = dir.path();
    fs::write(d.join("b.bin"), [0u8, 1, 2, 3]).expect("write");
    commit_all(d);
    fs::write(d.join("b.bin"), [0u8, 9, 9, 9, 9]).expect("write");
    fs::write(d.join("new.bin"), [1u8, 0, 2]).expect("write");
    let c = listed(d);
    let tracked = diff(d, find(&c, "b.bin"), DiffSide::Unstaged).expect("diff");
    assert!(tracked.binary && tracked.text.is_empty());
    let untracked = diff(d, find(&c, "new.bin"), DiffSide::Untracked).expect("diff");
    assert!(untracked.binary && untracked.text.is_empty());
}

#[test]
fn untracked_file_diff_is_synthesized() {
    let dir = init();
    let d = dir.path();
    write(d, "n.txt", "a\nb\nc");
    let c = listed(d);
    let diff = diff(d, find(&c, "n.txt"), DiffSide::Untracked).expect("diff");
    assert_eq!(
        diff.text,
        "diff --git a/n.txt b/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1,3 @@\n+a\n+b\n+c\n\\ No newline at end of file\n"
    );
    assert_eq!((diff.added, diff.removed), (3, 0));
    write(d, "empty.txt", "");
    let c = listed(d);
    let empty = super::diff(d, find(&c, "empty.txt"), DiffSide::Untracked).expect("diff");
    assert_eq!(
        empty.text,
        "diff --git a/empty.txt b/empty.txt\nnew file mode 100644\n"
    );
    assert_eq!(empty.added, 0);
}

#[test]
fn untracked_directory_diff_covers_its_files_and_skips_ignored() {
    let dir = init();
    let d = dir.path();
    write(d, ".gitignore", "*.log\n");
    commit_all(d);
    write(d, "newdir/a.txt", "A\n");
    write(d, "newdir/sub/b.txt", "B1\nB2\n");
    write(d, "newdir/skip.log", "ignored\n");
    fs::write(d.join("newdir/c.bin"), [0u8, 0]).expect("write");
    let c = listed(d);
    let f = find(&c, "newdir/");
    let diff = diff(d, f, DiffSide::Untracked).expect("diff");
    assert!(
        diff.text.contains("+++ b/newdir/a.txt") && diff.text.contains("+++ b/newdir/sub/b.txt")
    );
    assert!(
        diff.text
            .contains("Binary files /dev/null and b/newdir/c.bin differ")
    );
    assert!(!diff.text.contains("ignored"));
    assert_eq!(diff.added, 3);
    assert!(!diff.binary);
}

#[test]
fn oversized_diffs_are_truncated_at_line_boundaries() {
    let dir = init();
    let d = dir.path();
    // Too many lines.
    let many: String = (0..30_000).map(|i| format!("l{i}\n")).collect();
    write(d, "many.txt", "x\n");
    commit_all(d);
    write(d, "many.txt", &many);
    // Too many bytes (3 MiB of long lines), untracked.
    let long = format!("{}\n", "y".repeat(1000)).repeat(3 * 1024);
    write(d, "huge.txt", &long);
    let c = listed(d);

    let tracked = diff(d, find(&c, "many.txt"), DiffSide::Unstaged).expect("diff");
    assert!(tracked.truncated);
    assert_eq!(tracked.text.lines().count(), MAX_DIFF_LINES);
    assert!(tracked.text.ends_with('\n'));

    let untracked = diff(d, find(&c, "huge.txt"), DiffSide::Untracked).expect("diff");
    assert!(untracked.truncated && !untracked.binary);
    // One `+` per kept line is added on top of the 2 MiB of content.
    assert!(untracked.text.len() <= MAX_DIFF_BYTES + 10_000);
    assert!(untracked.text.ends_with('\n'));
    // The synthesized hunk header matches the lines actually kept.
    let kept = untracked
        .text
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    assert_eq!(kept as u32, untracked.added);
    assert!(untracked.text.contains(&format!("@@ -0,0 +1,{kept} @@")));
}

#[test]
fn stage_and_unstage_move_changes_between_sides() {
    let dir = init();
    let d = dir.path();
    write(d, "a.txt", "a\n");
    write(d, "b.txt", "b\n");
    commit_all(d);
    write(d, "a.txt", "a2\n");
    write(d, "b.txt", "b2\n");
    write(d, "new.txt", "n\n");
    let c = listed(d);
    stage(d, &[find(&c, "a.txt"), find(&c, "new.txt")]).expect("stage");
    let c = listed(d);
    assert_eq!(find(&c, "a.txt").staged, Some(Change::Modified));
    assert_eq!(find(&c, "a.txt").unstaged, None);
    assert_eq!(find(&c, "new.txt").staged, Some(Change::Added));
    assert_eq!(find(&c, "b.txt").staged, None);

    unstage(d, &[find(&c, "a.txt"), find(&c, "new.txt")]).expect("unstage");
    let c = listed(d);
    assert_eq!(find(&c, "a.txt").staged, None);
    assert_eq!(find(&c, "a.txt").unstaged, Some(Change::Modified));
    assert!(find(&c, "new.txt").untracked);
}

#[test]
fn unstaging_a_rename_restores_both_sides() {
    let dir = init();
    let d = dir.path();
    write(d, "old.txt", "alpha\nbeta\ngamma\ndelta\n");
    commit_all(d);
    git(d, &["mv", "old.txt", "new.txt"]);
    let c = listed(d);
    unstage(d, &[find(&c, "new.txt")]).expect("unstage");
    let c = listed(d);
    assert_eq!(find(&c, "old.txt").unstaged, Some(Change::Deleted));
    assert!(find(&c, "new.txt").untracked);
}

#[test]
fn unstage_works_before_the_first_commit() {
    let dir = init();
    let d = dir.path();
    write(d, "a.txt", "a\n");
    write(d, "b.txt", "b\n");
    git(d, &["add", "."]);
    let c = listed(d);
    unstage(d, &[find(&c, "a.txt")]).expect("unstage on unborn HEAD");
    let c = listed(d);
    assert!(find(&c, "a.txt").untracked);
    assert_eq!(find(&c, "b.txt").staged, Some(Change::Added));
    assert!(d.join("a.txt").exists(), "the file itself stays");
}

#[test]
fn discard_restores_tracked_files_only() {
    let dir = init();
    let d = dir.path();
    write(d, "a.txt", "a\n");
    write(d, "gone.txt", "g\n");
    write(d, "staged.txt", "s\n");
    commit_all(d);
    write(d, "a.txt", "changed\n");
    fs::remove_file(d.join("gone.txt")).expect("rm");
    write(d, "staged.txt", "s2\n");
    git(d, &["add", "staged.txt"]);
    write(d, "untracked.txt", "u\n");
    let c = listed(d);
    let all: Vec<&ChangedFile> = c.files.iter().collect();
    discard(d, &all).expect("discard");
    assert_eq!(fs::read_to_string(d.join("a.txt")).expect("a"), "a\n");
    assert_eq!(fs::read_to_string(d.join("gone.txt")).expect("gone"), "g\n");
    assert_eq!(
        fs::read_to_string(d.join("staged.txt")).expect("staged"),
        "s2\n",
        "staged work untouched"
    );
    assert!(d.join("untracked.txt").exists());
}

#[test]
fn discard_leaves_intent_to_add_files_intact() {
    let dir = init();
    let d = dir.path();
    write(d, "keep.txt", "k\n");
    commit_all(d);
    write(d, "ita.txt", "precious\n");
    git(d, &["add", "-N", "ita.txt"]);
    let c = listed(d);
    let f = find(&c, "ita.txt");
    assert_eq!(f.unstaged, Some(Change::Added));
    discard(d, &[f]).expect("discard");
    assert_eq!(
        fs::read_to_string(d.join("ita.txt")).expect("read"),
        "precious\n"
    );
}

#[test]
fn pathspec_magic_in_names_only_touches_that_file() {
    if !cfg!(unix) {
        return;
    }
    let dir = init();
    let d = dir.path();
    write(d, "*", "star\n");
    write(d, "a.txt", "a\n");
    write(d, ":(exclude)a.txt", "magic\n");
    commit_all(d);
    write(d, "*", "star2\n");
    write(d, "a.txt", "a2\n");
    write(d, ":(exclude)a.txt", "magic2\n");
    let c = listed(d);

    stage(d, &[find(&c, "*")]).expect("stage");
    let c = listed(d);
    assert_eq!(find(&c, "*").staged, Some(Change::Modified));
    assert_eq!(
        find(&c, "a.txt").staged,
        None,
        "a glob must not stage a.txt"
    );

    discard(d, &[find(&c, ":(exclude)a.txt")]).expect("discard magic name");
    assert_eq!(
        fs::read_to_string(d.join(":(exclude)a.txt")).expect("read"),
        "magic\n"
    );
    assert_eq!(fs::read_to_string(d.join("a.txt")).expect("read"), "a2\n");

    let c = listed(d);
    unstage(d, &[find(&c, "*")]).expect("unstage");
    let c = listed(d);
    assert_eq!(find(&c, "*").unstaged, Some(Change::Modified));
}

#[test]
fn leading_dash_names_are_operands_not_options() {
    let dir = init();
    let d = dir.path();
    write(d, "-f", "x\n");
    write(d, "other.txt", "o\n");
    let c = listed(d);
    stage(d, &[find(&c, "-f")]).expect("stage");
    let c = listed(d);
    assert_eq!(find(&c, "-f").staged, Some(Change::Added));
    assert!(find(&c, "other.txt").untracked);
    unstage(d, &[find(&c, "-f")]).expect("unstage");
    let c = listed(d);
    delete_untracked(d, &[find(&c, "-f")]).expect("delete");
    assert!(!d.join("-f").exists());
    assert!(d.join("other.txt").exists());
}

#[test]
fn delete_untracked_removes_files_and_dirs_but_never_ignored_files() {
    let dir = init();
    let d = dir.path();
    write(d, ".gitignore", "*.log\n");
    write(d, "tracked.txt", "t\n");
    commit_all(d);
    write(d, "junk.txt", "j\n");
    write(d, "keep-untracked.txt", "k\n");
    write(d, "dir/x.txt", "x\n");
    write(d, "dir/y.log", "ignored\n");
    write(d, "space dir/ü.txt", "u\n");
    let c = listed(d);
    delete_untracked(
        d,
        &[
            find(&c, "junk.txt"),
            find(&c, "dir/"),
            find(&c, "space dir/"),
        ],
    )
    .expect("delete");
    assert!(!d.join("junk.txt").exists());
    assert!(!d.join("dir/x.txt").exists());
    assert_eq!(
        fs::read_to_string(d.join("dir/y.log")).expect("ignored"),
        "ignored\n",
        "no -x"
    );
    assert!(!d.join("space dir").exists());
    assert!(d.join("keep-untracked.txt").exists());
    assert!(d.join("tracked.txt").exists());
}

#[test]
fn delete_untracked_refuses_tracked_files_and_nested_repositories() {
    let dir = init();
    let d = dir.path();
    write(d, "tracked.txt", "t\n");
    commit_all(d);
    write(d, "tracked.txt", "t2\n");
    let c = listed(d);
    let tracked = find(&c, "tracked.txt").clone();
    let err = delete_untracked(d, &[&tracked]).expect_err("tracked");
    assert!(matches!(err, ChangesError::Git { .. }));
    assert!(d.join("tracked.txt").exists());

    // A nested repository: `git clean -fd` skips it, so the dry run does not list it.
    let nested = d.join("nested");
    fs::create_dir_all(&nested).expect("mkdir");
    git(&nested, &["init", "-q", "-b", "main"]);
    write(&nested, "f.txt", "f\n");
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "-q", "-m", "n"]);
    let c = listed(d);
    let n = find(&c, "nested/");
    let err = delete_untracked(d, &[n]).expect_err("nested repo");
    assert!(matches!(err, ChangesError::Git { .. }), "{err}");
    assert!(nested.join("f.txt").exists());
}

#[test]
fn writes_refuse_a_root_that_is_not_the_top_level() {
    let dir = init();
    let d = dir.path();
    write(d, "sub/a.txt", "a\n");
    write(d, "sub/b.txt", "b\n");
    let c = listed(d);
    let f = find(&c, "sub/").clone();
    let sub = d.join("sub");
    assert!(matches!(stage(&sub, &[&f]), Err(ChangesError::RootChanged)));
    assert!(matches!(
        delete_untracked(&sub, &[&f]),
        Err(ChangesError::RootChanged)
    ));
    assert!(sub.join("a.txt").exists());

    let elsewhere = tempfile::tempdir().expect("tempdir");
    assert!(matches!(
        stage(elsewhere.path(), &[&f]),
        Err(ChangesError::RootChanged)
    ));
    let gone = elsewhere.path().join("missing");
    assert!(matches!(
        unstage(&gone, &[&f]),
        Err(ChangesError::RootChanged)
    ));
}

#[test]
fn operands_outside_the_work_tree_are_rejected() {
    let dir = init();
    let d = dir.path();
    write(d, "a.txt", "a\n");
    let mut f = find(&listed(d), "a.txt").clone();
    f.os_path = OsString::from("../escape.txt");
    assert!(matches!(stage(d, &[&f]), Err(ChangesError::Git { .. })));
    f.os_path = d.join("a.txt").into_os_string();
    assert!(matches!(
        delete_untracked(d, &[&f]),
        Err(ChangesError::Git { .. })
    ));
    assert!(d.join("a.txt").exists());
}

#[test]
fn patch_combines_staged_and_unstaged_and_works_before_the_first_commit() {
    let dir = init();
    let d = dir.path();
    write(d, "a.txt", "a\n");
    git(d, &["add", "a.txt"]);
    let unborn = patch(d).expect("patch on unborn HEAD");
    assert!(unborn.contains("+++ b/a.txt") && unborn.contains("+a"));

    commit_all(d);
    write(d, "a.txt", "a\nstaged\n");
    git(d, &["add", "a.txt"]);
    write(d, "a.txt", "a\nstaged\nunstaged\n");
    write(d, "b.txt", "untracked\n");
    let p = patch(d).expect("patch");
    assert!(p.contains("+staged") && p.contains("+unstaged"));
    assert!(!p.contains("untracked"));
}

#[test]
fn status_parser_handles_all_record_kinds() {
    let raw = b"# branch.oid abc\0# branch.head feature/x y\0\
1 .M N... 100644 100644 100644 aaa bbb dir/with space.txt\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new name.txt\0old name.txt\0\
u UU N... 100644 100644 100644 100644 a b c conflict.txt\0\
? plain.txt\0? untracked dir/\0garbage\0";
    let (branch, files) = parse_status(raw);
    assert_eq!(branch.as_deref(), Some("feature/x y"));
    assert_eq!(files.len(), 5);
    assert_eq!(files[0].path, "dir/with space.txt");
    assert_eq!(files[1].orig_path.as_deref(), Some("old name.txt"));
    assert_eq!(files[1].staged, Some(Change::Renamed));
    assert!(files[2].conflicted);
    assert!(files[3].untracked && !files[3].is_dir);
    assert!(files[4].is_dir && files[4].untracked);
    let (detached, _) = parse_status(b"# branch.head (detached)\0");
    assert_eq!(detached, None);
}

#[test]
fn c_quoted_paths_are_decoded() {
    assert_eq!(unquote_c("plain"), "plain");
    assert_eq!(unquote_c(r#""a\tb\"c\\d""#), "a\tb\"c\\d");
    assert_eq!(unquote_c(r#""\303\251.txt""#), "é.txt");
}

#[test]
fn untracked_directory_diff_shares_one_byte_budget_across_files() {
    let dir = init();
    let d = dir.path();
    write(d, "keep.txt", "k\n");
    commit_all(d);
    let line = format!("{}\n", "z".repeat(99));
    let file = line.repeat(1_500 * 1024 / 100); // ~1.5 MiB
    for n in ["a.txt", "b.txt", "c.txt"] {
        write(d, &format!("big/{n}"), &file);
    }
    fs::write(d.join("big/d.bin"), [0u8, 1]).expect("write");
    let c = listed(d);
    let diff = diff(d, find(&c, "big/"), DiffSide::Untracked).expect("diff");
    assert!(diff.truncated);
    assert!(diff.text.len() <= MAX_DIFF_BYTES, "{}", diff.text.len());
    assert!(diff.text.ends_with('\n'));
    assert!(diff.text.contains("+++ b/big/a.txt"));
}

/// A repository's `core.fsmonitor` names a program git would run on status and diff; OmpDeck
/// browses arbitrary folders, so none of its read-only git runs may execute it.
#[cfg(unix)]
#[test]
fn repository_fsmonitor_hook_never_runs() {
    // The hook reaches its executed inode through `install` (another process), so no sibling
    // test's fork can inherit a write fd to it (git's exec would fail with ETXTBSY).

    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let dir = init();
    let d = dir.path();
    write(d, "f.txt", "one\n");
    commit_all(d);
    let marker = d.join(".git/fsmonitor-ran");
    let hook = d.join(".git/fsmonitor-hook");
    let stage = d.join(".git/fsmonitor-hook.src");
    fs::write(
        &stage,
        format!("#!/bin/sh\necho ran >> '{}'\nexit 1\n", marker.display()),
    )
    .expect("write hook");
    let installed = Command::new("install")
        .args(["-m", "755"])
        .arg(&stage)
        .arg(&hook)
        .status()
        .expect("install hook");
    assert!(installed.success());
    git(d, &["config", "core.fsmonitor", &hook.to_string_lossy()]);
    write(d, "f.txt", "two\n");

    // Control: plain git does run the hook (else this git cannot show the difference).
    let _ = Command::new("git")
        .arg("-C")
        .arg(d)
        .args(["status", "--porcelain"])
        .output()
        .expect("git runs");
    if !marker.exists() {
        return;
    }
    fs::remove_file(&marker).expect("rm marker");

    let c = listed(d);
    let f = find(&c, "f.txt");
    let unstaged = diff(d, f, DiffSide::Unstaged).expect("diff");
    assert!(unstaged.text.contains("+two"), "{}", unstaged.text);
    patch(d).expect("patch");
    let info = crate::status::git_info(d).expect("status");
    assert_eq!(info.unstaged, 1);
    crate::worktree::find_merge_target(d, "main").expect("worktree list");
    assert!(
        !marker.exists(),
        "the repository's fsmonitor hook ran: {:?}",
        fs::read_to_string(&marker)
    );
}
