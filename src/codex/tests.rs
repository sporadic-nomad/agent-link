//! Controlled protocol-state tests; the real pipe/dispatcher is covered separately.
use super::*;
use std::sync::Barrier;
use std::time::Duration;

#[path = "evidence_tests.rs"]
mod evidence;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}
fn harness(limits: CodexLimits) -> (CodexAppServer, CodexEvents) {
    harness_with_options(CodexOptions {
        limits,
        ..CodexOptions::default()
    })
}
fn harness_with_options(options: CodexOptions) -> (CodexAppServer, CodexEvents) {
    let limits = options.limits;
    validate_limits(limits).unwrap();
    let (wake, _) = bounded(1);
    let shared = Arc::new(Shared {
        generation: SessionId::next(),
        native_events: options.native_events,
        limits,
        state: Mutex::new(State::default()),
        events: Queue::new(limits.events, limits.event_bytes),
        wake,
        cleanup: Mutex::new(None),
        dispatcher_finished: std::sync::atomic::AtomicBool::new(false),
    });
    let events = CodexEvents(Arc::clone(&shared.events));
    (CodexAppServer { shared }, events)
}
fn ingress(app: &CodexAppServer, value: Value) {
    if let Err(failure) = driver::ingress(&app.shared, 7, &value.to_string()) {
        app.shared.stop(failure);
    }
}
fn publish(app: &CodexAppServer, ticket: &RequestTicket) {
    let mut state = lock(&app.shared.state);
    let p = state
        .pending
        .iter_mut()
        .find(|p| p.id.as_deref() == Some(ticket.id()))
        .unwrap();
    p.dispatched = true;
    p.line = None;
}
fn response(app: &CodexAppServer, ticket: &RequestTicket, result: Value) {
    publish(app, ticket);
    ingress(app, json!({"id": ticket.id(), "result":result}));
    assert!(matches!(
        &*ticket.outcome().unwrap(),
        RequestOutcome::Response(_)
    ));
}
fn ready(limits: CodexLimits) -> (CodexAppServer, CodexEvents) {
    ready_with_options(CodexOptions {
        limits,
        ..CodexOptions::default()
    })
}
fn ready_with_options(options: CodexOptions) -> (CodexAppServer, CodexEvents) {
    let (app, events) = harness_with_options(options);
    let ticket = app.initialize("test", "1", deadline()).unwrap();
    response(&app, &ticket, json!({"userAgent":"fixture", "future":true}));
    let ticket = app.thread_start(json!({}), deadline()).unwrap();
    response(
        &app,
        &ticket,
        json!({"thread":{"id":"t","turns":[]},"approvalPolicy":"never"}),
    );
    (app, events)
}

fn rpc_line(app: &CodexAppServer, seq: u64, value: Value) -> crate::rpc::RpcEvent {
    crate::rpc::RpcEvent::Line {
        id: app.generation(),
        seq,
        line: value.to_string(),
    }
}

#[test]
fn failed_receipt_preserves_buffered_native_response_and_terminal_turn() {
    use crate::rpc::{RpcEvent, RpcWriteFailure, RpcWriteOutcome};

    for pipe_error in [false, true] {
        for remote_error in [false, true] {
            let (app, events) = ready(CodexLimits::default());
            lock(&app.shared.state).pending.clear(); // completed setup writes
            let ticket = app.turn_start("x", deadline()).unwrap();
            let (tx, incoming) = bounded(5);
            let failure = Failure::Eof;
            let payload = if remote_error {
                json!({"code":-1,"message":"refused","data":{"future":true}})
            } else {
                json!({"turn":{"id":"a","status":"inProgress"},"future":true})
            };
            let native_response = if remote_error {
                json!({"id":ticket.id(),"error":payload})
            } else {
                json!({"id":ticket.id(),"result":payload})
            };
            let mut writes = 0;
            driver::run_loop(
                Arc::clone(&app.shared),
                incoming,
                crossbeam_channel::never(),
                |line, _| {
                    writes += 1;
                    assert_eq!(
                        serde_json::from_str::<Value>(line).unwrap()["id"],
                        ticket.id()
                    );
                    if pipe_error {
                        tx.try_send(RpcEvent::StdinError {
                            id: app.generation(),
                            kind: std::io::ErrorKind::BrokenPipe,
                        })
                        .unwrap();
                    }
                    // Receipt failure is already visible when the dispatcher resumes,
                    // but these native frames precede the transport's terminal event.
                    tx.try_send(rpc_line(
                        &app,
                        1,
                        json!({"method":"turn/started","params":{
                            "threadId":"t","turn":{"id":"a","status":"inProgress"}
                        }}),
                    ))
                    .unwrap();
                    tx.try_send(rpc_line(&app, 2, native_response.clone()))
                        .unwrap();
                    tx.try_send(rpc_line(
                        &app,
                        3,
                        json!({"method":"turn/completed","params":{
                            "threadId":"t","turn":{"id":"a","status":"completed"}
                        }}),
                    ))
                    .unwrap();
                    tx.try_send(RpcEvent::Exit {
                        id: app.generation(),
                        stderr_tail: String::new(),
                    })
                    .unwrap();
                    Ok(RpcWriteReceipt::finished_for_test(
                        RpcWriteOutcome::PossiblyWritten(if pipe_error {
                            RpcWriteFailure::Io(std::io::ErrorKind::BrokenPipe)
                        } else {
                            RpcWriteFailure::Closed
                        }),
                    ))
                },
            );
            match (&*ticket.outcome().unwrap(), remote_error) {
                (RequestOutcome::Response(value), false)
                | (RequestOutcome::RemoteError(value), true) => assert_eq!(**value, payload),
                (outcome, _) => panic!("buffered native response lost: {outcome:?}"),
            }
            assert_eq!(app.failure(), Some(failure));
            assert!(app.snapshot().active_turn.is_none());
            assert_eq!(
                app.snapshot().last_turn.unwrap().status,
                TurnStatus::Completed
            );
            assert_eq!(events.recv_until(deadline()).unwrap().seq, 1);
            assert_eq!(events.recv_until(deadline()).unwrap().seq, 3);
            assert!(events.recv_until(deadline()).is_none());
            assert_eq!(writes, 1, "failed writes must not be resent");
        }
    }
}

#[test]
fn closed_dispatch_preserves_buffered_notifications() {
    use crate::rpc::{RpcEvent, RpcWriteOutcome};

    let (app, events) = harness(CodexLimits::default());
    let ticket = app.initialize("test", "1", deadline()).unwrap();
    let (tx, incoming) = bounded(3);
    let mut writes = 0;
    driver::run_loop(
        Arc::clone(&app.shared),
        incoming,
        crossbeam_channel::never(),
        |line, _| {
            writes += 1;
            let request: Value = serde_json::from_str(line).unwrap();
            if writes == 1 {
                assert_eq!(request["method"], "initialize");
                tx.try_send(rpc_line(
                    &app,
                    1,
                    json!({"id":ticket.id(),"result":{"userAgent":"fixture"}}),
                ))
                .unwrap();
                tx.try_send(rpc_line(
                    &app,
                    2,
                    json!({"method":"extension","params":{"future":true}}),
                ))
                .unwrap();
                tx.try_send(RpcEvent::Exit {
                    id: app.generation(),
                    stderr_tail: String::new(),
                })
                .unwrap();
                Ok(RpcWriteReceipt::finished_for_test(RpcWriteOutcome::Written))
            } else {
                assert_eq!(request["method"], "initialized");
                Err(Error::Closed)
            }
        },
    );
    assert!(matches!(
        &*ticket.outcome().unwrap(),
        RequestOutcome::Response(_)
    ));
    let event = events
        .recv_until(deadline())
        .expect("buffered notification must survive a closed send");
    assert_eq!(event.seq, 2);
    assert_eq!(event.native.as_ref().unwrap()["method"], "extension");
    assert_eq!(app.failure(), Some(Failure::Eof));
    assert_eq!(writes, 2);
}

