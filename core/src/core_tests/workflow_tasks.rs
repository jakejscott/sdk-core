use crate::{
    errors::PollWfError,
    job_assert,
    replay::TestHistoryBuilder,
    test_help::{
        build_fake_worker, build_mock_pollers, build_multihist_mock_sg, canned_histories,
        gen_assert_and_fail, gen_assert_and_reply, hist_to_poll_resp, mock_worker, poll_and_reply,
        poll_and_reply_clears_outstanding_evicts, single_hist_mock_sg, FakeWfResponses,
        MockPollCfg, MocksHolder, ResponseType, NO_MORE_WORK_ERROR_MSG, TEST_Q,
    },
    worker::client::mocks::mock_workflow_client,
    workflow::WorkflowCachingPolicy::{self, AfterEveryReply, NonSticky},
    Worker,
};
use rstest::{fixture, rstest};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};
use temporal_sdk_core_api::Worker as WorkerTrait;
use temporal_sdk_core_protos::{
    coresdk::{
        activity_result::{self as ar, activity_resolution, ActivityResolution},
        workflow_activation::{
            remove_from_cache::EvictionReason, workflow_activation_job, FireTimer, ResolveActivity,
            StartWorkflow, UpdateRandomSeed, WorkflowActivationJob,
        },
        workflow_commands::{
            ActivityCancellationType, CancelTimer, CompleteWorkflowExecution,
            FailWorkflowExecution, RequestCancelActivity, ScheduleActivity,
        },
        workflow_completion::WorkflowActivationCompletion,
    },
    temporal::api::{
        enums::v1::{EventType, WorkflowTaskFailedCause},
        failure::v1::Failure,
        history::v1::{history_event, History, TimerFiredEventAttributes},
        workflowservice::v1::{
            GetWorkflowExecutionHistoryResponse, RespondWorkflowTaskCompletedResponse,
        },
    },
};
use temporal_sdk_core_test_utils::{fanout_tasks, start_timer_cmd};

#[fixture(hist_batches = &[])]
fn single_timer_setup(hist_batches: &'static [usize]) -> Worker {
    let wfid = "fake_wf_id";

    let t = canned_histories::single_timer("1");
    build_fake_worker(wfid, t, hist_batches)
}

#[fixture(hist_batches = &[])]
fn single_activity_setup(hist_batches: &'static [usize]) -> Worker {
    let wfid = "fake_wf_id";

    let t = canned_histories::single_activity("fake_activity");
    build_fake_worker(wfid, t, hist_batches)
}

#[fixture(hist_batches = &[])]
fn single_activity_failure_setup(hist_batches: &'static [usize]) -> Worker {
    let wfid = "fake_wf_id";

    let t = canned_histories::single_failed_activity("fake_activity");
    build_fake_worker(wfid, t, hist_batches)
}

#[rstest]
#[case::incremental(single_timer_setup(&[1, 2]), NonSticky)]
#[case::replay(single_timer_setup(&[2]), NonSticky)]
#[case::incremental_evict(single_timer_setup(&[1, 2]), AfterEveryReply)]
#[case::replay_evict(single_timer_setup(&[2]), AfterEveryReply)]
#[tokio::test]
async fn single_timer(#[case] worker: Worker, #[case] evict: WorkflowCachingPolicy) {
    poll_and_reply(
        &worker,
        evict,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![start_timer_cmd(1, Duration::from_secs(1))],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::FireTimer(_)),
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[rstest(worker,
case::incremental(single_activity_setup(&[1, 2])),
case::incremental_activity_failure(single_activity_failure_setup(&[1, 2])),
case::replay(single_activity_setup(&[2])),
case::replay_activity_failure(single_activity_failure_setup(&[2]))
)]
#[tokio::test]
async fn single_activity_completion(worker: Worker) {
    poll_and_reply(
        &worker,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![ScheduleActivity {
                    activity_id: "fake_activity".to_string(),
                    ..Default::default()
                }
                .into()],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::ResolveActivity(_)),
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[2]))]
#[tokio::test]
async fn parallel_timer_test_across_wf_bridge(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let timer_1_id = 1;
    let timer_2_id = 2;

    let t = canned_histories::parallel_timer(
        timer_1_id.to_string().as_str(),
        timer_2_id.to_string().as_str(),
    );
    let core = build_fake_worker(wfid, t, hist_batches);

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![
                    start_timer_cmd(timer_1_id, Duration::from_secs(1)),
                    start_timer_cmd(timer_2_id, Duration::from_secs(1)),
                ],
            ),
            gen_assert_and_reply(
                &|res| {
                    assert_matches!(
                        res.jobs.as_slice(),
                        [
                            WorkflowActivationJob {
                                variant: Some(workflow_activation_job::Variant::FireTimer(
                                    FireTimer { seq: t1_id }
                                )),
                            },
                            WorkflowActivationJob {
                                variant: Some(workflow_activation_job::Variant::FireTimer(
                                    FireTimer { seq: t2_id }
                                )),
                            }
                        ] => {
                            assert_eq!(t1_id, &timer_1_id);
                            assert_eq!(t2_id, &timer_2_id);
                        }
                    );
                },
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[2]))]
#[tokio::test]
async fn timer_cancel(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let timer_id = 1;
    let cancel_timer_id = 2;

    let t = canned_histories::cancel_timer(
        timer_id.to_string().as_str(),
        cancel_timer_id.to_string().as_str(),
    );
    let core = build_fake_worker(wfid, t, hist_batches);

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![
                    start_timer_cmd(cancel_timer_id, Duration::from_secs(1)),
                    start_timer_cmd(timer_id, Duration::from_secs(1)),
                ],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::FireTimer(_)),
                vec![
                    CancelTimer {
                        seq: cancel_timer_id,
                    }
                    .into(),
                    CompleteWorkflowExecution { result: None }.into(),
                ],
            ),
        ],
    )
    .await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[2]))]
