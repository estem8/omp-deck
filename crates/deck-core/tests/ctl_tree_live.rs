//! The Wave 21 ctl commands against a real omp driven by a scripted model, launched through the
//! session host as a TSP terminal (the chat view's way), with the sessions helper alongside.
//!
//! Runs only when `OMPDECK_TEST_OMP=<path to omp>` is set (sandboxed HOME / `PI_*` / `XDG_*`; no
//! real model, no network beyond loopback). The `omp-deck-host` binary is built when missing.

mod support;

use std::time::Duration;

use deck_core::ctl::{BranchSummary, CtlCommand};
use deck_core::omp::{AdvisorScope, SessError};
use deck_tsp::wire::Event;
use serde_json::{Value, json};
use support::fake_model::{self as fm, Reply};
use support::{Live, TURN, wait_for};

const SUMMARY: &str = "Fake branch summary";

/// Answers a conversation turn with `Reply to <prompt>` and every side request (summaries, titles:
/// no tools offered) with [`SUMMARY`].
fn script(request: &Value) -> Reply {
    if !fm::has_tools(request) {
        return Reply::text(SUMMARY);
    }
    let prompt = prompt_of(request);
    if prompt == "slow" {
        return Reply::slow_text("Reply to slow", Duration::from_secs(6));
    }
    Reply::text(&format!("Reply to {prompt}"))
}

/// The user's own text: omp prefixes the first message of a turn with a system reminder.
fn prompt_of(request: &Value) -> String {
    fm::last_user_text(request)
        .lines()
        .last()
        .unwrap_or_default()
        .to_owned()
}

fn message_ids(entries: &[Value], role: &str) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e["type"] == "message" && e["message"]["role"] == role)
        .filter_map(|e| e["id"].as_str().map(str::to_owned))
        .collect()
}

/// Submits `prompt` and waits until its reply is persisted and omp is idle.
fn turn(live: &Live, prompt: &str) {
    let want = message_ids(&live.entries(), "assistant").len() + 1;
    live.send(prompt);
    wait_for("the reply", TURN, || {
        (message_ids(&live.entries(), "assistant").len() >= want && live.main_idle()).then_some(())
    });
}

fn nav(live: &Live, id: &str, summary: BranchSummary) -> deck_core::ctl::CtlReply {
    live.ctl(
        CtlCommand::Navigate {
            root: live.session_file(),
            entry_id: id.to_owned(),
            summary,
        },
        TURN,
    )
}

#[test]
fn navigate_summarises_the_branch_left_and_redraws_the_tab() {
    let Some(live) = Live::start("nav", script, |_| {}, |_| None) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    turn(&live, "one");
    turn(&live, "two");
    let rows = live.entries();
    let (assistants, users) = (message_ids(&rows, "assistant"), message_ids(&rows, "user"));
    assert_eq!((assistants.len(), users.len()), (2, 2));
    wait_for("the tab to show the second answer", TURN, || {
        live.peek()
            .to_string()
            .contains("Reply to two")
            .then_some(())
    });

    // Plain move: no summary, the tab is redrawn by omp's own command context.
    let plain = nav(&live, &assistants[0], BranchSummary::None);
    assert!(plain.ok, "{plain:?}");
    assert_eq!(plain.leaf.as_deref(), Some(assistants[0].as_str()));
    wait_for("the tab to drop the second answer", TURN, || {
        (!live.peek().to_string().contains("Reply to two")).then_some(())
    });
    assert!(live.entries().iter().all(|e| e["type"] != "branch_summary"));

    // Already there: omp's no-op, reported as its message.
    let same = nav(&live, &assistants[0], BranchSummary::None);
    assert!(same.ok);
    assert_eq!(same.message.as_deref(), Some("Already at this point"));

    // With a summary: the branch left (u1 a1 → back at a1's sibling work) is summarised by the
    // model into a `branch_summary` entry on the new leaf's path.
    turn(&live, "three");
    let before = live.model.count();
    let reply = nav(
        &live,
        &assistants[0],
        BranchSummary::Custom("focus on perf".into()),
    );
    assert!(reply.ok, "{reply:?}");
    let summaries: Vec<Value> = live
        .entries()
        .into_iter()
        .filter(|e| e["type"] == "branch_summary")
        .collect();
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert!(
        summaries[0]["summary"]
            .as_str()
            .is_some_and(|t| t.ends_with(SUMMARY)),
        "{summaries:?}"
    );
    assert_eq!(summaries[0]["parentId"], assistants[0].as_str());
    assert_eq!(
        reply.leaf.as_deref(),
        summaries[0]["id"].as_str(),
        "the summary is the new leaf"
    );
    let request = live
        .model
        .find(before, |r| {
            fm::messages(r)
                .iter()
                .any(|m| fm::text(m).contains("focus on perf"))
        })
        .expect("the custom instructions reached the model");
    assert!(!fm::has_tools(&request), "the summary is a side request");
    wait_for("the summary on screen", TURN, || {
        let tab = live.peek().to_string();
        (tab.contains(SUMMARY) && !tab.contains("Reply to three")).then_some(())
    });

    // Navigating while omp works is refused, never queued.
    live.send("slow");
    wait_for("omp to be streaming", TURN, || {
        (!live.main_idle()).then_some(())
    });
    let refused = nav(&live, &assistants[0], BranchSummary::None);
    assert_eq!(
        (refused.ok, refused.error.as_deref()),
        (false, Some("busy"))
    );
}

