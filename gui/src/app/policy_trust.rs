use super::{AppState, HeldPaths, Inner, WriterView, run_status, wait_apply, writer_view};
use dotman_gui::policy_trust::{Action, PolicyReview};
use dotman_gui::{protocol, runner};
use std::collections::BTreeSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn redacted(report: &protocol::StatusReport) -> BTreeSet<String> {
    let paths = HeldPaths::new(report);
    report
        .entries
        .iter()
        .filter(|entry| paths.matches(entry, true))
        .map(|entry| entry.key.raw.clone())
        .collect()
}

fn reserve(
    inner: &mut Inner,
    id: &str,
    flag: Arc<AtomicBool>,
) -> Result<protocol::StatusReport, &'static str> {
    if inner.active.is_some() || inner.recovery.is_some() {
        return Err("Another operation or terminal recovery is pending");
    }
    if inner.generation == u64::MAX || id != format!("policy:{}", inner.generation) {
        return Err("Policy review is stale");
    }
    let Some(protocol::StatusReply::Report(report)) = inner.reply.take() else {
        return Err("Status is unavailable");
    };
    inner.review = None;
    inner.diff_receipts.clear();
    inner.generation = inner.generation.saturating_add(1);
    inner.active = Some(flag);
    Ok(report)
}

fn after_confirmation<T>(
    cancelled: &AtomicBool,
    approved: bool,
    start: impl FnOnce() -> Result<T, &'static str>,
) -> Result<Option<T>, &'static str> {
    if cancelled.load(Ordering::Acquire) || !approved {
        return Ok(None);
    }
    start().map(Some)
}

fn preflight(
    report: &protocol::StatusReport,
    action: Action,
    flag: &AtomicBool,
) -> Result<PolicyReview, WriterView> {
    if flag.load(Ordering::Acquire) {
        return Err(writer_view("cancelled", "Policy review cancelled.", true));
    }
    let mut run = runner::start_policy_preflight(&report.context, action)
        .map_err(|_| writer_view("error", "Could not start policy review.", true))?;
    let receipt = wait_apply(&mut run, flag)?;
    PolicyReview::parse(
        &receipt.stdout,
        receipt.status.code(),
        report,
        action,
        &redacted(report),
    )
    .map_err(|_| {
        writer_view(
            "attention",
            "Policy review is unavailable or cannot be shown safely. Refresh or use a terminal.",
            true,
        )
    })
}

const fn unverified() -> WriterView {
    writer_view(
        "attention",
        "Policy approval could not be verified. Refresh status before continuing.",
        true,
    )
}

#[tauri::command]
pub async fn review_policy(
    id: String,
    action: Action,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    let flag = Arc::new(AtomicBool::new(false));
    let report = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        reserve(&mut inner, &id, flag.clone())?
    };
    let shared = state.inner.clone();
    let cancelled = flag.clone();
    let uncertain = Arc::new(AtomicBool::new(false));
    let worker_uncertain = uncertain.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || -> Result<WriterView, String> {
        let review = match preflight(&report, action, &cancelled) {
            Ok(review) if review.eligible() => review,
            Ok(_) => {
                return Ok(writer_view(
                    "done",
                    "No local policy approval is pending.",
                    true,
                ));
            }
            Err(view) => return Ok(view),
        };
        let text = review.modal()?;
        let approved = crate::confirm::ask(
            &window,
            "Approve local capture policy?",
            "Approve capture policy",
            text,
            cancelled.clone(),
        )?;
        let mut run = {
            let guard = shared.lock().map_err(|_| "Status state unavailable")?;
            let run = after_confirmation(&cancelled, approved, || {
                runner::start_policy_writer(&review, action).map_err(|_| {
                    worker_uncertain.store(true, Ordering::Release);
                    "Could not start policy writer; check status in a terminal"
                })
            })?;
            let Some(run) = run else {
                return Ok(if cancelled.load(Ordering::Acquire) {
                    writer_view("cancelled", "Policy review cancelled before writing.", true)
                } else {
                    writer_view("declined", "Policy approval declined.", true)
                });
            };
            drop(guard);
            run
        };
        let receipt = match wait_apply(&mut run, &cancelled) {
            Ok(receipt) => receipt,
            Err(view) => return Ok(view),
        };
        if !review.receipt_matches(
            &receipt.stdout,
            receipt.status.code(),
            &report,
            &redacted(&report),
        ) {
            return Ok(unverified());
        }
        let Ok(protocol::StatusReply::Report(after)) = run_status(&cancelled) else {
            return Ok(unverified());
        };
        let next = match preflight(&after, action, &cancelled) {
            Ok(next) => next,
            Err(view) if view.outcome == "cleanup_unresolved" => return Ok(view),
            Err(_) => return Ok(unverified()),
        };
        if !review.verified(&after, &next) {
            return Ok(unverified());
        }
        Ok(writer_view(
            "done",
            "Local policy approved. Review a future sync to capture files.",
            true,
        ))
    })
    .await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    let Ok(result) = worker else {
        return Err("Policy worker failed; check the terminal before continuing".into());
    };
    if !uncertain.load(Ordering::Acquire)
        && !matches!(&result, Ok(view) if view.outcome == "cleanup_unresolved")
    {
        inner.active = None;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::after_confirmation;
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn denied_or_cancelled_confirmation_never_starts_writer() {
        let starts = Cell::new(0_usize);
        for (cancelled, approved) in [(false, false), (true, true)] {
            let result = after_confirmation(&AtomicBool::new(cancelled), approved, || {
                starts.set(starts.get().saturating_add(1));
                Ok::<_, &'static str>(())
            });
            assert!(matches!(result, Ok(None)));
        }
        assert_eq!(starts.get(), 0);
    }
}
