use super::{
    accepted_gateway_health, app_state_gateway_slot_is_empty, cleanup_tracked_gateway_with,
    configure_acceptance_native_upstream_override, configure_managed_proxy_command,
    find_gateway_in, finish_failed_gateway_candidate, finish_interrupted_gateway_recovery,
    formal_proxy_env, gateway_bin_path_from, interrupted_health_matches,
    probe_gateway_reuse_health_with, recover_interrupted_gateway_from_dir,
    retry_rejected_gateway_candidates_outside_state, run_legacy_cleanup_outside_state_with,
    skill_install_bridge_token, stage_skill_install_bridge_key_at, GatewayCandidateLog,
    GatewayLaunchRecipe, GatewayProcessLocalOwner, GatewaySpawnCandidate,
    GatewaySpawnCandidateOwner, InterruptedGatewayRecoveryErrorKind,
    InterruptedGatewayRecoveryOutcome, InterruptedGatewayStopUnknownKind, ManagedGatewayCleanup,
    ManagedGatewayStopUnknownKind, PreparedSkillInstallHost,
};
use crate::provider_contracts::{
    CachePolicy, EndpointPolicy, ModelPolicy, TimeoutPolicy, Transport,
};
use crate::runtime::legacy_proxy::stop_managed_gateway_on_port_with;
use crate::runtime::provider::{FormalCredential, FormalGatewayPlan};
use std::fs;
use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static R0_RECOVERY_TERM_OBSERVED: AtomicBool = AtomicBool::new(false);

#[test]
fn acceptance_native_upstream_override_is_loopback_only_and_native_only() {
    let upstream = |cmd: &Command| {
        cmd.get_envs()
            .find(|(key, _)| *key == "CSSWITCH_UPSTREAM_URL")
            .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
    };
    let loopback_only = |cmd: &Command| {
        cmd.get_envs()
            .find(|(key, _)| *key == "CSSWITCH_CONNECT_LOOPBACK_ONLY")
            .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
    };
    let mut deepseek = Command::new("/usr/bin/true");
    configure_managed_proxy_command(&mut deepseek, "deepseek", "off", 32120, "secret", "launch")
        .unwrap();
    assert_eq!(
        loopback_only(&deepseek),
        None,
        "normal native production launch must not restrict CONNECT"
    );
    configure_acceptance_native_upstream_override(
        &mut deepseek,
        "deepseek",
        Some(std::ffi::OsStr::new(
            "http://127.0.0.1:32123/deepseek/v1/messages",
        )),
    )
    .unwrap();
    assert_eq!(
        upstream(&deepseek).as_deref(),
        Some("http://127.0.0.1:32123/deepseek/v1/messages")
    );
    assert_eq!(loopback_only(&deepseek).as_deref(), Some("1"));

    let mut qwen = Command::new("/usr/bin/true");
    configure_managed_proxy_command(&mut qwen, "qwen", "off", 32121, "secret", "launch").unwrap();
    assert_eq!(
        loopback_only(&qwen),
        None,
        "normal native production launch must not restrict CONNECT"
    );
    configure_acceptance_native_upstream_override(
        &mut qwen,
        "qwen",
        Some(std::ffi::OsStr::new(
            "http://[::1]:32124/qwen/v1/chat/completions",
        )),
    )
    .unwrap();
    assert_eq!(
        upstream(&qwen).as_deref(),
        Some("http://[::1]:32124/qwen/v1/chat/completions")
    );
    assert_eq!(loopback_only(&qwen).as_deref(), Some("1"));

    let mut relay = Command::new("/usr/bin/true");
    configure_managed_proxy_command(&mut relay, "relay", "off", 32122, "secret", "launch").unwrap();
    configure_acceptance_native_upstream_override(
        &mut relay,
        "relay",
        Some(std::ffi::OsStr::new("https://provider.invalid/anthropic")),
    )
    .unwrap();
    assert_eq!(
        upstream(&relay),
        None,
        "relay/custom must continue to use the profile endpoint"
    );
    assert_eq!(loopback_only(&relay), None);
    for rejected in [
        "https://provider.invalid/anthropic/v1/messages",
        "not-a-url",
        "http://127.0.0.1@provider.invalid/v1/messages",
    ] {
        let mut cmd = Command::new("/usr/bin/true");
        assert!(
            configure_acceptance_native_upstream_override(
                &mut cmd,
                "deepseek",
                Some(std::ffi::OsStr::new(rejected)),
            )
            .is_err(),
            "native acceptance override must reject {rejected}"
        );
        assert_eq!(upstream(&cmd), None);
        assert_eq!(loopback_only(&cmd), None);
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let mut cmd = Command::new("/usr/bin/true");
        assert!(configure_acceptance_native_upstream_override(
            &mut cmd,
            "deepseek",
            Some(std::ffi::OsStr::from_bytes(
                b"http://127.0.0.1:32123/v1/\xff/messages"
            )),
        )
        .is_err());
        assert_eq!(upstream(&cmd), None);
        assert_eq!(loopback_only(&cmd), None);
    }
}

#[test]
fn acceptance_override_is_wired_after_env_clear_and_before_formal_spawn() {
    let lifecycle = include_str!("lifecycle.rs");
    let formal_start = lifecycle
        .split_once("fn start_proxy_for_inner")
        .map(|(_, suffix)| suffix)
        .expect("production formal start owner must exist");
    let configure = formal_start
        .find("configure_managed_proxy_command(")
        .expect("formal start must configure the env-cleared managed command");
    let acceptance = formal_start
        .find("#[cfg(feature = \"acceptance-build\")]\n        configure_acceptance_native_upstream_override(")
        .expect("formal start must wire the compile-gated acceptance override");
    let spawn = formal_start
        .find(".spawn()")
        .expect("formal start must spawn the configured Gateway command");
    assert!(
        configure < acceptance,
        "override must follow env_clear setup"
    );
    assert!(
        acceptance < spawn,
        "override must reach the final spawn command"
    );
}

#[test]
fn acceptance_github_fixture_is_wired_to_the_host_gateway_spawn() {
    let lifecycle = include_str!("lifecycle.rs");
    let formal_start = lifecycle
        .split_once("fn start_proxy_for_inner")
        .map(|(_, suffix)| suffix)
        .expect("production formal start owner must exist");
    let configure = formal_start
        .find("configure_managed_proxy_command(")
        .expect("formal start must configure the env-cleared managed command");
    let github_fixture = formal_start
        .find("configure_acceptance_github_host_command(&mut cmd)")
        .expect("formal start must wire the host Gateway GitHub fixture");
    let prepare_bridge = formal_start
        .find("prepare_skill_install_host(")
        .expect("formal start must prepare the host Skill bridge");
    let spawn = formal_start
        .find(".spawn()")
        .expect("formal start must spawn the configured Gateway command");
    assert!(
        configure < github_fixture,
        "fixture must follow env_clear setup"
    );
    assert!(
        github_fixture < prepare_bridge,
        "fixture must be present before the host Skill bridge is prepared"
    );
    assert!(
        github_fixture < spawn,
        "fixture must reach the final host spawn"
    );
}

extern "C" fn observe_r0_recovery_term(_signal: libc::c_int) {
    R0_RECOVERY_TERM_OBSERVED.store(true, Ordering::SeqCst);
}

struct TestOwnedChild(std::process::Child);

impl TestOwnedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }

    fn stop(&mut self) -> std::io::Result<()> {
        if self.0.try_wait()?.is_none() {
            self.0.kill()?;
            self.0.wait()?;
        }
        Ok(())
    }
}