#[test]
fn labels_advisors_and_the_task_model_apply_to_the_running_session() {
    let Some(live) = Live::start(
        "cfg",
        script,
        |live| {
            // Advisors are off by default: with the setting off omp stores a saved roster for
            // the next enable and reports 0 active.
            let config = live.agent.join("config.yml");
            let text = std::fs::read_to_string(&config).unwrap();
            std::fs::write(config, text + "advisor:\n  enabled: true\n").unwrap();
        },
        |_| None,
    ) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    turn(&live, "one");
    let rows = live.entries();
    let user = message_ids(&rows, "user")[0].clone();
    let file = live.session_file();

    // Labels: appended to the live session in any state; removal appends an entry without one.
    let label = |text: Option<&str>| {
        live.ctl(
            CtlCommand::SetLabel {
                root: file.clone(),
                entry_id: user.clone(),
                label: text.map(str::to_owned),
            },
            TURN,
        )
    };
    assert!(label(Some("  start here  ")).ok);
    let last_label = || {
        live.entries()
            .into_iter()
            .rev()
            .find(|e| e["type"] == "label")
            .map(|e| (e["targetId"].clone(), e["label"].clone()))
    };
    wait_for("the label on disk", TURN, || {
        (last_label() == Some((json!(user), json!("start here")))).then_some(())
    });
    assert!(label(None).ok);
    wait_for("the label removal on disk", TURN, || {
        (last_label() == Some((json!(user), Value::Null))).then_some(())
    });
    let unknown = live.ctl(
        CtlCommand::SetLabel {
            root: file.clone(),
            entry_id: "nope".into(),
            label: Some("x".into()),
        },
        TURN,
    );
    assert_eq!(
        (unknown.ok, unknown.error.as_deref()),
        (false, Some("no such entry"))
    );
    let stale = live.ctl(
        CtlCommand::SetLabel {
            root: "/some/other.jsonl".into(),
            entry_id: user.clone(),
            label: None,
        },
        TURN,
    );
    assert_eq!(stale.error.as_deref(), Some("session changed"));

    // A session a live omp owns is refused by the closed-session helpers, nothing written.
    let size = std::fs::metadata(&file).unwrap().len();
    assert!(matches!(
        live.omp.rename_session(&file, "Taken"),
        Err(SessError::Busy)
    ));
    assert!(matches!(
        live.omp.set_label(&file, &user, Some("x")),
        Err(SessError::Busy)
    ));
    assert_eq!(std::fs::metadata(&file).unwrap().len(), size);

    // Advisors: saved to the project's WATCHDOG.yml, then applied to this session.
    let advisor = |doc: Option<Value>| {
        live.ctl(
            CtlCommand::AdvisorConfig {
                root: Some(file.clone()),
                scope: AdvisorScope::Project,
                doc,
            },
            TURN,
        )
    };
    let read = advisor(None);
    assert!(read.ok, "{read:?}");
    let watchdog = live.proj.join("WATCHDOG.yml");
    assert_eq!(
        read.value.as_ref().unwrap()["path"],
        watchdog.to_string_lossy().as_ref()
    );
    assert_eq!(read.value.as_ref().unwrap()["doc"]["advisors"], json!([]));

    let doc = json!({
        "instructions": "Keep notes short.",
        "futureTop": {"k": 1},
        "advisors": [{"name": "Reviewer", "model": "fake/fake-1", "tools": ["read"],
            "reviewMode": "turn", "syncBacklog": "3", "futureKnob": ["x"]}],
    });
    let saved = advisor(Some(doc));
    assert!(saved.ok, "{saved:?}");
    assert_eq!(
        saved.message.as_deref(),
        Some("Saved project WATCHDOG.yml — 1 advisor active.")
    );
    assert!(watchdog.is_file());
    let saved_doc = &saved.value.as_ref().unwrap()["doc"];
    assert_eq!(saved_doc["advisors"][0]["name"], "Reviewer");
    assert_eq!(saved_doc["advisors"][0]["syncBacklog"], "3");
    let reread = advisor(None);
    assert_eq!(
        reread.value.as_ref().unwrap()["doc"]["instructions"],
        "Keep notes short."
    );

    let bad = advisor(Some(
        json!({"advisors": [{"name": "X", "reviewMode": "sometimes"}]}),
    ));
    assert!(!bad.ok, "an invalid entry is refused, not written: {bad:?}");
    assert!(
        std::fs::read_to_string(&watchdog)
            .unwrap()
            .contains("Reviewer")
    );

    let cleared = advisor(Some(json!({"advisors": []})));
    assert_eq!(
        cleared.message.as_deref(),
        Some("Removed project WATCHDOG.yml — 0 advisors active.")
    );
    assert!(!watchdog.exists());

    // The Task subagent model: session-only; clearing returns to what is configured (nothing).
    let task = |model: Option<&str>| {
        live.ctl(
            CtlCommand::TaskModelOverride {
                root: Some(file.clone()),
                model: model.map(str::to_owned),
            },
            TURN,
        )
    };
    let set = task(Some("fake/fake-1"));
    assert!(set.ok, "{set:?}");
    assert_eq!(set.value, Some(json!("fake/fake-1")));
    let off = task(None);
    assert_eq!(off.value, None, "nothing configured: no override remains");
    assert!(!task(Some("not a model")).ok);
    assert!(
        !std::fs::read_to_string(live.agent.join("config.yml"))
            .unwrap()
            .contains("agentModelOverrides"),
        "the override is never persisted"
    );
}

