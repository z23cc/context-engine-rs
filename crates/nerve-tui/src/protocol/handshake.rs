//! `runtime/info` handshake validation — the Rust mirror of the TS client's
//! `validateRuntimeInfo`. Asserts the daemon speaks the exact protocol this
//! client was compiled against (name `nerve-runtime`, version `3`, the event
//! method, and all four job methods).

use nerve_runtime::protocol::{
    RUNTIME_EVENT_METHOD, RUNTIME_JOB_METHODS, RUNTIME_PROTOCOL_NAME, RUNTIME_PROTOCOL_VERSION,
    RuntimeInfo,
};

/// Validate a [`RuntimeInfo`] handshake or return a descriptive error. Pure.
pub fn validate_runtime_info(info: &RuntimeInfo) -> anyhow::Result<()> {
    if info.protocol != RUNTIME_PROTOCOL_NAME {
        anyhow::bail!(
            "unsupported runtime protocol: {} (expected {RUNTIME_PROTOCOL_NAME})",
            info.protocol
        );
    }
    if info.protocol_version != RUNTIME_PROTOCOL_VERSION {
        anyhow::bail!(
            "unsupported runtime protocol version: {} (expected {RUNTIME_PROTOCOL_VERSION})",
            info.protocol_version
        );
    }
    if info.capabilities.events.method != RUNTIME_EVENT_METHOD {
        anyhow::bail!(
            "unsupported runtime event method: {} (expected {RUNTIME_EVENT_METHOD})",
            info.capabilities.events.method
        );
    }
    for method in RUNTIME_JOB_METHODS {
        if !info.capabilities.jobs.methods.iter().any(|m| m == method) {
            anyhow::bail!("missing runtime job method: {method}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_runtime::protocol::RuntimeInfo;

    #[test]
    fn current_info_validates() {
        let info = RuntimeInfo::current("nerve", "0.0.0");
        validate_runtime_info(&info).expect("current info should validate");
    }

    #[test]
    fn wrong_protocol_name_fails() {
        let mut info = RuntimeInfo::current("nerve", "0.0.0");
        info.protocol = "not-nerve".to_string();
        let err = validate_runtime_info(&info).expect_err("should fail");
        assert!(err.to_string().contains("unsupported runtime protocol"));
    }

    #[test]
    fn wrong_version_fails() {
        let mut info = RuntimeInfo::current("nerve", "0.0.0");
        info.protocol_version = "2".to_string();
        let err = validate_runtime_info(&info).expect_err("should fail");
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn missing_job_method_fails() {
        let mut info = RuntimeInfo::current("nerve", "0.0.0");
        info.capabilities.jobs.methods.clear();
        let err = validate_runtime_info(&info).expect_err("should fail");
        assert!(err.to_string().contains("missing runtime job method"));
    }
}