impl Drop for TestOwnedChild {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn r0_interrupted_recovery_fixture(
    label: &str,
) -> (std::path::PathBuf, crate::config::RuntimeTransactionRecord) {
    let dir = std::env::temp_dir().join(format!(
        "csswitch-r0-interrupted-recovery-real-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let journal: crate::config::RuntimeTransactionRecord =
        crate::config::RuntimeTransactionJournal {
            transaction_id: format!("tx-real-{label}"),
            target_profile_id: "fixture-profile".into(),
            stage: "start_formal_gateway".into(),
            previous_binding: None,
            previous_gateway: None,
        }
        .into();
    crate::config::save_to(
        &dir,
        &crate::config::Config {
            runtime_transaction: Some(journal.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    (dir, journal)
}

fn spawn_r0_recovery_listener(
    dir: &std::path::Path,
    label: &str,
    mode: &str,
) -> (TestOwnedChild, u16, std::path::PathBuf) {
    let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    assert_ne!(port, 8765);
    drop(reservation);
    let ready = dir.join(format!("{label}-ready"));
    let allow_exit = dir.join(format!("{label}-allow-exit"));
    let current_exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    let mut listener_child = TestOwnedChild(
        Command::new(&current_exe)
            .arg("--exact")
            .arg("runtime::proxy_lifecycle::tests::isolated_r0_interrupted_recovery_identity_recheck_listener")
            .arg("--ignored")
            .arg("--nocapture")
            .env("CSSWITCH_TEST_R0_RECOVERY_PORT", port.to_string())
            .env("CSSWITCH_TEST_R0_RECOVERY_READY", &ready)
            .env("CSSWITCH_TEST_R0_RECOVERY_MODE", mode)
            .env("CSSWITCH_TEST_R0_RECOVERY_ALLOW_EXIT", &allow_exit)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..200 {
        if ready.is_file() {
            return (listener_child, port, allow_exit);
        }
        assert!(
            listener_child.try_wait().unwrap().is_none(),
            "{label} recovery listener exited before readiness"
        );
        thread::sleep(Duration::from_millis(10));
    }
    panic!("{label} recovery listener did not become ready");
}

fn assert_r0_recovery_stage(
    dir: &std::path::Path,
    journal: &crate::config::RuntimeTransactionRecord,
    expected_outcome: crate::config::RuntimeGatewayStopOutcome,
) {
    let current = crate::config::load_from(dir).unwrap();
    let current_journal = current.runtime_transaction.unwrap();
    assert_eq!(current_journal.transaction_id(), journal.transaction_id());
    assert_eq!(
        current_journal.target_profile_id(),
        journal.target_profile_id()
    );
    assert_eq!(
        current_journal.previous_binding(),
        journal.previous_binding()
    );
    assert_eq!(
        current_journal.previous_gateway(),
        journal.previous_gateway()
    );
    let typed = current_journal
        .as_v2()
        .expect("interrupted Gateway recovery must publish a V2 journal");
    assert_eq!(
        typed.operation,
        crate::config::RuntimeTransactionOperation::ProfileSwitch
    );
    assert_eq!(
        typed.phase,
        crate::config::RuntimeTransactionPhase::RecoverInterruptedGateway
    );
    assert_eq!(
        typed.environment_exposure,
        crate::config::RuntimeEnvironmentExposure::NotExposed
    );
    assert_eq!(typed.runtime_fingerprint, None);
    assert_eq!(typed.snapshot_ticket, None);
    assert_eq!(
        typed.compensation,
        crate::config::RuntimeCompensationState::NotStarted
    );
    assert_eq!(typed.gateway_stop_outcome, expected_outcome);
    let encoded = serde_json::to_string(typed).unwrap();
    for forbidden in ["api_key", "base_url", "credential", "secret", "/Users/"] {
        assert!(!encoded.contains(forbidden), "journal leaked `{forbidden}`");
    }
}

fn health(provider: &str, launch_id: &str, catalog_fp: &str) -> crate::proc::GatewayHealth {
    let provider_contract_id = match provider {
        "deepseek" => "deepseek-native",
        "qwen" => "qwen-native",
        other => panic!("missing test contract for {other}"),
    };
    crate::proc::GatewayHealth {
        gateway: "rust".into(),
        provider: provider.into(),
        shim: "off".into(),
        launch_id: launch_id.into(),
        provider_contract_id: provider_contract_id.into(),
        provider_contract_digest: crate::provider_contracts::static_catalog_digest(),
        catalog_fp: catalog_fp.into(),
        intent: "formal".into(),
    }
}

#[test]
fn gateway_acceptance_binds_one_health_response_to_identity_intent_and_catalog() {
    let launch_id = "0123456789abcdef0123456789abcdef";
    let static_health = health("deepseek", launch_id, "static-catalog");
    let contract_digest = crate::provider_contracts::static_catalog_digest();
    let expected = crate::proc::GatewayHealthExpectation {
        gateway: "rust",
        provider: Some("deepseek"),
        shim: Some("off"),
        launch_id: Some(launch_id),
        provider_contract_id: Some("deepseek-native"),
        provider_contract_digest: Some(&contract_digest),
    };
    assert!(accepted_gateway_health(
        &static_health,
        expected,
        Some("static-catalog")
    ));

    let dynamic_health = health("deepseek", launch_id, "");
    assert!(accepted_gateway_health(&dynamic_health, expected, None));

    let mut identity_drift = static_health.clone();
    identity_drift.provider_contract_id = "other-contract".into();
    assert!(!accepted_gateway_health(
        &identity_drift,
        expected,
        Some("static-catalog")
    ));

    let mut intent_drift = static_health.clone();
    intent_drift.intent = "scratch".into();
    assert!(!accepted_gateway_health(
        &intent_drift,
        expected,
        Some("static-catalog")
    ));
    assert!(!accepted_gateway_health(
        &static_health,
        expected,
        Some("other-catalog")
    ));
    assert!(!accepted_gateway_health(&static_health, expected, None));
}

#[test]
fn gateway_reuse_health_releases_app_state_and_rejects_stale_owner_result() {
    let launch_id = "0123456789abcdef0123456789abcdef";
    let route_secret = "fixture-route-secret-never-log";
    let accepted = health("deepseek", launch_id, "");

    for case in ["unchanged", "generation-drift", "identity-drift"] {
        let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let child_pid = child.id();
        let launch_context = GatewayLaunchRecipe {
            profile: crate::config::Profile {
                id: "reuse-owner-profile".into(),
                ..Default::default()
            },
            science_runtime: None,
        };
        let mut authority = crate::AppState::default();
        authority.proxy = Some(child);
        authority.proxy_port = 32120;
        authority.secret = route_secret.into();
        authority.provider = "deepseek".into();
        authority.gateway_kind = "rust".into();
        authority.shim_mode = "off".into();
        authority.launch_id = launch_id.into();
        authority.key_fp = 17;
        authority.gateway_launch_context = Some(launch_context);
        let state: crate::SharedAppState = Arc::new(Mutex::new(authority));
        let lifecycle = crate::lifecycle::Lifecycle::new();
        let generation = lifecycle.current_generation();
        let guard = crate::lock(&state);
        let owner = GatewayProcessLocalOwner::claim(&guard, generation).unwrap();
        let probe_state = state.clone();
        let result = probe_gateway_reuse_health_with(&state, &lifecycle, guard, &owner, |_| {
            let mut current = probe_state
                .try_lock()
                .expect("Gateway reuse HTTP health must run outside AppState");
            if case == "identity-drift" {
                current.launch_id = "replacement-launch-identity".into();
            }
            drop(current);
            if case == "generation-drift" {
                lifecycle.bump_generation();
            }
            Some(accepted.clone())
        });

        match case {
            "unchanged" => {
                let (mut current, health) = result.expect("current owner must accept health");
                assert_eq!(health, Some(accepted.clone()));
                let tracked = current.proxy.as_mut().unwrap();
                assert_eq!(tracked.id(), child_pid);
                assert!(tracked.try_wait().unwrap().is_none());
                tracked.kill().unwrap();
                tracked.wait().unwrap();
                current.proxy = None;
            }
            _ => {
                let error = result
                    .err()
                    .expect("generation or full owner drift must reject stale health");
                assert!(
                    error.contains("process-local owner 已变化"),
                    "{case}: {error}"
                );
                assert!(!error.contains(route_secret), "{case}: {error}");
                let mut current = crate::lock(&state);
                let tracked = current.proxy.as_mut().unwrap();
                assert_eq!(tracked.id(), child_pid, "{case}");
                assert!(tracked.try_wait().unwrap().is_none(), "{case}");
                tracked.kill().unwrap();
                tracked.wait().unwrap();
                current.proxy = None;
            }
        }
    }
}

#[test]
fn gateway_old_cleanup_releases_app_state_restores_failure_and_rejects_replacement() {
    let launch_id = "0123456789abcdef0123456789abcdef";
    let route_secret = "fixture-cleanup-secret-never-log";

    for case in [
        "success",
        "stop-failed",
        "generation-drift",
        "replacement",
        "replacement-stop-failed",
    ] {
        let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let child_pid = child.id();
        let launch_context = GatewayLaunchRecipe {
            profile: crate::config::Profile {
                id: "cleanup-owner-profile".into(),
                ..Default::default()
            },
            science_runtime: None,
        };
        let mut authority = crate::AppState::default();
        authority.proxy = Some(child);
        authority.proxy_port = 32121;
        authority.secret = route_secret.into();
        authority.provider = "deepseek".into();
        authority.gateway_kind = "rust".into();
        authority.shim_mode = "off".into();
        authority.launch_id = launch_id.into();
        authority.key_fp = 19;
        authority.gateway_launch_context = Some(launch_context.clone());
        let state: crate::SharedAppState = Arc::new(Mutex::new(authority));
        let lifecycle = crate::lifecycle::Lifecycle::new();
        let generation = lifecycle.current_generation();
        let guard = crate::lock(&state);
        let cleanup_state = state.clone();

        let result =
            cleanup_tracked_gateway_with(&state, &lifecycle, guard, generation, |owned_child| {
                let current = cleanup_state
                    .try_lock()
                    .expect("tracked Gateway stop/wait must run outside AppState");
                assert!(current.proxy.is_none(), "{case}");
                assert_eq!(current.launch_id, launch_id, "{case}");
                drop(current);

                match case {
                    "success" => {
                        owned_child.kill().unwrap();
                        owned_child.wait().unwrap();
                        Ok(())
                    }
                    "stop-failed" => Err("injected stop failure".into()),
                    "generation-drift" => {
                        owned_child.kill().unwrap();
                        owned_child.wait().unwrap();
                        lifecycle.bump_generation();
                        Ok(())
                    }
                    "replacement" => {
                        owned_child.kill().unwrap();
                        owned_child.wait().unwrap();
                        lifecycle.bump_generation();
                        let replacement = Command::new("/bin/sleep").arg("30").spawn().unwrap();
                        let mut current = crate::lock(&cleanup_state);
                        current.proxy = Some(replacement);
                        current.launch_id = "replacement-launch-id".into();
                        Ok(())
                    }
                    "replacement-stop-failed" => {
                        lifecycle.bump_generation();
                        let replacement = Command::new("/bin/sleep").arg("30").spawn().unwrap();
                        let mut current = crate::lock(&cleanup_state);
                        current.proxy = Some(replacement);
                        current.launch_id = "replacement-launch-id".into();
                        Err("injected stop failure after replacement".into())
                    }
                    _ => unreachable!(),
                }
            });

        match case {
            "success" => {
                let current = result.expect("confirmed stop must commit empty Gateway state");
                assert!(current.proxy.is_none());
                assert!(current.secret.is_empty());
                assert!(current.launch_id.is_empty());
                let legacy_state = state.clone();
                let (current, stopped_pid) = run_legacy_cleanup_outside_state_with(
                    &state,
                    &lifecycle,
                    current,
                    generation,
                    || {
                        let observed = legacy_state
                            .try_lock()
                            .expect("legacy listener cleanup must run outside AppState");
                        assert!(observed.proxy.is_none());
                        drop(observed);
                        Ok(Some(4321))
                    },
                )
                .expect("unchanged empty Gateway slot accepts legacy cleanup result");
                assert_eq!(stopped_pid, Some(4321));
                assert!(current.proxy.is_none());
            }
            "stop-failed" => {
                let error = result.err().expect("injected stop failure must fail");
                assert!(error.contains("已保留 process-local owner"), "{error}");
                assert!(!error.contains(route_secret), "{error}");
                let mut current = crate::lock(&state);
                assert_eq!(current.launch_id, launch_id);
                let restored = current.proxy.as_mut().expect("failed stop restores owner");
                assert_eq!(restored.id(), child_pid);
                restored.kill().unwrap();
                restored.wait().unwrap();
                current.proxy = None;
            }
            "generation-drift" => {
                let error = result.err().expect("generation drift must fail");
                assert!(error.contains("process-local owner 已变化"), "{error}");
                assert!(!error.contains(route_secret), "{error}");
                assert!(app_state_gateway_slot_is_empty(&crate::lock(&state)));
            }
            "replacement" => {
                let error = result.err().expect("replacement drift must fail");
                assert!(error.contains("拒绝覆盖 replacement"), "{error}");
                assert!(!error.contains(route_secret), "{error}");
                let mut current = crate::lock(&state);
                assert_eq!(current.launch_id, "replacement-launch-id");
                let replacement = current.proxy.as_mut().expect("replacement is preserved");
                replacement.kill().unwrap();
                replacement.wait().unwrap();
                current.proxy = None;
            }
            "replacement-stop-failed" => {
                let error = result
                    .err()
                    .expect("replacement plus injected stop failure must fail");
                assert!(error.contains("拒绝覆盖 replacement"), "{error}");
                assert!(error.contains("独立 cleanup owner"), "{error}");
                assert!(!error.contains(route_secret), "{error}");
                let rejected = {
                    let mut current = crate::lock(&state);
                    assert_eq!(current.launch_id, "replacement-launch-id");
                    let replacement = current.proxy.as_mut().expect("replacement is preserved");
                    assert_ne!(replacement.id(), child_pid);
                    assert!(replacement.try_wait().unwrap().is_none());
                    let rejected = current.rejected_gateway_candidates.clone();
                    assert_eq!(rejected.owned_pids(), vec![child_pid]);
                    assert_eq!(unsafe { libc::kill(child_pid as i32, 0) }, 0);
                    rejected
                };
                rejected
                    .retry_with(crate::runtime::system::stop_child_confirmed)
                    .unwrap();
                assert!(rejected.is_idle());
                let _ = crate::lock(&state).stop_proxy();
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn tracked_gateway_cleanup_panic_retains_old_child_and_preserves_replacement() {
    let old_child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let old_pid = old_child.id();
    let launch_context = GatewayLaunchRecipe {
        profile: crate::config::Profile {
            id: "panic-cleanup-owner-profile".into(),
            ..Default::default()
        },
        science_runtime: None,
    };
    let mut authority = crate::AppState::default();
    authority.proxy = Some(old_child);
    authority.proxy_port = 32129;
    authority.secret = "panic-cleanup-secret-never-log".into();
    authority.provider = "deepseek".into();
    authority.gateway_kind = "rust".into();
    authority.shim_mode = "off".into();
    authority.launch_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1".into();
    authority.key_fp = 47;
    authority.gateway_launch_context = Some(launch_context);
    let state: crate::SharedAppState = Arc::new(Mutex::new(authority));
    let lifecycle = crate::lifecycle::Lifecycle::new();
    let generation = lifecycle.current_generation();
    let unwind_state = state.clone();

    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let guard = crate::lock(&unwind_state);
        let cleanup_state = unwind_state.clone();
        let _cleanup_result = cleanup_tracked_gateway_with(
            &unwind_state,
            &lifecycle,
            guard,
            generation,
            |_owned_child| -> Result<(), String> {
                lifecycle.bump_generation();
                let replacement = Command::new("/bin/sleep").arg("30").spawn().unwrap();
                let mut current = crate::lock(&cleanup_state);
                current.proxy = Some(replacement);
                current.launch_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2".into();
                panic!("injected tracked cleanup panic after replacement")
            },
        );
    }));
    assert!(unwind.is_err());

    let rejected = {
        let mut current = crate::lock(&state);
        assert_eq!(current.launch_id, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2");
        let replacement = current.proxy.as_mut().expect("replacement is preserved");
        assert_ne!(replacement.id(), old_pid);
        assert!(replacement.try_wait().unwrap().is_none());
        let rejected = current.rejected_gateway_candidates.clone();
        assert_eq!(rejected.owned_pids(), vec![old_pid]);
        assert_eq!(unsafe { libc::kill(old_pid as i32, 0) }, 0);
        rejected
    };
    rejected
        .retry_with(crate::runtime::system::stop_child_confirmed)
        .unwrap();
    assert!(rejected.is_idle());
    let _ = crate::lock(&state).stop_proxy();
}

fn gateway_spawn_recipe(profile_id: &str) -> GatewayLaunchRecipe {
    GatewayLaunchRecipe {
        profile: crate::config::Profile {
            id: profile_id.into(),
            ..Default::default()
        },
        science_runtime: None,
    }
}

#[test]
fn gateway_spawn_candidate_reserves_a_unique_full_owner_and_preserves_replacement() {
    let state: crate::SharedAppState = Arc::new(Mutex::new(crate::AppState::default()));
    let lifecycle = crate::lifecycle::Lifecycle::new();
    let generation = lifecycle.current_generation();
    let recipe = gateway_spawn_recipe("spawn-owner-profile");
    let owner = {
        let mut current = crate::lock(&state);
        GatewaySpawnCandidateOwner::reserve(
            &mut current,
            generation,
            32122,
            "same-persistent-secret".into(),
            "deepseek".into(),
            "rust".into(),
            "off".into(),
            "11111111111111111111111111111111".into(),
            23,
            recipe.clone(),
        )
        .unwrap()
    };
    {
        let mut current = crate::lock(&state);
        assert!(owner.still_owns(&current, generation));
        assert!(GatewaySpawnCandidateOwner::reserve(
            &mut current,
            generation,
            32122,
            "same-persistent-secret".into(),
            "deepseek".into(),
            "rust".into(),
            "off".into(),
            "22222222222222222222222222222222".into(),
            23,
            recipe.clone(),
        )
        .is_err());
    }

    let replacement = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let replacement_pid = replacement.id();
    {
        let mut current = crate::lock(&state);
        current.proxy = Some(replacement);
        current.launch_id = "22222222222222222222222222222222".into();
    }
    assert!(!finish_failed_gateway_candidate(
        &state, &lifecycle, &owner, None, None,
    ));
    {
        let mut current = crate::lock(&state);
        let replacement = current.proxy.as_mut().expect("replacement must survive");
        assert_eq!(replacement.id(), replacement_pid);
        assert!(replacement.try_wait().unwrap().is_none());
        assert_eq!(current.launch_id, "22222222222222222222222222222222");
        let _ = current.stop_proxy();
    }
}

#[test]
fn gateway_spawn_generation_drift_clears_only_its_marker_and_never_publishes_stale_log() {
    let dir = temp_dir("spawn-owner-stale-log");
    fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let canonical = dir.join("proxy.log");
    let candidate_path = dir.join("proxy-candidate.log");
    let bridge_key = dir.join("skill-install-bridge.key");
    fs::write(&canonical, b"replacement-log\n").unwrap();
    fs::write(&candidate_path, b"stale-candidate-log\n").unwrap();
    fs::write(&bridge_key, b"replacement-token\n").unwrap();
    let mut candidate_log = GatewayCandidateLog {
        candidate_path: Some(candidate_path.clone()),
        canonical_path: canonical.clone(),
    };
    let staged_bridge = stage_skill_install_bridge_key_at(
        dir.clone(),
        bridge_key.clone(),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .unwrap();
    let mut skill_host = PreparedSkillInstallHost {
        key: Some(staged_bridge),
        authority_fence: None,
    };
    let state: crate::SharedAppState = Arc::new(Mutex::new(crate::AppState::default()));
    let lifecycle = crate::lifecycle::Lifecycle::new();
    let owner = {
        let mut current = crate::lock(&state);
        GatewaySpawnCandidateOwner::reserve(
            &mut current,
            lifecycle.current_generation(),
            32123,
            "same-persistent-secret".into(),
            "deepseek".into(),
            "rust".into(),
            "off".into(),
            "33333333333333333333333333333333".into(),
            29,
            gateway_spawn_recipe("stale-owner-profile"),
        )
        .unwrap()
    };
    lifecycle.bump_generation();
    assert!(!finish_failed_gateway_candidate(
        &state,
        &lifecycle,
        &owner,
        Some(&mut candidate_log),
        Some(&mut skill_host),
    ));
    assert!(app_state_gateway_slot_is_empty(&crate::lock(&state)));
    assert_eq!(fs::read(&canonical).unwrap(), b"replacement-log\n");
    assert_eq!(fs::read(&bridge_key).unwrap(), b"replacement-token\n");
    drop(candidate_log);
    drop(skill_host);
    assert!(!candidate_path.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rejected_gateway_candidate_retains_a_separate_cleanup_owner_on_stop_uncertainty() {
    let state: crate::SharedAppState = Arc::new(Mutex::new(crate::AppState::default()));
    let lifecycle = crate::lifecycle::Lifecycle::new();
    let owner = {
        let mut current = crate::lock(&state);
        GatewaySpawnCandidateOwner::reserve(
            &mut current,
            lifecycle.current_generation(),
            32125,
            "same-persistent-secret".into(),
            "deepseek".into(),
            "rust".into(),
            "off".into(),
            "55555555555555555555555555555555".into(),
            37,
            gateway_spawn_recipe("uncertain-stop-profile"),
        )
        .unwrap()
    };
    let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let child_pid = child.id();
    let mut candidate = GatewaySpawnCandidate::new(child, &state, owner.clone());
    let error = candidate
        .reject_with(|_| Err("injected stop uncertainty".into()))
        .unwrap_err();
    assert!(error.contains("独立 cleanup owner"));
    assert!(finish_failed_gateway_candidate(
        &state, &lifecycle, &owner, None, None,
    ));
    {
        let current = crate::lock(&state);
        assert!(current.proxy.is_none());
        assert_eq!(current.rejected_gateway_candidates.len(), 1);
        assert_eq!(
            current.rejected_gateway_candidates.owned_pids(),
            vec![child_pid]
        );
        assert_eq!(unsafe { libc::kill(child_pid as i32, 0) }, 0);
    }
    retry_rejected_gateway_candidates_outside_state(&state).unwrap();
    assert!(crate::lock(&state).rejected_gateway_candidates.is_idle());
}

#[test]
fn rejected_gateway_candidate_panic_restores_owner_before_unwind_completes() {
    let state: crate::SharedAppState = Arc::new(Mutex::new(crate::AppState::default()));
    let lifecycle = crate::lifecycle::Lifecycle::new();
    let owner = {
        let mut current = crate::lock(&state);
        GatewaySpawnCandidateOwner::reserve(
            &mut current,
            lifecycle.current_generation(),
            32126,
            "same-persistent-secret".into(),
            "deepseek".into(),
            "rust".into(),
            "off".into(),
            "66666666666666666666666666666666".into(),
            41,
            gateway_spawn_recipe("panic-stop-profile"),
        )
        .unwrap()
    };
    let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let child_pid = child.id();
    let unwind_state = state.clone();
    let unwind_owner = owner.clone();
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut candidate = GatewaySpawnCandidate::new(child, &unwind_state, unwind_owner);
        let _ = candidate
            .reject_with(|_| -> Result<(), String> { panic!("injected candidate stop panic") });
    }));
    assert!(unwind.is_err());
    let rejected = crate::lock(&state).rejected_gateway_candidates.clone();
    assert_eq!(rejected.owned_pids(), vec![child_pid]);
    assert_eq!(unsafe { libc::kill(child_pid as i32, 0) }, 0);
    {
        let current = crate::lock(&state);
        assert!(!owner.marker_matches(&current));
        assert!(current.secret.is_empty());
        assert!(current.gateway_launch_context.is_none());
    }

    rejected
        .retry_with(crate::runtime::system::stop_child_confirmed)
        .unwrap();
    assert!(rejected.is_idle());
    assert_ne!(unsafe { libc::kill(child_pid as i32, 0) }, 0);
    let retry_owner = {
        let mut current = crate::lock(&state);
        GatewaySpawnCandidateOwner::reserve(
            &mut current,
            lifecycle.current_generation(),
            32126,
            "same-persistent-secret".into(),
            "deepseek".into(),
            "rust".into(),
            "off".into(),
            "77777777777777777777777777777777".into(),
            43,
            gateway_spawn_recipe("panic-stop-retry-profile"),
        )
        .expect("reaping an unwound candidate must unblock the next reservation")
    };
    assert!(finish_failed_gateway_candidate(
        &state,
        &lifecycle,
        &retry_owner,
        None,
        None,
    ));
}

#[test]
fn rejected_gateway_batch_panic_restores_every_affine_owner() {
    let rejected = crate::RejectedGatewayRegistry::default();
    let mut pids = Vec::new();
    for _ in 0..3 {
        let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        pids.push(child.id());
        rejected.retain(child);
    }
    pids.sort_unstable();

    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = rejected
            .retry_with(|_| -> Result<(), String> { panic!("injected rejected-batch panic") });
    }));
    assert!(unwind.is_err());
    let mut retained = rejected.owned_pids();
    retained.sort_unstable();
    assert_eq!(retained, pids);
    assert!(pids
        .iter()
        .all(|pid| unsafe { libc::kill(*pid as i32, 0) } == 0));

    rejected
        .retry_with(crate::runtime::system::stop_child_confirmed)
        .unwrap();
    assert!(rejected.is_idle());
}

#[test]
fn rejected_gateway_cleanup_claim_blocks_a_concurrent_spawn_reservation() {
    let state: crate::SharedAppState = Arc::new(Mutex::new(crate::AppState::default()));
    let rejected = crate::lock(&state).rejected_gateway_candidates.clone();
    let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let child_pid = child.id();
    rejected.retain(child);
    let worker_registry = rejected.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        worker_registry.retry_with(|_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Err("injected persistent uncertainty".into())
        })
    });
    started_rx.recv().unwrap();
    assert_eq!(rejected.len(), 1);

    let lifecycle = crate::lifecycle::Lifecycle::new();
    let reserve = GatewaySpawnCandidateOwner::reserve(
        &mut crate::lock(&state),
        lifecycle.current_generation(),
        32124,
        "same-persistent-secret".into(),
        "deepseek".into(),
        "rust".into(),
        "off".into(),
        "44444444444444444444444444444444".into(),
        31,
        gateway_spawn_recipe("cleanup-in-flight-profile"),
    );
    assert!(reserve.is_err());
    assert!(rejected
        .retry_with(crate::runtime::system::stop_child_confirmed)
        .unwrap_err()
        .contains("cleanup 正在进行"));
    assert!(matches!(
        crate::lock(&state).stop_proxy_with(crate::runtime::system::stop_child_confirmed),
        crate::GatewayStopOutcome::Uncertain { owned_count: 1, .. }
    ));

    release_tx.send(()).unwrap();
    assert!(worker.join().unwrap().is_err());
    assert_eq!(rejected.owned_pids(), vec![child_pid]);
    rejected
        .retry_with(crate::runtime::system::stop_child_confirmed)
        .unwrap();
    assert!(rejected.is_idle());
}

#[test]
fn skill_bridge_key_staging_cannot_overwrite_canonical_before_publish() {
    let dir = temp_dir("skill-bridge-staging");
    fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let key_file = dir.join("skill-install-bridge.key");
    fs::write(&key_file, b"replacement-token\n").unwrap();

    let staged = stage_skill_install_bridge_key_at(
        dir.clone(),
        key_file.clone(),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .unwrap();
    let temporary = staged.temporary.as_ref().unwrap().clone();
    assert!(temporary.is_file());
    assert_eq!(fs::read(&key_file).unwrap(), b"replacement-token\n");
    drop(staged);
    assert!(!temporary.exists());
    assert_eq!(fs::read(&key_file).unwrap(), b"replacement-token\n");

    let mut accepted = stage_skill_install_bridge_key_at(
        dir.clone(),
        key_file.clone(),
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .unwrap();
    accepted.publish_canonical_key().unwrap();
    assert_eq!(
        fs::read(&key_file).unwrap(),
        b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn interrupted_gateway_accepts_only_committed_target_or_exact_previous_identity() {
    let target = health("qwen", "0123456789abcdef0123456789abcdef", "target-catalog");
    assert!(interrupted_health_matches(
        &target,
        "qwen",
        "off",
        "qwen-native",
        &crate::provider_contracts::static_catalog_digest(),
        Some("target-catalog"),
        None,
    ));

    let previous_identity = crate::config::GatewayRuntimeJournalIdentity {
        provider: "deepseek".into(),
        shim: "off".into(),
        launch_id: "abcdef0123456789abcdef0123456789".into(),
        provider_contract_id: "deepseek-native".into(),
        provider_contract_digest: crate::provider_contracts::static_catalog_digest(),
        catalog_fp: "previous-catalog".into(),
    };
    let previous = health(
        "deepseek",
        "abcdef0123456789abcdef0123456789",
        "previous-catalog",
    );
    assert!(interrupted_health_matches(
        &previous,
        "qwen",
        "off",
        "qwen-native",
        &crate::provider_contracts::static_catalog_digest(),
        Some("target-catalog"),
        Some(&previous_identity),
    ));

    let spoof = health("deepseek", "not-a-managed-launch-id", "previous-catalog");
    assert!(!interrupted_health_matches(
        &spoof,
        "qwen",
        "off",
        "qwen-native",
        &crate::provider_contracts::static_catalog_digest(),
        Some("target-catalog"),
        Some(&previous_identity),
    ));
}

#[test]
fn r0_interrupted_recovery_freezes_post_stage_stop_outcomes() {
    for (label, cleanup, expected_result, expected_outcome) in [
        (
            "not-managed",
            ManagedGatewayCleanup::NotManaged,
            Err(InterruptedGatewayRecoveryErrorKind::NotManaged),
            crate::config::RuntimeGatewayStopOutcome::NotManaged,
        ),
        (
            "signal-failed",
            ManagedGatewayCleanup::StopUnknown {
                pid: 4242,
                kind: ManagedGatewayStopUnknownKind::SignalFailed,
            },
            Err(InterruptedGatewayRecoveryErrorKind::StopUnknown(
                InterruptedGatewayStopUnknownKind::SignalFailed,
            )),
            crate::config::RuntimeGatewayStopOutcome::SignalFailed,
        ),
        (
            "exit-unconfirmed",
            ManagedGatewayCleanup::StopUnknown {
                pid: 4242,
                kind: ManagedGatewayStopUnknownKind::ExitUnconfirmed,
            },
            Err(InterruptedGatewayRecoveryErrorKind::StopUnknown(
                InterruptedGatewayStopUnknownKind::ExitUnconfirmed,
            )),
            crate::config::RuntimeGatewayStopOutcome::ExitUnconfirmed,
        ),
        (
            "stopped",
            ManagedGatewayCleanup::Stopped(4242),
            Ok(Some(4242)),
            crate::config::RuntimeGatewayStopOutcome::Stopped,
        ),
    ] {
        let dir = std::env::temp_dir().join(format!(
            "csswitch-r0-interrupted-recovery-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let previous_binding = crate::config::RuntimeBindingCommit {
            profile_id: "prior-profile".into(),
            route_fp: "prior-route".into(),
            catalog_fp: "prior-catalog".into(),
            binding_fp: "prior-binding".into(),
            science_adoption_attempt_id: None,
        };
        let legacy_journal = crate::config::RuntimeTransactionJournal {
            transaction_id: format!("tx-{label}"),
            target_profile_id: "target-profile".into(),
            stage: "start_formal_gateway".into(),
            previous_binding: Some(previous_binding.clone()),
            previous_gateway: None,
        };
        let journal = if label == "stopped" {
            crate::config::RuntimeTransactionRecord::V2(crate::config::RuntimeTransactionV2 {
                schema_version: crate::config::RUNTIME_TRANSACTION_SCHEMA_VERSION_V2,
                transaction_id: legacy_journal.transaction_id.clone(),
                operation: crate::config::RuntimeTransactionOperation::ProfileSwitch,
                target_profile_id: legacy_journal.target_profile_id.clone(),
                phase: crate::config::RuntimeTransactionPhase::StartFormalGateway,
                runtime_fingerprint: None,
                environment_exposure: crate::config::RuntimeEnvironmentExposure::NotExposed,
                snapshot_ticket: None,
                previous_binding: legacy_journal.previous_binding.clone(),
                previous_gateway: legacy_journal.previous_gateway.clone(),
                compensation: crate::config::RuntimeCompensationState::NotStarted,
                gateway_stop_outcome: crate::config::RuntimeGatewayStopOutcome::NotAttempted,
                prior_stop: crate::config::RuntimePriorStopState::NotRequired,
                finalize: crate::config::RuntimeFinalizeState::NotStarted,
            })
        } else {
            legacy_journal.into()
        };
        let (model_catalog, default_model_route_id, role_bindings) =
            crate::model_catalog::new_profile_catalog(
                "deepseek",
                "anthropic",
                Some("deepseek-v4-flash"),
            )
            .unwrap();
        let cfg = crate::config::Config {
            profiles: vec![crate::config::Profile {
                id: "target-profile".into(),
                template_id: "deepseek".into(),
                api_format: "anthropic".into(),
                model: "deepseek-v4-flash".into(),
                model_catalog,
                default_model_route_id,
                role_bindings,
                model_policy: crate::provider_contracts::ModelPolicy::SavedCatalog,
                ..Default::default()
            }],
            active_id: "target-profile".into(),
            runtime_binding: Some(previous_binding.clone()),
            runtime_transaction: Some(journal.clone()),
            ..Default::default()
        };
        crate::config::save_to(&dir, &cfg).unwrap();

        let result = finish_interrupted_gateway_recovery(&dir, &journal, || cleanup);
        assert_eq!(
            result
                .as_ref()
                .map(|outcome| match outcome {
                    InterruptedGatewayRecoveryOutcome::Stopped(pid, _) => Some(*pid),
                    _ => None,
                })
                .map_err(|error| error.kind()),
            expected_result,
            "{label} must keep its exact typed post-stage outcome"
        );
        if let Err(error) = &result {
            let expected = if label == "not-managed" {
                "未通过精确 Gateway binary/uid/PID 复核"
            } else {
                "安全停止失败"
            };
            assert!(
                error.to_string().contains(expected),
                "{label} must keep its existing safe detail: {error}"
            );
        }
        let after = crate::config::load_from(&dir).unwrap();
        let after_journal = after.runtime_transaction.unwrap();
        assert_eq!(after_journal.transaction_id(), journal.transaction_id());
        assert_eq!(
            after_journal.target_profile_id(),
            journal.target_profile_id()
        );
        assert_eq!(after_journal.previous_binding(), journal.previous_binding());
        assert_eq!(after_journal.previous_gateway(), journal.previous_gateway());
        assert_r0_recovery_stage(&dir, &journal, expected_outcome);
        assert_eq!(after.runtime_binding, Some(previous_binding));
        fs::remove_dir_all(&dir).unwrap();
    }

    let cas_dir = std::env::temp_dir().join(format!(
        "csswitch-r2-d-interrupted-recovery-cas-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&cas_dir).unwrap();
    let previous_gateway = crate::config::GatewayRuntimeJournalIdentity {
        provider: "deepseek".into(),
        shim: "off".into(),
        launch_id: "0123456789abcdef0123456789abcdef".into(),
        provider_contract_id: "deepseek-native".into(),
        provider_contract_digest: crate::provider_contracts::static_catalog_digest(),
        catalog_fp: "previous-catalog".into(),
    };
    let expected_record =
        crate::config::RuntimeTransactionRecord::V2(crate::config::RuntimeTransactionV2 {
            schema_version: crate::config::RUNTIME_TRANSACTION_SCHEMA_VERSION_V2,
            transaction_id: "r2-d-complete-record-cas".into(),
            operation: crate::config::RuntimeTransactionOperation::ProfileSwitch,
            target_profile_id: "target-profile".into(),
            phase: crate::config::RuntimeTransactionPhase::StartFormalGateway,
            runtime_fingerprint: None,
            environment_exposure: crate::config::RuntimeEnvironmentExposure::NotExposed,
            snapshot_ticket: None,
            previous_binding: None,
            previous_gateway: Some(previous_gateway),
            compensation: crate::config::RuntimeCompensationState::NotStarted,
            gateway_stop_outcome: crate::config::RuntimeGatewayStopOutcome::NotAttempted,
            prior_stop: crate::config::RuntimePriorStopState::NotRequired,
            finalize: crate::config::RuntimeFinalizeState::NotStarted,
        });
    let mut retargeted = expected_record.clone();
    retargeted
        .as_v2_mut()
        .unwrap()
        .previous_gateway
        .as_mut()
        .unwrap()
        .launch_id = "fedcba9876543210fedcba9876543210".into();
    crate::config::save_to(
        &cas_dir,
        &crate::config::Config {
            runtime_transaction: Some(retargeted),
            ..Default::default()
        },
    )
    .unwrap();
    let before_intent_rejection = fs::read(cas_dir.join("config.json")).unwrap();
    let cleanup_called = std::cell::Cell::new(false);
    let intent_error = finish_interrupted_gateway_recovery(&cas_dir, &expected_record, || {
        cleanup_called.set(true);
        ManagedGatewayCleanup::Stopped(4242)
    })
    .unwrap_err();
    assert_eq!(
        intent_error.kind(),
        InterruptedGatewayRecoveryErrorKind::GatewayStart
    );
    assert!(!cleanup_called.get());
    assert_eq!(
        fs::read(cas_dir.join("config.json")).unwrap(),
        before_intent_rejection,
        "same-id field drift before intent publication must preserve exact current bytes"
    );

    crate::config::save_to(
        &cas_dir,
        &crate::config::Config {
            runtime_transaction: Some(expected_record.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let outcome_error = finish_interrupted_gateway_recovery(&cas_dir, &expected_record, || {
        crate::config::update(&cas_dir, |current| {
            current
                .runtime_transaction
                .as_mut()
                .and_then(crate::config::RuntimeTransactionRecord::as_v2_mut)
                .unwrap()
                .compensation = crate::config::RuntimeCompensationState::InProgress;
        })
        .unwrap();
        ManagedGatewayCleanup::Stopped(4242)
    })
    .unwrap_err();
    assert_eq!(
        outcome_error.kind(),
        InterruptedGatewayRecoveryErrorKind::GatewayStart
    );
    let preserved_drift = crate::config::load_from(&cas_dir)
        .unwrap()
        .runtime_transaction
        .unwrap();
    let preserved_drift = preserved_drift.as_v2().unwrap();
    assert_eq!(
        preserved_drift.phase,
        crate::config::RuntimeTransactionPhase::RecoverInterruptedGateway
    );
    assert_eq!(
        preserved_drift.gateway_stop_outcome,
        crate::config::RuntimeGatewayStopOutcome::Pending
    );
    assert_eq!(
        preserved_drift.compensation,
        crate::config::RuntimeCompensationState::InProgress
    );
    fs::remove_dir_all(&cas_dir).unwrap();

    let dir = std::env::temp_dir().join(format!(
        "csswitch-r0-interrupted-recovery-recheck-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    assert_ne!(port, 8765);
    drop(reservation);
    let ready = dir.join("ready");
    let current_exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    let mut listener_child = TestOwnedChild(
        Command::new(&current_exe)
        .arg("--exact")
        .arg("runtime::proxy_lifecycle::tests::isolated_r0_interrupted_recovery_identity_recheck_listener")
        .arg("--ignored")
        .arg("--nocapture")
        .env("CSSWITCH_TEST_R0_RECOVERY_PORT", port.to_string())
        .env("CSSWITCH_TEST_R0_RECOVERY_READY", &ready)
        .stdout(Stdio::null())
        .spawn()
        .unwrap(),
    );
    let mut ready_observed = false;
    for _ in 0..100 {
        if ready.is_file() {
            ready_observed = true;
            break;
        }
        assert!(
            listener_child.try_wait().unwrap().is_none(),
            "identity-recheck listener child exited before readiness"
        );
        thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        ready_observed,
        "identity-recheck listener did not become ready"
    );

    let journal: crate::config::RuntimeTransactionRecord =
        crate::config::RuntimeTransactionJournal {
            transaction_id: "tx-identity-recheck".into(),
            target_profile_id: "identity-recheck-profile".into(),
            stage: "start_formal_gateway".into(),
            previous_binding: None,
            previous_gateway: None,
        }
        .into();
    let cfg = crate::config::Config {
        runtime_transaction: Some(journal.clone()),
        ..Default::default()
    };
    crate::config::save_to(&dir, &cfg).unwrap();
    let recheck_observed = std::cell::Cell::new(false);
    let result = finish_interrupted_gateway_recovery(&dir, &journal, || {
        assert_eq!(
            crate::config::load_from(&dir)
                .unwrap()
                .runtime_transaction
                .as_ref()
                .unwrap()
                .as_v2()
                .unwrap()
                .gateway_stop_outcome,
            crate::config::RuntimeGatewayStopOutcome::Pending,
            "durable recovery stage must precede the final process identity recheck"
        );
        super::stop_managed_gateway_on_port(port, &current_exe, || {
            recheck_observed.set(true);
            assert_eq!(
                crate::config::load_from(&dir)
                    .unwrap()
                    .runtime_transaction
                    .as_ref()
                    .unwrap()
                    .as_v2()
                    .unwrap()
                    .gateway_stop_outcome,
                crate::config::RuntimeGatewayStopOutcome::Pending
            );
            false
        })
    });
    assert!(
        result.as_ref().is_err_and(|error| error.kind()
            == InterruptedGatewayRecoveryErrorKind::NotManaged
            && error
                .to_string()
                .contains("未通过精确 Gateway binary/uid/PID 复核")),
        "failed final identity recheck must preserve the post-stage NotManaged result: {result:?}"
    );
    assert!(
        recheck_observed.get(),
        "the real managed-listener path must execute its final identity callback"
    );
    assert!(
        listener_child.try_wait().unwrap().is_none(),
        "identity recheck refusal must not stop the listener"
    );
    assert_r0_recovery_stage(
        &dir,
        &journal,
        crate::config::RuntimeGatewayStopOutcome::NotManaged,
    );
    listener_child.stop().unwrap();
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn r0_interrupted_recovery_executes_signal_wait_late_exit_and_retry_identity_matrix() {
    let expected_binary = std::env::current_exe().unwrap().canonicalize().unwrap();

    let (signal_dir, signal_journal) = r0_interrupted_recovery_fixture("signal-failure");
    let (mut signal_child, signal_port, _) =
        spawn_r0_recovery_listener(&signal_dir, "signal-failure", "retry");
    let signal_pid = signal_child.0.id();
    let signal_rechecks = std::cell::Cell::new(0);
    let signal_attempts = std::cell::Cell::new(0);
    let signal_outcome = std::cell::Cell::new(None);
    let signal_result = finish_interrupted_gateway_recovery(&signal_dir, &signal_journal, || {
        let outcome = stop_managed_gateway_on_port_with(
            signal_port,
            &expected_binary,
            || {
                signal_rechecks.set(signal_rechecks.get() + 1);
                true
            },
            |pid| {
                signal_attempts.set(signal_attempts.get() + 1);
                assert_eq!(pid, signal_pid);
                assert_r0_recovery_stage(
                    &signal_dir,
                    &signal_journal,
                    crate::config::RuntimeGatewayStopOutcome::Pending,
                );
                Err(())
            },
        );
        signal_outcome.set(Some(outcome));
        outcome
    });
    assert_eq!(signal_rechecks.get(), 1);
    assert_eq!(signal_attempts.get(), 1);
    assert_eq!(
        signal_outcome.get(),
        Some(ManagedGatewayCleanup::StopUnknown {
            pid: signal_pid,
            kind: ManagedGatewayStopUnknownKind::SignalFailed,
        })
    );
    assert!(signal_result.as_ref().is_err_and(|error| error.kind()
        == InterruptedGatewayRecoveryErrorKind::StopUnknown(
            InterruptedGatewayStopUnknownKind::SignalFailed
        )
        && error.to_string().contains("安全停止失败")));
    assert_r0_recovery_stage(
        &signal_dir,
        &signal_journal,
        crate::config::RuntimeGatewayStopOutcome::SignalFailed,
    );
    assert!(
        signal_child.try_wait().unwrap().is_none(),
        "injected signal failure must leave the exact controlled listener alive"
    );
    signal_child.stop().unwrap();
    fs::remove_dir_all(signal_dir).unwrap();

    let (retry_dir, retry_journal) = r0_interrupted_recovery_fixture("wait-retry");
    let (mut retry_child, retry_port, allow_retry_exit) =
        spawn_r0_recovery_listener(&retry_dir, "wait-retry", "retry");
    let retry_pid = retry_child.0.id();
    let first_rechecks = std::cell::Cell::new(0);
    let first_outcome = std::cell::Cell::new(None);
    let first_result = finish_interrupted_gateway_recovery(&retry_dir, &retry_journal, || {
        let outcome = super::stop_managed_gateway_on_port(retry_port, &expected_binary, || {
            first_rechecks.set(first_rechecks.get() + 1);
            true
        });
        first_outcome.set(Some(outcome));
        outcome
    });
    assert_eq!(first_rechecks.get(), 1);
    assert_eq!(
        first_outcome.get(),
        Some(ManagedGatewayCleanup::StopUnknown {
            pid: retry_pid,
            kind: ManagedGatewayStopUnknownKind::ExitUnconfirmed,
        }),
        "a listener that ignores the first TERM must exhaust the production wait budget"
    );
    assert!(first_result.as_ref().is_err_and(|error| error.kind()
        == InterruptedGatewayRecoveryErrorKind::StopUnknown(
            InterruptedGatewayStopUnknownKind::ExitUnconfirmed
        )));
    assert_r0_recovery_stage(
        &retry_dir,
        &retry_journal,
        crate::config::RuntimeGatewayStopOutcome::ExitUnconfirmed,
    );
    assert!(retry_child.try_wait().unwrap().is_none());

    let retry_after_first = crate::config::load_from(&retry_dir)
        .unwrap()
        .runtime_transaction
        .unwrap();
    let refused_rechecks = std::cell::Cell::new(0);
    let refused_outcome = std::cell::Cell::new(None);
    let refused_result =
        finish_interrupted_gateway_recovery(&retry_dir, &retry_after_first, || {
            let outcome = super::stop_managed_gateway_on_port(retry_port, &expected_binary, || {
                refused_rechecks.set(refused_rechecks.get() + 1);
                false
            });
            refused_outcome.set(Some(outcome));
            outcome
        });
    assert_eq!(refused_rechecks.get(), 1);
    assert_eq!(
        refused_outcome.get(),
        Some(ManagedGatewayCleanup::NotManaged)
    );
    assert!(refused_result.as_ref().is_err_and(|error| error.kind()
        == InterruptedGatewayRecoveryErrorKind::NotManaged
        && error
            .to_string()
            .contains("未通过精确 Gateway binary/uid/PID 复核")));
    assert!(retry_child.try_wait().unwrap().is_none());
    assert_r0_recovery_stage(
        &retry_dir,
        &retry_journal,
        crate::config::RuntimeGatewayStopOutcome::NotManaged,
    );

    fs::write(&allow_retry_exit, b"allow\n").unwrap();
    let retry_after_refusal = crate::config::load_from(&retry_dir)
        .unwrap()
        .runtime_transaction
        .unwrap();
    let accepted_rechecks = std::cell::Cell::new(0);
    let accepted_outcome = std::cell::Cell::new(None);
    let accepted_result =
        finish_interrupted_gateway_recovery(&retry_dir, &retry_after_refusal, || {
            let outcome = super::stop_managed_gateway_on_port(retry_port, &expected_binary, || {
                accepted_rechecks.set(accepted_rechecks.get() + 1);
                true
            });
            accepted_outcome.set(Some(outcome));
            outcome
        });
    assert_eq!(accepted_rechecks.get(), 1);
    assert_eq!(
        accepted_outcome.get(),
        Some(ManagedGatewayCleanup::Stopped(retry_pid))
    );
    assert!(matches!(
        accepted_result,
        Ok(InterruptedGatewayRecoveryOutcome::Stopped(pid, _)) if pid == retry_pid
    ));
    assert_r0_recovery_stage(
        &retry_dir,
        &retry_journal,
        crate::config::RuntimeGatewayStopOutcome::Stopped,
    );
    retry_child.stop().unwrap();
    fs::remove_dir_all(retry_dir).unwrap();

    let (late_dir, late_journal) = r0_interrupted_recovery_fixture("late-exit");
    let (mut late_child, late_port, allow_late_exit) =
        spawn_r0_recovery_listener(&late_dir, "late-exit", "late");
    let late_pid = late_child.0.id();
    let late_outcome = std::cell::Cell::new(None);
    let late_result = finish_interrupted_gateway_recovery(&late_dir, &late_journal, || {
        let outcome = super::stop_managed_gateway_on_port(late_port, &expected_binary, || true);
        late_outcome.set(Some(outcome));
        outcome
    });
    assert_eq!(
        late_outcome.get(),
        Some(ManagedGatewayCleanup::StopUnknown {
            pid: late_pid,
            kind: ManagedGatewayStopUnknownKind::ExitUnconfirmed,
        }),
        "the controlled late exit must occur after the production wait budget"
    );
    assert!(late_result.as_ref().is_err_and(|error| error.kind()
        == InterruptedGatewayRecoveryErrorKind::StopUnknown(
            InterruptedGatewayStopUnknownKind::ExitUnconfirmed
        )));
    assert_r0_recovery_stage(
        &late_dir,
        &late_journal,
        crate::config::RuntimeGatewayStopOutcome::ExitUnconfirmed,
    );

    fs::write(&allow_late_exit, b"allow\n").unwrap();
    let mut late_exit_observed = false;
    for _ in 0..100 {
        if late_child.try_wait().unwrap().is_some() {
            late_exit_observed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        late_exit_observed,
        "the controlled Gateway must exit shortly after the timeout result"
    );

    let late_after_first = crate::config::load_from(&late_dir)
        .unwrap()
        .runtime_transaction
        .unwrap();
    let late_retry_rechecks = std::cell::Cell::new(0);
    let late_retry_outcome = std::cell::Cell::new(None);
    let late_retry_result =
        finish_interrupted_gateway_recovery(&late_dir, &late_after_first, || {
            let outcome = super::stop_managed_gateway_on_port(late_port, &expected_binary, || {
                late_retry_rechecks.set(late_retry_rechecks.get() + 1);
                true
            });
            late_retry_outcome.set(Some(outcome));
            outcome
        });
    assert_eq!(late_retry_rechecks.get(), 0);
    assert_eq!(
        late_retry_outcome.get(),
        Some(ManagedGatewayCleanup::NotManaged),
        "retry after late exit must rediscover the absent listener instead of reusing StopUnknown"
    );
    assert!(late_retry_result.is_err());
    assert_eq!(
        late_retry_result.unwrap_err().kind(),
        InterruptedGatewayRecoveryErrorKind::NotManaged
    );
    assert_r0_recovery_stage(
        &late_dir,
        &late_journal,
        crate::config::RuntimeGatewayStopOutcome::NotManaged,
    );
    fs::remove_dir_all(late_dir).unwrap();
}

#[test]
#[ignore = "explicit Acceptance-boundary interrupted-recovery identity recheck listener; test binary, temp state, and dynamic loopback only"]
fn isolated_r0_interrupted_recovery_identity_recheck_listener() {
    let port = std::env::var("CSSWITCH_TEST_R0_RECOVERY_PORT")
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let ready = std::env::var_os("CSSWITCH_TEST_R0_RECOVERY_READY").unwrap();
    let _listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    fs::write(ready, b"ready\n").unwrap();
    let mode = std::env::var("CSSWITCH_TEST_R0_RECOVERY_MODE").unwrap_or_default();
    let allow_exit =
        std::env::var_os("CSSWITCH_TEST_R0_RECOVERY_ALLOW_EXIT").map(std::path::PathBuf::from);
    if !mode.is_empty() {
        R0_RECOVERY_TERM_OBSERVED.store(false, Ordering::SeqCst);
        unsafe {
            libc::signal(
                libc::SIGTERM,
                observe_r0_recovery_term as *const () as libc::sighandler_t,
            );
        }
    }
    let mut late_exit_armed = false;
    loop {
        if R0_RECOVERY_TERM_OBSERVED.swap(false, Ordering::SeqCst) {
            match mode.as_str() {
                "retry" if allow_exit.as_ref().is_some_and(|path| path.is_file()) => return,
                "late" => late_exit_armed = true,
                "retry" => {}
                other => panic!("unexpected recovery listener mode: {other}"),
            }
        }
        if late_exit_armed && allow_exit.as_ref().is_some_and(|path| path.is_file()) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn mismatched_recovery_target_preserves_listener_and_journal() {
    let dir = std::env::temp_dir().join(format!(
        "csswitch-recovery-target-mismatch-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (model_catalog, default_model_route_id, role_bindings) =
        crate::model_catalog::new_profile_catalog(
            "deepseek",
            "anthropic",
            Some("deepseek-v4-flash"),
        )
        .unwrap();
    let journal = crate::config::RuntimeTransactionJournal {
        transaction_id: "tx-stale-target".into(),
        target_profile_id: "stale-profile".into(),
        stage: "start_formal_gateway".into(),
        previous_binding: None,
        previous_gateway: None,
    };
    let cfg = crate::config::Config {
        profiles: vec![crate::config::Profile {
            id: "active-profile".into(),
            template_id: "deepseek".into(),
            api_format: "anthropic".into(),
            model: "deepseek-v4-flash".into(),
            model_catalog,
            default_model_route_id,
            role_bindings,
            model_policy: crate::provider_contracts::ModelPolicy::SavedCatalog,
            ..Default::default()
        }],
        active_id: "active-profile".into(),
        proxy_port: address.port(),
        sandbox_port: if address.port() == 8990 { 8991 } else { 8990 },
        runtime_transaction: Some(journal.clone().into()),
        ..Default::default()
    };
    crate::config::save_to(&dir, &cfg).unwrap();
    let before = fs::read(dir.join("config.json")).unwrap();
    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let state = Arc::new(Mutex::new(crate::AppState::default()));

    let error = recover_interrupted_gateway_from_dir(app.handle(), &state, &dir).unwrap_err();
    assert_eq!(
        error.kind(),
        InterruptedGatewayRecoveryErrorKind::GatewayStart
    );
    assert!(error
        .to_string()
        .contains("target profile 与当前 active profile 不一致"));
    assert!(error.to_string().contains("已保留 listener 和事务 journal"));
    assert_eq!(fs::read(dir.join("config.json")).unwrap(), before);
    assert_eq!(
        crate::config::load_from(&dir).unwrap().runtime_transaction,
        Some(journal.into())
    );
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);

    let mut legacy_cfg = crate::config::load_from(&dir).unwrap();
    let legacy_journal = crate::config::RuntimeTransactionJournal {
        transaction_id: "tx-legacy-science".into(),
        target_profile_id: legacy_cfg.active_id.clone(),
        stage: "start_science".into(),
        previous_binding: None,
        previous_gateway: None,
    };
    legacy_cfg.runtime_transaction = Some(legacy_journal.clone().into());
    crate::config::save_to(&dir, &legacy_cfg).unwrap();
    let legacy_before = fs::read(dir.join("config.json")).unwrap();
    let legacy_error =
        recover_interrupted_gateway_from_dir(app.handle(), &state, &dir).unwrap_err();
    assert_eq!(
        legacy_error.kind(),
        InterruptedGatewayRecoveryErrorKind::GatewayStart
    );
    assert!(
        legacy_error
            .to_string()
            .contains("unknown or malformed V1 runtime_transaction stage"),
        "legacy Science exposure without a fingerprint must fail during journal decoding before listener probing: {legacy_error}"
    );
    assert_eq!(fs::read(dir.join("config.json")).unwrap(), legacy_before);
    assert!(crate::config::load_from(&dir).is_err());
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);

    let _client = TcpStream::connect(address).unwrap();
    let mut accepted = false;
    for _ in 0..50 {
        match listener.accept() {
            Ok(_) => {
                accepted = true;
                break;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => panic!("listener accept failed: {error}"),
        }
    }
    assert!(
        accepted,
        "mismatched recovery must leave the listener usable"
    );

    let mut no_journal_cfg = legacy_cfg;
    no_journal_cfg.runtime_transaction = None;
    crate::config::save_to(&dir, &no_journal_cfg).unwrap();
    let no_journal_before = fs::read(dir.join("config.json")).unwrap();
    assert_eq!(
        recover_interrupted_gateway_from_dir(app.handle(), &state, &dir),
        Ok(InterruptedGatewayRecoveryOutcome::NotNeeded)
    );
    assert_eq!(
        fs::read(dir.join("config.json")).unwrap(),
        no_journal_before,
        "no-journal recovery must not rewrite config"
    );

    let managed_journal = crate::config::RuntimeTransactionJournal {
        transaction_id: "tx-same-process-owned".into(),
        target_profile_id: no_journal_cfg.active_id.clone(),
        stage: "start_formal_gateway".into(),
        previous_binding: None,
        previous_gateway: None,
    };
    let mut managed_cfg = no_journal_cfg.clone();
    managed_cfg.runtime_transaction = Some(managed_journal.clone().into());
    crate::config::save_to(&dir, &managed_cfg).unwrap();
    let managed_before = fs::read(dir.join("config.json")).unwrap();
    state.lock().unwrap().launch_id = "same-process-owned".into();
    assert_eq!(
        recover_interrupted_gateway_from_dir(app.handle(), &state, &dir),
        Ok(InterruptedGatewayRecoveryOutcome::NotNeeded)
    );
    state.lock().unwrap().launch_id.clear();
    assert_eq!(fs::read(dir.join("config.json")).unwrap(), managed_before);
    assert_eq!(
        crate::config::load_from(&dir).unwrap().runtime_transaction,
        Some(managed_journal.clone().into()),
        "same-process ownership must preserve the journal for its tracked Child path"
    );
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);

    let unused_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let unused_port = unused_reservation.local_addr().unwrap().port();
    assert_ne!(unused_port, 8765);
    drop(unused_reservation);
    let mut no_listener_cfg = managed_cfg;
    no_listener_cfg.proxy_port = unused_port;
    no_listener_cfg.runtime_transaction = Some(managed_journal.clone().into());
    crate::config::save_to(&dir, &no_listener_cfg).unwrap();
    let no_listener_before = fs::read(dir.join("config.json")).unwrap();
    assert_eq!(
        recover_interrupted_gateway_from_dir(app.handle(), &state, &dir),
        Ok(InterruptedGatewayRecoveryOutcome::NotNeeded)
    );
    assert_eq!(
        fs::read(dir.join("config.json")).unwrap(),
        no_listener_before,
        "absent-listener recovery must not rewrite the journal"
    );
    assert_eq!(
        crate::config::load_from(&dir).unwrap().runtime_transaction,
        Some(managed_journal.clone().into())
    );

    let healthy_start_intent =
        crate::config::RuntimeTransactionRecord::V2(crate::config::RuntimeTransactionV2 {
            schema_version: crate::config::RUNTIME_TRANSACTION_SCHEMA_VERSION_V2,
            transaction_id: "tx-healthy-reopen-before-gateway-effect".into(),
            operation: crate::config::RuntimeTransactionOperation::ProfileSwitch,
            target_profile_id: managed_journal.target_profile_id.clone(),
            phase: crate::config::RuntimeTransactionPhase::StartFormalGateway,
            runtime_fingerprint: None,
            environment_exposure: crate::config::RuntimeEnvironmentExposure::NotExposed,
            snapshot_ticket: None,
            previous_binding: managed_journal.previous_binding.clone(),
            previous_gateway: managed_journal.previous_gateway.clone(),
            compensation: crate::config::RuntimeCompensationState::NotStarted,
            gateway_stop_outcome: crate::config::RuntimeGatewayStopOutcome::NotAttempted,
            prior_stop: crate::config::RuntimePriorStopState::NotRequired,
            finalize: crate::config::RuntimeFinalizeState::NotStarted,
        });
    no_listener_cfg.runtime_transaction = Some(healthy_start_intent.clone());
    crate::config::save_to(&dir, &no_listener_cfg).unwrap();
    let healthy_start_outcome =
        recover_interrupted_gateway_from_dir(app.handle(), &state, &dir).unwrap();
    assert!(matches!(
        healthy_start_outcome,
        InterruptedGatewayRecoveryOutcome::Terminal(_)
    ));
    assert_r0_recovery_stage(
        &dir,
        &healthy_start_intent,
        crate::config::RuntimeGatewayStopOutcome::AbsentAfterAttempt,
    );

    let attempted_journal =
        crate::config::RuntimeTransactionRecord::V2(crate::config::RuntimeTransactionV2 {
            schema_version: crate::config::RUNTIME_TRANSACTION_SCHEMA_VERSION_V2,
            transaction_id: managed_journal.transaction_id.clone(),
            operation: crate::config::RuntimeTransactionOperation::ProfileSwitch,
            target_profile_id: managed_journal.target_profile_id.clone(),
            phase: crate::config::RuntimeTransactionPhase::RecoverInterruptedGateway,
            runtime_fingerprint: None,
            environment_exposure: crate::config::RuntimeEnvironmentExposure::NotExposed,
            snapshot_ticket: None,
            previous_binding: managed_journal.previous_binding.clone(),
            previous_gateway: managed_journal.previous_gateway.clone(),
            compensation: crate::config::RuntimeCompensationState::NotStarted,
            gateway_stop_outcome: crate::config::RuntimeGatewayStopOutcome::ExitUnconfirmed,
            prior_stop: crate::config::RuntimePriorStopState::NotRequired,
            finalize: crate::config::RuntimeFinalizeState::NotStarted,
        });
    no_listener_cfg.runtime_transaction = Some(attempted_journal.clone());
    crate::config::save_to(&dir, &no_listener_cfg).unwrap();
    let attempted_outcome =
        recover_interrupted_gateway_from_dir(app.handle(), &state, &dir).unwrap();
    assert!(matches!(
        attempted_outcome,
        InterruptedGatewayRecoveryOutcome::Terminal(_)
    ));
    assert_r0_recovery_stage(
        &dir,
        &attempted_journal,
        crate::config::RuntimeGatewayStopOutcome::AbsentAfterAttempt,
    );

    let unsupported_legacy: crate::config::RuntimeTransactionRecord =
        crate::config::RuntimeTransactionJournal {
            transaction_id: "tx-stale-one-click".into(),
            target_profile_id: no_listener_cfg.active_id.clone(),
            stage: "stop_old_science".into(),
            previous_binding: None,
            previous_gateway: None,
        }
        .into();
    no_listener_cfg.runtime_transaction = Some(unsupported_legacy.clone());
    crate::config::save_to(&dir, &no_listener_cfg).unwrap();
    let unsupported_before = fs::read(dir.join("config.json")).unwrap();
    let unsupported_error =
        recover_interrupted_gateway_from_dir(app.handle(), &state, &dir).unwrap_err();
    assert_eq!(
        unsupported_error.kind(),
        InterruptedGatewayRecoveryErrorKind::AuthoritySnapshot
    );
    assert_eq!(
        fs::read(dir.join("config.json")).unwrap(),
        unsupported_before
    );
    assert_eq!(
        crate::config::load_from(&dir).unwrap().runtime_transaction,
        Some(unsupported_legacy)
    );

    let unsupported_one_click =
        crate::config::RuntimeTransactionRecord::V2(crate::config::RuntimeTransactionV2 {
            schema_version: crate::config::RUNTIME_TRANSACTION_SCHEMA_VERSION_V2,
            transaction_id: "tx-typed-one-click".into(),
            operation: crate::config::RuntimeTransactionOperation::OneClick,
            target_profile_id: no_listener_cfg.active_id.clone(),
            phase: crate::config::RuntimeTransactionPhase::StartGateway,
            runtime_fingerprint: Some("a".repeat(64)),
            environment_exposure: crate::config::RuntimeEnvironmentExposure::NotExposed,
            snapshot_ticket: Some(
                crate::config::RuntimeSnapshotTicket::verified(format!(
                    ".one-click-rollback-{}",
                    "b".repeat(32)
                ))
                .unwrap(),
            ),
            previous_binding: None,
            previous_gateway: None,
            compensation: crate::config::RuntimeCompensationState::NotStarted,
            gateway_stop_outcome: crate::config::RuntimeGatewayStopOutcome::NotAttempted,
            prior_stop: crate::config::RuntimePriorStopState::NotRequired,
            finalize: crate::config::RuntimeFinalizeState::NotStarted,
        });
    no_listener_cfg.proxy_port = address.port();
    no_listener_cfg.runtime_transaction = Some(unsupported_one_click.clone());
    crate::config::save_to(&dir, &no_listener_cfg).unwrap();
    let unsupported_v2_before = fs::read(dir.join("config.json")).unwrap();
    let unsupported_v2_error =
        recover_interrupted_gateway_from_dir(app.handle(), &state, &dir).unwrap_err();
    assert_eq!(
        unsupported_v2_error.kind(),
        InterruptedGatewayRecoveryErrorKind::AuthoritySnapshot
    );
    assert_eq!(
        fs::read(dir.join("config.json")).unwrap(),
        unsupported_v2_before,
        "a valid one-click V2 journal must be rejected before listener probing"
    );
    assert_eq!(
        crate::config::load_from(&dir).unwrap().runtime_transaction,
        Some(unsupported_one_click)
    );
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);

    let drifted_restart_journal =
        crate::config::RuntimeTransactionRecord::V2(crate::config::RuntimeTransactionV2 {
            schema_version: crate::config::RUNTIME_TRANSACTION_SCHEMA_VERSION_V2,
            transaction_id: "tx-drifted-recovery-restart".into(),
            operation: crate::config::RuntimeTransactionOperation::ProfileSwitch,
            target_profile_id: no_listener_cfg.active_id.clone(),
            phase: crate::config::RuntimeTransactionPhase::RecoverInterruptedGateway,
            runtime_fingerprint: None,
            environment_exposure: crate::config::RuntimeEnvironmentExposure::NotExposed,
            snapshot_ticket: None,
            previous_binding: None,
            previous_gateway: None,
            compensation: crate::config::RuntimeCompensationState::InProgress,
            gateway_stop_outcome: crate::config::RuntimeGatewayStopOutcome::Pending,
            prior_stop: crate::config::RuntimePriorStopState::NotRequired,
            finalize: crate::config::RuntimeFinalizeState::NotStarted,
        });
    no_listener_cfg.runtime_transaction = Some(drifted_restart_journal.clone());
    crate::config::save_to(&dir, &no_listener_cfg).unwrap();
    let drifted_restart_before = fs::read(dir.join("config.json")).unwrap();
    let drifted_restart_error =
        recover_interrupted_gateway_from_dir(app.handle(), &state, &dir).unwrap_err();
    assert_eq!(
        drifted_restart_error.kind(),
        InterruptedGatewayRecoveryErrorKind::AuthoritySnapshot
    );
    assert_eq!(
        fs::read(dir.join("config.json")).unwrap(),
        drifted_restart_before,
        "a compensation-drifted recovery record must remain fail-closed after restart"
    );
    assert_eq!(
        crate::config::load_from(&dir).unwrap().runtime_transaction,
        Some(drifted_restart_journal)
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        ErrorKind::WouldBlock,
        "restart eligibility must reject compensation drift before listener probing"
    );

    let completed_journal =
        crate::config::RuntimeTransactionRecord::V2(crate::config::RuntimeTransactionV2 {
            schema_version: crate::config::RUNTIME_TRANSACTION_SCHEMA_VERSION_V2,
            transaction_id: "tx-completed-recovery".into(),
            operation: crate::config::RuntimeTransactionOperation::ProfileSwitch,
            target_profile_id: no_listener_cfg.active_id.clone(),
            phase: crate::config::RuntimeTransactionPhase::RecoverInterruptedGateway,
            runtime_fingerprint: None,
            environment_exposure: crate::config::RuntimeEnvironmentExposure::NotExposed,
            snapshot_ticket: None,
            previous_binding: None,
            previous_gateway: None,
            compensation: crate::config::RuntimeCompensationState::NotStarted,
            gateway_stop_outcome: crate::config::RuntimeGatewayStopOutcome::Stopped,
            prior_stop: crate::config::RuntimePriorStopState::NotRequired,
            finalize: crate::config::RuntimeFinalizeState::NotStarted,
        });
    no_listener_cfg.proxy_port = address.port();
    no_listener_cfg.runtime_transaction = Some(completed_journal.clone());
    crate::config::save_to(&dir, &no_listener_cfg).unwrap();
    let completed_before = fs::read(dir.join("config.json")).unwrap();
    let completed_outcome =
        recover_interrupted_gateway_from_dir(app.handle(), &state, &dir).unwrap();
    assert!(matches!(
        completed_outcome,
        InterruptedGatewayRecoveryOutcome::Terminal(_)
    ));
    assert_eq!(fs::read(dir.join("config.json")).unwrap(), completed_before);
    assert_eq!(
        crate::config::load_from(&dir).unwrap().runtime_transaction,
        Some(completed_journal),
        "a terminal typed recovery must never probe or stop a later listener"
    );
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    fs::remove_dir_all(&dir).unwrap();
}

fn launch(adapter: &str, model: &str) -> FormalGatewayPlan {
    launch_with_thinking(adapter, model, "")
}

fn launch_with_thinking(
    adapter: &str,
    model: &str,
    thinking_policy: &'static str,
) -> FormalGatewayPlan {
    FormalGatewayPlan {
            contract_id: if adapter == "openai-custom" {
                "custom-openai-chat"
            } else {
                "anthropic-relay"
            }
            .into(),
            contract_digest: crate::provider_contracts::static_catalog_digest(),
            adapter: adapter.to_string(),
            auth_scheme: if matches!(adapter, "openai-custom" | "openai-responses") {
                crate::provider_contracts::AuthScheme::Bearer
            } else {
                crate::provider_contracts::AuthScheme::AnthropicDual
            },
            endpoint: "https://upstream.example/api".to_string(),
            model: model.to_string(),
            static_model_catalog: Some(format!(
                "{{\"schema_version\":1,\"default_selector_id\":\"selector-test\",\"routes\":[{{\"selector_id\":\"selector-test\",\"display_name\":\"{model}\",\"upstream_model\":\"{model}\",\"supports_tools\":true}}],\"role_bindings\":{{\"sonnet\":\"selector-test\",\"opus\":\"selector-test\",\"haiku\":\"selector-test\",\"fable\":\"selector-test\"}}}}"
            )),
            credential: FormalCredential::ApiKey {
                env: if matches!(adapter, "openai-custom" | "openai-responses") {
                    "CSSWITCH_OPENAI_KEY".into()
                } else {
                    "CSSWITCH_RELAY_KEY".into()
                },
                value: "test-key".into(),
            },
            model_policy: ModelPolicy::SavedCatalog,
            transport: if adapter == "openai-custom" {
                Transport::OpenaiChat
            } else {
                Transport::AnthropicMessages
            },
            endpoint_policy: EndpointPolicy::ProfileRequired,
            endpoint_join: if adapter == "openai-custom" {
                crate::provider_contracts::EndpointJoin::OpenaiV1
            } else {
                crate::provider_contracts::EndpointJoin::AnthropicV1
            },
            timeouts: TimeoutPolicy {
                connect_ms: 10_000,
                total_ms: 30_000,
                read_idle_ms: 300_000,
            },
            cache: CachePolicy {
                normal_ttl_seconds: 0,
                stale_ttl_seconds: 0,
            },
            thinking_policy: thinking_policy.to_string(),
            codex_network_route: None,
        }
}

#[test]
fn formal_proxy_env_injects_relay_catalog_without_legacy_model_override() {
    let env = formal_proxy_env(&launch("relay", "glm-5.2")).unwrap();
    assert!(env.contains(&(
        "CSSWITCH_RELAY_BASE_URL".to_string(),
        "https://upstream.example/api".to_string()
    )));
    assert!(env.iter().any(|(key, value)| {
        key == "CSSWITCH_STATIC_MODEL_CATALOG_V1" && value.contains("glm-5.2")
    }));
    assert!(!env.iter().any(|(key, _)| key == "CSSWITCH_RELAY_MODEL"));
}

#[test]
fn managed_proxy_command_keeps_secret_out_of_argv_and_injects_canonical_shim() {
    let fake_secret = "fake-managed-secret";
    for (provider, raw_shim, expected_shim, removes_upstream) in [
        ("deepseek", " Rewrite ", "rewrite", false),
        ("deepseek", "DETECT", "detect", false),
        ("qwen", " Rewrite ", "off", false),
        ("qwen", "off", "off", false),
        ("openai-custom", "DETECT", "off", true),
        ("openai-custom", "off", "off", true),
        ("openai-responses", "rewrite", "off", true),
        ("openai-responses", "off", "off", true),
        ("relay", "rewrite", "off", true),
        ("relay", "off", "off", true),
        ("codex", "rewrite", "off", true),
        ("codex", "off", "off", true),
    ] {
        let mut cmd = Command::new("csswitch-gateway");
        configure_managed_proxy_command(
            &mut cmd,
            provider,
            raw_shim,
            18991,
            fake_secret,
            "fake-launch-id",
        )
        .unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|arg| arg == "--auth-token"));
        assert!(!args.iter().any(|arg| arg == fake_secret));
        assert!(cmd.get_envs().any(|(key, value)| {
            key == "CSSWITCH_AUTH_TOKEN"
                && value
                    .map(|value| value.to_string_lossy() == fake_secret)
                    .unwrap_or(false)
        }));
        assert!(cmd.get_envs().any(|(key, value)| {
            key == "CSSWITCH_TOOLUSE_SHIM"
                && value
                    .map(|value| value.to_string_lossy() == expected_shim)
                    .unwrap_or(false)
        }));
        assert!(cmd.get_envs().any(|(key, value)| {
            key == "CSSWITCH_LAUNCH_ID"
                && value
                    .map(|value| value.to_string_lossy() == "fake-launch-id")
                    .unwrap_or(false)
        }));
        // Allowlist base: no ambient inheritance, so CSSWITCH_UPSTREAM_URL is
        // absent for every adapter until a caller sets it explicitly.
        let upstream_override = cmd
            .get_envs()
            .find(|(key, _)| *key == "CSSWITCH_UPSTREAM_URL")
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()));
        assert_eq!(
            upstream_override, None,
            "{provider} must not inherit CSSWITCH_UPSTREAM_URL from ambient env"
        );
        let _ = removes_upstream; // table still documents former denylist intent
        let contract_id = cmd
            .get_envs()
            .find(|(key, _)| *key == "CSSWITCH_PROVIDER_CONTRACT_ID")
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        let contract_digest = cmd
            .get_envs()
            .find(|(key, _)| *key == "CSSWITCH_PROVIDER_CONTRACT_DIGEST")
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        assert_eq!(contract_id, None);
        assert_eq!(contract_digest, None);
        // Base allowlist surface is present and closed.
        assert!(cmd.get_envs().any(|(key, value)| {
            key == "PATH"
                && value
                    .map(|value| value.to_string_lossy() == crate::runtime::launch_env::SAFE_PATH)
                    .unwrap_or(false)
        }));
        let env_keys: Vec<String> = cmd
            .get_envs()
            .filter_map(|(key, value)| value.map(|_| key.to_string_lossy().into_owned()))
            .collect();
        for key in &env_keys {
            assert!(
                crate::runtime::launch_env::gateway_base_env_keys().contains(&key.as_str()),
                "unexpected gateway base env key {key}"
            );
        }
    }
}

#[test]
fn managed_proxy_command_allowlist_drops_parent_provider_secret() {
    // Spawn a real child: Command::get_envs alone cannot prove ambient isolation.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("OPENAI_API_KEY");
    std::env::set_var("OPENAI_API_KEY", "sk-parent-must-not-reach-gateway");

    let mut env_probe = Command::new("/usr/bin/env");
    crate::runtime::launch_env::configure_gateway_base_command(&mut env_probe);
    env_probe
        .env("CSSWITCH_AUTH_TOKEN", "fake-managed-secret")
        .env("CSSWITCH_LAUNCH_ID", "fake-launch-id")
        .env("CSSWITCH_TOOLUSE_SHIM", "detect");
    let output = env_probe
        .output()
        .expect("spawn env under gateway allowlist");
    assert!(
        output.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("OPENAI_API_KEY"));
    assert!(!stdout.contains("sk-parent-must-not-reach-gateway"));
    assert!(stdout.contains("HOME="));
    let home = stdout
        .lines()
        .find(|line| line.starts_with("HOME="))
        .map(|line| &line["HOME=".len()..])
        .expect("HOME");
    assert!(
        std::path::Path::new(home).is_absolute(),
        "gateway child HOME must be absolute: {home}"
    );
    assert!(stdout.contains("CSSWITCH_AUTH_TOKEN=fake-managed-secret"));

    let mut cmd = Command::new("csswitch-gateway");
    configure_managed_proxy_command(
        &mut cmd,
        "deepseek",
        "detect",
        18991,
        "fake-managed-secret",
        "fake-launch-id",
    )
    .unwrap();
    let home_from_cmd = cmd
        .get_envs()
        .find(|(key, _)| *key == "HOME")
        .and_then(|(_, value)| value)
        .map(|value| value.to_string_lossy().into_owned())
        .expect("configure_managed_proxy_command must set HOME");
    assert!(
        std::path::Path::new(&home_from_cmd).is_absolute(),
        "configured HOME not absolute: {home_from_cmd}"
    );

    match previous {
        Some(value) => std::env::set_var("OPENAI_API_KEY", value),
        None => std::env::remove_var("OPENAI_API_KEY"),
    }
}

#[test]
fn formal_proxy_env_injects_openai_catalog_without_legacy_model_override() {
    let env = formal_proxy_env(&launch("openai-custom", "gpt-5.2")).unwrap();
    assert!(env.iter().any(|(key, value)| {
        key == "CSSWITCH_PROVIDER_CONTRACT_ID" && value == "custom-openai-chat"
    }));
    assert!(env
        .iter()
        .any(|(key, value)| { key == "CSSWITCH_PROVIDER_CONTRACT_DIGEST" && value.len() == 64 }));
    assert!(env.contains(&(
        "CSSWITCH_OPENAI_BASE_URL".to_string(),
        "https://upstream.example/api".to_string()
    )));
    assert!(env.iter().any(|(key, value)| {
        key == "CSSWITCH_STATIC_MODEL_CATALOG_V1" && value.contains("gpt-5.2")
    }));
    assert!(!env.iter().any(|(key, _)| key == "CSSWITCH_OPENAI_MODEL"));
}

#[test]
fn formal_proxy_env_native_adapter_only_sets_native_key() {
    let mut native = launch("deepseek", "");
    native.credential = FormalCredential::ApiKey {
        env: "DEEPSEEK_API_KEY".into(),
        value: "test-key".into(),
    };
    native.endpoint_policy = EndpointPolicy::GatewayManagedOfficial;
    native.model_policy = ModelPolicy::SavedCatalog;
    let env = formal_proxy_env(&native).unwrap();
    assert!(env.contains(&("DEEPSEEK_API_KEY".to_string(), "test-key".to_string())));
    assert!(env
        .iter()
        .any(|(key, _)| key == "CSSWITCH_STATIC_MODEL_CATALOG_V1"));
}

#[test]
fn formal_proxy_env_empty_model_does_not_pin_model() {
    let env = formal_proxy_env(&launch("relay", "")).unwrap();
    assert!(env.iter().any(|(k, _)| *k == "CSSWITCH_RELAY_BASE_URL"));
    assert!(!env.iter().any(|(k, _)| *k == "CSSWITCH_RELAY_MODEL"));
    assert!(!env.iter().any(|(k, _)| *k == "CSSWITCH_OPENAI_MODEL"));
}

#[test]
fn formal_proxy_env_preserves_relay_thinking_policy() {
    let env = formal_proxy_env(&launch_with_thinking("relay", "glm-5.2", "enabled")).unwrap();
    assert!(env.contains(&("CSSWITCH_RELAY_THINKING".to_string(), "enabled".to_string())));
}

#[test]
fn formal_proxy_env_injects_only_a_validated_codex_route() {
    let mut codex = launch("codex", "");
    codex.static_model_catalog = None;
    codex.codex_network_route = Some(
        csswitch_codex_network::resolve(
            &csswitch_codex_network::CodexNetworkSettings {
                mode: csswitch_codex_network::CodexNetworkMode::Custom,
                proxy_url: "socks5h://127.0.0.1:7890".into(),
                ..Default::default()
            },
            &csswitch_codex_network::EnvironmentSnapshot::default(),
        )
        .unwrap(),
    );
    let env = formal_proxy_env(&codex).unwrap();
    let encoded = env
        .iter()
        .find(|(key, _)| key == csswitch_codex_network::ROUTE_ENV)
        .map(|(_, value)| value)
        .unwrap();
    let decoded = csswitch_codex_network::decode_route(encoded).unwrap();
    assert_eq!(decoded.source, csswitch_codex_network::RouteSource::Custom);
    assert_eq!(decoded.proxy_scheme.as_deref(), Some("socks5h"));
}

#[test]
fn skill_bridge_token_rotates_with_gateway_launch_identity() {
    let secret = "0123456789abcdef0123456789abcdef";
    let first = skill_install_bridge_token(secret, "11111111111111111111111111111111").unwrap();
    let second = skill_install_bridge_token(secret, "22222222222222222222222222222222").unwrap();
    assert_eq!(first.len(), 64);
    assert_ne!(first, second);
    assert!(!first.contains(secret));
}

#[test]
fn find_gateway_in_accepts_plain_or_tauri_suffixed_binary() {
    let dir = temp_dir("find-test");
    fs::create_dir_all(&dir).unwrap();
    let wrong = dir.join("csswitch-gateway-other-target");
    fs::write(&wrong, b"wrong target").unwrap();
    assert_eq!(find_gateway_in(&dir), None);
    let path = dir.join(sidecar_name());
    fs::write(&path, b"bin").unwrap();
    assert_eq!(find_gateway_in(&dir), Some(path.clone()));
    let plain = dir.join(if cfg!(windows) {
        "csswitch-gateway.exe"
    } else {
        "csswitch-gateway"
    });
    fs::write(&plain, b"bundled").unwrap();
    assert_eq!(find_gateway_in(&dir), Some(plain.clone()));
    let _ = fs::remove_file(plain);
    let _ = fs::remove_file(wrong);
    let _ = fs::remove_file(path);
    let _ = fs::remove_dir(dir);
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "csswitch-gateway-{label}-{}-{unique}",
        std::process::id()
    ))
}

fn sidecar_name() -> String {
    format!(
        "csswitch-gateway-{}{}",
        env!("CSSWITCH_BUILD_TARGET"),
        if cfg!(windows) { ".exe" } else { "" }
    )
}

fn write_marker(path: &std::path::Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"bin").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[test]
fn gateway_lookup_prefers_explicit_env_binary() {
    let dir = temp_dir("env-override");
    let env_bin = dir.join("custom-gateway");
    write_marker(&env_bin);
    let canonical_env_bin = env_bin.canonicalize().unwrap();
    let found = gateway_bin_path_from(Some(canonical_env_bin.clone()), None, None, None);
    assert_eq!(found, Some(canonical_env_bin));
    let _ = fs::remove_file(env_bin);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn invalid_explicit_gateway_binary_fails_closed_without_fallback() {
    let dir = temp_dir("invalid-env-override");
    let fallback = dir.join(sidecar_name());
    write_marker(&fallback);
    assert_eq!(
        gateway_bin_path_from(
            Some(std::path::PathBuf::from("relative-gateway")),
            None,
            Some(dir.clone()),
            None,
        ),
        None
    );
    assert_eq!(
        gateway_bin_path_from(
            Some(dir.join("missing-gateway")),
            None,
            Some(dir.clone()),
            None,
        ),
        None
    );
    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn explicit_gateway_binary_rejects_symlink_components() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("symlink-env-override");
    let real_dir = dir.join("real");
    let real_bin = real_dir.join("gateway");
    write_marker(&real_bin);
    let linked_dir = dir.join("linked");
    symlink(&real_dir, &linked_dir).unwrap();
    assert_eq!(
        gateway_bin_path_from(Some(linked_dir.join("gateway")), None, None, None),
        None
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn gateway_lookup_finds_packaged_resource_sidecar_layouts() {
    let dir = temp_dir("packaged-resource");
    let direct = dir.join(sidecar_name());
    write_marker(&direct);
    assert_eq!(
        gateway_bin_path_from(None, None, Some(dir.clone()), None),
        Some(direct.clone())
    );
    fs::remove_file(&direct).unwrap();

    let nested = dir.join("binaries").join(sidecar_name());
    write_marker(&nested);
    assert_eq!(
        gateway_bin_path_from(None, None, Some(dir.clone()), None),
        Some(nested.clone())
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn gateway_lookup_finds_dev_repo_and_staged_sidecar_layouts() {
    let root = temp_dir("dev-repo");
    let debug = root
        .join("desktop/gateway/target/debug")
        .join(if cfg!(windows) {
            "csswitch-gateway.exe"
        } else {
            "csswitch-gateway"
        });
    write_marker(&debug);
    assert_eq!(
        gateway_bin_path_from(None, None, None, Some(root.clone())),
        Some(debug.clone())
    );
    fs::remove_file(&debug).unwrap();

    let staged = root.join("desktop/src-tauri/binaries").join(sidecar_name());
    write_marker(&staged);
    write_marker(&debug);
    assert_eq!(
        gateway_bin_path_from(None, None, None, Some(root.clone())),
        Some(staged.clone())
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn build_rs_stages_executable_sidecar_for_tauri_external_bin() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let staged_dir = manifest_dir.join("binaries");
    let staged = find_gateway_in(&staged_dir)
        .unwrap_or_else(|| panic!("missing staged sidecar in {}", staged_dir.display()));
    let name = staged.file_name().and_then(|n| n.to_str()).unwrap_or("");
    assert!(name.starts_with("csswitch-gateway-"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&staged).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "{} is not executable", staged.display());
    }
}
