//! A combo's `models` list may name another combo, so expansion is a walk over
//! user-written data. Before the chain guard, a combo that listed itself (or two
//! that listed each other) recursed until the process aborted with
//! `has overflowed its stack`; a larger stack only moved the abort further out.
//!
//! These run the real `handle_chat` on a deliberately small stack so a
//! regression fails as a test failure rather than a hang.

use router_db::Db;
use router_sse::handlers::chat::{ChatRequest, handle_chat};
use serde_json::{Value, json};

fn db_with(combos: &[Value]) -> Db {
    let db = Db::open(":memory:", 1).unwrap();
    db.with_conn(router_db::migrations::ensure_schema).unwrap();
    db.write(|tx| router_db::repos::settings::update_settings(tx, json!({"requireApiKey": false})))
        .unwrap();
    for combo in combos {
        let combo = combo.clone();
        db.write(move |tx| router_db::repos::combos::create_combo(tx, &combo).map(|_| ()))
            .unwrap();
    }
    db
}

/// Run one chat request on a 1 MiB stack, which is below what the unbounded
/// recursion needed to survive, and return the result.
fn chat(db: Db, model: &str) -> router_sse::handlers::chat_core::ChatResult {
    let model = model.to_string();
    let handle = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let headers = std::collections::HashMap::new();
            rt.block_on(async move {
                handle_chat(
                    &db,
                    ChatRequest {
                        body: json!({
                            "model": model,
                            "messages": [{"role": "user", "content": "x"}],
                        }),
                        headers: &headers,
                        endpoint: Some("/v1/chat/completions"),
                        api_key: None,
                    },
                )
                .await
            })
        })
        .unwrap();
    handle.join().expect("chat thread must not abort")
}

#[test]
fn a_combo_listing_itself_is_rejected_not_recursed() {
    let db = db_with(&[json!({"name": "loop", "models": ["loop"]})]);
    let result = chat(db, "loop");
    assert_eq!(result.status, 400);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Combo cycle") && e.contains("loop -> loop")),
        "unexpected error: {:?}",
        result.error
    );
}

#[test]
fn two_combos_listing_each_other_are_rejected() {
    let db = db_with(&[
        json!({"name": "ping", "models": ["pong"]}),
        json!({"name": "pong", "models": ["ping"]}),
    ]);
    let result = chat(db, "ping");
    assert_eq!(result.status, 400);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Combo cycle")),
        "unexpected error: {:?}",
        result.error
    );
}

#[test]
fn a_three_hop_cycle_is_rejected() {
    let db = db_with(&[
        json!({"name": "a", "models": ["b"]}),
        json!({"name": "b", "models": ["c"]}),
        json!({"name": "c", "models": ["a"]}),
    ]);
    let result = chat(db, "a");
    assert_eq!(result.status, 400);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Combo cycle")),
        "unexpected error: {:?}",
        result.error
    );
}

#[test]
fn a_self_cycle_does_not_poison_the_next_request() {
    // The chain is per request, so a rejected cycle must not make the following
    // request look cyclic.
    let db = db_with(&[json!({"name": "loop", "models": ["loop"]})]);
    assert_eq!(chat(db.clone(), "loop").status, 400);
    let second = chat(db, "loop");
    assert_eq!(second.status, 400);
    assert!(
        second
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Combo cycle")),
        "unexpected error: {:?}",
        second.error
    );
}

#[test]
fn a_cycle_is_caught_on_a_multi_thread_runtime() {
    // The chain travels with the request rather than in thread-local state, so a
    // task migrated between workers must still see it.
    let db = db_with(&[json!({"name": "loop", "models": ["loop"]})]);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let result = rt.block_on(async move {
        let headers = std::collections::HashMap::new();
        handle_chat(
            &db,
            ChatRequest {
                body: json!({
                    "model": "loop",
                    "messages": [{"role": "user", "content": "x"}],
                }),
                headers: &headers,
                endpoint: Some("/v1/chat/completions"),
                api_key: None,
            },
        )
        .await
    });
    assert_eq!(result.status, 400);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Combo cycle")),
        "unexpected error: {:?}",
        result.error
    );
}
