use crate::{
    error::ChatError,
    error::ChatResult,
    message::{Message, MessageInput},
    signal_ui,
    utils::{get_local_header, to_date},
    SignalPayload,
};
use hdk3::prelude::*;
use link::Link;
use metadata::EntryDetails;

use super::{
    LastSeen, LastSeenKey, ListMessages, ListMessagesInput, MessageData, SignalMessageData, SigResults
};

/// Create a new message
pub(crate) fn create_message(message_input: MessageInput) -> ChatResult<MessageData> {
    let MessageInput {
        last_seen,
        channel,
        message,
        chunk,
    } = message_input;

    // Commit the message
    let header_hash = create_entry(&message)?;

    // Get the local header and create the message type for the UI
    let header = get_local_header(&header_hash)?.ok_or(ChatError::MissingLocalHeader)?;
    let message = MessageData::new(header, message)?;

    // Get the channel hash
    let path: Path = channel.clone().into();

    // Add the current time components
    let path = add_chunk_path(path, chunk)?;

    // Ensure the path exists
    path.ensure()?;

    // The actual hash we are going to hang this message on
    let channel_entry_hash = path.hash()?;

    // Get the hash of the last_seen of this message
    let parent_hash_entry = match last_seen {
        LastSeen::Message(hash_entry) => hash_entry,
        LastSeen::First => channel_entry_hash.clone(),
    };

    // Turn the reply to and timestamp into a link tag
    let tag = LastSeenKey::new(parent_hash_entry, message.created_at);
    create_link(
        channel_entry_hash,
        message.entry_hash.clone(),
        LinkTag::from(tag),
    )?;

    // Return the message for the UI
    Ok(message)
}

/// List all the messages on this channel
pub(crate) fn list_messages(list_message_input: ListMessagesInput) -> ChatResult<ListMessages> {
    let ListMessagesInput { channel, chunk, active_chatter: _ } = list_message_input;

    // Removing for now and expecting UI to call add_chatter once every 2 hours.
    // Check if our agent key is active on this path and
    // add it if it's not
    //if active_chatter {
    //    add_chatter(/*channel.chatters_path()*/)?;
    //}

    let mut links: Vec<Link> = Vec::new();
    let mut counter = chunk.start;
    loop {
        // Get the channel hash
        let path: Path = channel.clone().into();

        // Add the chunk component
        let path = add_chunk_path(path, counter)?;

        // Ensure the path exists
        path.ensure()?;

        // Get the actual hash we are going to pull the messages from
        let channel_entry_hash = path.hash()?;

        // Get the message links on this channel
        links.append(&mut get_links(channel_entry_hash.clone(), None)?.into_inner());
        if counter == chunk.end {
            break;
        }
        counter += 1
    }

    links.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    let sorted_messages = get_messages(links)?;
    Ok(sorted_messages.into())
}

pub(crate) fn new_message_signal(message: SignalMessageData) -> ChatResult<()> {
    debug!(format!(
        "Received message: {:?}",
        message.message_data.message.content
    ));
    // emit signal alerting all connected uis about new message
    signal_ui(SignalPayload::SignalMessageData(message))
}

// Turn all the link targets into the actual message
fn get_messages(links: Vec<Link>) -> ChatResult<Vec<MessageData>> {
    let mut messages = Vec::with_capacity(links.len());

    // for every link get details on the target and create the message
    for target in links.into_iter().map(|link| link.target) {
        // Get details because we are going to return the original message and
        // allow the UI to follow the CRUD tree to find which message
        // to actually display.
        let message = match get_details(target, GetOptions::content())? {
            Some(Details::Entry(EntryDetails {
                entry, mut headers, ..
            })) => {
                // Turn the entry into a MessageEntry
                let message: Message = entry.try_into()?;
                let signed_header = match headers.pop() {
                    Some(h) => h,
                    // Ignoring missing messages
                    None => continue,
                };

                // Create the message type for the UI
                MessageData::new(signed_header.header().clone(), message)?
            }
            // Message is missing. This could be an error but we are
            // going to ignore it.
            _ => continue,
        };
        messages.push(message);
    }
    Ok(messages)
}

/// Add the chunk index from the Date type to this path
fn add_chunk_path(path: Path, chunk: u32) -> ChatResult<Path> {
    let mut components: Vec<_> = path.into();

    components.push(format!("{}", chunk).into());
    Ok(components.into())
}

fn chatters_path() -> Path {
    Path::from("chatters")
}