#[tokio::test]
async fn scheduled_activity_cancellation_try_cancel(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let activity_seq = 1;
    let activity_id = "fake_activity";
    let signal_id = "signal";

    let t = canned_histories::cancel_scheduled_activity(activity_id, signal_id);
    let core = build_fake_worker(wfid, t, hist_batches);

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![ScheduleActivity {
                    seq: activity_seq,
                    activity_id: activity_id.to_string(),
                    cancellation_type: ActivityCancellationType::TryCancel as i32,
                    ..Default::default()
                }
                .into()],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::SignalWorkflow(_)),
                vec![RequestCancelActivity { seq: activity_seq }.into()],
            ),
            // Activity is getting resolved right away as we are in the TryCancel mode.
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::ResolveActivity(_)),
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[2]))]
#[tokio::test]
async fn scheduled_activity_timeout(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let activity_seq = 1;
    let activity_id = "fake_activity";

    let t = canned_histories::scheduled_activity_timeout(activity_id);
    let core = build_fake_worker(wfid, t, hist_batches);
    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![ScheduleActivity {
                    seq: activity_seq,
                    activity_id: activity_id.to_string(),
                    ..Default::default()
                }
                    .into()],
            ),
            // Activity is getting resolved right away as it has been timed out.
            gen_assert_and_reply(
                &|res| {
                    assert_matches!(
                    res.jobs.as_slice(),
                    [
                        WorkflowActivationJob {
                            variant: Some(workflow_activation_job::Variant::ResolveActivity(
                                ResolveActivity {
                                    seq,
                                    result: Some(ActivityResolution {
                                        status: Some(activity_resolution::Status::Failed(ar::Failure {
                                            failure: Some(failure)
                                        })),
                                    })
                                }
                            )),
                        }
                    ] => {
                        assert_eq!(failure.message, "Activity task timed out".to_string());
                        assert_eq!(*seq, activity_seq);
                    }
                );
                },
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
        .await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[2]))]
#[tokio::test]
async fn started_activity_timeout(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let activity_seq = 1;

    let t = canned_histories::started_activity_timeout(activity_seq.to_string().as_str());
    let core = build_fake_worker(wfid, t, hist_batches);

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![ScheduleActivity {
                    seq: activity_seq,
                    activity_id: activity_seq.to_string(),
                    ..Default::default()
                }
                    .into()],
            ),
            // Activity is getting resolved right away as it has been timed out.
            gen_assert_and_reply(
                &|res| {
                assert_matches!(
                    res.jobs.as_slice(),
                    [
                        WorkflowActivationJob {
                            variant: Some(workflow_activation_job::Variant::ResolveActivity(
                                ResolveActivity {
                                    seq,
                                    result: Some(ActivityResolution {
                                        status: Some(activity_resolution::Status::Failed(ar::Failure {
                                            failure: Some(failure)
                                        })),
                                    })
                                }
                            )),
                        }
                    ] => {
                        assert_eq!(failure.message, "Activity task timed out".to_string());
                        assert_eq!(*seq, activity_seq);
                    }
                );
                },
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
        .await;
}

#[rstest(hist_batches, case::incremental(&[1, 3]), case::replay(&[3]))]
#[tokio::test]
async fn cancelled_activity_timeout(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let activity_seq = 0;
    let activity_id = "fake_activity";
    let signal_id = "signal";

    let t = canned_histories::scheduled_cancelled_activity_timeout(activity_id, signal_id);
    let core = build_fake_worker(wfid, t, hist_batches);

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![ScheduleActivity {
                    seq: activity_seq,
                    activity_id: activity_id.to_string(),
                    ..Default::default()
                }
                .into()],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::SignalWorkflow(_)),
                vec![RequestCancelActivity { seq: activity_seq }.into()],
            ),
            // Activity is resolved right away as it has timed out.
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::ResolveActivity(
                    ResolveActivity {
                        seq: _,
                        result: Some(ActivityResolution {
                            status: Some(activity_resolution::Status::Cancelled(..)),
                        })
                    }
                )),
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[2]))]
#[tokio::test]
async fn scheduled_activity_cancellation_abandon(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let activity_id = 1;
    let signal_id = "signal";

    let t = canned_histories::cancel_scheduled_activity_abandon(
        activity_id.to_string().as_str(),
        signal_id,
    );
    let core = build_fake_worker(wfid, t, hist_batches);

    verify_activity_cancellation(&core, activity_id, ActivityCancellationType::Abandon).await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[2]))]
#[tokio::test]
async fn started_activity_cancellation_abandon(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let activity_id = 1;
    let signal_id = "signal";

    let t = canned_histories::cancel_started_activity_abandon(
        activity_id.to_string().as_str(),
        signal_id,
    );
    let core = build_fake_worker(wfid, t, hist_batches);

    verify_activity_cancellation(&core, activity_id, ActivityCancellationType::Abandon).await;
}

#[rstest(hist_batches, case::incremental(&[1, 3]), case::replay(&[3]))]
#[tokio::test]
async fn scheduled_activity_cancellation_try_cancel_task_canceled(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let activity_id = 1;
    let signal_id = "signal";

    let t = canned_histories::cancel_scheduled_activity_with_activity_task_cancel(
        activity_id.to_string().as_str(),
        signal_id,
    );
    let core = build_fake_worker(wfid, t, hist_batches);

    verify_activity_cancellation(&core, activity_id, ActivityCancellationType::TryCancel).await;
}

#[rstest(hist_batches, case::incremental(&[1, 3]), case::replay(&[3]))]
#[tokio::test]
async fn started_activity_cancellation_try_cancel_task_canceled(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let activity_id = 1;
    let signal_id = "signal";

    let t = canned_histories::cancel_started_activity_with_activity_task_cancel(
        activity_id.to_string().as_str(),
        signal_id,
    );
    let core = build_fake_worker(wfid, t, hist_batches);

    verify_activity_cancellation(&core, activity_id, ActivityCancellationType::TryCancel).await;
}

/// Verification for try cancel & abandon histories
async fn verify_activity_cancellation(
    worker: &Worker,
    activity_seq: u32,
    cancel_type: ActivityCancellationType,
) {
    poll_and_reply(
        worker,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![ScheduleActivity {
                    seq: activity_seq,
                    activity_id: activity_seq.to_string(),
                    cancellation_type: cancel_type as i32,
                    ..Default::default()
                }
                .into()],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::SignalWorkflow(_)),
                vec![RequestCancelActivity { seq: activity_seq }.into()],
            ),
            // Activity should be resolved right away
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::ResolveActivity(
                    ResolveActivity {
                        seq: _,
                        result: Some(ActivityResolution {
                            status: Some(activity_resolution::Status::Cancelled(..)),
                        })
                    }
                )),
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[rstest(hist_batches, case::incremental(&[1, 2, 3, 4]), case::replay(&[4]))]
#[tokio::test]
async fn scheduled_activity_cancellation_wait_for_cancellation(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let activity_id = 1;
    let signal_id = "signal";

    let t = canned_histories::cancel_scheduled_activity_with_signal_and_activity_task_cancel(
        activity_id.to_string().as_str(),
        signal_id,
    );
    let core = build_fake_worker(wfid, t, hist_batches);

    verify_activity_cancellation_wait_for_cancellation(activity_id, &core).await;
}

#[rstest(hist_batches, case::incremental(&[1, 2, 3, 4]), case::replay(&[4]))]
#[tokio::test]
async fn started_activity_cancellation_wait_for_cancellation(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let activity_id = 1;
    let signal_id = "signal";

    let t = canned_histories::cancel_started_activity_with_signal_and_activity_task_cancel(
        activity_id.to_string().as_str(),
        signal_id,
    );
    let core = build_fake_worker(wfid, t, hist_batches);

    verify_activity_cancellation_wait_for_cancellation(activity_id, &core).await;
}