#[test]
fn failed_receipt_drain_keeps_absolute_deadlines_even_with_buffered_traffic() {
    use crate::rpc::{RpcWriteFailure, RpcWriteOutcome};

    for buffered in [false, true] {
        let (app, _events) = ready(CodexLimits::default());
        lock(&app.shared.state).pending.clear();
        let ticket = app.turn_start("x", deadline()).unwrap();
        let (tx, incoming) = bounded(2);
        let mut writes = 0;
        driver::run_loop(
            Arc::clone(&app.shared),
            incoming,
            crossbeam_channel::never(),
            |_, _| {
                writes += 1;
                // Advance the request's clock without sleeping; no terminal event will
                // arrive, and queued traffic must not extend this absolute deadline.
                lock(&app.shared.state).pending.back_mut().unwrap().deadline = Instant::now();
                if buffered {
                    tx.try_send(rpc_line(&app, 1, json!({"method":"extension"})))
                        .unwrap();
                    tx.try_send(rpc_line(
                        &app,
                        2,
                        json!({"id":ticket.id(),"result":{
                            "turn":{"id":"a","status":"inProgress"}
                        }}),
                    ))
                    .unwrap();
                }
                Ok(RpcWriteReceipt::finished_for_test(
                    RpcWriteOutcome::PossiblyWritten(RpcWriteFailure::Closed),
                ))
            },
        );
        assert_eq!(app.failure(), Some(Failure::DeadlineExceeded));
        assert!(matches!(
            &*ticket.outcome().unwrap(),
            RequestOutcome::OutcomeUnknown {
                failure: Failure::DeadlineExceeded,
                ..
            }
        ));
        assert_eq!(writes, 1);
    }
}

fn turn(app: &CodexAppServer, method: &str, id: &str, status: &str) {
    ingress(
        app,
        json!({"method":method,"params":{"threadId":"t","turn":{"id":id,"status":status,"items":[]}}}),
    );
}
fn approval(app: &CodexAppServer, events: &CodexEvents, id: Value) -> Approval {
    ingress(
        app,
        json!({"id":id,"method":"item/commandExecution/requestApproval",
        "params":{"threadId":"t","turnId":"a","itemId":"i", "availableDecisions":["decline","accept"]}}),
    );
    loop {
        let event = events.recv_until(deadline()).unwrap();
        if let Some(token) = event.approval {
            return token;
        }
    }
}

#[test]
fn other_threads_forward_in_order_without_changing_root_turn_or_approval() {
    let (app, events) = ready(CodexLimits::default());
    let ticket = app.turn_start("parent", deadline()).unwrap();
    publish(&app, &ticket);
    let token = approval(&app, &events, json!(1));
    let before = app.snapshot();
    // Deliberately collide with root identities: scope must include the thread.
    let notifications = [
        json!({"method":"thread/started","params":{"thread":{"id":"child"}}}),
        json!({"method":"turn/started","params":{"threadId":"child",
            "turn":{"id":"a","status":"inProgress"}}}),
        json!({"method":"item/agentMessage/delta","params":{"threadId":"child",
            "turnId":"a","itemId":"i","delta":"child output"}}),
        json!({"method":"serverRequest/resolved","params":{"threadId":"child","requestId":1}}),
        json!({"method":"turn/completed","params":{"threadId":"child",
            "turn":{"id":"a","status":"completed"}}}),
    ];
    for (seq, native) in notifications.iter().enumerate() {
        assert_eq!(
            driver::ingress(&app.shared, seq as u64, &native.to_string()),
            Ok(())
        );
        let event = events.recv_until(deadline()).unwrap();
        assert_eq!(event.seq, seq as u64);
        assert_eq!(*event.native.unwrap(), *native);
        assert!(event.approval.is_none());
        let after = app.snapshot();
        assert_eq!(after.thread_id, before.thread_id);
        assert_eq!(after.thread, before.thread);
        assert_eq!(after.active_turn, before.active_turn);
        assert!(after.last_turn.is_none());
        assert!(after.failure.is_none());
        assert!(ticket.outcome().is_none());
    }
    token.respond(json!("decline")).unwrap();
    response(
        &app,
        &ticket,
        json!({"turn":{"id":"a","status":"inProgress"}}),
    );
    assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
    assert!(matches!(
        app.turn_start("busy", deadline()),
        Err(CodexError::Busy)
    ));
    assert!(app.turn_steer("a", "more", deadline()).is_ok());
    turn(&app, "turn/completed", "a", "completed");
    let outcome = app.snapshot().last_turn.unwrap();
    assert_eq!(outcome.thread_id, "t");
    assert_eq!(outcome.turn_id, "a");
    assert!(app.snapshot().active_turn.is_none());
}

#[test]
fn unsolicited_notifications_cannot_choose_the_root_before_start_or_resume_response() {
    for resume in [false, true] {
        let (app, events) = harness(CodexLimits::default());
        let init = app.initialize("test", "1", deadline()).unwrap();
        response(&app, &init, json!({"userAgent":"fixture"}));
        for pending in [false, true] {
            let ticket = pending.then(|| {
                if resume {
                    app.thread_resume("t", json!({}), deadline()).unwrap()
                } else {
                    app.thread_start(json!({}), deadline()).unwrap()
                }
            });
            for native in [
                json!({"method":"thread/started","params":{"thread":{"id":"child"}}}),
                json!({"method":"turn/started","params":{"threadId":"child",
                    "turn":{"id":"a","status":"inProgress"}}}),
                json!({"method":"turn/completed","params":{"threadId":"child",
                    "turn":{"id":"a","status":"completed"}}}),
            ] {
                ingress(&app, native.clone());
                assert!(app.failure().is_none());
                assert_eq!(
                    *events.recv_until(deadline()).unwrap().native.unwrap(),
                    native
                );
            }
            assert_eq!(
                app.snapshot().thread_id.as_deref(),
                (resume && pending).then_some("t")
            );
            assert!(app.snapshot().active_turn.is_none());
            assert!(app.snapshot().last_turn.is_none());
            if let Some(ticket) = ticket {
                if resume {
                    turn(&app, "turn/started", "a", "inProgress");
                }
                response(&app, &ticket, json!({"thread":{"id":"t","turns":[]}}));
                assert_eq!(app.snapshot().thread_id.as_deref(), Some("t"));
                assert_eq!(app.snapshot().active_turn.as_deref(), resume.then_some("a"));
            }
        }
    }
}

#[test]
fn other_thread_approvals_are_explicitly_unsupported_without_changing_root() {
    for method in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
    ] {
        let (app, events) = ready(CodexLimits::default());
        let token = approval(&app, &events, json!(1));
        ingress(
            &app,
            json!({"id":"child-request","method":method,
            "params":{"threadId":"child","turnId":"different","itemId":"i"}}),
        );
        assert!(app.failure().is_none());
        let event = events.recv_until(deadline()).unwrap();
        assert_eq!(event.native.as_ref().unwrap()["id"], "child-request");
        assert!(event.approval.is_none());
        let state = lock(&app.shared.state);
        let reply: Value =
            serde_json::from_str(state.pending.back().unwrap().line.as_ref().unwrap()).unwrap();
        assert_eq!(reply["id"], "child-request");
        assert_eq!(reply["error"]["code"], -32601);
        assert!(reply.get("result").is_none());
        drop(state);
        assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
        token.respond(json!("decline")).unwrap();
    }
}