/// The ask the tab currently shows: `(surface, option list id)`.
fn ask_dock(live: &Live) -> Option<(String, String)> {
    let doc = live.peek();
    let list = support::nodes(&doc)
        .into_iter()
        .find(|(_, k, p)| k == "list" && p["role"] == "omp.ask.options")?;
    let sf = doc["surfaces"]
        .as_array()?
        .iter()
        .find(|s| s["open"] == true)?["id"]
        .as_str()?;
    Some((sf.to_owned(), list.0))
}

/// Answers the shown ask: picks option `index` (a pick submits it, like the Enter key), or
/// skips the dialog (`None`, its "Skip" button).
fn answer_ask(live: &Live, index: Option<usize>) {
    let (sf, list) = wait_for("omp's ask dialog in the tab", TURN, || ask_dock(live));
    let event = match index {
        Some(i) => Event::Select {
            sf,
            id: list.clone(),
            item: format!("{list}/option:{i}"),
        },
        None => Event::Action {
            sf,
            id: format!("{}.actions/cancel", list.trim_end_matches(".q0")),
            act: "cancel".to_owned(),
            value: None,
            mods: None,
        },
    };
    live.events(&[event]);
}

/// The model sees the ask's answer as a tool result and echoes it.
fn ask_script(request: &Value) -> Reply {
    if !fm::has_tools(request) {
        return Reply::text(SUMMARY);
    }
    let tail = fm::after_last_user(request);
    if let Some(result) = tail.iter().rev().find(|m| fm::role(m) == "tool") {
        return Reply::text(&format!("Answered: {}", fm::text(result)));
    }
    if prompt_of(request) == "ASKME" {
        return Reply::tool_call(
            "ask",
            &json!({"questions":[{"id":"q1","question":"Pick one","options":[{"label":"alpha"},{"label":"beta"}]}]}),
        );
    }
    Reply::text("unscripted")
}