async fn verify_activity_cancellation_wait_for_cancellation(activity_id: u32, worker: &Worker) {
    poll_and_reply(
        worker,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![ScheduleActivity {
                    seq: activity_id,
                    activity_id: activity_id.to_string(),
                    cancellation_type: ActivityCancellationType::WaitCancellationCompleted as i32,
                    ..Default::default()
                }
                .into()],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::SignalWorkflow(_)),
                vec![RequestCancelActivity { seq: activity_id }.into()],
            ),
            // Making sure that activity is not resolved until it's cancelled.
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::SignalWorkflow(_)),
                vec![],
            ),
            // Now ActivityTaskCanceled has been processed and activity can be resolved.
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::ResolveActivity(
                    ResolveActivity {
                        seq: _,
                        result: Some(ActivityResolution {
                            status: Some(activity_resolution::Status::Cancelled(..)),
                        })
                    }
                )),
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn workflow_update_random_seed_on_workflow_reset() {
    let wfid = "fake_wf_id";
    let new_run_id = "86E39A5F-AE31-4626-BDFE-398EE072D156";
    let timer_1_id = 1;
    let randomness_seed_from_start = AtomicU64::new(0);

    let t = canned_histories::workflow_fails_with_reset_after_timer(
        timer_1_id.to_string().as_str(),
        new_run_id,
    );
    let core = build_fake_worker(wfid, t, &[2]);

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &|res| {
                    assert_matches!(
                        res.jobs.as_slice(),
                        [WorkflowActivationJob {
                            variant: Some(workflow_activation_job::Variant::StartWorkflow(
                            StartWorkflow{randomness_seed, ..}
                            )),
                        }] => {
                        randomness_seed_from_start.store(*randomness_seed, Ordering::SeqCst);
                        }
                    );
                },
                vec![start_timer_cmd(timer_1_id, Duration::from_secs(1))],
            ),
            gen_assert_and_reply(
                &|res| {
                    assert_matches!(
                        res.jobs.as_slice(),
                        [WorkflowActivationJob {
                            variant: Some(workflow_activation_job::Variant::FireTimer(_),),
                        },
                        WorkflowActivationJob {
                            variant: Some(workflow_activation_job::Variant::UpdateRandomSeed(
                                UpdateRandomSeed{randomness_seed})),
                        }] => {
                            assert_ne!(randomness_seed_from_start.load(Ordering::SeqCst),
                                      *randomness_seed);
                        }
                    );
                },
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn cancel_timer_before_sent_wf_bridge() {
    let wfid = "fake_wf_id";
    let cancel_timer_id = 1;

    let mut t = TestHistoryBuilder::default();
    t.add_by_type(EventType::WorkflowExecutionStarted);
    t.add_full_wf_task();
    t.add_workflow_execution_completed();

    let core = build_fake_worker(wfid, t, &[1]);

    poll_and_reply(
        &core,
        NonSticky,
        &[gen_assert_and_reply(
            &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
            vec![
                start_timer_cmd(cancel_timer_id, Duration::from_secs(1)),
                CancelTimer {
                    seq: cancel_timer_id,
                }
                .into(),
                CompleteWorkflowExecution { result: None }.into(),
            ],
        )],
    )
    .await;
}

#[rstest]
#[case::no_evict_inc(&[1, 2, 2], NonSticky)]
#[case::no_evict(&[2, 2], NonSticky)]
#[tokio::test]
async fn complete_activation_with_failure(
    #[case] batches: &'static [usize],
    #[case] evict: WorkflowCachingPolicy,
) {
    let wfid = "fake_wf_id";
    let timer_id = 1;

    let hist =
        canned_histories::workflow_fails_with_failure_after_timer(timer_id.to_string().as_str());
    let mock_sg = build_multihist_mock_sg(
        vec![FakeWfResponses {
            wf_id: wfid.to_string(),
            hist,
            response_batches: batches.iter().map(Into::into).collect(),
        }],
        true,
        Some(1),
    );
    let core = mock_worker(mock_sg);

    poll_and_reply(
        &core,
        evict,
        &[
            gen_assert_and_reply(
                &|_| {},
                vec![start_timer_cmd(timer_id, Duration::from_secs(1))],
            ),
            gen_assert_and_fail(&|_| {}),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::FireTimer(_)),
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
    core.shutdown().await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[2]))]
#[tokio::test]
async fn simple_timer_fail_wf_execution(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let timer_id = 1;

    let t = canned_histories::single_timer(timer_id.to_string().as_str());
    let core = build_fake_worker(wfid, t, hist_batches);

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![start_timer_cmd(timer_id, Duration::from_secs(1))],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::FireTimer(_)),
                vec![FailWorkflowExecution {
                    failure: Some(Failure {
                        message: "I'm ded".to_string(),
                        ..Default::default()
                    }),
                }
                .into()],
            ),
        ],
    )
    .await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[2]))]
