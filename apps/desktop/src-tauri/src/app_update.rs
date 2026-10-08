//! Keep downloads and verification in the signed Tauri updater, but own the
//! Windows handoff. The stock updater exits immediately and skips our routing
//! shutdown; it also cannot report installation progress after that exit.

use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{ipc::Channel, Manager, ResourceId, Webview};
use tauri_plugin_updater::Update;

#[cfg(any(test, target_os = "windows"))]
#[path = "windows_update.rs"]
mod windows;

static UPDATING: AtomicBool = AtomicBool::new(false);
const DOWNLOAD_TIMEOUT_MS: u64 = 10 * 60 * 1000;

#[derive(Clone, Serialize)]
#[serde(tag = "event", content = "data")]
pub(crate) enum AppUpdateEvent {
    #[serde(rename_all = "camelCase")]
    Started {
        content_length: Option<u64>,
    },
    #[serde(rename_all = "camelCase")]
    Progress {
        chunk_length: usize,
    },
    Verifying,
    #[allow(dead_code)] // Emitted by the Windows handoff; other platforms install in place.
    Preparing,
    Installing,
    #[allow(dead_code)]
    HandedOff,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FailureStage {
    Download,
    Verify,
    Prepare,
    Install,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateFailure {
    stage: FailureStage,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_path: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallResult {
    restart_required: bool,
}

struct UpdateLease<'a>(&'a AtomicBool);
impl<'a> UpdateLease<'a> {
    fn acquire(flag: &'a AtomicBool) -> Result<Self, UpdateFailure> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                failure(
                    FailureStage::Prepare,
                    "已有更新正在进行，请等待当前操作完成。",
                    None,
                )
            })?;
        Ok(Self(flag))
    }
    #[cfg(target_os = "windows")]
    fn handoff(self) {
        // Never permit a second installer while the original app is exiting.
        std::mem::forget(self);
    }
}
impl Drop for UpdateLease<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn failure(stage: FailureStage, message: &str, log: Option<&UpdateLog>) -> UpdateFailure {
    UpdateFailure {
        stage,
        message: message.into(),
        log_path: log.map(|log| log.path.to_string_lossy().into_owned()),
    }
}

/// Check before changing the installed application on every platform. The
/// converted supplier cannot resume through a raw Responses endpoint after exit.
fn install_with_route_preflight<T>(
    check: impl FnOnce() -> Result<(), ()>,
    install: impl FnOnce() -> Result<T, UpdateFailure>,
    log: &UpdateLog,
) -> Result<T, UpdateFailure> {
    if check().is_err() {
        log.record("conversion_route_blocks_install");
        return Err(failure(
            FailureStage::Prepare,
            "当前供应商需要协议转换。请先切换到 Responses 供应商或官方账号，再更新 Codex-X；安装尚未开始。",
            Some(log),
        ));
    }
    install()
}

/// This log contains only our fixed stage names and version, never URLs,
/// headers, auth/config contents or raw network/parser error messages.
struct UpdateLog {
    path: PathBuf,
    file: Mutex<File>,
}
impl UpdateLog {
    fn create(dir: &Path, version: &str) -> std::io::Result<Self> {
        let version = semver::Version::parse(version).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid version")
        })?;
        fs::create_dir_all(dir)?;
        let mut nonce = [0u8; 8];
        getrandom::getrandom(&mut nonce)
            .map_err(|_| std::io::Error::other("random source unavailable"))?;
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!(
            "update-{}-{}-{nonce}.log",
            chrono::Utc::now().format("%Y%m%d-%H%M%S"),
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = Mutex::new(options.open(&path)?);
        let log = Self { path, file };
        log.record(&format!("target_version={version}"));
        Ok(log)
    }
    fn record(&self, stage: &str) {
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{} {stage}", chrono::Utc::now().to_rfc3339());
            let _ = file.flush();
        }
    }
}

fn timeout_ms(value: Option<u64>) -> u64 {
    value
        .unwrap_or(DOWNLOAD_TIMEOUT_MS)
        .clamp(30_000, DOWNLOAD_TIMEOUT_MS)
}

