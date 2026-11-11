// SPDX-License-Identifier: BSD-2-Clause
//! The request states, cancellation, `aio_fsync`'s order, and
//! `lio_listio`'s counting, as vfs_aio.c has them.

use super::*;

const CANCELED: Option<Errno> = Some(Errno::ECANCELED);

fn kaio_with(jobs: &[(JobId, u64, usize, bool)]) -> Kaio {
    let mut k = Kaio::new();
    for &(id, ujob, file, sync) in jobs {
        k.begin(id, ujob, 3, file, sync, None);
        assert!(k.queued(id).is_none());
    }
    k
}

#[test]
fn queued_then_finished_then_returned() {
    let mut k = kaio_with(&[(1, 0x1000, 7, false)]);
    assert_eq!(k.count(), 1);
    assert_eq!(k.error_of(0x1000), Some(Errno::EINPROGRESS.get()));
    assert_eq!(k.suspend(&[0x1000]), Suspend::Wait);
    let d = k.complete(1, 512, None).expect("completes");
    assert!(d.notify);
    assert_eq!(d.lio_done, None);
    assert_eq!(k.error_of(0x1000), Some(0));
    assert_eq!(k.suspend(&[0x2000, 0x1000]), Suspend::Ready);
    assert!(k.complete(1, 0, None).is_none(), "only once");
    let (j, lio) = k.take_done(Some(0x1000)).expect("aio_return");
    assert_eq!((j.status, j.error, lio), (512, None, None));
    assert_eq!(k.count(), 0);
    assert_eq!(k.error_of(0x1000), None);
    assert!(
        k.take_done(Some(0x1000)).is_none(),
        "EINVAL the second time"
    );
    assert_eq!(k.suspend(&[0x1000]), Suspend::NoJobs);
}

#[test]
fn a_request_finished_while_queueing_completes_when_queued() {
    let mut k = Kaio::new();
    k.begin(1, 0x10, 3, 1, false, None);
    // Not visible yet.
    assert_eq!(k.error_of(0x10), None);
    assert!(k.complete(1, 8, None).is_none(), "deferred");
    let d = k.queued(1).expect("the deferred completion");
    assert_eq!(d.id, 1);
    assert_eq!(k.error_of(0x10), Some(0));
    assert!(k.idle());
}

#[test]
fn abandoned_requests_leave_no_trace() {
    let mut k = Kaio::new();
    k.begin(1, 0x10, 3, 1, false, None);
    k.abandon(1);
    assert_eq!(k.count(), 0);
    assert!(k.get(1).is_none());
    assert!(k.idle());
}

#[test]
fn waitcomplete_takes_the_first_finished() {
    let mut k = kaio_with(&[
        (1, 0x10, 1, false),
        (2, 0x20, 1, false),
        (3, 0x30, 1, false),
    ]);
    k.complete(2, 2, None);
    k.complete(1, 1, None);
    assert_eq!(k.take_done(None).map(|(j, _)| j.ujob), Some(0x20));
    assert_eq!(k.take_done(None).map(|(j, _)| j.ujob), Some(0x10));
    assert!(k.take_done(None).is_none());
    assert_eq!(k.count(), 1);
}

#[test]
fn cancelling_a_waiting_request() {
    let mut k = kaio_with(&[(1, 0x10, 1, false)]);
    assert!(k.set_cancel(1, Backend::Daemon));
    assert_eq!(k.cancel_targets(3, None), [1]);
    assert_eq!(k.cancel_begin(1), CancelStep::Call(Backend::Daemon));
    // The hook cancels it; the completion waits for the cancel to end.
    assert!(k.complete(1, -1, CANCELED).is_none());
    let (cancelled, done) = k.cancel_end(1);
    assert!(cancelled);
    assert!(done.is_some());
    assert_eq!(k.error_of(0x10), Some(Errno::ECANCELED.get()));
    // A second try finds it done.
    assert_eq!(k.cancel_begin(1), CancelStep::Skip);
}