#[tokio::test]
async fn two_signals(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";

    let t = canned_histories::two_signals("sig1", "sig2");
    let core = build_fake_worker(wfid, t, hist_batches);

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                // Task is completed with no commands
                vec![],
            ),
            gen_assert_and_reply(
                &job_assert!(
                    workflow_activation_job::Variant::SignalWorkflow(_),
                    workflow_activation_job::Variant::SignalWorkflow(_)
                ),
                vec![],
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn workflow_failures_only_reported_once() {
    let wfid = "fake_wf_id";
    let timer_1 = 1;
    let timer_2 = 2;

    let hist = canned_histories::workflow_fails_with_failure_two_different_points(
        timer_1.to_string().as_str(),
        timer_2.to_string().as_str(),
    );
    let response_batches = vec![
        1, 2, // Start then first good reply
        2, 2, 2, // Poll for every failure
        // Poll again after evicting after second good reply, then two more fails
        3, 3, 3,
    ];
    let mocks = build_multihist_mock_sg(
        vec![FakeWfResponses {
            wf_id: wfid.to_string(),
            hist,
            response_batches: response_batches.into_iter().map(Into::into).collect(),
        }],
        true,
        // We should only call the server to say we failed twice (once after each success)
        Some(2),
    );
    let omap = mocks.outstanding_task_map.clone();
    let core = mock_worker(mocks);

    poll_and_reply_clears_outstanding_evicts(
        &core,
        omap,
        NonSticky,
        &[
            gen_assert_and_reply(
                &|_| {},
                vec![start_timer_cmd(timer_1, Duration::from_secs(1))],
            ),
            // Fail a few times in a row (only one of which should be reported)
            gen_assert_and_fail(&|_| {}),
            gen_assert_and_fail(&|_| {}),
            gen_assert_and_fail(&|_| {}),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::FireTimer(_)),
                vec![start_timer_cmd(timer_2, Duration::from_secs(1))],
            ),
            // Again (a new fail should be reported here)
            gen_assert_and_fail(&|_| {}),
            gen_assert_and_fail(&|_| {}),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::FireTimer(_)),
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn max_concurrent_wft_respected() {
    // Create long histories for three workflows
    let t1 = canned_histories::long_sequential_timers(20);
    let t2 = canned_histories::long_sequential_timers(20);
    let mh = MockPollCfg::new(
        vec![
            FakeWfResponses {
                wf_id: "wf1".to_string(),
                hist: t1,
                response_batches: vec![ResponseType::AllHistory],
            },
            FakeWfResponses {
                wf_id: "wf2".to_string(),
                hist: t2,
                response_batches: vec![ResponseType::AllHistory],
            },
        ],
        true,
        None,
    );
    let mut mock = build_mock_pollers(mh);
    // Limit the core to two outstanding workflow tasks, hence we should only see polling
    // happen twice, since we will not actually finish the two workflows
    mock.worker_cfg(|cfg| {
        cfg.max_cached_workflows = 2;
        cfg.max_outstanding_workflow_tasks = 2;
    });
    let core = mock_worker(mock);

    // Poll twice in a row before completing -- we should be at limit
    let r1 = core.poll_workflow_activation().await.unwrap();
    let r1_run_id = r1.run_id.clone();
    let r2 = core.poll_workflow_activation().await.unwrap();
    // Now we immediately poll for new work, and complete the r1 activation. The poll must not
    // unblock until the completion goes through.
    let last_finisher = AtomicUsize::new(0);
    let (_, mut r1) = tokio::join! {
        async {
            core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
                r1.run_id,
                start_timer_cmd(1, Duration::from_secs(1)))
            ).await.unwrap();
            last_finisher.store(1, Ordering::SeqCst);
        },
        async {
            let r = core.poll_workflow_activation().await.unwrap();
            last_finisher.store(2, Ordering::SeqCst);
            r
        }
    };
    // So that we know we blocked
    assert_eq!(last_finisher.load(Ordering::Acquire), 2);

    // Since we never did anything with r2, all subsequent activations should be for wf1
    for i in 2..=20 {
        core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
            r1.run_id,
            start_timer_cmd(i, Duration::from_secs(1)),
        ))
        .await
        .unwrap();
        r1 = core.poll_workflow_activation().await.unwrap();
        assert_eq!(r1.run_id, r1_run_id);
    }
    // Finish the tasks so we can shut down
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
        r1.run_id,
        CompleteWorkflowExecution { result: None }.into(),
    ))
    .await
    .unwrap();
    // Evict r2
    core.request_workflow_eviction(&r2.run_id);
    // We have to properly complete the outstanding task (or the mock will be confused why a task
    // failure was reported)
    let _ = core
        .complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
            r2.run_id,
            start_timer_cmd(1, Duration::from_secs(1)),
        ))
        .await;
    // Get and complete eviction
    let r2 = core.poll_workflow_activation().await.unwrap();
    let _ = core
        .complete_workflow_activation(WorkflowActivationCompletion::empty(r2.run_id))
        .await;
    core.shutdown().await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[3]))]
#[tokio::test]
async fn activity_not_canceled_on_replay_repro(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let t = canned_histories::unsent_at_cancel_repro();
    let core = build_fake_worker(wfid, t, hist_batches);
    let activity_id = 1;

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                // Start timer and activity
                vec![
                    ScheduleActivity {
                        seq: activity_id,
                        activity_id: activity_id.to_string(),
                        cancellation_type: ActivityCancellationType::TryCancel as i32,
                        ..Default::default()
                    }
                    .into(),
                    start_timer_cmd(1, Duration::from_secs(1)),
                ],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::FireTimer(_)),
                vec![RequestCancelActivity { seq: activity_id }.into()],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::ResolveActivity(
                    ResolveActivity {
                        result: Some(ActivityResolution {
                            status: Some(activity_resolution::Status::Cancelled(..)),
                        }),
                        ..
                    }
                )),
                vec![start_timer_cmd(2, Duration::from_secs(1))],
            ),
        ],
    )
    .await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[3]))]
#[tokio::test]
async fn activity_not_canceled_when_also_completed_repro(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let t = canned_histories::cancel_not_sent_when_also_complete_repro();
    let core = build_fake_worker(wfid, t, hist_batches);
    let activity_id = 1;

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![ScheduleActivity {
                    seq: activity_id,
                    activity_id: activity_id.to_string(),
                    cancellation_type: ActivityCancellationType::TryCancel as i32,
                    ..Default::default()
                }
                .into()],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::SignalWorkflow(_)),
                vec![
                    RequestCancelActivity { seq: activity_id }.into(),
                    start_timer_cmd(2, Duration::from_secs(1)),
                ],
            ),
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::ResolveActivity(
                    ResolveActivity {
                        result: Some(ActivityResolution {
                            status: Some(activity_resolution::Status::Cancelled(..)),
                        }),
                        ..
                    }
                )),
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn lots_of_workflows() {
    let hists = (0..500).into_iter().map(|i| {
        let wf_id = format!("fake-wf-{}", i);
        let hist = canned_histories::single_timer("1");
        FakeWfResponses {
            wf_id,
            hist,
            response_batches: vec![1.into(), 2.into()],
        }
    });
    let mock = build_multihist_mock_sg(hists, false, None);
    let worker = &mock_worker(mock);

    fanout_tasks(5, |_| async move {
        while let Ok(wft) = worker.poll_workflow_activation().await {
            let job = &wft.jobs[0];
            let reply = match job.variant {
                Some(workflow_activation_job::Variant::StartWorkflow(_)) => {
                    start_timer_cmd(1, Duration::from_secs(1))
                }
                Some(workflow_activation_job::Variant::RemoveFromCache(_)) => {
                    worker
                        .complete_workflow_activation(WorkflowActivationCompletion::empty(
                            wft.run_id,
                        ))
                        .await
                        .unwrap();
                    continue;
                }
                _ => CompleteWorkflowExecution { result: None }.into(),
            };
            worker
                .complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
                    wft.run_id, reply,
                ))
                .await
                .unwrap();
        }
    })
    .await;
    assert_eq!(worker.outstanding_workflow_tasks(), 0);
    worker.shutdown().await;
}

