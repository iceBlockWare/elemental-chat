#![allow(dead_code)] // FIXME(timo): remove before merging

use std::{
    sync::{
        atomic::{AtomicU32, Ordering::SeqCst},
        Arc,
    },
    time::Duration,
};

// use chat::channel::*;
// use chat::message::*;
use chat::{
    message::handlers::{FakeMessage, InsertFakeMessagesPayload},
    *,
};
use chrono::{DateTime, TimeZone, Timelike};
use hc_joining_code::Props;
// use holochain::conductor::api::error::{ConductorApiError, ConductorApiResult};
use holochain::sweettest::*;
use holochain_types::prelude::DnaFile;
use proptest::{prelude::*, test_runner::TestRunner};

// Two main time consuming parts to this test:
// - Writing the zome call
// - Getting the holochain setup right. (What part of the holochain setup can we re-use between iterations?)
// - Using proptest for my first time and figuring out how to use it
//
// Possible next steps:
// - [x] Get to a place where we have the proptest randomizing, and can print the inputs
// - [ ] Get to a place where we have the proptest randomizing and holochain usable in each iteration
// - [ ] Write the zome call

prop_compose! {
    fn generate_timestamp()(
        hour in (0_u32..3),
        day in (0_u32..3),
        month in (0_u32..3),
        year in (0_i32..3)
    ) -> Timestamp {
        Timestamp::from(chrono::Utc.ymd(2022 + year, 1 + month, 1 + day).and_hms(hour, 0, 0))
    }
}

prop_compose! {
    fn generate_message_history(length: usize)(
        timestamps in prop::collection::vec(generate_timestamp(), length)
    ) -> Vec<FakeMessage> {
        timestamps.into_iter().enumerate().map(|(i, timestamp)| FakeMessage { content: format!("{}", i), timestamp}).collect()
    }
}

#[derive(Debug)]
struct TestInput {
    message_history: Vec<FakeMessage>,
    earliest_seen: Timestamp,
    target_message_count: usize,
}