#[test]
fn a_running_request_is_not_cancelled() {
    let mut k = kaio_with(&[(1, 0x10, 1, false)]);
    assert!(k.set_cancel(1, Backend::Daemon));
    // A worker takes it.
    assert!(k.clear_cancel(1));
    assert_eq!(k.cancel_begin(1), CancelStep::Marked);
    assert_eq!(k.cancel_begin(1), CancelStep::Skip, "already marked");
    assert!(k.complete(1, 4, None).is_some(), "it finishes normally");
}

#[test]
fn a_backend_finds_the_cancel_first() {
    let mut k = kaio_with(&[(1, 0x10, 1, false)]);
    // Cancel asked before the backend set its hook: the backend must
    // cancel it itself.
    assert_eq!(k.cancel_begin(1), CancelStep::Marked);
    assert!(!k.set_cancel(1, Backend::Socket));
    // A worker racing a cancel that is running the hook loses.
    let mut k = kaio_with(&[(1, 0x10, 1, false)]);
    k.set_cancel(1, Backend::Daemon);
    assert_eq!(k.cancel_begin(1), CancelStep::Call(Backend::Daemon));
    assert!(!k.clear_cancel(1));
}

#[test]
fn a_partial_socket_write_cancelled_is_not_cancelled() {
    let mut k = kaio_with(&[(1, 0x10, 1, false)]);
    k.set_cancel(1, Backend::Socket);
    assert_eq!(k.cancel_begin(1), CancelStep::Call(Backend::Socket));
    // `soo_aio_cancel` completes it with what went.
    k.complete(1, 8192, None);
    let (cancelled, done) = k.cancel_end(1);
    assert!(!cancelled);
    assert!(done.is_some());
}

#[test]
fn cancel_targets_match_descriptor_and_aiocb() {
    let mut k = Kaio::new();
    k.begin(1, 0x10, 3, 1, false, None);
    k.queued(1);
    k.begin(2, 0x20, 4, 2, false, None);
    k.queued(2);
    k.begin(3, 0x30, 3, 1, false, None);
    k.queued(3);
    assert_eq!(k.cancel_targets(3, None), [1, 3]);
    assert_eq!(k.cancel_targets(3, Some(0x30)), [3]);
    assert_eq!(k.cancel_targets(3, Some(0x20)), [] as [JobId; 0]);
    k.complete(1, 0, None);
    assert_eq!(
        k.cancel_targets(3, None),
        [3],
        "finished ones are not tried"
    );
}

#[test]
fn fsync_waits_for_earlier_requests_on_its_file() {
    // Two writes on file 1, one on file 2, then an fsync of file 1.
    let mut k = kaio_with(&[
        (1, 0x10, 1, false),
        (2, 0x20, 2, false),
        (3, 0x30, 1, false),
    ]);
    k.begin(4, 0x40, 3, 1, true, None);
    assert_eq!(k.sync_deps(4), 2);
    assert!(k.set_cancel(4, Backend::Sync));
    assert!(k.queued(4).is_none());
    // A request after it is not waited for.
    k.begin(5, 0x50, 3, 1, false, None);
    k.queued(5);
    let d = k.complete(2, 0, None).unwrap();
    assert!(d.fsync_ready.is_empty(), "another file");
    let d = k.complete(3, 0, None).unwrap();
    assert!(d.fsync_ready.is_empty(), "one left");
    let d = k.complete(5, 0, None).unwrap();
    assert!(d.fsync_ready.is_empty(), "queued after");
    let d = k.complete(1, 0, None).unwrap();
    assert_eq!(d.fsync_ready, [4]);
    // It runs now: its hook is gone.
    assert_eq!(k.cancel_begin(4), CancelStep::Marked);
}

#[test]
fn fsync_with_nothing_before_it_runs_at_once() {
    let mut k = Kaio::new();
    k.begin(1, 0x10, 3, 1, true, None);
    assert_eq!(k.sync_deps(1), 0);
}