#[rstest(hist_batches, case::incremental(&[1, 2]), case::replay(&[2]))]
#[tokio::test]
async fn wft_timeout_repro(hist_batches: &'static [usize]) {
    let wfid = "fake_wf_id";
    let t = canned_histories::wft_timeout_repro();
    let core = build_fake_worker(wfid, t, hist_batches);
    let activity_id = 1;

    poll_and_reply(
        &core,
        NonSticky,
        &[
            gen_assert_and_reply(
                &job_assert!(workflow_activation_job::Variant::StartWorkflow(_)),
                vec![ScheduleActivity {
                    seq: activity_id,
                    activity_id: activity_id.to_string(),
                    cancellation_type: ActivityCancellationType::TryCancel as i32,
                    ..Default::default()
                }
                .into()],
            ),
            gen_assert_and_reply(
                &job_assert!(
                    workflow_activation_job::Variant::SignalWorkflow(_),
                    workflow_activation_job::Variant::SignalWorkflow(_),
                    workflow_activation_job::Variant::ResolveActivity(ResolveActivity {
                        result: Some(ActivityResolution {
                            status: Some(activity_resolution::Status::Completed(..)),
                        }),
                        ..
                    })
                ),
                vec![CompleteWorkflowExecution { result: None }.into()],
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn complete_after_eviction() {
    let wfid = "fake_wf_id";
    let t = canned_histories::single_timer("1");
    let mut mock = mock_workflow_client();
    mock.expect_complete_workflow_task().times(0);
    let mock = single_hist_mock_sg(wfid, t, &[2], mock, true);
    let core = mock_worker(mock);

    let activation = core.poll_workflow_activation().await.unwrap();
    // We just got start workflow, immediately evict
    core.request_workflow_eviction(&activation.run_id);
    // Original task must be completed before we get the eviction
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
        activation.run_id,
        start_timer_cmd(1, Duration::from_secs(1)),
    ))
    .await
    .unwrap();
    let eviction_activation = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        eviction_activation.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::FireTimer(_)),
        },]
    );
    // Complete the activation containing the eviction, the way we normally would have
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
        eviction_activation.run_id,
        vec![CompleteWorkflowExecution { result: None }.into()],
    ))
    .await
    .unwrap();
    let eviction = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        eviction.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::RemoveFromCache(_)),
        }]
    );
    core.shutdown().await;
}

#[tokio::test]
async fn sends_appropriate_sticky_task_queue_responses() {
    // This test verifies that when completions are sent with sticky queues enabled, that they
    // include the information that tells the server to enqueue the next task on a sticky queue.
    let wfid = "fake_wf_id";
    let t = canned_histories::single_timer("1");
    let mut mock = mock_workflow_client();
    mock.expect_complete_workflow_task()
        .withf(|comp| comp.sticky_attributes.is_some())
        .times(1)
        .returning(|_| Ok(Default::default()));
    mock.expect_complete_workflow_task().times(0);
    let mut mock = single_hist_mock_sg(wfid, t, &[1], mock, false);
    mock.worker_cfg(|wc| wc.max_cached_workflows = 10);
    let core = mock_worker(mock);

    let activation = core.poll_workflow_activation().await.unwrap();
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
        activation.run_id,
        start_timer_cmd(1, Duration::from_secs(1)),
    ))
    .await
    .unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn new_server_work_while_eviction_outstanding_doesnt_overwrite_activation() {
    let wfid = "fake_wf_id";
    let t = canned_histories::single_timer("1");
    let mock = single_hist_mock_sg(wfid, t, &[1, 2], mock_workflow_client(), false);
    let core = mock_worker(mock);

    // Poll for and complete first workflow task
    let activation = core.poll_workflow_activation().await.unwrap();
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
        activation.run_id,
        start_timer_cmd(1, Duration::from_secs(1)),
    ))
    .await
    .unwrap();
    let eviction_activation = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        eviction_activation.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::RemoveFromCache(_)),
        }]
    );
    // Poll again. We should not overwrite the eviction with the new work from the server to fire
    // the timer, so polling will try again, and run into the mock being out of responses.
    let act = core.poll_workflow_activation().await;
    assert_matches!(act, Err(PollWfError::TonicError(err))
                    if err.message() == NO_MORE_WORK_ERROR_MSG);
    core.shutdown().await;
}

#[tokio::test]
async fn buffered_work_drained_on_shutdown() {
    let wfid = "fake_wf_id";
    // Build a one-timer history where first task times out
    let mut t = TestHistoryBuilder::default();
    t.add_by_type(EventType::WorkflowExecutionStarted);
    t.add_workflow_task_scheduled_and_started();
    // Need to build the first response before adding the timeout events b/c otherwise the history
    // builder will include them in the first task
    let resp_1 = hist_to_poll_resp(&t, wfid.to_owned(), 1.into(), TEST_Q.to_string());
    t.add_workflow_task_timed_out();
    t.add_full_wf_task();
    let timer_started_event_id = t.add_get_event_id(EventType::TimerStarted, None);
    t.add(
        EventType::TimerFired,
        history_event::Attributes::TimerFiredEventAttributes(TimerFiredEventAttributes {
            started_event_id: timer_started_event_id,
            timer_id: "1".to_string(),
        }),
    );
    t.add_full_wf_task();
    t.add_workflow_execution_completed();

    let mut tasks = VecDeque::from(vec![resp_1]);
    // Extend the task list with the now timeout-included version of the task. We add a bunch of
    // them because the poll loop will spin while new tasks are available and it is buffering them
    tasks.extend(
        std::iter::repeat(hist_to_poll_resp(
            &t,
            wfid.to_owned(),
            2.into(),
            TEST_Q.to_string(),
        ))
        .take(50),
    );
    let mut mock = mock_workflow_client();
    mock.expect_complete_workflow_task()
        .returning(|_| Ok(RespondWorkflowTaskCompletedResponse::default()));
    let mut mock = MocksHolder::from_client_with_responses(mock, tasks, []);
    // Cache on to avoid being super repetitive
    mock.worker_cfg(|wc| wc.max_cached_workflows = 10);
    let core = &mock_worker(mock);

    // Poll for first WFT
    let act1 = core.poll_workflow_activation().await.unwrap();
    let poll_fut = async move {
        // Now poll again, which will start spinning, and buffer the next WFT with timer fired in it
        // - it won't stop spinning until the first task is complete
        let t = core.poll_workflow_activation().await.unwrap();
        core.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
            t.run_id,
            vec![CompleteWorkflowExecution { result: None }.into()],
        ))
        .await
        .unwrap();
    };
    let complete_first = async move {
        core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
            act1.run_id,
            start_timer_cmd(1, Duration::from_secs(1)),
        ))
        .await
        .unwrap();
    };
    tokio::join!(poll_fut, complete_first, async {
        // If the shutdown is sent too too fast, we might not have got a chance to even buffer work
        tokio::time::sleep(Duration::from_millis(5)).await;
        core.shutdown().await;
    });
}

