use tokio_util::sync::CancellationToken;

use std::ops::ControlFlow;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use probe_rs::{Core, Error, HaltReason, VectorCatchCondition};
use probe_rs::architecture::arm::{ArmError, DapError};
use probe_rs::probe::DebugProbeError;

use crate::rpc::{ObjectStorage, SessionState};

/// Duration to wait between retry attempts when a transient probe error occurs.
const PROBE_RECONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Returns `true` if the error looks like a transient probe communication failure that
/// may be caused by a hardware reset (e.g. the user pressing the reset button).
///
/// Such errors are expected to resolve themselves once the target comes out of reset, so
/// the run loop should retry rather than treating them as fatal.
fn is_transient_probe_error(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        if cause
            .downcast_ref::<Error>()
            .is_some_and(|e| matches!(e, Error::Timeout))
        {
            return true;
        }
        if cause
            .downcast_ref::<ArmError>()
            .is_some_and(|e| matches!(e, ArmError::Timeout | ArmError::Dap(_)))
        {
            return true;
        }
        if cause
            .downcast_ref::<DapError>()
            .is_some_and(|e| matches!(e, DapError::NoAcknowledge | DapError::FaultResponse))
        {
            return true;
        }
        // Probe-specific errors (e.g. ST-Link SwdApFault) that surface during active monitoring
        // are treated as transient because probe initialisation errors are caught much earlier.
        if cause
            .downcast_ref::<DebugProbeError>()
            .is_some_and(|e| matches!(e, DebugProbeError::ProbeSpecific(_)))
        {
            return true;
        }
    }
    false
}

pub struct RunLoop {
    pub core_id: usize,
    pub cancellation_token: CancellationToken,
}

#[derive(PartialEq, Debug)]
pub enum ReturnReason<R> {
    /// The predicate requested a return
    Predicate(R),
    /// Timeout elapsed
    Timeout,
    /// Cancelled
    Cancelled,
    /// The core locked up
    LockedUp,
}

impl RunLoop {
    /// Attaches to RTT and runs the core until it halts.
    ///
    /// Upon halt the predicate is invoked with the halt reason:
    /// * If the predicate returns `Ok(Some(r))` the run loop returns `Ok(ReturnReason::Predicate(r))`.
    /// * If the predicate returns `Ok(None)` the run loop will continue running the core.
    /// * If the predicate returns `Err(e)` the run loop will return `Err(e)`.
    ///
    /// The function will also return on timeout with `Ok(ReturnReason::Timeout)` or if the user presses CTRL + C with `Ok(ReturnReason::User)`.
    pub fn run_until<F, R>(
        &mut self,
        shared_session: &SessionState<'_>,
        catch_hardfault: bool,
        catch_reset: bool,
        mut poller: impl RunLoopPoller,
        timeout: Option<Duration>,
        mut predicate: F,
    ) -> Result<ReturnReason<R>>
    where
        F: FnMut(HaltReason, &mut Core) -> Result<Option<R>>,
    {
        // Prepare run loop
        {
            let mut session = shared_session.session_blocking();
            let mut core = session.core(self.core_id)?;
            if catch_hardfault || catch_reset {
                if !core.core_halted()? {
                    core.halt(Duration::from_millis(100))?;
                }

                if catch_hardfault {
                    match core.enable_vector_catch(VectorCatchCondition::HardFault) {
                        Ok(_) | Err(Error::NotImplemented(_)) => {} // Don't output an error if vector_catch hasn't been implemented
                        Err(e) => tracing::error!("Failed to enable_vector_catch: {:?}", e),
                    }
                }
                if catch_reset {
                    match core.enable_vector_catch(VectorCatchCondition::CoreReset) {
                        Ok(_) | Err(Error::NotImplemented(_)) => {} // Don't output an error if vector_catch hasn't been implemented
                        Err(e) => tracing::error!("Failed to enable_vector_catch: {:?}", e),
                    }
                }
            }

            let object_storage = shared_session.object_storage();
            poller.start(&object_storage, &mut core)?;

            if core.core_halted()? {
                core.run()?;
            }
        }

        let result = self.do_run_until(shared_session, &mut poller, timeout, &mut predicate);

        // Clean up run loop
        let mut session = shared_session.session_blocking();
        match session.core(self.core_id) {
            Ok(mut core) => {
                let object_storage = shared_session.object_storage();
                // Always clean up after RTT but don't overwrite the original result.
                let poller_exit_result = poller.exit(&object_storage, &mut core);
                if result.is_ok() {
                    // If the result is Ok, we return the potential error during cleanup.
                    poller_exit_result?;
                }
            }
            Err(e) => {
                let wrapped = anyhow::Error::from(e);
                if result.is_ok() && !is_transient_probe_error(&wrapped) {
                    // Only propagate the cleanup error if the original result was OK and the
                    // error is not a transient probe communication issue.
                    return Err(wrapped);
                }
                // Otherwise, best-effort cleanup: if the probe is unreachable (e.g. during a
                // hardware reset when the user presses Ctrl+C), just skip cleanup.
                tracing::debug!("Skipping RTT cleanup: probe not reachable ({wrapped:#})");
            }
        }

        result
    }

