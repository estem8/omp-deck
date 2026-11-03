//! Model favorites and provider filtering use the gateway's real settings writer and wire path.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_core::config::{AppConfig, GatewayMode};
use deck_service::gateway::Profile;
use deck_service::gateway::client::{GatewayClient, GatewayOptions};
use deck_service::gateway::protocol::Role;
use deck_service::settings::{Edit, Snapshot};

const WAIT: Duration = Duration::from_secs(20);

struct Owner(Child);

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn owner(profile: &Profile) -> Owner {
    Owner(
        Command::new(env!("CARGO_BIN_EXE_omp-deck-gateway"))
            .args(["serve", "--foreground", "--ns", &profile.host_namespace])
            .arg("--settings")
            .arg(&profile.settings)
            .arg("--sessions-root")
            .arg(&profile.sessions_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start private gateway"),
    )
}

fn client(profile: &Profile) -> GatewayClient {
    let mut opts = GatewayOptions::new(profile.clone(), Role::Gui, Arc::new(|| {}));
    opts.autostart = false;
    GatewayClient::start(opts)
}

fn snapshot(client: &GatewayClient) -> Snapshot {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(snap) = client.settings().snapshot() {
            return snap;
        }
        assert!(Instant::now() < end, "gateway settings did not arrive");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn isolate(root: &Path) {
    // This integration-test binary contains one test; set the environment before starting threads.
    for (key, dir) in [
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("APPDATA", "home/AppData/Roaming"),
        ("LOCALAPPDATA", "home/AppData/Local"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
        ("PI_CODING_AGENT_DIR", "agent"),
    ] {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).expect("sandbox directory");
        unsafe { std::env::set_var(key, path) };
    }
    unsafe {
        std::env::set_var("PI_NOTIFICATIONS", "off");
        std::env::set_var(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", root.join("run/no-bus").display()),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700))
            .expect("private runtime");
    }
    std::fs::write(
        root.join("agent/config.yml"),
        "startup:\n  setupWizard: false\n",
    )
    .expect("sandbox omp settings");
}

#[test]
fn gateway_owned_model_preferences_survive_a_new_gateway() {
    let dir = tempfile::Builder::new()
        .prefix("od-fav-")
        .tempdir()
        .expect("sandbox");
    isolate(dir.path());
    for (i, mode) in [GatewayMode::Off, GatewayMode::Background]
        .into_iter()
        .enumerate()
    {
        let profile = Profile {
            host_namespace: format!("omp-deck-test-fav-{}-{i}", std::process::id()),
            settings: dir.path().join(format!("state-{i}.json")),
            sessions_root: dir.path().join("agent/sessions"),
        };
        unsafe {
            std::env::set_var("OMP_DECK_HOST_NS", &profile.host_namespace);
            std::env::set_var("OMP_DECK_TMUX_SOCKET", &profile.host_namespace);
        }
        AppConfig {
            gateway_mode: mode,
            ..AppConfig::default()
        }
        .save_to(&profile.settings)
        .expect("initial settings");
        let first = owner(&profile);
        let first_client = client(&profile);
        let base = snapshot(&first_client).config;
        let mut edited = (*base).clone();
        edited.toggle_favorite("fake/fake-1");
        edited.model_provider_filter = Some("fake".into());
        let committed = first_client
            .settings()
            .submit(Edit::between(base, &edited))
            .wait(WAIT)
            .expect("settings reply")
            .expect("settings commit");
        assert_eq!(committed.config.favorite_models, ["fake/fake-1"]);
        first_client
            .flush()
            .wait(WAIT)
            .expect("flush reply")
            .expect("flush");
        drop(first_client);
        drop(first);
        let second = owner(&profile);
        let second_client = client(&profile);
        let cfg = snapshot(&second_client).config;
        assert_eq!(cfg.favorite_models, ["fake/fake-1"], "{mode:?}");
        assert_eq!(
            cfg.model_provider_filter.as_deref(),
            Some("fake"),
            "{mode:?}"
        );
        assert_eq!(cfg.gateway_mode, mode);
        drop(second_client);
        drop(second);
    }
}