/// No installer is launched until all preparation succeeds. If launch fails,
/// restore routing while the original app and single-instance lock still live.
#[cfg(any(test, target_os = "windows"))]
fn prepared_install<T>(
    prepare: impl FnOnce() -> Result<(), ()>,
    launch: impl FnOnce() -> Result<T, ()>,
    resume: impl FnOnce() -> Result<(), ()>,
    log: &UpdateLog,
) -> Result<T, UpdateFailure> {
    if prepare().is_err() {
        log.record("prepare_failed");
        let resumed = resume().is_ok();
        log.record(if resumed {
            "routing_resumed"
        } else {
            "routing_resume_failed"
        });
        return Err(failure(
            FailureStage::Prepare,
            if resumed {
                "更新前未能恢复路由配置，安装尚未启动。请检查配置文件后重试。"
            } else {
                "更新前的路由清理未完成，安装尚未启动。请检查路由设置后重试。"
            },
            Some(log),
        ));
    }
    match launch() {
        Ok(result) => Ok(result),
        Err(()) => {
            log.record("installer_launch_failed");
            let resumed = resume().is_ok();
            log.record(if resumed {
                "routing_resumed"
            } else {
                "routing_resume_failed"
            });
            Err(failure(
                FailureStage::Install,
                if resumed {
                    "无法启动更新安装器，原有路由已恢复。请重试或打开下载页安装。"
                } else {
                    "无法启动更新安装器。请检查路由设置；也可打开下载页安装。"
                },
                Some(log),
            ))
        }
    }
}

#[tauri::command]
pub(crate) async fn install_app_update(
    webview: Webview,
    update_rid: ResourceId,
    on_event: Channel<AppUpdateEvent>,
    timeout: Option<u64>,
    headers: Option<Vec<(String, String)>>,
) -> Result<InstallResult, UpdateFailure> {
    let _lease = UpdateLease::acquire(&UPDATING)?;
    let update = webview
        .resources_table()
        .get::<Update>(update_rid)
        .map_err(|_| {
            failure(
                FailureStage::Prepare,
                "更新信息已失效，请重新检查更新。",
                None,
            )
        })?;
    let mut update = (*update).clone();
    // The 15s metadata-check timeout must not become the package-download limit.
    update.timeout = Some(Duration::from_millis(timeout_ms(timeout)));
    if let Some(headers) = headers {
        update.headers.clear();
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| failure(FailureStage::Prepare, "更新请求参数无效。", None))?;
            let value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| failure(FailureStage::Prepare, "更新请求参数无效。", None))?;
            update.headers.append(name, value);
        }
    }
    let dir = crate::paths::app_home()
        .map_err(|_| failure(FailureStage::Prepare, "无法读取更新日志目录。", None))?
        .join("update-logs");
    let log = UpdateLog::create(&dir, &update.version).map_err(|_| {
        failure(
            FailureStage::Prepare,
            "无法创建更新日志，请检查文件夹权限后重试。",
            None,
        )
    })?;
    log.record("download_started");
    let finished = AtomicBool::new(false);
    let mut started = false;
    let bytes = update
        .download(
            |chunk_length, content_length| {
                if !started {
                    started = true;
                    let _ = on_event.send(AppUpdateEvent::Started { content_length });
                }
                let _ = on_event.send(AppUpdateEvent::Progress { chunk_length });
            },
            || {
                finished.store(true, Ordering::Release);
                log.record("verifying_signature");
                let _ = on_event.send(AppUpdateEvent::Verifying);
            },
        )
        .await
        .map_err(|_| {
            if finished.load(Ordering::Acquire) {
                log.record("signature_verification_failed");
                failure(
                    FailureStage::Verify,
                    "更新包校验未通过，安装尚未启动。请重新下载或前往下载页。",
                    Some(&log),
                )
            } else {
                log.record("download_failed");
                failure(
                    FailureStage::Download,
                    "更新下载未完成或连接超时。请检查网络后重试。",
                    Some(&log),
                )
            }
        })?;
    log.record("signature_verified");

    #[cfg(target_os = "windows")]
    {
        let app = webview.app_handle().clone();
        let result = install_windows(app, bytes, on_event, log).await;
        if result.is_ok() {
            _lease.handoff();
        }
        result
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = on_event.send(AppUpdateEvent::Preparing);
        tauri::async_runtime::spawn_blocking(move || {
            install_with_route_preflight(
                || crate::failover::ensure_shutdown_allowed().map_err(|_| ()),
                || {
                    let _ = on_event.send(AppUpdateEvent::Installing);
                    log.record("install_started");
                    update.install(bytes).map_err(|_| {
                        log.record("install_failed");
                        failure(
                            FailureStage::Install,
                            "安装更新未完成，请重试或前往下载页安装。",
                            Some(&log),
                        )
                    })
                },
                &log,
            )?;
            log.record("install_finished_restart_required");
            Ok(InstallResult {
                restart_required: true,
            })
        })
        .await
        .map_err(|_| {
            failure(
                FailureStage::Install,
                "安装更新未完成，请重新打开软件后检查更新。",
                None,
            )
        })?
    }
}

