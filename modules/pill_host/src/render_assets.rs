//! Native asset cooking and manifest watching; renderer runtime only sees bytes.
use pill_master_renderer::{
    assets::{load_manifest, DirectorySource, RenderAssetRequests},
    RendererError,
};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant, SystemTime},
};
pub(crate) struct NativeAssets {
    root: PathBuf,
    stamp: Option<SystemTime>,
    poll: Instant,
    worker: Option<Child>,
}
impl NativeAssets {
    pub fn prepare(project: Option<&Path>, workspace: &Path) -> Result<Self, RendererError> {
        let mut worker = None;
        let root = if let Some(project) = project {
            let input = project.join("assets/render_source");
            let output = project.join("build/render_assets");
            if input.is_dir() {
                let status = Command::new("cargo")
                    .env("RUSTFLAGS", "")
                    .current_dir(workspace)
                    .args([
                        "run",
                        "--offline",
                        "--quiet",
                        "-p",
                        "pill_master_renderer",
                        "--no-default-features",
                        "--features",
                        "asset-cooking",
                        "--bin",
                        "pill-cook",
                        "--target-dir",
                        "target/asset-cooking",
                        "--",
                    ])
                    .arg(&input)
                    .arg(&output)
                    .status()
                    .map_err(failure)?;
                if !status.success() {
                    return Err(failure("asset cooking failed; see cooker diagnostics"));
                }
                let executable =
                    workspace
                        .join("target/asset-cooking/debug")
                        .join(if cfg!(windows) {
                            "pill-cook.exe"
                        } else {
                            "pill-cook"
                        });
                let mut cmd = Command::new(executable);
                cmd.arg(input).arg(&output).arg("--watch");
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000);
                }
                worker = Some(cmd.spawn().map_err(failure)?);
            }
            output
        } else {
            std::env::current_exe()
                .map_err(failure)?
                .parent()
                .unwrap()
                .join("assets/render")
        };
        Ok(Self {
            root,
            stamp: None,
            poll: Instant::now() - Duration::from_secs(1),
            worker,
        })
    }
    pub fn update(&mut self, engine: &mut pill_engine::Engine) -> Result<(), RendererError> {
        if self.poll.elapsed() < Duration::from_millis(500) {
            return Ok(());
        }
        self.poll = Instant::now();
        if let Some(worker) = self.worker.as_mut() {
            if let Some(status) = worker.try_wait().map_err(failure)? {
                eprintln!("[assets] cooker watcher exited: {status}; restart the frontend to resume automatic cooking");
                self.worker = None;
            }
        }
        let stamp = std::fs::metadata(self.root.join("manifest.json"))
            .and_then(|m| m.modified())
            .ok();
        if stamp.is_none() || stamp == self.stamp {
            return Ok(());
        }
        let mut requests = RenderAssetRequests::default();
        if let Err(error) = load_manifest(&DirectorySource(self.root.clone()), &mut requests) {
            eprintln!("[render] retaining previous assets: {error}");
            self.stamp = stamp;
            return Ok(());
        }
        if let Some(queue) = engine.world_mut().get_resource_mut::<RenderAssetRequests>() {
            *queue = requests;
        }
        self.stamp = stamp;
        Ok(())
    }
}
fn failure(e: impl std::fmt::Display) -> RendererError {
    RendererError::Assets {
        detail: e.to_string(),
    }
}
impl Drop for NativeAssets {
    fn drop(&mut self) {
        if let Some(child) = self.worker.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
