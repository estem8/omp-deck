#![cfg(unix)]
//! ctl directory bounds/fallback, `prepare_ctl_dir`, `prune_ctl_dirs`, and `install_extension` hardening.
//! Env-dependent assertions live in ONE test so the process-wide environment is never raced.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
use std::path::Path;

use deck_core::ctl::{
    EXTENSION_SOURCE, ctl_dir, install_extension, prepare_ctl_dir, prune_ctl_dirs,
};

fn mode(p: &Path) -> u32 {
    fs::symlink_metadata(p).unwrap().mode() & 0o777
}

fn private_dir(p: &Path) {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
        .unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn ctl_dir_is_bounded_hashed_and_never_a_predictable_tmp_path() {
    let tmp = tempfile::tempdir().unwrap();
    let rt = tmp.path().join("run");
    let cache = tmp.path().join("cache");
    private_dir(&rt);
    private_dir(&cache);
    // SAFETY: the only test in this binary touching the environment, and it runs alone.
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", &cache);
        std::env::set_var("XDG_RUNTIME_DIR", &rt);
    }

    // Valid runtime dir: used, with a 16-hex leaf, bounded no matter how long/odd the key.
    let long = "k".repeat(5000);
    for key in ["omp-1", "../../etc", "with/slash", "", long.as_str()] {
        let d = ctl_dir("ns", key);
        assert_eq!(
            d.parent().unwrap(),
            rt.join("omp-deck").join("ctl").join("ns"),
            "key {key:?}"
        );
        let leaf = d.file_name().unwrap().to_str().unwrap();
        assert_eq!(leaf.len(), 16);
        assert!(leaf.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(d.join("ctl.sock").as_os_str().len() < 108);
    }
    assert_eq!(ctl_dir("ns", "omp-1"), ctl_dir("ns", "omp-1"));
    assert_ne!(ctl_dir("ns", "omp-1"), ctl_dir("ns", "omp-2"));
    // Stable across builds: the hash is a fixed function of the key.
    assert_eq!(
        ctl_dir("ns", "omp-1").file_name().unwrap(),
        ctl_dir("ns", "omp-1").file_name().unwrap()
    );

    // Group/other-writable runtime dir -> cache fallback.
    fs::set_permissions(&rt, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(ctl_dir("ns", "omp-1").starts_with(cache.join("omp-deck").join("ctl")));
    fs::set_permissions(&rt, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(ctl_dir("ns", "omp-1").starts_with(rt.join("omp-deck").join("ctl")));

    // Relative, empty or missing runtime dir -> cache fallback, never <temp>/omp-deck-*.
    for bad in [
        "relative/dir".as_ref(),
        "".as_ref(),
        tmp.path().join("nope").as_path(),
    ] {
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", bad) };
        let d = ctl_dir("ns", "omp-1");
        assert!(
            d.starts_with(cache.join("omp-deck").join("ctl")),
            "{bad:?} -> {d:?}"
        );
        assert!(!d.starts_with(std::env::temp_dir().join("omp-deck-0")));
    }

    // prune_ctl_dirs works against the (fallback) root: removes dead leaves + their sock/lock,
    // keeps alive ones, ignores non-hex names and symlinks.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &rt) };
    let alive_dir = ctl_dir("ns", "alive");
    let dead_dir = ctl_dir("ns", "dead");
    let busy_dir = ctl_dir("ns", "busy");
    for d in [&alive_dir, &dead_dir, &busy_dir] {
        prepare_ctl_dir(d).unwrap();
    }
    let root = alive_dir.parent().unwrap().to_path_buf();
    drop(std::os::unix::net::UnixListener::bind(dead_dir.join("ctl.sock")).unwrap()); // leftover socket file, nobody listening
    fs::write(dead_dir.join("ctl.lock"), "999999999 1").unwrap();
    fs::write(busy_dir.join("unrelated"), "x").unwrap();
    fs::create_dir(root.join("not-a-leaf")).unwrap();
    let outside = tmp.path().join("outside");
    private_dir(&outside);
    fs::write(outside.join("keep"), "x").unwrap();
    symlink(&outside, root.join("0123456789abcdef")).unwrap();

    prune_ctl_dirs("ns", &HashSet::from(["alive".to_string()]));
    assert!(alive_dir.is_dir(), "alive leaf kept");
    assert!(!dead_dir.exists(), "dead leaf (with sock+lock) removed");
    assert!(
        busy_dir.exists(),
        "non-recursive: leaf with foreign content kept"
    );
    assert!(root.join("not-a-leaf").exists());
    assert!(
        outside.join("keep").exists(),
        "symlinked leaf never followed"
    );
    assert!(fs::symlink_metadata(root.join("0123456789abcdef")).is_ok());

    // Namespaces are isolated: another socket's prune never touches these leaves.
    let other = ctl_dir("other", "alive");
    assert_ne!(other.parent(), alive_dir.parent());
    prepare_ctl_dir(&other).unwrap();
    prune_ctl_dirs("ns", &HashSet::new());
    assert!(other.is_dir(), "other namespace untouched");
    assert!(!alive_dir.exists(), "own namespace pruned");
    prune_ctl_dirs("other", &HashSet::new());
    assert!(!other.exists());
    // Sanitized or over-long namespaces keep a readable prefix plus a hash of the full name.
    let comp = |ns: &str| -> String {
        ctl_dir(ns, "k")
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    };
    assert!(comp("a/b").starts_with("a_b-"));
    assert_ne!(comp("a/b"), comp("a_b"));
    assert_eq!(comp("a_b"), "a_b");
    assert!(comp("..").starts_with("_-"));
    let (la, lb) = (
        format!("{}1", "n".repeat(60)),
        format!("{}2", "n".repeat(60)),
    );
    assert_ne!(comp(&la), comp(&lb));
    assert!(comp(&la).len() <= 41);

    // A live lock owner (pid + matching start time) protects the leaf, even with no socket.
    let me = std::process::id();
    let start = deck_core::ctl::proc_start_time(me).expect("own start time");
    let held = ctl_dir("ns", "held");
    prepare_ctl_dir(&held).unwrap();
    fs::write(held.join("ctl.lock"), format!("{me} {start}")).unwrap();
    prune_ctl_dirs("ns", &HashSet::new());
    assert!(held.join("ctl.lock").exists(), "live lock owner kept");
    // Same pid, wrong start time = pid reuse: stale, removed.
    fs::write(held.join("ctl.lock"), format!("{me} {}", start + 1)).unwrap();
    prune_ctl_dirs("ns", &HashSet::new());
    assert!(!held.exists(), "reused pid is stale");

    // A socket that accepts connections protects the leaf even with a stale/missing lock.
    let served = ctl_dir("ns", "served");
    prepare_ctl_dir(&served).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(served.join("ctl.sock")).unwrap();
    prune_ctl_dirs("ns", &HashSet::new());
    assert!(served.join("ctl.sock").exists(), "listening socket kept");
    drop(listener); // leaves the socket file behind, nobody accepting
    prune_ctl_dirs("ns", &HashSet::new());
    assert!(!served.exists(), "dead socket file removed");

    // A symlinked root is refused: nothing outside is touched.
    let real = tmp.path().join("real-root");
    private_dir(&real.join("ns").join("0123456789abcdef"));
    fs::write(
        real.join("ns").join("0123456789abcdef").join("ctl.sock"),
        "x",
    )
    .unwrap();
    let ns_dir = ctl_dir("ns", "x").parent().unwrap().to_path_buf();
    let ctl_root = ns_dir.parent().unwrap().to_path_buf();
    fs::remove_dir_all(&ctl_root).unwrap();
    symlink(&real, &ctl_root).unwrap();
    prune_ctl_dirs("ns", &HashSet::new());
    assert!(
        real.join("ns")
            .join("0123456789abcdef")
            .join("ctl.sock")
            .exists(),
        "symlinked root never traversed"
    );
    assert!(prepare_ctl_dir(&ctl_dir("ns", "x")).is_err());
}

#[test]
fn prepare_creates_0700_components_and_rejects_unsafe_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("a").join("ctl").join("0123456789abcdef");
    prepare_ctl_dir(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(dir.parent().unwrap()), 0o700);
    prepare_ctl_dir(&dir).unwrap(); // idempotent

    // Leaf must be exactly 0700 (not merely non-writable).
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(prepare_ctl_dir(&dir).is_err());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(prepare_ctl_dir(&dir).is_err());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

    // Group/other-writable root rejected.
    let root = dir.parent().unwrap();
    fs::set_permissions(root, fs::Permissions::from_mode(0o722)).unwrap();
    assert!(prepare_ctl_dir(&dir).is_err());
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    prepare_ctl_dir(&dir).unwrap();

    // Symlinked leaf (even to a perfect 0700 dir) rejected.
    let real = tmp.path().join("real");
    private_dir(&real);
    let link = root.join("fedcba9876543210");
    symlink(&real, &link).unwrap();
    assert!(prepare_ctl_dir(&link).is_err());

    // Symlinked root rejected.
    let linked_root = tmp.path().join("linked-root");
    symlink(&real, &linked_root).unwrap();
    assert!(prepare_ctl_dir(&linked_root.join("0123456789abcdef")).is_err());

    // A non-directory leaf, a relative path and an over-long socket path are rejected.
    let file = root.join("0000000000000000");
    fs::write(&file, "x").unwrap();
    assert!(prepare_ctl_dir(&file).is_err());
    assert!(prepare_ctl_dir(Path::new("relative/0123456789abcdef")).is_err());
    let long = tmp.path().join("x".repeat(120));
    assert!(prepare_ctl_dir(&long).is_err());
    assert!(
        !long.exists(),
        "over-long path rejected before creating anything"
    );
}

#[test]
fn install_extension_enforces_file_properties_even_when_content_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let path = install_extension(tmp.path()).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), EXTENSION_SOURCE);
    assert_eq!(mode(&path), 0o600);

    // Right bytes, wrong mode -> repaired.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    install_extension(tmp.path()).unwrap();
    assert_eq!(mode(&path), 0o600);

    // Symlink with identical content -> replaced by a regular file; the target is untouched.
    let target = tmp.path().join("target.ts");
    fs::write(&target, EXTENSION_SOURCE).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(&path).unwrap();
    symlink(&target, &path).unwrap();
    install_extension(tmp.path()).unwrap();
    let m = fs::symlink_metadata(&path).unwrap();
    assert!(m.file_type().is_file());
    assert_eq!(m.mode() & 0o777, 0o600);
    assert_eq!(mode(&target), 0o644);

    // Different bytes -> rewritten; no temp files left behind.
    fs::write(&path, "// tampered").unwrap();
    install_extension(tmp.path()).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), EXTENSION_SOURCE);
    let names: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    // The symlink's target, the extension, and the advisor module it imports; nothing else.
    assert_eq!(names.len(), 3, "{names:?}");

    // A directory in the way is an error, not silently ignored.
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(install_extension(tmp.path()).is_err());
}