#[test]
fn other_thread_notifications_use_event_budgets_without_retaining_terminal_history() {
    let child = json!({"method":"turn/completed","params":{"threadId":"child",
        "turn":{"id":"a","status":"completed"}}});
    let (app, events) = ready(CodexLimits {
        events: 1,
        ..CodexLimits::default()
    });
    for id in 0..10 {
        let mut native = child.clone();
        native["params"]["turn"]["id"] = json!(format!("child-{id}"));
        ingress(&app, native.clone());
        assert!(app.failure().is_none());
        assert_eq!(
            *events.recv_until(deadline()).unwrap().native.unwrap(),
            native
        );
    }
    turn(&app, "turn/completed", "a", "completed");
    assert!(app.failure().is_none());
    assert_eq!(app.snapshot().last_turn.unwrap().thread_id, "t");
    for limits in [
        CodexLimits {
            events: 1,
            ..CodexLimits::default()
        },
        CodexLimits {
            // Enough for one frame and projection; insufficient for two frames.
            event_bytes: child.to_string().len() + 64,
            ..CodexLimits::default()
        },
    ] {
        let (app, _events) = ready(limits);
        ingress(&app, child.clone());
        assert!(app.failure().is_none());
        ingress(&app, child.clone());
        assert_eq!(app.failure(), Some(Failure::ConsumerLagged));
        assert!(app.snapshot().last_turn.is_none());
    }
}

#[test]
fn notification_routing_still_rejects_malformed_required_fields() {
    for thread in ["t", "child"] {
        for native in [
            json!({"method":"thread/started","params":{"thread":{"id":""}}}),
            json!({"method":"turn/started","params":{"threadId":thread,"turn":{"id":""}}}),
            json!({"method":"turn/started","params":{"threadId":thread,"turn":{"id":"a"}}}),
            json!({"method":"turn/completed","params":{"threadId":thread,
                "turn":{"id":"a","status":"inProgress"}}}),
            json!({"method":"serverRequest/resolved","params":{"threadId":thread,"requestId":null}}),
        ] {
            let (app, events) = ready(CodexLimits::default());
            ingress(&app, native);
            assert_eq!(app.failure(), Some(Failure::Protocol));
            assert!(events.recv_until(deadline()).is_none());
        }
    }
}

#[test]
fn response_before_observer_and_terminal_before_start_response_do_not_resurrect_turn() {
    let (app, events) = ready(CodexLimits::default());
    let ticket = app.turn_start("hello", deadline()).unwrap();
    publish(&app, &ticket);
    turn(&app, "turn/started", "a", "inProgress");
    turn(&app, "turn/completed", "a", "failed");
    response(
        &app,
        &ticket,
        json!({"turn":{"id":"a","status":"inProgress","items":[]}}),
    );
    assert_eq!(app.snapshot().active_turn, None);
    assert_eq!(app.snapshot().last_turn.unwrap().status, TurnStatus::Failed);
    assert_eq!(app.snapshot().thread.unwrap()["approvalPolicy"], "never");
    drop(events); // state is committed even if observation ends immediately.
    assert_eq!(app.snapshot().last_turn.unwrap().turn_id, "a");
}

#[test]
fn active_turn_and_expected_identity_guard_all_mutations() {
    let (app, _events) = ready(CodexLimits::default());
    let a = app.turn_start("one", deadline()).unwrap();
    assert!(matches!(
        app.turn_start("two", deadline()),
        Err(CodexError::Busy)
    ));
    response(&app, &a, json!({"turn":{"id":"a","status":"inProgress"}}));
    assert!(matches!(
        app.turn_start("two", deadline()),
        Err(CodexError::Busy)
    ));
    assert!(matches!(
        app.turn_steer("wrong", "hi", deadline()),
        Err(CodexError::TurnMismatch)
    ));
    assert!(matches!(
        app.turn_interrupt("wrong", deadline()),
        Err(CodexError::TurnMismatch)
    ));
    let steer = app.turn_steer("a", "more", deadline()).unwrap();
    response(&app, &steer, json!({"turnId":"a"}));
    turn(&app, "turn/completed", "a", "completed");
    let b = app.turn_start("two", deadline()).unwrap();
    response(&app, &b, json!({"turn":{"id":"b","status":"inProgress"}}));
    turn(&app, "turn/completed", "a", "completed");
    assert_eq!(app.snapshot().active_turn.as_deref(), Some("b"));
}

#[test]
fn admission_refuses_without_publication_and_control_keeps_reserved_capacity() {
    let (app, events) = ready(CodexLimits::default());
    // Retained admitted requests deliberately have no completed write receipt.
    app.shared.state.lock().unwrap().pending.clear();
    let token = approval(&app, &events, json!(1));
    turn(&app, "turn/started", "a", "inProgress");
    let ticket = app.turn_steer("a", "x", deadline()).unwrap();
    assert!(matches!(
        app.turn_start("x", deadline()),
        Err(CodexError::Busy)
    ));
    let interrupt = app.turn_interrupt("a", deadline()).unwrap();
    let reply = token.respond(json!("decline")).unwrap();
    app.close();
    assert!(matches!(
        &*ticket.outcome().unwrap(),
        RequestOutcome::NotSent(Failure::Closed)
    ));
    assert!(matches!(
        &*interrupt.outcome().unwrap(),
        RequestOutcome::NotSent(Failure::Closed)
    ));
    assert_eq!(
        *reply.outcome().unwrap(),
        ReplyOutcome::Unknown(Failure::Closed)
    );
}

#[test]
fn overload_is_sticky_and_settles_all_waiters_without_consumer_progress() {
    let (app, _events) = ready(CodexLimits {
        events: 1,
        ..CodexLimits::default()
    });
    let ticket = app.turn_start("x", deadline()).unwrap();
    publish(&app, &ticket);
    ingress(
        &app,
        json!({"method":"item/agentMessage/delta","params":{"delta":"first"}}),
    );
    ingress(
        &app,
        json!({"method":"extension","params":{"unknown":true}}),
    );
    assert_eq!(app.failure(), Some(Failure::ConsumerLagged));
    assert!(matches!(
        &*ticket.outcome().unwrap(),
        RequestOutcome::OutcomeUnknown {
            failure: Failure::ConsumerLagged,
            ..
        }
    ));
    app.close();
    assert_eq!(app.failure(), Some(Failure::ConsumerLagged));
}

#[test]
fn duplicate_unknown_and_old_generation_responses_never_match_new_requests() {
    let (app, _events) = ready(CodexLimits::default());
    let ticket = app.turn_start("x", deadline()).unwrap();
    publish(&app, &ticket);
    ingress(
        &app,
        json!({"id":format!("{}:1", app.generation().0 + 1),"result":{}}),
    );
    ingress(&app, json!({"id":-1,"result":{}}));
    assert!(ticket.outcome().is_none());
    response(
        &app,
        &ticket,
        json!({"turn":{"id":"a","status":"inProgress"}}),
    );
    ingress(
        &app,
        json!({"id":ticket.id(),"error":{"code":-1,"message":"private"}}),
    );
    assert!(matches!(
        &*ticket.outcome().unwrap(),
        RequestOutcome::Response(_)
    ));
}

