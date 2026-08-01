//! the one place a failure turns into a question.
//!
//! the transports below this already retry what is mechanically retryable —
//! yamuse retries idempotent gets, [`crate::spotify`] honours `Retry-After`.
//! what reaches here has survived that, so it is a real failure and the only
//! remaining choices are human ones: try again, drop this item, or stop.

use crate::error::{Error, Result};
use crate::ui::{Recovery, Ui};

/// run `op`, asking the user what to do if it fails.
///
/// `Ok(None)` means the item was skipped and the caller should carry on with
/// the rest; [`Error::Aborted`] means the user asked to stop.
/// generic over the error type so a `yamuse::Result` can be passed straight in
/// without a `map_err` at every one of the two dozen call sites.
pub async fn guarded<T, E, F, Fut>(ui: &Ui, what: &str, mut op: F) -> Result<Option<T>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
    E: Into<Error>,
{
    loop {
        let error: Error = match op().await {
            Ok(value) => return Ok(Some(value)),
            Err(e) => e.into(),
        };

        // a prompt that cannot be answered is not a recovery path. the same
        // goes for an abort already in flight: asking about it would be asking
        // the user to confirm their own decision.
        if matches!(error, Error::Aborted | Error::Prompt(_)) {
            return Err(error);
        }

        match ui.recover(what, &error)? {
            Recovery::Retry => tracing::warn!(what, %error, "retrying at the user's request"),
            Recovery::Skip => {
                tracing::error!(what, %error, "skipped");
                return Ok(None);
            }
            Recovery::Abort => return Err(Error::Aborted),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Ambiguous;
    use std::cell::Cell;

    #[tokio::test]
    async fn a_call_that_works_first_time_never_reaches_the_prompt() {
        let ui = Ui::new(false, Ambiguous::Ask);
        let got = guarded(&ui, "x", || async { Ok::<_, Error>(7) })
            .await
            .unwrap();
        assert_eq!(got, Some(7));
    }

    #[tokio::test]
    async fn a_persistent_failure_is_skipped_when_nobody_can_be_asked() {
        let ui = Ui::new(false, Ambiguous::Ask);
        let calls = Cell::new(0);

        let got: Option<()> = guarded(&ui, "x", || {
            calls.set(calls.get() + 1);
            async { Err(Error::Config("boom".into())) }
        })
        .await
        .unwrap();

        assert!(got.is_none());
        // exactly once: the non-interactive path must not spin.
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn an_abort_travels_straight_out_instead_of_prompting_about_itself() {
        let ui = Ui::new(true, Ambiguous::Ask);
        let err = guarded(&ui, "x", || async { Err::<(), _>(Error::Aborted) })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Aborted));
    }
}