prop_compose! {
    fn generate_test_input(length: usize)(
        message_history in generate_message_history(length),
        index in (0..length),
        target_message_count in 0..(length + 2)
    ) -> TestInput {
        dbg!(index, target_message_count);
        TestInput {
            earliest_seen: message_history[index].timestamp.clone(),
            message_history,
            target_message_count,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_batching() {
    // Use prebuilt DNA bundle.
    // You must build the DNA bundle as a separate step before running the test.
    let dna_path = std::env::current_dir()
        .unwrap()
        .join("../../elemental-chat.dna");

    // Convert to DnaFile and apply property overrides
    let dna = SweetDnaFile::from_bundle_with_overrides(
        &dna_path,
        Some(format!("test-{}", chrono::Utc::now().to_rfc3339())),
        // Note that we can use our own native `Props` type
        Some(Props {
            skip_proof: true,
            holo_agent_override: None,
            development_stage: None,
            t_and_c: None,
            t_and_c_agreement: None,
        }),
    )
    .await
    .unwrap();

    let shared_test_state = SharedTestState::new(dna).await;

    struct SharedTestState {
        conductor: SweetConductor,
        alice_chat: SweetZome,
        next_channel: AtomicU32,
    }

    impl SharedTestState {
        async fn new(dna: DnaFile) -> Self {
            // Set up conductor
            let mut conductor = SweetConductor::from_standard_config().await;

            let agents = SweetAgents::get(conductor.keystore(), 1).await;

            // Install apps with single DNA
            let apps = conductor
                .setup_app_for_agents("elemental-chat", &agents, &[dna.into()])
                .await
                .unwrap();
            let ((alice_cell,),) = apps.into_tuples();
            let alice_chat = alice_cell.zome("chat");

            // Setup complete.
            SharedTestState {
                conductor,
                alice_chat,
                next_channel: AtomicU32::new(0),
            }
        }

        fn next_channel_name(&self) -> String {
            let channel_idx = self.next_channel.fetch_add(1, SeqCst);
            format!("Test #{}", channel_idx)
        }

        async fn run(&self, test_input: TestInput) {
            let channel: ChannelData = self
                .conductor
                .call(
                    &self.alice_chat,
                    "create_channel",
                    ChannelInput {
                        name: self.next_channel_name(),
                        entry: Channel {
                            category: "General".into(),
                            uuid: uuid::Uuid::new_v4().to_string(),
                        },
                    },
                )
                .await;

            // Insert messages with artificial timestamps into the DHT
            let _: () = self
                .conductor
                .call(
                    &self.alice_chat,
                    "insert_fake_messages",
                    InsertFakeMessagesPayload {
                        messages: test_input.message_history.clone(),
                        channel: channel.entry.clone(),
                    },
                )
                .await;

            let ListMessages { messages } = tokio::time::timeout(
                Duration::from_millis(2000),
                self.conductor.call(
                    &self.alice_chat,
                    "list_messages",
                    ListMessagesInput {
                        channel: channel.entry,
                        earliest_seen: Some(test_input.earliest_seen.clone()),
                        target_message_count: test_input.target_message_count,
                    },
                ),
            )
            .await
            .unwrap();

            let messages: Vec<_> = messages.into_iter().map(original_fake_message).collect();

            let input = test_input.message_history.clone();

            assert_eq!(
                messages,
                expected_messages(test_input),
                "(returned messages) == (expected_messages). input: {:?}",
                input
            );
            dbg!("compared equal", messages.len());
        }
    }

    let shared_test_state = Arc::new(shared_test_state);

    let test_result = tokio::task::spawn_blocking({
        let shared_test_state = Arc::clone(&shared_test_state);
        move || {
            let mut runner =
                TestRunner::new(proptest::test_runner::Config::with_source_file(file!()));
            runner.run(&generate_test_input(22), move |test_input| {
                tokio::runtime::Handle::current().block_on(shared_test_state.run(test_input));
                Ok(())
            })
        }
    })
    .await
    .unwrap();

    Arc::try_unwrap(shared_test_state)
        .unwrap_or_else(|_| panic!("shared test state should not have outstanding references because test has completed"))
        .conductor
        .shutdown()
        .await;

    test_result.unwrap();
}

fn expected_messages(test_input: TestInput) -> Vec<FakeMessage> {
    fn same_hour(a: &Timestamp, b: &Timestamp) -> bool {
        DateTime::try_from(a).unwrap().time().hour() == DateTime::try_from(b).unwrap().time().hour()
    }

    let TestInput {
        message_history: mut messages,
        earliest_seen,
        target_message_count,
    } = test_input;
    messages.retain(|m| m.timestamp < earliest_seen);
    messages.sort_by_key(|m| m.timestamp);
    let (only_included_if_same_hour, included) =
        messages.split_at(messages.len().saturating_sub(target_message_count));
    let earliest_included_hour = if let Some(m) = included.first() {
        &m.timestamp
    } else {
        return Vec::new();
    };
    let final_cutoff = only_included_if_same_hour
        .iter()
        .rposition(|m| !same_hour(&m.timestamp, &earliest_included_hour))
        .unwrap_or(0);
    messages.drain(0..final_cutoff);
    messages
}

fn original_fake_message(returned_message: MessageData) -> FakeMessage {
    FakeMessage {
        content: returned_message.entry.content,
        timestamp: returned_message.created_at,
    }
}

#[test]
fn expected_messages_works() {
    assert_eq!(
        expected_messages(TestInput {
            message_history: vec![
                FakeMessage {
                    content: "0".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "1".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "2".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "3".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "4".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "5".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "6".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "7".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "8".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "9".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "10".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "11".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "12".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "13".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "14".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "15".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "16".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "17".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "18".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "19".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "20".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                },
                FakeMessage {
                    content: "21".to_owned(),
                    timestamp: Timestamp::from(
                        chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
                    )
                }
            ],
            earliest_seen: Timestamp::from(
                chrono::Utc.ymd(2022 + 0, 1 + 0, 1 + 0).and_hms(0, 0, 0)
            ),
            target_message_count: 1
        }),
        vec![],
    );
}