#[test]
fn close_settles_an_observed_response_while_publication_is_paused() {
    for remote_error in [false, true] {
        let (app, _events) = ready(CodexLimits::default());
        let ticket = app.turn_start("x", deadline()).unwrap();
        publish(&app, &ticket);
        let payload = if remote_error {
            json!({"code":-1,"message":"refused","data":{"future":true}})
        } else {
            json!({"turn":{"id":"a","status":"inProgress"},"future":true})
        };
        let envelope = if remote_error {
            json!({"id":ticket.id(),"error":payload})
        } else {
            json!({"id":ticket.id(),"result":payload})
        };
        std::thread::scope(|scope| {
            let (paused, resume) = ticket.completion.pause_next_finish();
            let worker = scope.spawn(|| ingress(&app, envelope));
            paused
                .recv_deadline(deadline())
                .expect("response publication paused");
            assert!(ticket.outcome().is_none());

            app.close();
            let outcome = ticket.outcome().expect("close must settle every ticket");
            assert_eq!(app.failure(), Some(Failure::Closed));
            match (&*outcome, remote_error) {
                (RequestOutcome::Response(value), false)
                | (RequestOutcome::RemoteError(value), true) => assert_eq!(**value, payload),
                _ => panic!("close lost the observed native outcome: {outcome:?}"),
            }
            drop(resume);
            worker.join().unwrap();
            assert!(Arc::ptr_eq(&outcome, &ticket.outcome().unwrap()));
        });
    }
}

#[test]
fn close_settles_tickets_while_another_shutdown_publication_is_paused() {
    let (app, events) = ready(CodexLimits::default());
    let token = approval(&app, &events, json!(1));
    let sent = app.turn_steer("a", "x", deadline()).unwrap();
    publish(&app, &sent);
    let queued = app.turn_interrupt("a", deadline()).unwrap();
    let reply = token.respond(json!("decline")).unwrap();
    std::thread::scope(|scope| {
        let (paused, resume) = sent.completion.pause_next_finish();
        let worker = scope.spawn(|| app.shared.stop(Failure::Eof));
        paused
            .recv_deadline(deadline())
            .expect("shutdown publication paused");
        assert!(sent.outcome().is_none());

        app.close();
        let outcome = sent
            .outcome()
            .expect("concurrent close must settle tickets");
        assert!(matches!(
            &*outcome,
            RequestOutcome::OutcomeUnknown {
                failure: Failure::Eof,
                ..
            }
        ));
        assert!(matches!(
            &*queued.outcome().expect("queued ticket settled"),
            RequestOutcome::NotSent(Failure::Eof)
        ));
        assert_eq!(
            *reply.outcome().expect("reply settled"),
            ReplyOutcome::Unknown(Failure::Eof)
        );
        assert_eq!(app.failure(), Some(Failure::Eof));
        drop(resume);
        worker.join().unwrap();
        assert!(Arc::ptr_eq(&outcome, &sent.outcome().unwrap()));
    });
}

#[test]
fn cloned_approval_is_claimed_once_and_stale_id_cannot_retarget() {
    let (app, events) = ready(CodexLimits::default());
    let token = approval(&app, &events, json!(9));
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let token = token.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                token.respond(json!("accept")).is_ok()
            })
        })
        .collect();
    barrier.wait();
    assert_eq!(
        workers
            .into_iter()
            .map(|w| usize::from(w.join().unwrap()))
            .sum::<usize>(),
        1
    );
    let replacement = approval(&app, &events, json!(9));
    assert!(matches!(
        token.respond(json!("accept")),
        Err(CodexError::StaleApproval)
    ));
    assert!(replacement.respond(json!("decline")).is_ok());
}

#[test]
fn duplicate_replay_is_one_capability_and_resolved_or_completed_tokens_are_stale() {
    let (app, events) = ready(CodexLimits::default());
    let token = approval(&app, &events, json!("server-id"));
    ingress(
        &app,
        json!({"id":"server-id","method":"item/commandExecution/requestApproval",
        "params":{"threadId":"t","turnId":"a","itemId":"i","availableDecisions":["decline","accept"]}}),
    );
    assert!(events.recv_until(Instant::now()).is_none());
    ingress(
        &app,
        json!({"method":"serverRequest/resolved","params":{"threadId":"t","requestId":"server-id"}}),
    );
    assert!(matches!(
        token.respond(json!("accept")),
        Err(CodexError::StaleApproval)
    ));
    let token = approval(&app, &events, json!(2));
    turn(&app, "turn/completed", "a", "interrupted");
    assert!(matches!(
        token.respond(json!("accept")),
        Err(CodexError::StaleApproval)
    ));
}

#[test]
fn terminal_start_results_invalidate_approvals_without_a_completion_notification() {
    for (status, expected) in [
        ("completed", TurnStatus::Completed),
        ("interrupted", TurnStatus::Interrupted),
        ("failed", TurnStatus::Failed),
        ("futureStatus", TurnStatus::Unknown),
    ] {
        let (app, events) = ready(CodexLimits::default());
        let start = app.turn_start("x", deadline()).unwrap();
        publish(&app, &start);
        let command = approval(&app, &events, json!(1));
        let command_clone = command.clone();
        ingress(
            &app,
            json!({"id":"file","method":"item/fileChange/requestApproval",
                "params":{"threadId":"t","turnId":"a","itemId":"f"}}),
        );
        let file = events.recv_until(deadline()).unwrap().approval.unwrap();
        response(&app, &start, json!({"turn":{"id":"a","status":status}}));
        assert!(app.snapshot().active_turn.is_none());
        assert_eq!(app.snapshot().last_turn.unwrap().status, expected);
        let pending = lock(&app.shared.state).pending.len();
        for token in [command, command_clone] {
            assert!(matches!(
                token.respond(json!("accept")),
                Err(CodexError::StaleApproval)
            ));
        }
        // Advance any remaining approval deadlines without sleeping. A token whose
        // turn ended must neither enqueue a reply nor expire the connection later.
        for approval in lock(&app.shared.state).approvals.values_mut() {
            Arc::make_mut(&mut approval.request).deadline = Instant::now();
        }
        assert_eq!(
            driver::maintenance(&app.shared),
            Ok(driver::WriteState::Open)
        );
        assert!(matches!(
            file.respond(json!("accept")),
            Err(CodexError::StaleApproval)
        ));
        assert_eq!(lock(&app.shared.state).pending.len(), pending);
        assert!(app.failure().is_none());
        assert!(events.recv_until(Instant::now()).is_none());
        assert!(app.turn_start("next", deadline()).is_ok());
    }
}

#[test]
fn late_terminal_turn_preserves_the_active_turn_approval() {
    let (app, events) = ready(CodexLimits::default());
    turn(&app, "turn/completed", "old", "completed");
    let token = approval(&app, &events, json!(1));
    turn(&app, "turn/completed", "old", "completed");
    assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
    assert!(token.respond(json!("decline")).is_ok());
    assert!(app.failure().is_none());
}

#[test]
fn invalid_decision_never_grants_permission() {
    let (app, events) = ready(CodexLimits::default());
    let token = approval(&app, &events, json!(1));
    assert!(matches!(
        token.respond(json!("acceptForSession")),
        Err(CodexError::InvalidDecision)
    ));
    assert_eq!(app.failure(), Some(Failure::Protocol));
}

#[test]
fn expired_approval_is_stale_regardless_of_maintenance_order() {
    for maintenance_first in [false, true] {
        for decision in ["decline", "accept", "unoffered"] {
            let (app, events) = ready(CodexLimits::default());
            let token = approval(&app, &events, json!(1));
            let clone = token.clone();
            let pending = app.turn_steer("a", "pending", deadline()).unwrap();
            for a in lock(&app.shared.state).approvals.values_mut() {
                Arc::make_mut(&mut a.request).deadline = Instant::now();
            }
            if maintenance_first {
                app.shared
                    .stop(driver::maintenance(&app.shared).unwrap_err());
            }
            assert!(matches!(
                token.respond(json!(decision)),
                Err(CodexError::StaleApproval)
            ));
            assert_eq!(app.failure(), Some(Failure::ApprovalExpired));
            assert!(matches!(
                &*pending.wait_until(deadline()).unwrap(),
                RequestOutcome::NotSent(Failure::ApprovalExpired)
            ));
            assert!(matches!(
                clone.respond(json!("decline")),
                Err(CodexError::StaleApproval)
            ));
            assert!(lock(&app.shared.state).pending.is_empty());
            assert!(events.recv_until(Instant::now()).is_none());
        }
    }
}