#[test]
fn reanswering_an_ask_opens_omps_dialog_and_resumes_the_turn() {
    let Some(live) = Live::start("ask", ask_script, |_| {}, |_| None) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    live.send("ASKME");
    answer_ask(&live, Some(1));
    wait_for("the first answer's reply", TURN, || {
        (message_ids(&live.entries(), "assistant").len() >= 2 && live.main_idle()).then_some(())
    });
    let ask_results = |rows: &[Value]| -> Vec<Value> {
        rows.iter()
            .filter(|e| e["type"] == "message" && e["message"]["toolName"] == "ask")
            .cloned()
            .collect()
    };
    let rows = live.entries();
    let first = ask_results(&rows);
    assert_eq!(first.len(), 1);
    let first_text = first[0]["message"]["content"].to_string();
    assert!(first_text.contains("beta"), "{first_text}");
    let file = live.session_file();
    let target = first[0]["id"].as_str().unwrap().to_owned();
    let reask = |id: &str| {
        live.client().send(CtlCommand::ReanswerAsk {
            root: file.clone(),
            entry_id: id.to_owned(),
        })
    };
    let reply_of = |rid: u64| {
        wait_for("the re-answer's reply", TURN, || {
            live.client()
                .take_replies()
                .into_iter()
                .find(|(r, _)| *r == rid)
                .map(|(_, r)| r)
        })
    };

    // Only an ask's result can be re-answered.
    let user = message_ids(&rows, "user")[0].clone();
    let refused = reply_of(reask(&user));
    assert_eq!(
        (refused.ok, refused.error.as_deref()),
        (false, Some("not an ask result"))
    );

    // Skipping omp's dialog leaves the session untouched.
    let leaf = live.client().latest().and_then(|s| s.leaf.clone());
    let cancelled = reask(&target);
    answer_ask(&live, None);
    let cancelled = reply_of(cancelled);
    assert_eq!(
        (cancelled.ok, cancelled.error.as_deref()),
        (false, Some("cancelled"))
    );
    assert_eq!(
        ask_results(&live.entries()).len(),
        1,
        "no answer was recorded"
    );
    assert_eq!(live.client().latest().and_then(|s| s.leaf.clone()), leaf);

    // Answering: a sibling result on the same tool call, and the turn resumes with it.
    let before = live.model.count();
    let rid = reask(&target);
    answer_ask(&live, Some(0));
    let done = reply_of(rid);
    assert!(done.ok, "{done:?}");
    let all = ask_results(&live.entries());
    assert_eq!(all.len(), 2, "the old answer stays as a sibling branch");
    assert_eq!(all[0]["parentId"], all[1]["parentId"]);
    assert_eq!(
        all[0]["message"]["toolCallId"],
        all[1]["message"]["toolCallId"]
    );
    let new_text = all[1]["message"]["content"].to_string();
    assert!(
        new_text.contains("alpha") && !new_text.contains("beta"),
        "{new_text}"
    );
    wait_for("the resumed turn", TURN, || {
        (live.model.count() > before && live.main_idle()).then_some(())
    });
    let resumed = live
        .model
        .find(before, |r| {
            fm::has_tools(r)
                && fm::after_last_user(r)
                    .iter()
                    .any(|m| fm::text(m).contains("alpha"))
        })
        .expect("the model received the new answer");
    assert!(
        !fm::after_last_user(&resumed)
            .iter()
            .any(|m| fm::text(m).contains("beta"))
    );
    let last = live
        .entries()
        .into_iter()
        .rev()
        .find(|e| e["type"] == "message" && e["message"]["role"] == "assistant")
        .unwrap();
    assert!(last["message"]["content"].to_string().contains("alpha"));
}