/*  At some point maybe add back in chatters  on a channel, but for now
simple list of global chatters.
pub(crate) fn signal_users_on_channel(signal_message_data: SignalMessageData) -> ChatResult<()> {
    let me = agent_info()?.agent_latest_pubkey;

    let path: Path = signal_message_data.channel_data.channel.chatters_path();
    let hour_path = add_current_hour_path(path.clone())?;
    hour_path.ensure()?;
    signal_hour(hour_path, signal_message_data.clone(), me.clone())?;
    let hour_path = add_current_hour_minus_n_path(path, 1)?;
    hour_path.ensure()?;
    signal_hour(hour_path, signal_message_data, me)?;

    let path: Path = chatters_path();
    signal_chatters(path, signal_message_data, me)?;

    Ok(())
} */

const CHATTER_REFRESH_HOURS : i64 = 2;

use std::collections::HashSet;

pub(crate) fn signal_chatters(
    signal_message_data: SignalMessageData,
) -> ChatResult<SigResults> {
    let me = agent_info()?.agent_latest_pubkey;
    let chatters_path: Path = chatters_path();
    let chatters = get_links(chatters_path.hash()?, None)?.into_inner();
    debug!(format!("num online chatters {}", chatters.len()));
    let now = to_date(sys_time()?);
    let mut sent: usize = 0;
    let mut active: usize = 0;
    let total = chatters.len();
    let mut agents = HashSet::new();
    agents.insert(me);
    for link in chatters.into_iter().filter(|l| {
        let link_time = chrono::DateTime::<chrono::Utc>::from(l.timestamp);
        now.signed_duration_since(link_time).num_hours() < CHATTER_REFRESH_HOURS
    }
    ) {
        let tag = link.tag;
        let agent = tag_to_agent(tag)?;
        if agents.contains(&agent) {
            continue;
        }
        debug!(format!("Signaling {:?}", agent));
        // ignore any errors coming back from call_remotes
        let r:HdkResult<()> = call_remote(
            agent,
            "chat".to_string().into(),
            "new_message_signal".to_string().into(),
            None,
            &signal_message_data,
        );
        if !r.is_err() {
            sent += 1;
        }
        active += 1;
    }
    // temporary debugging result of sending.  This will be removed when we have
    // remote_signal.
    Ok(SigResults {
        total,
        active,
        sent
    })
}

// simplified and expected as a zome call
pub(crate)  fn refresh_chatter() -> ChatResult<()> {
    let path: Path = chatters_path();
    path.ensure()?;
    let agent = agent_info()?.agent_latest_pubkey;
    let agent_tag = agent_to_tag(&agent);
    create_link(path.hash()?, agent.into(), agent_tag.clone())?;
    Ok(())
}

/* old way using hours
fn add_chatter(path: Path) -> ChatResult<()> {
    let agent = agent_info()?.agent_latest_pubkey;
    let agent_tag = agent_to_tag(&agent);

    let hour_path = add_current_hour_path(path.clone())?;
    hour_path.ensure()?;
    let my_chatter = get_links(hour_path.hash()?, Some(agent_tag.clone()))?.into_inner();
    debug!(format!("checking chatters"));
    if my_chatter.is_empty() {
        debug!(format!("adding chatters"));
        create_link(hour_path.hash()?, agent.into(), agent_tag.clone())?;
        let hour_path = add_current_hour_minus_n_path(path, 1)?;
        hour_path.ensure()?;
        for link in get_links(hour_path.hash()?, Some(agent_tag.clone()))?.into_inner() {
            delete_link(link.create_link_hash)?;
        }
    }

    Ok(())
}
*/

fn agent_to_tag(agent: &AgentPubKey) -> LinkTag {
    let agent_tag: &[u8] = agent.as_ref();
    LinkTag::new(agent_tag)
}

fn tag_to_agent(tag: LinkTag) -> ChatResult<AgentPubKey> {
    Ok(AgentPubKey::from_raw_39(tag.0).map_err(|_| ChatError::AgentTag)?)
}

/*
fn add_current_hour_path(path: Path) -> ChatResult<Path> {
    add_current_hour_path_inner(path, None)
}

fn add_current_hour_minus_n_path(path: Path, sub: u64) -> ChatResult<Path> {
    add_current_hour_path_inner(path, Some(sub))
}

fn add_current_hour_path_inner(path: Path, sub: Option<u64>) -> ChatResult<Path> {
    use chrono::{Datelike, Timelike};
    let mut components: Vec<_> = path.into();

    // Get the current times and turn them to dates;
    let mut now = to_date(sys_time()?);
    if let Some(sub) = sub {
        now = date_minus_hours(now, sub);
    }
    let year = now.year().to_string();
    let month = now.month().to_string();
    let day = now.day().to_string();
    let hour = now.hour().to_string();

    // Add the date parts as components to the path
    components.push(year.into());
    components.push(month.into());
    components.push(day.into());
    components.push(hour.into());
    Ok(components.into())
}

fn date_minus_hours(
    date: chrono::DateTime<chrono::Utc>,
    hours: u64,
) -> chrono::DateTime<chrono::Utc> {
    let hours = chrono::Duration::hours(hours as i64);
    date - hours
}
*/