#[test]
fn metadata_only_resume_is_refused_without_consuming_admission_or_binding_a_thread() {
    let (app, events) = harness(CodexLimits {
        pending_requests: 2,
        ..CodexLimits::default()
    });
    let init = app.initialize("test", "1", deadline()).unwrap();
    response(&app, &init, json!({"userAgent":"fixture"}));
    // Initialize retains one slot in this harness. Rejections must leave the other
    // slot and the one-thread binding available for a subsequent supported resume.
    for exclude_turns in [
        json!(true),
        json!(null),
        json!("true"),
        json!(1),
        json!({}),
        json!([]),
    ] {
        assert!(matches!(
            app.thread_resume("hidden", json!({"excludeTurns":exclude_turns}), deadline()),
            Err(CodexError::Transport(Error::InvalidInput(_)))
        ));
        assert!(app.snapshot().thread_id.is_none());
        assert!(app.failure().is_none());
        assert!(events.recv_until(Instant::now()).is_none());
    }
    let resume = app
        .thread_resume("t", json!({"excludeTurns":false}), deadline())
        .unwrap();
    response(&app, &resume, json!({"thread":{"id":"t","turns":[]}}));
    assert_eq!(app.snapshot().thread_id.as_deref(), Some("t"));
}

#[test]
fn resume_with_turn_history_guards_activity_without_replayed_events() {
    for params in [json!({}), json!({"excludeTurns":false})] {
        let (app, events) = harness(CodexLimits::default());
        let init = app.initialize("test", "1", deadline()).unwrap();
        response(&app, &init, json!({"userAgent":"fixture"}));
        let resume = app.thread_resume("t", params, deadline()).unwrap();
        response(
            &app,
            &resume,
            json!({"thread":{
                "id":"t","status":{"type":"active","activeFlags":[]},
                "turns":[{"id":"a","status":"inProgress"}]
            }}),
        );
        assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
        assert!(events.recv_until(Instant::now()).is_none());
        assert!(matches!(
            app.turn_start("must not steer", deadline()),
            Err(CodexError::Busy)
        ));
        assert!(matches!(
            app.turn_steer("wrong", "more", deadline()),
            Err(CodexError::TurnMismatch)
        ));
        assert!(matches!(
            app.turn_interrupt("wrong", deadline()),
            Err(CodexError::TurnMismatch)
        ));
        let steer = app
            .turn_steer("a", "explicit steering", deadline())
            .unwrap();
        response(&app, &steer, json!({"turnId":"a"}));
        let interrupt = app.turn_interrupt("a", deadline()).unwrap();
        response(&app, &interrupt, json!({}));
        assert!(matches!(
            app.turn_start("still busy", deadline()),
            Err(CodexError::Busy)
        ));
        turn(&app, "turn/completed", "a", "interrupted");
        assert!(app.turn_start("next", deadline()).is_ok());
    }
}

#[test]
fn resume_without_active_identity_stays_busy_until_identifying_turn() {
    for turns in [
        None,
        Some(json!([])),
        Some(json!([{"id":"old","status":"completed"}])),
    ] {
        let (app, events) = harness(CodexLimits::default());
        let init = app.initialize("test", "1", deadline()).unwrap();
        response(&app, &init, json!({"userAgent":"fixture"}));
        let resume = app
            .thread_resume("t", json!({"excludeTurns":false}), deadline())
            .unwrap();
        // An unrelated terminal observation cannot identify the active turn.
        turn(&app, "turn/completed", "old", "completed");
        let mut resumed = json!({"thread":{
            "id":"t","status":{"type":"active","activeFlags":[]}
        }});
        if let Some(turns) = turns {
            resumed["thread"]["turns"] = turns;
        }
        response(&app, &resume, resumed);
        assert!(matches!(
            &*resume.wait_until(deadline()).unwrap(),
            RequestOutcome::Response(_)
        ));
        assert!(matches!(
            app.turn_start("before replay", deadline()),
            Err(CodexError::Busy)
        ));
        turn(&app, "turn/completed", "old", "completed");
        ingress(
            &app,
            json!({"method":"turn/started","params":{"threadId":"child",
                "turn":{"id":"a","status":"inProgress"}}}),
        );
        assert!(app.snapshot().active_turn.is_none());
        assert!(matches!(
            app.turn_start("still unidentified", deadline()),
            Err(CodexError::Busy)
        ));
        assert_eq!(
            driver::maintenance(&app.shared),
            Ok(driver::WriteState::Open)
        );
        turn(&app, "turn/started", "a", "inProgress");
        assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
        assert!(matches!(
            app.turn_start("identified but active", deadline()),
            Err(CodexError::Busy)
        ));
        let interrupt = app.turn_interrupt("a", deadline()).unwrap();
        response(&app, &interrupt, json!({}));
        turn(&app, "turn/completed", "a", "interrupted");
        assert!(app.turn_start("next", deadline()).is_ok());
        assert!(app.failure().is_none());
        drop(events);
    }
}

#[test]
fn replayed_approvals_preserve_active_turn_without_a_started_event_or_resume_turn() {
    for method in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
    ] {
        for response_first in [false, true] {
            let (app, events) = harness(CodexLimits::default());
            let init = app.initialize("test", "1", deadline()).unwrap();
            response(&app, &init, json!({"userAgent":"fixture"}));
            let resume = app.thread_resume("t", json!({}), deadline()).unwrap();
            let resumed = json!({"thread":{
                "id":"t","status":{"type":"active","activeFlags":[]},"turns":[]
            }});
            if response_first {
                response(&app, &resume, resumed.clone());
                // The host can act on the completed resume ticket before replay
                // identifies the turn. Refusals must not enqueue work or events.
                assert!(app.snapshot().active_turn.is_none());
                let pending = lock(&app.shared.state).pending.len();
                assert!(matches!(
                    app.turn_start("must not steer before replay", deadline()),
                    Err(CodexError::Busy)
                ));
                assert!(matches!(
                    app.turn_steer("a", "guessed identity", deadline()),
                    Err(CodexError::TurnMismatch)
                ));
                assert!(matches!(
                    app.turn_interrupt("a", deadline()),
                    Err(CodexError::TurnMismatch)
                ));
                assert_eq!(lock(&app.shared.state).pending.len(), pending);
                assert!(events.recv_until(Instant::now()).is_none());
            }
            ingress(
                &app,
                json!({"id":1,"method":method,"params":{"threadId":"t","turnId":"a","itemId":"i"}}),
            );
            let token = events.recv_until(deadline()).unwrap().approval.unwrap();
            assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
            if !response_first {
                response(&app, &resume, resumed);
            }

            assert!(matches!(
                app.turn_start("must not implicitly steer", deadline()),
                Err(CodexError::Busy)
            ));
            assert!(matches!(
                app.turn_steer("wrong", "more", deadline()),
                Err(CodexError::TurnMismatch)
            ));
            assert!(matches!(
                app.turn_interrupt("wrong", deadline()),
                Err(CodexError::TurnMismatch)
            ));
            let steer = app.turn_steer("a", "more", deadline()).unwrap();
            response(&app, &steer, json!({"turnId":"a"}));
            let interrupt = app.turn_interrupt("a", deadline()).unwrap();
            response(&app, &interrupt, json!({}));

            // Resolving an approval or acknowledging interruption does not establish
            // turn completion. Only the matching terminal turn releases the guard.
            ingress(
                &app,
                json!({"method":"serverRequest/resolved","params":{"threadId":"t","requestId":1}}),
            );
            assert!(matches!(
                token.respond(json!("accept")),
                Err(CodexError::StaleApproval)
            ));
            assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
            assert!(matches!(
                app.turn_start("still busy", deadline()),
                Err(CodexError::Busy)
            ));
            turn(&app, "turn/completed", "a", "interrupted");
            assert!(app.snapshot().active_turn.is_none());
            let next = app.turn_start("next turn", deadline()).unwrap();
            response(
                &app,
                &next,
                json!({"turn":{"id":"b","status":"inProgress"}}),
            );
            assert_eq!(app.snapshot().active_turn.as_deref(), Some("b"));
        }
    }
}