#[tokio::test]
async fn fail_wft_then_recover() {
    let t = canned_histories::long_sequential_timers(1);
    let mut mh = MockPollCfg::from_resp_batches(
        "fake_wf_id",
        t,
        // We need to deliver all of history twice because of eviction
        [ResponseType::AllHistory, ResponseType::AllHistory],
        mock_workflow_client(),
    );
    mh.num_expected_fails = Some(1);
    mh.expect_fail_wft_matcher =
        Box::new(|_, cause, _| matches!(cause, WorkflowTaskFailedCause::NonDeterministicError));
    let mut mock = build_mock_pollers(mh);
    mock.worker_cfg(|wc| {
        wc.max_cached_workflows = 2;
    });
    let core = mock_worker(mock);

    let act = core.poll_workflow_activation().await.unwrap();
    // Start an activity instead of a timer, triggering nondeterminism error
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
        act.run_id.clone(),
        vec![ScheduleActivity {
            activity_id: "fake_activity".to_string(),
            ..Default::default()
        }
        .into()],
    ))
    .await
    .unwrap();
    // We must handle an eviction now
    let evict_act = core.poll_workflow_activation().await.unwrap();
    assert_eq!(evict_act.run_id, act.run_id);
    assert_matches!(
        evict_act.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::RemoveFromCache(_)),
        }]
    );
    core.complete_workflow_activation(WorkflowActivationCompletion::empty(evict_act.run_id))
        .await
        .unwrap();

    // Workflow starting over, this time issue the right command
    let act = core.poll_workflow_activation().await.unwrap();
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
        act.run_id,
        vec![start_timer_cmd(1, Duration::from_secs(1))],
    ))
    .await
    .unwrap();
    let act = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        act.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::FireTimer(_)),
        },]
    );
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
        act.run_id,
        vec![CompleteWorkflowExecution { result: None }.into()],
    ))
    .await
    .unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn poll_response_triggers_wf_error() {
    let mut t = TestHistoryBuilder::default();
    t.add_by_type(EventType::WorkflowExecutionStarted);
    // Add this nonsense event here to make applying the poll response fail
    t.add_external_signal_completed(100);
    t.add_full_wf_task();
    t.add_workflow_execution_completed();

    let mut mh = MockPollCfg::from_resp_batches(
        "fake_wf_id",
        t,
        [ResponseType::AllHistory],
        mock_workflow_client(),
    );
    // Since applying the poll response immediately generates an error core will start polling again
    // Rather than panic on bad expectation we want to return the magic "no more work" error
    mh.enforce_correct_number_of_polls = false;
    let mock = build_mock_pollers(mh);
    let core = mock_worker(mock);
    // Poll for first WFT, which is immediately an eviction
    let act = core.poll_workflow_activation().await;
    assert_matches!(act, Err(PollWfError::TonicError(err))
                    if err.message() == NO_MORE_WORK_ERROR_MSG);
}

// Verifies we can handle multiple wft timeouts in a row if lang is being very slow in responding
#[tokio::test]
async fn lang_slower_than_wft_timeouts() {
    let wfid = "fake_wf_id";
    let mut t = TestHistoryBuilder::default();
    t.add_by_type(EventType::WorkflowExecutionStarted);
    t.add_workflow_task_scheduled_and_started();
    t.add_workflow_task_timed_out();
    t.add_full_wf_task();
    t.add_workflow_execution_completed();

    let tasks = [
        hist_to_poll_resp(&t, wfid.to_owned(), 1.into(), TEST_Q.to_string()),
        hist_to_poll_resp(&t, wfid.to_owned(), 1.into(), TEST_Q.to_string()),
        hist_to_poll_resp(&t, wfid.to_owned(), 1.into(), TEST_Q.to_string()),
    ];
    let mut mock = mock_workflow_client();
    mock.expect_complete_workflow_task()
        .times(1)
        .returning(|_| Err(tonic::Status::not_found("Workflow task not found.")));
    mock.expect_complete_workflow_task()
        .times(1)
        .returning(|_| Ok(Default::default()));
    let mut mock = MocksHolder::from_client_with_responses(mock, tasks, []);
    mock.worker_cfg(|wc| {
        wc.max_cached_workflows = 2;
    });
    let core = mock_worker(mock);

    let wf_task = core.poll_workflow_activation().await.unwrap();
    let poll_until_no_work = core.poll_workflow_activation().await;
    assert_matches!(poll_until_no_work, Err(PollWfError::TonicError(err))
                    if err.message() == NO_MORE_WORK_ERROR_MSG);
    // This completion runs into a workflow task not found error, since it's completing a stale
    // task.
    core.complete_workflow_activation(WorkflowActivationCompletion::empty(wf_task.run_id))
        .await
        .unwrap();
    // Now we should get an eviction
    let wf_task = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        wf_task.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::RemoveFromCache(_)),
        }]
    );
    core.complete_workflow_activation(WorkflowActivationCompletion::empty(wf_task.run_id))
        .await
        .unwrap();
    // The last WFT buffered should be applied now
    let start_again = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        start_again.jobs[0].variant,
        Some(workflow_activation_job::Variant::StartWorkflow(_))
    );
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
        start_again.run_id,
        vec![CompleteWorkflowExecution { result: None }.into()],
    ))
    .await
    .unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn tries_cancel_of_completed_activity() {
    let mut t = TestHistoryBuilder::default();
    t.add_by_type(EventType::WorkflowExecutionStarted);
    t.add_full_wf_task();
    let scheduled_event_id = t.add_activity_task_scheduled("1");
    t.add_we_signaled("sig", vec![]);
    let started_event_id = t.add_activity_task_started(scheduled_event_id);
    t.add_activity_task_completed(scheduled_event_id, started_event_id, Default::default());
    t.add_workflow_task_scheduled_and_started();

    let mock = mock_workflow_client();
    let mut mock = single_hist_mock_sg("fake_wf_id", t, &[1, 2], mock, true);
    mock.worker_cfg(|cfg| cfg.max_cached_workflows = 1);
    let core = mock_worker(mock);

    let activation = core.poll_workflow_activation().await.unwrap();
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
        activation.run_id,
        ScheduleActivity {
            seq: 1,
            activity_id: "1".to_string(),
            ..Default::default()
        }
        .into(),
    ))
    .await
    .unwrap();
    let activation = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        activation.jobs.as_slice(),
        [
            WorkflowActivationJob {
                variant: Some(workflow_activation_job::Variant::SignalWorkflow(_)),
            },
            WorkflowActivationJob {
                variant: Some(workflow_activation_job::Variant::ResolveActivity(_)),
            }
        ]
    );
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
        activation.run_id,
        vec![
            RequestCancelActivity { seq: 1 }.into(),
            CompleteWorkflowExecution { result: None }.into(),
        ],
    ))
    .await
    .unwrap();

    core.shutdown().await;
}