#[test]
fn fsync_does_not_wait_for_an_earlier_fsync() {
    let mut k = kaio_with(&[(1, 0x10, 1, true)]);
    k.begin(2, 0x20, 3, 1, true, None);
    assert_eq!(k.sync_deps(2), 0);
}

#[test]
fn a_cancelled_fsync_leaves_the_sync_queue() {
    let mut k = kaio_with(&[(1, 0x10, 1, false)]);
    k.begin(2, 0x20, 3, 1, true, None);
    assert_eq!(k.sync_deps(2), 1);
    k.set_cancel(2, Backend::Sync);
    k.queued(2);
    assert_eq!(k.cancel_begin(2), CancelStep::Call(Backend::Sync));
    k.unsync(2);
    k.complete(2, -1, CANCELED);
    assert!(k.cancel_end(2).0);
    let d = k.complete(1, 0, None).unwrap();
    assert!(d.fsync_ready.is_empty());
}

#[test]
fn lio_counts_its_requests() {
    let mut k = Kaio::new();
    let l = k.lio_new(true);
    for (id, u) in [(1, 0x10), (2, 0x20)] {
        k.begin(id, u, 3, 1, false, Some(l));
        k.queued(id);
    }
    // The extra count keeps a finish during submission from completing
    // the group.
    let d = k.complete(1, 0, None).unwrap();
    assert_eq!(d.lio_done, None);
    assert!(!k.lio_all_done(l));
    let d = k.complete(2, 0, None).unwrap();
    assert_eq!(d.lio_done, None, "the submission still holds it");
    assert!(k.lio_all_done(l));
    assert!(!k.lio_release(l), "two requests still hold it");
    assert!(k.lio_mut(l).is_some());
    k.take_done(Some(0x10));
    let (_, freed) = k.take_done(Some(0x20)).unwrap();
    assert_eq!(freed, Some(l), "the last request frees it");
    assert_eq!(k.lios(), 0);
}

#[test]
fn lio_finishing_after_submission() {
    let mut k = Kaio::new();
    let l = k.lio_new(false);
    k.begin(1, 0x10, 3, 1, false, Some(l));
    k.queued(1);
    assert!(!k.lio_all_done(l));
    assert!(!k.lio_release(l));
    let d = k.complete(1, 0, None).unwrap();
    assert_eq!(d.lio_done, Some(l));
}

#[test]
fn an_empty_lio_is_done_and_freed_at_once() {
    let mut k = Kaio::new();
    let l = k.lio_new(true);
    assert!(k.lio_all_done(l));
    assert!(k.lio_release(l));
    assert_eq!(k.lios(), 0);
}

#[test]
fn per_process_limits() {
    let mut k = kaio_with(&[(1, 0x10, 1, false), (2, 0x20, 1, false)]);
    assert!(k.admits(3));
    assert!(!k.admits(2));
    assert!(k.can_start(1));
    k.started();
    assert!(!k.can_start(1));
    k.stopped();
    assert!(k.can_start(1));
}

#[test]
fn rundown_silences_completions_and_frees_everything() {
    let mut k = kaio_with(&[(1, 0x10, 1, false), (2, 0x20, 1, false)]);
    k.complete(1, 0, None);
    k.set_cancel(2, Backend::Daemon);
    let to_cancel = k.begin_rundown();
    assert_eq!(to_cancel, [2]);
    assert_eq!(k.cancel_begin(2), CancelStep::Call(Backend::Daemon));
    k.complete(2, -1, CANCELED);
    let (_, d) = k.cancel_end(2);
    assert!(
        !d.unwrap().notify,
        "no signals or kevents while running down"
    );
    assert!(k.idle());
    assert_eq!(k.drain_done().len(), 2);
    assert_eq!(k.count(), 0);
}

#[test]
fn rundown_waits_for_running_requests() {
    let mut k = kaio_with(&[(1, 0x10, 1, false)]);
    k.started();
    k.begin_rundown();
    assert!(!k.idle());
    k.complete(1, 0, None);
    assert!(!k.idle(), "still on its worker");
    k.stopped();
    assert!(k.idle());
}