    fn do_run_until<F, R>(
        &mut self,
        shared_session: &SessionState<'_>,
        poller: &mut impl RunLoopPoller,
        timeout: Option<Duration>,
        predicate: &mut F,
    ) -> Result<ReturnReason<R>>
    where
        F: FnMut(HaltReason, &mut Core) -> Result<Option<R>>,
    {
        let start = Instant::now();
        // Tracks when the first transient probe error occurred in a streak, for logging.
        let mut transient_error_start: Option<Instant> = None;

        loop {
            match self.poll_once(shared_session, poller, predicate) {
                Ok(ControlFlow::Break(reason)) => {
                    if transient_error_start.is_some() {
                        tracing::info!("Probe connection recovered after hardware reset.");
                    }
                    return Ok(reason);
                }
                Ok(ControlFlow::Continue(next_poll)) => {
                    if transient_error_start.is_some() {
                        tracing::info!("Probe connection recovered after hardware reset.");
                        transient_error_start = None;
                    }
                    if let Some(timeout) = timeout
                        && start.elapsed() >= timeout
                    {
                        return Ok(ReturnReason::Timeout);
                    }

                    // If the polling frequency is too high, the USB connection to the probe
                    // can become unstable. Hence we only poll as little as necessary.
                    thread::sleep(next_poll);
                }
                Err(error) if is_transient_probe_error(&error) => {
                    // Log a single warning when the error streak begins, then keep retrying
                    // silently until the target reconnects (e.g. after a hardware reset).
                    if transient_error_start.is_none() {
                        transient_error_start = Some(Instant::now());
                        tracing::warn!(
                            "Probe communication error, possibly caused by a hardware reset: {error:#}"
                        );
                        tracing::info!(
                            "Waiting for the target to reconnect. Press Ctrl+C to stop."
                        );
                    }
                    thread::sleep(PROBE_RECONNECT_RETRY_INTERVAL);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn poll_once<F, R>(
        &self,
        shared_session: &SessionState<'_>,
        poller: &mut impl RunLoopPoller,
        predicate: &mut F,
    ) -> Result<ControlFlow<ReturnReason<R>, Duration>>
    where
        F: FnMut(HaltReason, &mut Core) -> Result<Option<R>>,
    {
        let mut session = shared_session.session_blocking();
        let mut core = session.core(self.core_id)?;

        let mut next_poll = Duration::from_millis(100);
        let object_storage = shared_session.object_storage();

        // check for halt first, poll rtt after.
        // this is important so we do one last poll after halt, so we flush all messages
        // the core printed before halting, such as a panic message.
        let return_reason = match core.status()? {
            probe_rs::CoreStatus::Halted(reason) => match predicate(reason, &mut core) {
                Ok(Some(r)) => Some(Ok(ReturnReason::Predicate(r))),
                Err(e) => Some(Err(e)),
                Ok(None) => {
                    // Re-poll immediately if the core was halted, to speed up reading strings
                    // from semihosting. The core is not expected to be halted for other reasons.
                    next_poll = Duration::ZERO;
                    core.run()?;
                    None
                }
            },
            probe_rs::CoreStatus::Running
            | probe_rs::CoreStatus::Sleeping
            | probe_rs::CoreStatus::Unknown => {
                // Carry on
                None
            }

            probe_rs::CoreStatus::LockedUp => Some(Ok(ReturnReason::LockedUp)),
        };

        let poller_result = poller.poll(&object_storage, &mut core);

        if let Some(reason) = return_reason {
            return reason.map(ControlFlow::Break);
        }
        if self.cancellation_token.is_cancelled() {
            return Ok(ControlFlow::Break(ReturnReason::Cancelled));
        }
        match poller_result {
            Ok(delay) => next_poll = next_poll.min(delay),
            Err(error) => return Err(error),
        }

        Ok(ControlFlow::Continue(next_poll))
    }
}

pub trait RunLoopPoller {
    fn start(&mut self, objs: &ObjectStorage, core: &mut Core<'_>) -> Result<()>;
    fn poll(&mut self, objs: &ObjectStorage, core: &mut Core<'_>) -> Result<Duration>;
    fn exit(&mut self, objs: &ObjectStorage, core: &mut Core<'_>) -> Result<()>;
}

pub struct NoopPoller;

impl RunLoopPoller for NoopPoller {
    fn start(&mut self, _: &ObjectStorage, _core: &mut Core<'_>) -> Result<()> {
        Ok(())
    }

    fn poll(&mut self, _: &ObjectStorage, _core: &mut Core<'_>) -> Result<Duration> {
        Ok(Duration::from_secs(u64::MAX))
    }

    fn exit(&mut self, _: &ObjectStorage, _core: &mut Core<'_>) -> Result<()> {
        Ok(())
    }
}

impl<T> RunLoopPoller for Option<T>
where
    T: RunLoopPoller,
{
    fn start(&mut self, objs: &ObjectStorage, core: &mut Core<'_>) -> Result<()> {
        if let Some(poller) = self {
            poller.start(objs, core)
        } else {
            NoopPoller.start(objs, core)
        }
    }

    fn poll(&mut self, objs: &ObjectStorage, core: &mut Core<'_>) -> Result<Duration> {
        if let Some(poller) = self {
            poller.poll(objs, core)
        } else {
            NoopPoller.poll(objs, core)
        }
    }

    fn exit(&mut self, objs: &ObjectStorage, core: &mut Core<'_>) -> Result<()> {
        if let Some(poller) = self {
            poller.exit(objs, core)
        } else {
            NoopPoller.exit(objs, core)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_error_detects_no_acknowledge() {
        let err = anyhow::Error::from(probe_rs::Error::Arm(ArmError::Dap(
            DapError::NoAcknowledge,
        )));
        assert!(is_transient_probe_error(&err));
    }

    #[test]
    fn transient_error_detects_fault_response() {
        let err = anyhow::Error::from(probe_rs::Error::Arm(ArmError::Dap(
            DapError::FaultResponse,
        )));
        assert!(is_transient_probe_error(&err));
    }

    #[test]
    fn transient_error_detects_arm_timeout() {
        let err = anyhow::Error::from(probe_rs::Error::Arm(ArmError::Timeout));
        assert!(is_transient_probe_error(&err));
    }

    #[test]
    fn transient_error_detects_probe_rs_timeout() {
        let err = anyhow::Error::from(probe_rs::Error::Timeout);
        assert!(is_transient_probe_error(&err));
    }

    #[test]
    fn transient_error_does_not_match_generic_error() {
        let err = anyhow::anyhow!("something unrelated went wrong");
        assert!(!is_transient_probe_error(&err));
    }

    #[test]
    fn transient_error_does_not_match_core_not_found() {
        let err = anyhow::Error::from(probe_rs::Error::CoreNotFound(0));
        assert!(!is_transient_probe_error(&err));
    }
}