#[tokio::test]
async fn failing_wft_doesnt_eat_permit_forever() {
    let mut t = TestHistoryBuilder::default();
    t.add_by_type(EventType::WorkflowExecutionStarted);
    t.add_workflow_task_scheduled_and_started();

    let mock = mock_workflow_client();
    let mut mock = single_hist_mock_sg("fake_wf_id", t, [1, 1, 1], mock, true);
    mock.worker_cfg(|cfg| {
        cfg.max_cached_workflows = 2;
        cfg.max_outstanding_workflow_tasks = 2;
    });
    let outstanding_mock_tasks = mock.outstanding_task_map.clone();
    let worker = mock_worker(mock);

    let mut run_id = "".to_string();
    // Fail twice, verifying a permit is eaten. We cannot fail the same run more than twice in a row
    // because we purposefully time out rather than spamming.
    for _ in 1..=2 {
        let activation = worker.poll_workflow_activation().await.unwrap();
        // Issue a nonsense completion that will trigger a WFT failure
        worker
            .complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
                activation.run_id,
                RequestCancelActivity { seq: 1 }.into(),
            ))
            .await
            .unwrap();
        let activation = worker.poll_workflow_activation().await.unwrap();
        assert_matches!(
            activation.jobs.as_slice(),
            [WorkflowActivationJob {
                variant: Some(workflow_activation_job::Variant::RemoveFromCache(_)),
            },]
        );
        run_id = activation.run_id.clone();
        worker
            .complete_workflow_activation(WorkflowActivationCompletion::empty(activation.run_id))
            .await
            .unwrap();
        assert_eq!(worker.outstanding_workflow_tasks(), 0);
        assert_eq!(worker.available_wft_permits(), 2);
    }
    // We should be "out of work" because the mock service thinks we didn't complete the last task,
    // which we didn't, because we don't spam failures. The real server would eventually time out
    // the task. Mock doesn't understand that, so the WFT permit is released because eventually a
    // new one will be generated. We manually clear the mock's outstanding task list so the next
    // poll will work.
    outstanding_mock_tasks
        .unwrap()
        .write()
        .remove_by_left(&run_id);
    let activation = worker.poll_workflow_activation().await.unwrap();
    worker
        .complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
            activation.run_id,
            CompleteWorkflowExecution { result: None }.into(),
        ))
        .await
        .unwrap();

    worker.shutdown().await;
}

#[tokio::test]
async fn cache_miss_will_fetch_history() {
    let mut t = TestHistoryBuilder::default();
    t.add_by_type(EventType::WorkflowExecutionStarted);
    t.add_full_wf_task();
    t.add_we_signaled("sig", vec![]);
    t.add_full_wf_task();
    t.add_workflow_execution_completed();
    let get_exec_resp: GetWorkflowExecutionHistoryResponse = t.get_history_info(2).unwrap().into();

    let mut mh = MockPollCfg::from_resp_batches(
        "fake_wf_id",
        t,
        [ResponseType::ToTaskNum(1), ResponseType::OneTask(2)],
        mock_workflow_client(),
    );
    mh.num_expected_fails = Some(0);
    mh.mock_client
        .expect_get_workflow_execution_history()
        .times(1)
        .returning(move |_, _, _| Ok(get_exec_resp.clone()));
    let mut mock = build_mock_pollers(mh);
    mock.worker_cfg(|cfg| {
        cfg.max_cached_workflows = 1;
    });
    let worker = mock_worker(mock);

    let activation = worker.poll_workflow_activation().await.unwrap();
    assert_matches!(
        activation.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::StartWorkflow(_)),
        }]
    );
    worker
        .complete_workflow_activation(WorkflowActivationCompletion::empty(&activation.run_id))
        .await
        .unwrap();
    // Force an eviction
    worker.request_wf_eviction(
        &activation.run_id,
        "whatever",
        EvictionReason::LangRequested,
    );
    // Handle the eviction, and the restart
    for _ in 1..=2 {
        let activation = worker.poll_workflow_activation().await.unwrap();
        worker
            .complete_workflow_activation(WorkflowActivationCompletion::empty(activation.run_id))
            .await
            .unwrap();
    }
    let activation = worker.poll_workflow_activation().await.unwrap();
    assert_matches!(
        activation.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::SignalWorkflow(_)),
        }]
    );
    worker
        .complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
            activation.run_id,
            CompleteWorkflowExecution { result: None }.into(),
        ))
        .await
        .unwrap();
    assert_eq!(worker.outstanding_workflow_tasks(), 0);
    worker.shutdown().await;
}

/// This test verifies that WFTs which come as replies to completing a WFT are properly delivered
/// via activation polling.
#[tokio::test]
async fn tasks_from_completion_are_delivered() {
    let wfid = "fake_wf_id";
    let mut t = TestHistoryBuilder::default();
    t.add_by_type(EventType::WorkflowExecutionStarted);
    t.add_full_wf_task();
    t.add_we_signaled("sig", vec![]);
    t.add_full_wf_task();
    t.add_workflow_execution_completed();

    let tasks = [hist_to_poll_resp(
        &t,
        wfid.to_owned(),
        1.into(),
        TEST_Q.to_string(),
    )];
    let mut mock = mock_workflow_client();
    mock.expect_complete_workflow_task()
        .times(1)
        .returning(move |_| {
            Ok(RespondWorkflowTaskCompletedResponse {
                workflow_task: Some(hist_to_poll_resp(
                    &t,
                    wfid.to_owned(),
                    2.into(),
                    TEST_Q.to_string(),
                )),
            })
        });
    mock.expect_complete_workflow_task()
        .times(1)
        .returning(|_| Ok(Default::default()));
    let mut mock = MocksHolder::from_client_with_responses(mock, tasks, []);
    mock.worker_cfg(|wc| wc.max_cached_workflows = 2);
    let core = mock_worker(mock);

    let wf_task = core.poll_workflow_activation().await.unwrap();
    core.complete_workflow_activation(WorkflowActivationCompletion::empty(wf_task.run_id))
        .await
        .unwrap();
    let wf_task = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        wf_task.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::SignalWorkflow(_)),
        },]
    );
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
        wf_task.run_id,
        vec![CompleteWorkflowExecution { result: None }.into()],
    ))
    .await
    .unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn evict_missing_wf_during_poll_doesnt_eat_permit() {
    let wfid = "fake_wf_id";
    let mut t = TestHistoryBuilder::default();
    t.add_by_type(EventType::WorkflowExecutionStarted);
    t.add_full_wf_task();
    t.add_we_signaled("sig", vec![]);
    t.add_full_wf_task();
    t.add_workflow_execution_completed();

    let tasks = [hist_to_poll_resp(
        &t,
        wfid.to_owned(),
        // Use a partial task so that we'll fetch history
        ResponseType::OneTask(2),
        TEST_Q.to_string(),
    )];
    let mut mock = mock_workflow_client();
    mock.expect_get_workflow_execution_history()
        .times(1)
        .returning(move |_, _, _| {
            Ok(GetWorkflowExecutionHistoryResponse {
                // Empty history so we error applying it (no jobs)
                history: Some(History { events: vec![] }),
                raw_history: vec![],
                next_page_token: vec![],
                archived: false,
            })
        });
    let mut mock = MocksHolder::from_client_with_responses(mock, tasks, []);
    mock.worker_cfg(|wc| {
        wc.max_cached_workflows = 1;
        wc.max_outstanding_workflow_tasks = 1;
    });
    let core = mock_worker(mock);

    // Should error because mock is out of work
    assert_matches!(core.poll_workflow_activation().await, Err(_));
    assert_eq!(core.available_wft_permits(), 1);

    core.shutdown().await;
}