// Compile this path in host tests too: staging/Command/IPC use portable Rust,
// so Windows handoff type errors are caught before the Windows build starts.
#[cfg(any(test, target_os = "windows"))]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
async fn install_windows(
    app: tauri::AppHandle,
    bytes: Vec<u8>,
    on_event: Channel<AppUpdateEvent>,
    log: UpdateLog,
) -> Result<InstallResult, UpdateFailure> {
    tauri::async_runtime::spawn_blocking(move || {
        // Stage bytes before changing live routes. Only signed .exe payloads
        // from this release pipeline are supported by the new Windows flow.
        let installer = windows::StagedInstaller::create(&bytes).map_err(|_| {
            failure(
                FailureStage::Install,
                "无法准备 Windows 更新包，请检查磁盘空间或前往下载页安装。",
                Some(&log),
            )
        })?;
        let _ = on_event.send(AppUpdateEvent::Preparing);
        log.record("preparing_route_shutdown");
        install_with_route_preflight(
            || crate::failover::ensure_shutdown_allowed().map_err(|_| ()),
            || {
                prepared_install(
                    || crate::failover::shutdown_all().map_err(|_| ()),
                    || {
                        let _ = on_event.send(AppUpdateEvent::Installing);
                        log.record("launching_installer");
                        installer.launch(std::process::id()).map_err(|_| ())
                    },
                    || crate::failover::resume_after_failed_update().map_err(|_| ()),
                    &log,
                )
            },
            &log,
        )?;
        log.record("installer_handed_off");
        let _ = on_event.send(AppUpdateEvent::HandedOff);
        // Normal Tauri exit destroys windows/tray/single-instance lock. The
        // new installer waits for this PID before touching installed files.
        app.exit(0);
        Ok(InstallResult {
            restart_required: false,
        })
    })
    .await
    .map_err(|_| {
        failure(
            FailureStage::Install,
            "更新准备过程被中断，请重新打开 Codex-X 后检查更新。",
            None,
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn conversion_preflight_blocks_install_before_application_bytes_change() {
        let temp = tempfile::tempdir().unwrap();
        let log = UpdateLog::create(temp.path(), "0.4.0").unwrap();
        let app = temp.path().join("fixture-app");
        fs::write(&app, b"original fixture").unwrap();
        let error = install_with_route_preflight(
            || Err(()),
            || {
                fs::write(&app, b"replacement fixture").unwrap();
                Ok(())
            },
            &log,
        )
        .unwrap_err();
        assert_eq!(error.stage, FailureStage::Prepare);
        assert!(error.message.contains("协议转换"));
        assert!(error.message.contains("Responses"));
        assert!(error.message.contains("安装尚未开始"));
        assert_eq!(fs::read(&app).unwrap(), b"original fixture");
        let stages = fs::read_to_string(&log.path).unwrap();
        assert!(stages.contains("conversion_route_blocks_install"));
        assert!(!stages.contains("install_started"));
    }

    #[test]
    fn native_route_preflight_installs_once_and_preserves_installer_errors() {
        let temp = tempfile::tempdir().unwrap();
        let log = UpdateLog::create(temp.path(), "0.4.0").unwrap();
        let calls = RefCell::new(vec![]);
        let error = install_with_route_preflight(
            || {
                calls.borrow_mut().push("check");
                Ok(())
            },
            || {
                calls.borrow_mut().push("install");
                Err::<(), _>(failure(
                    FailureStage::Install,
                    "fixture install error",
                    Some(&log),
                ))
            },
            &log,
        )
        .unwrap_err();
        assert_eq!(*calls.borrow(), vec!["check", "install"]);
        assert_eq!(error.stage, FailureStage::Install);
        assert_eq!(error.message, "fixture install error");
    }

    #[test]
    fn one_update_at_a_time_and_errors_release_the_guard() {
        let flag = AtomicBool::new(false);
        let first = UpdateLease::acquire(&flag).unwrap();
        assert!(UpdateLease::acquire(&flag).is_err());
        drop(first);
        assert!(UpdateLease::acquire(&flag).is_ok());
    }

    #[test]
    fn package_download_has_its_own_bounded_timeout() {
        assert_eq!(timeout_ms(None), 600_000);
        assert_eq!(timeout_ms(Some(15_000)), 30_000);
        assert_eq!(timeout_ms(Some(u64::MAX)), 600_000);
        assert_eq!(timeout_ms(Some(120_000)), 120_000);
    }

    #[test]
    fn handoff_never_launches_on_failed_prepare_and_resumes_on_launch_failure() {
        let temp = tempfile::tempdir().unwrap();
        let log = UpdateLog::create(temp.path(), "0.4.0").unwrap();
        let calls = RefCell::new(vec![]);
        let fail = prepared_install(
            || {
                calls.borrow_mut().push("prepare");
                Err(())
            },
            || {
                calls.borrow_mut().push("launch");
                Ok(())
            },
            || {
                calls.borrow_mut().push("resume");
                Ok(())
            },
            &log,
        )
        .unwrap_err();
        assert_eq!(fail.stage, FailureStage::Prepare);
        assert_eq!(*calls.borrow(), vec!["prepare", "resume"]);
        calls.borrow_mut().clear();
        let fail = prepared_install(
            || {
                calls.borrow_mut().push("prepare");
                Ok(())
            },
            || {
                calls.borrow_mut().push("launch");
                Err::<(), _>(())
            },
            || {
                calls.borrow_mut().push("resume");
                Err(())
            },
            &log,
        )
        .unwrap_err();
        assert_eq!(fail.stage, FailureStage::Install);
        assert_eq!(*calls.borrow(), vec!["prepare", "launch", "resume"]);
        assert!(fail.message.contains("检查路由设置"));
    }

    #[test]
    fn successful_handoff_does_not_resume_routing() {
        let temp = tempfile::tempdir().unwrap();
        let log = UpdateLog::create(temp.path(), "0.4.0").unwrap();
        assert_eq!(
            prepared_install(|| Ok(()), || Ok(42), || panic!("must not resume"), &log).unwrap(),
            42
        );
    }

    #[test]
    fn logs_are_unique_and_reject_untrusted_version_text() {
        let temp = tempfile::tempdir().unwrap();
        let first = UpdateLog::create(temp.path(), "0.4.0").unwrap();
        let second = UpdateLog::create(temp.path(), "0.4.0").unwrap();
        assert_ne!(first.path, second.path);
        assert!(UpdateLog::create(temp.path(), "0.4.0\nsecret").is_err());
        first.record("signature_verified");
        assert!(fs::read_to_string(&first.path)
            .unwrap()
            .contains("signature_verified"));
    }
}