#[test]
fn delayed_active_resume_without_history_preserves_completed_replay() {
    for method in [
        "turn/started",
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
    ] {
        for turns in [
            None,
            Some(json!([])),
            Some(json!([{"id":"old","status":"completed"}])),
        ] {
            let (app, events) = harness(CodexLimits::default());
            let init = app.initialize("test", "1", deadline()).unwrap();
            response(&app, &init, json!({"userAgent":"fixture"}));
            let resume = app.thread_resume("t", json!({}), deadline()).unwrap();
            let token = if method == "turn/started" {
                turn(&app, method, "a", "inProgress");
                None
            } else {
                ingress(
                    &app,
                    json!({"id":1,"method":method,
                        "params":{"threadId":"t","turnId":"a","itemId":"i"}}),
                );
                events.recv_until(deadline()).unwrap().approval
            };
            assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
            turn(&app, "turn/completed", "a", "completed");
            // A later unrelated completion must not erase the identifying replay.
            turn(&app, "turn/completed", "old", "completed");
            let mut resumed = json!({"thread":{
                "id":"t","status":{"type":"active","activeFlags":[]}
            }});
            if let Some(turns) = turns {
                resumed["thread"]["turns"] = turns;
            }
            response(&app, &resume, resumed);
            assert!(app.snapshot().active_turn.is_none());
            if let Some(token) = token {
                assert!(matches!(
                    token.respond(json!("accept")),
                    Err(CodexError::StaleApproval)
                ));
            }
            let next = app
                .turn_start("completed replay stays idle", deadline())
                .expect("a delayed identity-less snapshot must not restore busy");
            response(
                &app,
                &next,
                json!({"turn":{"id":"b","status":"inProgress"}}),
            );
            assert_eq!(app.snapshot().active_turn.as_deref(), Some("b"));
            assert!(app.failure().is_none());
        }
    }
}

#[test]
fn answering_replayed_approval_before_resume_response_does_not_establish_idle() {
    let (app, events) = harness(CodexLimits::default());
    let init = app.initialize("test", "1", deadline()).unwrap();
    response(&app, &init, json!({"userAgent":"fixture"}));
    let resume = app.thread_resume("t", json!({}), deadline()).unwrap();
    approval(&app, &events, json!(1))
        .respond(json!("decline"))
        .unwrap();
    response(&app, &resume, json!({"thread":{"id":"t","turns":[]}}));
    assert!(matches!(
        app.turn_start("must not steer", deadline()),
        Err(CodexError::Busy)
    ));
    assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
    assert!(app.turn_interrupt("a", deadline()).is_ok());
}

#[test]
fn replayed_approval_cannot_resurrect_an_earlier_completed_turn() {
    let (app, events) = ready(CodexLimits::default());
    turn(&app, "turn/completed", "a", "completed");
    turn(&app, "turn/completed", "b", "interrupted");
    ingress(
        &app,
        json!({"id":1,"method":"item/commandExecution/requestApproval",
            "params":{"threadId":"t","turnId":"a","itemId":"i"}}),
    );
    assert_eq!(app.failure(), Some(Failure::Protocol));
    assert!(app.snapshot().active_turn.is_none());
    while let Some(event) = events.recv_until(deadline()) {
        assert!(event.approval.is_none());
    }
}

#[test]
fn conflicting_replayed_approval_cannot_retarget_an_active_turn() {
    let (app, events) = ready(CodexLimits::default());
    let token = approval(&app, &events, json!(1));
    ingress(
        &app,
        json!({"id":2,"method":"item/fileChange/requestApproval",
            "params":{"threadId":"t","turnId":"b","itemId":"i"}}),
    );
    assert_eq!(app.failure(), Some(Failure::Protocol));
    assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
    assert!(matches!(
        token.respond(json!("accept")),
        Err(CodexError::StaleApproval)
    ));
    assert!(events.recv_until(deadline()).is_none());
}

#[test]
fn resume_replay_before_response_binds_identity_and_mismatch_is_terminal() {
    let (app, events) = harness(CodexLimits::default());
    let init = app.initialize("test", "1", deadline()).unwrap();
    response(&app, &init, json!({"userAgent":"fixture"}));
    let resume = app.thread_resume("t", json!({}), deadline()).unwrap();
    let token = approval(&app, &events, json!(1));
    turn(&app, "turn/completed", "a", "interrupted");
    response(
        &app,
        &resume,
        json!({"thread":{"id":"t","status":{"type":"active","activeFlags":[]},
            "turns":[{"id":"a","status":"inProgress"}]}}),
    );
    assert!(app.snapshot().active_turn.is_none());
    assert!(matches!(
        token.respond(json!("accept")),
        Err(CodexError::StaleApproval)
    ));
    assert!(matches!(
        app.thread_start(json!({}), deadline()),
        Err(CodexError::InvalidState)
    ));
    assert!(
        app.turn_start("terminal replay stays idle", deadline())
            .is_ok()
    );

    let (app, _events) = harness(CodexLimits::default());
    let init = app.initialize("test", "1", deadline()).unwrap();
    response(&app, &init, json!({"userAgent":"fixture"}));
    let resume = app.thread_resume("t", json!({}), deadline()).unwrap();
    publish(&app, &resume);
    ingress(
        &app,
        json!({"id":resume.id(),"result":{"thread":{"id":"wrong"}}}),
    );
    assert_eq!(app.failure(), Some(Failure::Protocol));
}

#[test]
fn all_terminal_paths_settle_unsent_and_dispatched_requests_once() {
    for failure in [
        Failure::Eof,
        Failure::Protocol,
        Failure::DeadlineExceeded,
        Failure::WriteFailed,
        Failure::Closed,
    ] {
        let (app, _events) = ready(CodexLimits::default());
        let sent = app.turn_start("x", deadline()).unwrap();
        publish(&app, &sent);
        turn(&app, "turn/started", "a", "inProgress");
        let queued = app.turn_interrupt("a", deadline()).unwrap();
        app.shared.stop(failure.clone());
        assert!(
            matches!(&*sent.outcome().unwrap(), RequestOutcome::OutcomeUnknown { failure: f, .. } if *f == failure)
        );
        assert!(matches!(&*queued.outcome().unwrap(), RequestOutcome::NotSent(f) if *f == failure));
        app.close();
        assert_eq!(app.failure(), Some(failure));
    }
}

#[test]
fn invalid_frames_unknown_status_and_payload_debug_are_conservative() {
    let (app, _events) = ready(CodexLimits::default());
    turn(&app, "turn/completed", "a", "newStatus");
    assert_eq!(
        app.snapshot().last_turn.unwrap().status,
        TurnStatus::Unknown
    );
    for line in [
        "not-json",
        "[]",
        "{\"id\":null,\"result\":{}}",
        "{\"id\":1,\"result\":{},\"error\":{}}",
    ] {
        assert_eq!(
            driver::ingress(&app.shared, 0, line),
            Err(Failure::Protocol)
        );
    }
    assert!(
        !format!(
            "{:?}",
            RequestOutcome::RemoteError(Arc::new(json!({"message":"private"})))
        )
        .contains("private")
    );
}