#[tokio::test]
async fn poll_faster_than_complete_wont_overflow_cache() {
    // Make workflow tasks for 5 different runs
    let tasks: Vec<_> = (1..=5)
        .map(|i| {
            hist_to_poll_resp(
                // New hist each time for new run ids
                &canned_histories::single_timer("1"),
                format!("wf-{}", i),
                ResponseType::ToTaskNum(1),
                TEST_Q.to_string(),
            )
        })
        .collect();
    let mut mock = mock_workflow_client();
    mock.expect_complete_workflow_task()
        .times(3)
        .returning(|_| Ok(Default::default()));
    let mut mock = MocksHolder::from_client_with_responses(mock, tasks, []);
    mock.worker_cfg(|wc| {
        wc.max_cached_workflows = 3;
        wc.max_outstanding_workflow_tasks = 3;
    });
    let core = mock_worker(mock);
    // Poll 4 times, completing once, such that max tasks are never exceeded
    let p1 = core.poll_workflow_activation().await.unwrap();
    let p2 = core.poll_workflow_activation().await.unwrap();
    let p3 = core.poll_workflow_activation().await.unwrap();
    for (i, p_res) in [&p1, &p2, &p3].into_iter().enumerate() {
        assert_matches!(
            &p_res.jobs[0].variant,
            Some(workflow_activation_job::Variant::StartWorkflow(sw))
            if sw.workflow_id == format!("wf-{}", i + 1)
        );
    }
    // Complete first task to free a wft slot. Cache size is at 3
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
        p1.run_id,
        start_timer_cmd(1, Duration::from_secs(1)),
    ))
    .await
    .unwrap();
    // Now we're at cache limit. We will poll for a task, discover it is for a new run, issue
    // an eviction, and buffer the new run task. However, the run we're trying to evict has pending
    // activations! Thus, we must complete them first before this poll will unblock, and then it
    // will unblock with the eviciton.
    let p4 = async {
        let p4 = core.poll_workflow_activation().await.unwrap();
        assert_matches!(
            &p4.jobs.as_slice(),
            [WorkflowActivationJob {
                variant: Some(workflow_activation_job::Variant::RemoveFromCache(_)),
            }]
        );
        p4
    };
    let p2_pending_completer = async {
        // Sleep needed because otherwise the complete unblocks waiting for the cache to free a slot
        // before we have a chance to actually... wait for it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
            p2.run_id,
            start_timer_cmd(1, Duration::from_secs(1)),
        ))
        .await
        .unwrap();
    };
    let (p4, _) = tokio::join!(p4, p2_pending_completer);
    assert_eq!(core.cached_workflows(), 3);

    // This poll should also block until the eviction is actually completed
    let blocking_poll = async {
        let res = core.poll_workflow_activation().await.unwrap();
        assert_matches!(
            &res.jobs[0].variant,
            Some(workflow_activation_job::Variant::StartWorkflow(sw))
            if sw.workflow_id == format!("wf-{}", 4)
        );
        res
    };
    let complete_evict = async {
        core.complete_workflow_activation(WorkflowActivationCompletion::empty(p4.run_id))
            .await
            .unwrap();
    };

    let (_p5, _) = tokio::join!(blocking_poll, complete_evict);
    assert_eq!(core.cached_workflows(), 3);
    // The next poll will get an buffer a task for a new run, and generate an eviction for p3 but
    // that eviction cannot be obtained until we complete the existing outstanding task.
    let p6 = async {
        let p6 = core.poll_workflow_activation().await.unwrap();
        assert_matches!(
            p6.jobs.as_slice(),
            [WorkflowActivationJob {
                variant: Some(workflow_activation_job::Variant::RemoveFromCache(_)),
            }]
        );
        p6
    };
    let completer = async {
        core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
            p3.run_id,
            start_timer_cmd(1, Duration::from_secs(1)),
        ))
        .await
        .unwrap();
    };
    let (p6, _) = tokio::join!(p6, completer);
    let complete_evict = async {
        core.complete_workflow_activation(WorkflowActivationCompletion::empty(p6.run_id))
            .await
            .unwrap();
    };
    let blocking_poll = async {
        // This poll will also block until the last eviction goes through, and when it does it'll
        // produce the final start workflow task
        let res = core.poll_workflow_activation().await.unwrap();
        assert_matches!(
            &res.jobs[0].variant,
            Some(workflow_activation_job::Variant::StartWorkflow(sw))
            if sw.workflow_id == "wf-5"
        );
    };

    tokio::join!(blocking_poll, complete_evict);
    // p5 outstanding and final poll outstanding -- hence one permit available
    assert_eq!(core.available_wft_permits(), 1);
    assert_eq!(core.cached_workflows(), 3);
}

#[tokio::test]
async fn eviction_waits_until_replay_finished() {
    let wfid = "fake_wf_id";
    let t = canned_histories::long_sequential_timers(3);
    let mock = mock_workflow_client();
    let mock = single_hist_mock_sg(wfid, t, &[3], mock, true);
    let core = mock_worker(mock);

    let activation = core.poll_workflow_activation().await.unwrap();
    // Immediately request eviction after getting start workflow
    core.request_workflow_eviction(&activation.run_id);
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
        activation.run_id,
        start_timer_cmd(1, Duration::from_secs(1)),
    ))
    .await
    .unwrap();
    let t1_fired = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        t1_fired.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::FireTimer(_)),
        }]
    );
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmd(
        t1_fired.run_id,
        start_timer_cmd(2, Duration::from_secs(1)),
    ))
    .await
    .unwrap();
    let t2_fired = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        t2_fired.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::FireTimer(_)),
        }]
    );
    core.complete_workflow_activation(WorkflowActivationCompletion::from_cmds(
        t2_fired.run_id,
        vec![CompleteWorkflowExecution { result: None }.into()],
    ))
    .await
    .unwrap();
    // The first two WFTs were replay, and now that we've caught up, the eviction will be sent
    let eviction = core.poll_workflow_activation().await.unwrap();
    assert_matches!(
        eviction.jobs.as_slice(),
        [WorkflowActivationJob {
            variant: Some(workflow_activation_job::Variant::RemoveFromCache(_)),
        }]
    );

    core.shutdown().await;
}
