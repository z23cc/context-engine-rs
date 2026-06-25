//! The `wechat.*` command executor for the [`JobManager`].
//!
//! Delegates to the daemon-hosted [`WechatHost`](crate::wechat): `wechat.login` runs
//! the cancellable QR flow on this job thread; `wechat.start` requires
//! `--allow-delegate` + a served `--root` (it drives delegated agents) and spawns the
//! bridge on its own thread; `stop` / `status` are immediate. An `impl JobManager`
//! method so it reaches the manager's private bridge handle / launcher / lift flag.

use super::JobManager;
use nerve_core::CancelToken;
use nerve_runtime::RuntimeCommand;
use serde_json::Value;
use std::sync::Arc;

impl JobManager {
    /// Execute a `wechat.*` command, delegating to the daemon-hosted [`WechatHost`]
    /// (`crate::wechat`). `wechat.login` runs the cancellable QR flow on this job
    /// thread; `wechat.start` requires `--allow-delegate` + a served `--root` (it
    /// drives delegated agents) and spawns the bridge on its own thread; `stop` /
    /// `status` are immediate.
    pub(super) fn run_wechat_command(
        &self,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::WechatLogin { bot_type, base_url } => {
                self.wechat.login(&bot_type, base_url.as_deref(), token)
            }
            RuntimeCommand::WechatStart {
                owners,
                agent,
                autonomy,
            } => {
                if !self.allow_delegate {
                    return Err(nerve_runtime::RuntimeError::adapter(
                        "the WeChat bridge drives delegated agents — start the daemon with \
                         --allow-delegate",
                    ));
                }
                let root = self.delegate_root()?;
                self.wechat.start(
                    Arc::clone(&self.delegate_launcher),
                    root,
                    owners,
                    agent,
                    autonomy,
                )
            }
            RuntimeCommand::WechatStop => self.wechat.stop(),
            RuntimeCommand::WechatStatus => Ok(self.wechat.status()),
            other => Err(nerve_runtime::RuntimeError::adapter(format!(
                "expected a wechat.* command, got {}",
                other.name()
            ))),
        }
    }
}