#[test]
fn bounded_admissions_validate_before_allocating_or_changing_state() {
    let limits = CodexLimits {
        max_frame_bytes: 256,
        pending_bytes: 256,
        pending_requests: 1,
        control_bytes: 512,
        ..CodexLimits::default()
    };
    let (app, events) = harness(limits);
    assert!(app.initialize("", "1", deadline()).is_err());
    assert!(app.initialize(&"x".repeat(256), "1", deadline()).is_err());
    assert!(!app.snapshot().initialized);
    let init = app.initialize("test", "1", deadline()).unwrap();
    response(&app, &init, json!({"userAgent":"fixture"}));
    assert!(matches!(
        app.thread_start(json!({}), deadline()),
        Err(CodexError::Transport(Error::QueueFull))
    ));
    assert!(events.recv_until(Instant::now()).is_none());
    assert!(app.snapshot().thread_id.is_none());
    assert!(
        validate_limits(CodexLimits {
            events: 0,
            ..limits
        })
        .is_err()
    );
}

#[test]
fn interleaved_old_completion_cannot_resurrect_new_terminal_turn() {
    let (app, _events) = ready(CodexLimits::default());
    let a = app.turn_start("one", deadline()).unwrap();
    response(&app, &a, json!({"turn":{"id":"a","status":"inProgress"}}));
    turn(&app, "turn/completed", "a", "completed");
    let b = app.turn_start("two", deadline()).unwrap();
    publish(&app, &b);
    turn(&app, "turn/started", "b", "inProgress");
    turn(&app, "turn/completed", "b", "completed");
    turn(&app, "turn/completed", "a", "completed");
    response(&app, &b, json!({"turn":{"id":"b","status":"inProgress"}}));
    assert!(app.snapshot().active_turn.is_none());
}

#[test]
fn full_ordinary_registry_still_admits_interrupt_and_approval_reply() {
    let (app, events) = ready(CodexLimits::default());
    let token = approval(&app, &events, json!(1));
    turn(&app, "turn/started", "a", "inProgress");
    // Controlled ingress leaves writes unflushed, so accepted responses still retain
    // their reservations. Two reservations belong to initialize/thread-start.
    for _ in 2..CodexLimits::default().pending_requests {
        let ticket = app.turn_steer("a", "more", deadline()).unwrap();
        response(&app, &ticket, json!({"turnId":"a"}));
    }
    assert!(matches!(
        app.turn_steer("a", "too much", deadline()),
        Err(CodexError::Transport(Error::QueueFull))
    ));
    let interrupt = app.turn_interrupt("a", deadline()).unwrap();
    let reply = token.respond(json!("decline")).unwrap();
    app.close();
    assert!(interrupt.outcome().is_some());
    assert!(reply.outcome().is_some());
}

#[test]
fn event_bytes_approval_bytes_and_control_count_enforce_terminal_overload() {
    let (app, _events) = ready(CodexLimits {
        event_bytes: 1,
        ..CodexLimits::default()
    });
    ingress(&app, json!({"method":"extension","params":{"payload":"x"}}));
    assert_eq!(app.failure(), Some(Failure::ConsumerLagged));
    let (app, _events) = ready(CodexLimits {
        approval_bytes: 1,
        ..CodexLimits::default()
    });
    ingress(
        &app,
        json!({"id":1,"method":"item/fileChange/requestApproval",
        "params":{"threadId":"t","turnId":"a","itemId":"i"}}),
    );
    assert_eq!(app.failure(), Some(Failure::Overloaded));
    let (app, _events) = ready(CodexLimits {
        control_capacity: 2,
        ..CodexLimits::default()
    });
    // initialized already holds one control reservation in this no-writer harness.
    ingress(&app, json!({"id":1,"method":"unsupported","params":{}}));
    ingress(&app, json!({"id":2,"method":"unsupported","params":{}}));
    assert_eq!(app.failure(), Some(Failure::Overloaded));
}

#[test]
fn oversized_host_json_is_rejected_without_waiting_for_the_controller() {
    for deeply_nested in [false, true] {
        let (app, events) = harness(CodexLimits {
            max_frame_bytes: 1024,
            pending_requests: 2,
            ..CodexLimits::default()
        });
        let init = app.initialize("test", "1", deadline()).unwrap();
        response(&app, &init, json!({"userAgent":"fixture"}));
        let value = if deeply_nested {
            let mut value = Value::Null;
            for _ in 0..100 {
                value = Value::Array(vec![value]);
            }
            value
        } else {
            Value::Array(vec![json!("element"); 16 * 1024])
        };
        let params = Value::Object(serde_json::Map::from_iter([("oversized".into(), value)]));
        std::thread::scope(|scope| {
            let app = &app;
            let state = lock(&app.shared.state);
            let (done, rejected) = bounded(1);
            let worker = scope.spawn(move || {
                done.send(app.thread_start(params, deadline())).unwrap();
            });
            // Rejection, including disposal of all host JSON, must finish while
            // another controller operation holds the state. Always release the
            // lock before asserting so a regression cannot strand the worker.
            let result = rejected.recv_deadline(deadline());
            drop(state);
            worker.join().unwrap();
            assert!(matches!(
                result.expect("invalid host JSON must not wait for the controller lock"),
                Err(CodexError::Transport(Error::InvalidInput(_)))
            ));
        });
        assert!(app.snapshot().thread_id.is_none());
        assert!(app.failure().is_none());
        assert!(events.recv_until(Instant::now()).is_none());
        // Initialize retains one slot here. Rejection must leave the other slot
        // and root binding available, and close must still settle accepted work.
        let start = app.thread_start(json!({}), deadline()).unwrap();
        app.close();
        assert!(matches!(
            &*start.outcome().unwrap(),
            RequestOutcome::NotSent(Failure::Closed)
        ));
    }
}

#[test]
fn deeply_nested_host_json_is_refused_before_admission() {
    let (app, _events) = ready(CodexLimits::default());
    let mut nested = json!(null);
    for _ in 0..100 {
        nested = Value::Array(vec![nested]);
    }
    assert!(encode(&nested, 1024).is_err());
    assert!(app.failure().is_none());
}

#[test]
fn compatibility_accepts_future_identity_but_requires_initialize_contract() {
    let (app, _events) = harness(CodexLimits::default());
    let init = app.initialize("test", "1", deadline()).unwrap();
    let future = json!({"userAgent":"codex/999.0.0", "futureCapability":{"new":true}});
    response(&app, &init, future.clone());
    assert_eq!(app.snapshot().server.as_deref(), Some(&future));
    assert!(app.snapshot().initialized);

    let (app, _events) = harness(CodexLimits::default());
    let init = app.initialize("test", "1", deadline()).unwrap();
    ingress(
        &app,
        json!({"id":init.id(), "result":{"futureCapability":true}}),
    );
    assert!(!app.snapshot().initialized);
    assert!(app.failure().is_some());
    assert!(init.outcome().is_some());
}

