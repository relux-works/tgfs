//! Review repro for TASK-260715-26dnp6: a chat committed as
//! `history_complete=true` with no window (an empty chat), that receives
//! more than one page of messages during downtime, then crashes right
//! after the resumed run's anchor commit.

use gramdrive_source_tdjson::history::{
    ChatCrawl, CrawlMachine, CrawlPlan, CrawlStep, CrawlWindow,
};
use serde_json::{Value, json};

fn message(chat_id: i64, id: i64) -> Value {
    json!({
        "@type": "message",
        "id": id,
        "chat_id": chat_id,
        "date": 1_700_000_000 + id,
        "sender_id": {"@type": "messageSenderUser", "user_id": 42},
        "can_be_saved": true,
        "content": {
            "@type": "messageText",
            "text": {"@type": "formattedText", "text": format!("m{id}"), "entities": []},
        },
    })
}

fn page(chat_id: i64, ids: &[i64]) -> Value {
    let messages: Vec<Value> = ids.iter().map(|id| message(chat_id, *id)).collect();
    json!({"@type": "messages", "total_count": ids.len(), "messages": messages})
}

fn submit(machine: &mut CrawlMachine) -> Value {
    match machine.next_step().expect("a step") {
        CrawlStep::Submit(request) => request,
        other => panic!("expected a submit, got {other:?}"),
    }
}

fn commit(machine: &mut CrawlMachine) -> (Option<CrawlWindow>, bool, usize) {
    match machine.next_step().expect("a step") {
        CrawlStep::Commit(c) => (c.window, c.history_complete, c.records.len()),
        other => panic!("expected a commit, got {other:?}"),
    }
}

fn main() {
    // Durable state after run 1: empty chat, history_complete=true, no
    // window (exactly what the machine itself commits for an empty chat,
    // asserted by `empty_chat_completes_without_a_window`).
    //
    // Downtime: messages 7,8,9,10 arrive (page_size 2 -> more than one
    // page's worth).
    //
    // Run 2 resumes from the durable rows:
    let resumed = ChatCrawl {
        chat_id: 5,
        window: None,
        history_complete: true,
        priority: gramdrive_source_tdjson::history::CrawlPriority::Background,
    };
    let mut machine = CrawlMachine::new(CrawlPlan {
        chats: vec![resumed],
        page_size: 2,
    })
    .expect("plan is valid");

    let request = submit(&mut machine);
    assert_eq!(request["from_message_id"].as_i64(), Some(0), "anchor from 0");
    machine
        .on_response(Ok(page(5, &[10, 9])))
        .expect("anchor page folds");
    let (window, complete, records) = commit(&mut machine);
    println!("run-2 anchor commit: window={window:?} history_complete={complete} records={records}");
    assert_eq!(
        window,
        Some(CrawlWindow {
            oldest_message_id: 9,
            newest_message_id: 10
        })
    );
    // The durable fact this commit persists:
    if complete {
        println!("!! anchor commit persisted history_complete=true while ids 7,8 are still unfetched");
    }

    // CRASH here (machine dropped after the caller persisted that commit).
    // Run 3 resumes from what the commit persisted:
    let resumed = ChatCrawl {
        chat_id: 5,
        window,
        history_complete: complete,
        priority: gramdrive_source_tdjson::history::CrawlPriority::Background,
    };
    let mut machine = CrawlMachine::new(CrawlPlan {
        chats: vec![resumed],
        page_size: 2,
    })
    .expect("plan is valid");
    let request = submit(&mut machine);
    assert_eq!(request["from_message_id"].as_i64(), Some(0), "catch-up from 0");
    machine
        .on_response(Ok(page(5, &[10, 9])))
        .expect("catch-up connects");
    let (window, complete, _) = commit(&mut machine);
    println!("run-3 catch-up commit: window={window:?} history_complete={complete}");
    match machine.next_step().expect("a step") {
        CrawlStep::Done => {
            println!("!! run 3 is Done: messages 7 and 8 are permanently orphaned (silent gap)");
        }
        CrawlStep::Submit(request) => {
            println!(
                "run 3 continues backfill from {:?} — no gap",
                request["from_message_id"]
            );
        }
        other => println!("run 3 next: {other:?}"),
    }
}