/// Two more models next to `fake-1`: a thinking-capable one and one with a tiny context window.
fn two_more_models(live: &Live) {
    let yml = live.model.models_yml().replace(
        "        input: [text, image]\n",
        "        input: [text, image]\n      - id: fake-think\n        name: Fake Think\n        reasoning: true\n        \
         contextWindow: 200000\n        maxTokens: 4096\n        input: [text]\n      - id: fake-small\n        \
         name: Fake Small\n        contextWindow: 8\n        maxTokens: 4\n        input: [text]\n",
    );
    std::fs::write(live.agent.join("models.yml"), yml).unwrap();
    let config = live.agent.join("config.yml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(config, text + "  slow: fake/fake-think\n").unwrap();
}

/// `(role, model)` of every `model_change` and the level of every `thinking_level_change`.
fn model_and_thinking_entries(rows: &[Value]) -> (Vec<(String, String)>, Vec<String>) {
    let models = rows
        .iter()
        .filter(|e| e["type"] == "model_change")
        .map(|e| {
            (
                e["role"].as_str().unwrap_or("").to_owned(),
                e["model"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let levels = rows
        .iter()
        .filter(|e| e["type"] == "thinking_level_change")
        .map(|e| e["thinkingLevel"].as_str().unwrap_or("").to_owned())
        .collect();
    (models, levels)
}

fn snapshot_model(live: &Live) -> (String, String) {
    let m = live
        .client()
        .latest()
        .and_then(|s| s.model.clone())
        .expect("a model in the snapshot");
    (m.selector, m.thinking)
}

/// Before the first reply omp may report no context usage; its TUI `/switch` counts that as 0 tokens
/// (`getContextUsage()?.tokens ?? 0`), so a fresh session switches directly instead of falling back.
#[test]
fn switch_model_before_the_first_reply_switches_directly() {
    let Some(live) = Live::start("sw0", script, two_more_models, |_| None) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    let r = live.ctl(
        CtlCommand::SwitchModel {
            selector: "fake/fake-think".to_owned(),
        },
        TURN,
    );
    assert!(r.ok, "{r:?}");
    wait_for("the snapshot to show the model", TURN, || {
        (snapshot_model(&live).0 == "fake/fake-think").then_some(())
    });
}

#[test]
fn switch_model_and_set_effort_match_the_typed_commands() {
    let Some(live) = Live::start("sw", script, two_more_models, |_| None) else {
        eprintln!("skipping: OMPDECK_TEST_OMP is not set");
        return;
    };
    turn(&live, "one");
    // An omp upgrade that breaks a route internal answers "unsupported" (the gateway then falls back to the typed
    // commands): a current omp must never do so, in any case below.
    let supported = |r: deck_core::ctl::CtlReply| {
        assert_ne!(
            r.error.as_deref(),
            Some("unsupported"),
            "omp's internals drifted: {r:?}"
        );
        r
    };
    let switch = |sel: &str| {
        supported(live.ctl(
            CtlCommand::SwitchModel {
                selector: sel.to_owned(),
            },
            TURN,
        ))
    };
    let effort = |lvl: &str| {
        supported(live.ctl(
            CtlCommand::SetEffort {
                level: lvl.to_owned(),
            },
            TURN,
        ))
    };
    let in_snapshot = |want: &str| {
        wait_for("the snapshot to show the model", TURN, || {
            (snapshot_model(&live).0 == want).then_some(())
        });
    };

    // By provider/id, by @role, with a :level suffix.
    let by_id = switch("fake/fake-think");
    assert!(by_id.ok, "{by_id:?}");
    assert_eq!(by_id.value.as_ref().unwrap()["model"], "fake/fake-think");
    assert_eq!(
        by_id.message.as_deref(),
        Some("Session-only model: fake/fake-think")
    );
    in_snapshot("fake/fake-think");
    let back = switch("fake/fake-1");
    assert!(back.ok, "{back:?}");
    in_snapshot("fake/fake-1");
    let by_role = switch("@slow");
    assert!(by_role.ok, "{by_role:?}");
    in_snapshot("fake/fake-think");
    let with_level = switch("fake/fake-think:high");
    assert!(with_level.ok, "{with_level:?}");
    assert_eq!(
        snapshot_model(&live),
        ("fake/fake-think".to_owned(), "high".to_owned())
    );
    let (models, levels) = model_and_thinking_entries(&live.entries());
    assert!(
        models.len() >= 4 && models.iter().skip(1).all(|(role, _)| role == "temporary"),
        "{models:?}"
    );
    assert_eq!(levels.last().map(String::as_str), Some("high"));

    // Unknown selector.
    let unknown = switch("nope/none");
    assert_eq!(
        (unknown.ok, unknown.error.as_deref()),
        (false, Some("Unknown model: nope/none"))
    );

    // Effort: a valid level is written, a bad one is omp's text.
    let ok = effort("low");
    assert!(ok.ok, "{ok:?}");
    assert_eq!(ok.message.as_deref(), Some("Thinking set to low."));
    assert_eq!(snapshot_model(&live).1, "low");
    assert_eq!(
        model_and_thinking_entries(&live.entries())
            .1
            .last()
            .map(String::as_str),
        Some("low")
    );
    let bad = effort("bogus");
    let err = bad.error.unwrap_or_default();
    assert!(
        !bad.ok && err.starts_with("Unknown thinking level: bogus. Available:"),
        "{err}"
    );

    // Identity with the typed commands: the same entries (roles, models, levels).
    let before = model_and_thinking_entries(&live.entries());
    live.send("/switch fake/fake-1");
    wait_for("the typed /switch", TURN, || {
        (model_and_thinking_entries(&live.entries()).0.len() > before.0.len()).then_some(())
    });
    let typed = model_and_thinking_entries(&live.entries());
    assert_eq!(
        typed.0.last().unwrap(),
        &("temporary".to_owned(), "fake/fake-1".to_owned())
    );
    live.send("/switch fake/fake-think");
    wait_for("the typed /switch back", TURN, || {
        (snapshot_model(&live).0 == "fake/fake-think").then_some(())
    });
    let n = model_and_thinking_entries(&live.entries()).1.len();
    live.send("/effort medium");
    wait_for("the typed /effort", TURN, || {
        (model_and_thinking_entries(&live.entries()).1.len() > n).then_some(())
    });
    let typed_levels = model_and_thinking_entries(&live.entries()).1;
    assert_eq!(typed_levels.last().map(String::as_str), Some("medium"));
    let ctl_medium = effort("high");
    assert!(ctl_medium.ok);
    let after = model_and_thinking_entries(&live.entries()).1;
    assert_eq!(
        after.last().map(String::as_str),
        Some("high"),
        "same entry kind as the typed /effort"
    );

    // Over context: the target's window is smaller than the transcript -> nothing changes.
    turn(&live, "fill the window");
    let before = (
        snapshot_model(&live),
        model_and_thinking_entries(&live.entries()),
    );
    let over = switch("fake/fake-small");
    assert_eq!(
        (over.ok, over.error.as_deref()),
        (false, Some("needs_compact")),
        "{over:?}"
    );
    assert_eq!(
        (
            snapshot_model(&live),
            model_and_thinking_entries(&live.entries())
        ),
        before
    );
}