#[cfg(feature = "approvals")]
#[test]
fn approval_callback_panic_invalid_choice_and_duplicates_never_auto_allow() {
    use crate::approvals::{CodexDecision, Executor, Limits, Outcome, Reply};
    let executor = Executor::new(Limits::default()).unwrap();
    for panic in [false, true] {
        let (app, events) = ready(CodexLimits::default());
        let token = approval(&app, &events, json!(17));
        let clone = token.clone();
        let ticket = executor
            .codex(token, deadline(), move |request| {
                assert_eq!(request.id, json!(17));
                if panic {
                    panic!("synthetic callback panic");
                }
                CodexDecision::Decision(json!("invented"))
            })
            .unwrap();
        assert!(matches!(
            &*ticket.wait_until(deadline()).unwrap(),
            Outcome::Replied(Reply::Codex(Ok(_)))
        ));
        assert!(matches!(
            clone.respond(json!("accept")),
            Err(CodexError::StaleApproval)
        ));
        let state = lock(&app.shared.state);
        let reply = state.pending.iter().find(|p| p.reply.is_some()).unwrap();
        let frame: Value = serde_json::from_str(reply.line.as_ref().unwrap()).unwrap();
        assert_eq!(frame["result"]["decision"], "decline");
    }
}

#[test]
fn input_closure_refuses_host_admissions_without_truncating_native_output() {
    use crate::rpc::{RpcEvent, RpcWriteFailure, RpcWriteOutcome};
    // Event notification, immediate failed receipt, refused send, later receipt observation.
    for source in 0..4 {
        let (app, events) = ready(CodexLimits::default());
        lock(&app.shared.state).pending.clear();
        let ticket = app.turn_start("x", deadline()).unwrap();
        let token = approval(&app, &events, json!(77));
        if source == 2 {
            {
                let mut state = lock(&app.shared.state);
                let pending = state.pending.front_mut().unwrap();
                pending.dispatched = true;
                pending.receipt =
                    Some(RpcWriteReceipt::finished_for_test(RpcWriteOutcome::Written));
            }
            ingress(
                &app,
                json!({"id":66,"method":"unsupported/earlier","params":{}}),
            );
            events.recv_until(deadline()).unwrap();
        }
        if source == 3 {
            let mut state = lock(&app.shared.state);
            let pending = state.pending.front_mut().unwrap();
            pending.dispatched = true;
            pending.receipt = Some(RpcWriteReceipt::finished_for_test(
                RpcWriteOutcome::PossiblyWritten(RpcWriteFailure::Closed),
            ));
        }
        let (tx, incoming) = bounded(8);
        if source == 0 {
            tx.send(RpcEvent::StdinError {
                id: app.generation(),
                kind: std::io::ErrorKind::BrokenPipe,
            })
            .unwrap();
        }
        tx.send(rpc_line(
            &app,
            100,
            json!({"method":"future/marker","params":{}}),
        ))
        .unwrap();
        std::thread::scope(|scope| {
            let app = &app;
            let ticket = &ticket;
            scope.spawn(move || {
                assert_eq!(events.recv_until(deadline()).unwrap().seq, 100);
                assert!(app.failure().is_none());
                let before = {
                    let state = lock(&app.shared.state);
                    (state.next_id, state.pending.iter().map(|p| (p.deadline, p.bytes)).collect::<Vec<_>>(), state.approvals.len())
                };
                assert!(matches!(
                    app.turn_interrupt("a", deadline()),
                    Err(CodexError::Transport(Error::Closed))
                ));
                assert!(matches!(
                    app.turn_steer("a", "later", deadline()),
                    Err(CodexError::Transport(Error::Closed))
                ));
                assert!(matches!(
                    token.respond(json!("decline")),
                    Err(CodexError::StaleApproval)
                ));
                assert!(matches!(
                    token.respond(json!("decline")),
                    Err(CodexError::StaleApproval)
                ));
                {
                    let state = lock(&app.shared.state);
                    assert_eq!(
                        (state.next_id, state.pending.iter().map(|p| (p.deadline, p.bytes)).collect::<Vec<_>>(), state.approvals.len()),
                        before
                    );
                }
                // Approvals can arrive after closure; their native events still survive.
                tx.send(rpc_line(app, 101, json!({"id":78,"method":"item/commandExecution/requestApproval",
                    "params":{"threadId":"t","turnId":"a","itemId":"late","availableDecisions":["decline"]}}))).unwrap();
                let late = events.recv_until(deadline()).unwrap().approval.unwrap();
                let retained = lock(&app.shared.state).approvals.len();
                assert!(matches!(late.respond(json!("decline")), Err(CodexError::StaleApproval)));
                assert_eq!(lock(&app.shared.state).approvals.len(), retained);
                tx.send(rpc_line(
                    app,
                    102,
                    json!({"id":88,"method":"unsupported/reverse","params":{}}),
                ))
                .unwrap();
                tx.send(rpc_line(
                    app,
                    103,
                    json!({"id":ticket.id(),"result":{"turn":{"id":"a","status":"completed"}}}),
                ))
                .unwrap();
                // A native response remains authoritative even though no new input is admitted.
                let outcome = ticket.wait_until(deadline()).unwrap();
                assert!(
                    matches!(&*outcome, RequestOutcome::Response(_)),
                    "source {source}: {outcome:?}"
                );
                assert!(app.failure().is_none());
                assert!(matches!(app.turn_start("after completion", deadline()), Err(CodexError::Transport(Error::Closed))));
                assert!(lock(&app.shared.state).pending.iter().filter(|p| p.control).count() <= usize::from(source == 2), "draining must not enqueue automatic controls");
                tx.send(RpcEvent::Exit {
                    id: app.generation(),
                    stderr_tail: String::new(),
                })
                .unwrap();
            });
            driver::run_loop(
                Arc::clone(&app.shared),
                incoming,
                crossbeam_channel::never(),
                |_, _| match source {
                    0 => Ok(RpcWriteReceipt::finished_for_test(RpcWriteOutcome::Written)),
                    1 => Ok(RpcWriteReceipt::finished_for_test(
                        RpcWriteOutcome::PossiblyWritten(RpcWriteFailure::Closed),
                    )),
                    2 => Err(Error::Closed),
                    _ => panic!("maintenance must stop dispatch"),
                },
            );
        });
        assert_eq!(app.failure(), Some(Failure::Eof));
    }
}

#[test]
fn input_closure_skips_initialized_notification_after_late_initialize_response() {
    use crate::rpc::{RpcEvent, RpcWriteFailure, RpcWriteOutcome};
    let (app, events) = harness(CodexLimits::default());
    let ticket = app.initialize("test", "1", deadline()).unwrap();
    let (tx, incoming) = bounded(4);
    tx.send(RpcEvent::StdinError {
        id: app.generation(),
        kind: std::io::ErrorKind::BrokenPipe,
    })
    .unwrap();
    tx.send(rpc_line(
        &app,
        1,
        json!({"id":ticket.id(),"result":{"userAgent":"fixture"}}),
    ))
    .unwrap();
    tx.send(rpc_line(
        &app,
        2,
        json!({"method":"future/marker","params":{}}),
    ))
    .unwrap();
    let mut writes = 0;
    std::thread::scope(|scope| {
        let app = &app;
        scope.spawn(move || {
            assert_eq!(events.recv_until(deadline()).unwrap().seq, 2);
            assert!(app.failure().is_none());
            assert!(
                !lock(&app.shared.state).pending.iter().any(|p| p.control),
                "closed input must not register initialized notification or its deadline"
            );
            tx.send(RpcEvent::Exit {
                id: app.generation(),
                stderr_tail: String::new(),
            })
            .unwrap();
        });
        driver::run_loop(
            Arc::clone(&app.shared),
            incoming,
            crossbeam_channel::never(),
            |_, _| {
                writes += 1;
                Ok(RpcWriteReceipt::finished_for_test(
                    RpcWriteOutcome::PossiblyWritten(RpcWriteFailure::Closed),
                ))
            },
        );
    });
    assert_eq!(writes, 1);
    assert!(matches!(
        &*ticket.outcome().unwrap(),
        RequestOutcome::Response(_)
    ));
    assert!(app.snapshot().initialized);
    assert_eq!(app.failure(), Some(Failure::Eof));
}
