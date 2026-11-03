//! The native chat view: omp's Tern Surface Protocol `omp.session` surface drawn in egui instead
//! of the terminal grid. The conversation (`main`) is a virtualized, stick-to-bottom column; the
//! dock (composer, ask/approval, working line) floats above its bottom edge; the layer (pickers,
//! overlays, toasts) floats over everything. Keys go to omp as terminal input, gestures go back
//! as TSP events (`Terminal::send_tsp`); nothing is edited locally.
//!
//! One [`ChatState`] per chat view lives in egui's temp memory under the view's id: the surface
//! snapshot ([`snap::Snap`], re-synced only when the mirrored document moved) and the view's local
//! state (folds, measured heights, parse caches, textures).

mod ansi;
mod code;
mod ctx;
mod dock;
mod events;
mod glyphs;
mod highlight;
mod layer;
mod md;
mod menu;
mod paint;
mod render;
mod select;
mod snap;
mod style;
mod text;
mod tools;
mod transcript;
mod tsel;

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use deck_term::{Terminal, TspRefused, input};
use egui::{Event, EventFilter, Rect, Sense, Ui, pos2, vec2};

use ctx::{Cx, Local};
use snap::Snap;
use style::Ink;

/// Space between the composer and the bottom edge of the pane (px).
const BOTTOM_PAD: f32 = 16.0;
/// How far the conversation runs under the dock's top edge (px).
const OVERLAP: f32 = 14.0;
/// Height of the fade the conversation takes towards the composer (px).
const FADE: f32 = 46.0;
/// Height of the soft fade at the top of the transcript.
const TOP_FADE: f32 = 24.0;

/// The chat view of one terminal.
pub struct ChatView<'a> {
    term: &'a mut Terminal,
    font_size: f32,
    id_salt: Option<u64>,
}

/// What a frame of the chat view reports back to the app.
pub struct ChatResponse {
    /// The whole chat area; `has_focus()` means the composer owns keyboard focus.
    pub response: egui::Response,
    /// A link or file activation the app should open.
    pub open_url: Option<String>,
    /// An event the terminal refused this frame.
    pub refused: Option<TspRefused>,
}

/// Per-view state kept in egui memory.
#[derive(Default)]
struct ChatState {
    snap: Snap,
    loc: Local,
}

/// How long a chat may go unshown before its state (snapshot, caches, fold flips) is dropped; a
/// tab that comes back after that rebuilds it from the terminal's mirror.
const IDLE_EVICT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Notes that the view `id` is shown now and drops the state of views unshown for
/// [`IDLE_EVICT`] (closed tabs would otherwise keep theirs for the life of the app).
fn evict_idle(ctx: &egui::Context, id: egui::Id, now: Instant) {
    let key = egui::Id::NULL.with("omp_deck::ChatView::seen");
    let stale: Vec<egui::Id> = ctx.data_mut(|d| {
        let seen =
            d.get_temp_mut_or_insert_with(key, std::collections::HashMap::<egui::Id, Instant>::new);
        seen.insert(id, now);
        let stale: Vec<egui::Id> = seen
            .iter()
            .filter(|(_, t)| now.saturating_duration_since(**t) > IDLE_EVICT)
            .map(|(k, _)| *k)
            .collect();
        for k in &stale {
            seen.remove(k);
        }
        stale
    });
    for k in stale {
        ctx.data_mut(|d| d.remove::<Arc<Mutex<ChatState>>>(k.with("state")));
    }
}

impl<'a> ChatView<'a> {
    pub fn new(term: &'a mut Terminal) -> Self {
        ChatView {
            term,
            font_size: 14.0,
            id_salt: None,
        }
    }

    /// Separates the state of several chat views (the tab's uid).
    pub fn id_salt(mut self, salt: impl Hash) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        salt.hash(&mut hasher);
        self.id_salt = Some(hasher.finish());
        self
    }

    /// The terminal font size: monospace blocks follow it and the column width is measured in it.
    pub fn font_size(mut self, px: f32) -> Self {
        self.font_size = px;
        self
    }

    pub fn show(self, ui: &mut Ui) -> ChatResponse {
        let ChatView {
            term,
            font_size,
            id_salt,
        } = self;
        term.poll();
        let ctx = ui.ctx().clone();
        let ppp = ctx.pixels_per_point();
        let now = Instant::now();

        let sense = Sense::click_and_drag().union(Sense::focusable_noninteractive());
        let (_, rect) = ui.allocate_space(ui.available_size());
        let id = match id_salt {
            Some(salt) => ui.make_persistent_id(("omp_deck::ChatView", salt)),
            None => ui.scope_id().with("omp_deck::ChatView"),
        };
        // Registered first: everything drawn after it sits above and takes its own clicks.
        let response = ui.interact(rect, id, sense);

        // Focus: a press anywhere on the chat itself (not on a floating layer above it).
        let pressed_at = ui.input(|i| {
            if i.pointer.primary_pressed() {
                i.pointer.interact_pos().filter(|p| rect.contains(*p))
            } else {
                None
            }
        });
        // (Outside the input lock: `layer_id_at` takes the context lock too.)
        let pressed_here =
            pressed_at.is_some_and(|p| ctx.layer_id_at(p).is_none_or(|l| l == ui.layer_id()));
        if response.clicked() || response.drag_started() || pressed_here {
            response.request_focus();
        }
        let focused = response.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }
        term.set_focused(focused);

        // A session that exited takes no input: nothing is sent to its (dead) omp.
        let exit = term.exit_code();

        // The surface, synced only when the document moved (the toast ring with it: omp deletes
        // a toast's node a frame after adding it, so the tree alone never shows one).
        let handle = term.tsp();
        let (rev, epoch) = (handle.revision(), handle.epoch());
        evict_idle(&ctx, id, now);
        let state = ctx.data_mut(|d| {
            d.get_temp_mut_or_insert_with(id.with("state"), || {
                Arc::new(Mutex::new(ChatState::default()))
            })
            .clone()
        });
        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        let ChatState { snap, loc } = &mut *guard;
        if !snap.is_current(rev, epoch) {
            let moved = handle.with_stamp(|doc, rev, epoch| {
                let moved = snap.sync(doc, rev, epoch);
                let ring = doc
                    .toasts(now)
                    .filter(|r| !r.text.trim().is_empty())
                    .map(layer::toast_of);
                layer::merge_toasts(&mut loc.toasts, ring, now);
                moved
            });
            if moved {
                loc.prune(snap, now);
            }
        }
        loc.toasts.retain(|t| now < t.at + t.ttl);
        loc.toast_galleys
            .retain(|seq, _| loc.toasts.iter().any(|t| t.seq == *seq));

        // Selections, then the keys to omp (before drawing: the echo of this frame's typing shows
        // next frame). See `select` for what Ctrl+C and typing over a selection do.
        let mut refused = None;
        loc.tsel.begin(snap);
        if let Some(s) = &loc.sel
            && (!snap.composer().is_some_and(|c| s.is_for(&c.id, c.text()))
                || loc.tsel.has_selection())
        {
            loc.sel = None;
        }
        if focused && exit.is_none() {
            let events: Vec<Event> = ui.input(|i| i.events.clone());
            let modifiers = ui.input(|i| i.modifiers);
            let plan = select::plan(
                &events,
                &mut loc.sel,
                snap.sf.as_str(),
                snap.can_edit,
                loc.tsel.has_selection(),
                &mut loc.swallowed,
            );
            for e in &plan.send {
                if let Err(r) = term.send_tsp(e) {
                    refused.get_or_insert(r);
                }
            }
            if let Some(text) = plan.copy {
                ctx.copy_text(text);
            }
            if plan.copy_transcript {
                // Made once the frame's view is known (below); the selection stays.
                loc.tsel.want_copy = true;
            }
            let mut keys = plan.keys;
            if plan.clear_transcript {
                loc.tsel.clear();
                // Esc interrupts a running turn, so a busy omp still gets it; an idle one
                // doesn't (the Esc only ended the selection, release included).
                if !snap.busy() {
                    keys.retain(|e| {
                        !matches!(
                            e,
                            Event::Key {
                                key: egui::Key::Escape,
                                pressed: true,
                                ..
                            }
                        )
                    });
                    select::swallow(&mut loc.swallowed, egui::Key::Escape);
                }
            }
            term.route_keyboard(&keys, modifiers);
            if plan.paste {
                term.paste_clipboard();
            }
            route_dropped_files(ui, term, &response);
        }
        // A right click that lands on the chat itself (not on a floating layer above it).
        let click = ui
            .input(|i| {
                i.pointer
                    .secondary_clicked()
                    .then(|| i.pointer.interact_pos())
                    .flatten()
            })
            .filter(|p| rect.contains(*p))
            .filter(|p| ctx.layer_id_at(*p).is_none_or(|l| l == ui.layer_id()));
        loc.ctx_click = click;
        loc.ctx_claim = None;

        if loc.preedit != term.preedit() {
            loc.preedit.clear();
            loc.preedit.push_str(term.preedit());
        }
        let ink = Ink::new(ui, font_size);
        let probe = ui.painter().layout_no_wrap(
            "Mg".to_owned(),
            ink.mono_px(ink.mono_size),
            egui::Color32::WHITE,
        );
        if loc.probe.as_ref().is_none_or(|p| !Arc::ptr_eq(p, &probe)) {
            loc.ansi.forget_galleys();
            loc.probe = Some(probe);
        }
        ui.painter().rect_filled(rect, 0.0, ink.p.bg);
        let (cols, rows, cell_w_px, cell_h_px) = grid(&ctx, &ink, rect, ppp);
        term.resize(cols, rows, cell_w_px, cell_h_px);

        if snap.region("main").is_none() {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Waiting for omp…",
                ink.label(13.0),
                ink.dim(),
            );
            return ChatResponse {
                response,
                open_url: None,
                refused,
            };
        }

        let cx = Cx::new(snap, &ink, loc, now, focused, snap.can_edit);
        // A copy of the transcript selection (Ctrl+C, the menu): built from the items themselves,
        // the ones never laid out included.
        if cx.loc(|l| std::mem::take(&mut l.tsel.want_copy))
            && let Some(text) = tsel::copy(&cx, ui, transcript::column_width(rect.width()))
        {
            ctx.copy_text(text);
        }

        // Dock first: its height decides where the conversation ends.
        let cw = transcript::column_width(rect.width());
        let draw_dock = |ui: &mut Ui| match exit {
            Some(code) => dock::exited(&cx, ui, code),
            None => dock::show(&cx, ui),
        };
        let dock_size = transcript::measure_size(ui, cw, &draw_dock);
        let dock_h = dock_size.y;
        let dock_rect = Rect::from_min_size(
            pos2(
                rect.center().x - cw / 2.0,
                (rect.bottom() - BOTTOM_PAD - dock_h).max(rect.top()),
            ),
            vec2(cw, dock_h.min(rect.height())),
        );
        let main_bottom = if dock_h > 0.0 {
            (dock_rect.top() + OVERLAP).min(rect.bottom())
        } else {
            rect.bottom()
        };
        let main_rect = Rect::from_min_max(rect.min, pos2(rect.right(), main_bottom));
        let view = Rect::from_min_max(
            main_rect.min,
            pos2(
                main_rect.right(),
                if dock_h > 0.0 {
                    dock_rect.top()
                } else {
                    main_bottom
                },
            ),
        );
        cx.loc(|l| {
            l.tsel.pointer(
                &ctx,
                cx.snap,
                &tsel::Area {
                    chat: rect,
                    view,
                    layer: ui.layer_id(),
                },
            )
        });
        // The scroll area runs under the floating composer and its fade: the last line gets that
        // much room, so it ends up fully visible above them.
        let end_pad = if dock_h > 0.0 {
            OVERLAP + FADE + 8.0
        } else {
            12.0
        };
        let away = transcript::show(&cx, ui, main_rect, id, end_pad);
        // The conversation also melts into the top edge.
        crate::deco::fill_vertical(
            ui.painter(),
            &[
                main_rect.left_top(),
                main_rect.right_top(),
                pos2(main_rect.right(), main_rect.top() + TOP_FADE),
                pos2(main_rect.left(), main_rect.top() + TOP_FADE),
            ],
            ink.p.bg.gamma_multiply(0.93),
            ink.p.bg.gamma_multiply(0.0),
        );

        if dock_h > 0.0 {
            // The conversation melts into the background above the composer.
            let fade = Rect::from_min_max(
                pos2(rect.left(), dock_rect.top() - FADE),
                pos2(rect.right(), rect.bottom()),
            );
            let pts = [
                fade.left_top(),
                fade.right_top(),
                fade.right_bottom(),
                fade.left_bottom(),
            ];
            let clear = ink.p.bg.gamma_multiply(0.0);
            let mut mesh = egui::Mesh::default();
            mesh.colored_vertex(pts[0], clear);
            mesh.colored_vertex(pts[1], clear);
            let solid_at = ((FADE + 10.0) / fade.height()).clamp(0.0, 1.0);
            let mid_y = fade.top() + fade.height() * solid_at;
            let c = ink.p.bg.gamma_multiply(0.96);
            mesh.colored_vertex(pos2(fade.left(), mid_y), c);
            mesh.colored_vertex(pos2(fade.right(), mid_y), c);
            mesh.colored_vertex(pts[3], c);
            mesh.colored_vertex(pts[2], c);
            mesh.add_triangle(0, 1, 3);
            mesh.add_triangle(0, 3, 2);
            mesh.add_triangle(2, 3, 5);
            mesh.add_triangle(2, 5, 4);
            ui.painter().add(egui::Shape::mesh(mesh));

            let mut dock_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(id.with("dock"))
                    .max_rect(dock_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            dock_ui.style_mut().interaction.selectable_labels = false;
            draw_dock(&mut dock_ui);
        }
        if away {
            // Above the dock and its fade, which would otherwise dim or cover the pill.
            let bottom =
                transcript::pill_bottom((dock_h > 0.0).then(|| dock_rect.top()), rect.bottom());
            transcript::jump_pill(&cx, ui, rect.center().x, bottom);
        }

        if exit.is_none() {
            layer::overlays(&cx, &ctx, rect, id);
        }

        if exit.is_none() {
            let opened = menu::open(&cx);
            menu::show(&cx, ui, id, opened);
        }

        let (out, animate) = cx.into_out();
        if let Some(blob) = out.zoom {
            loc.zoomed = Some(blob);
        }
        if let Some(text) = out.copy {
            ctx.copy_text(text);
        }
        if out.release {
            term.hold_input_until(Instant::now());
        }
        for e in out.events.iter().filter(|_| exit.is_none()) {
            if let Err(r) = term.send_tsp(e) {
                refused.get_or_insert(r);
            }
        }
        if out.paste && exit.is_none() {
            term.paste_clipboard();
        }
        if let Some(after) = animate {
            ctx.request_repaint_after(after);
        }
        ChatResponse {
            response,
            open_url: out.open_url,
            refused,
        }
    }
}

/// Terminal grid for the pane: columns from the chat column's width in the terminal font, rows
/// from the pane height. `(cols, rows, cell_w_px, cell_h_px)`.
fn grid(ctx: &egui::Context, ink: &Ink, rect: Rect, ppp: f32) -> (u16, u16, u16, u16) {
    let font = ink.mono_px(ink.mono_size);
    let (advance, row_h) = ctx.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
    let cell_w_px = (advance * ppp).round().max(1.0);
    let cell_h_px = (row_h * ppp).ceil().max(1.0);
    let column = transcript::column_width(rect.width());
    let cols = ((column * ppp) / cell_w_px).floor().clamp(2.0, 4096.0);
    let rows = ((rect.height() * ppp) / cell_h_px)
        .floor()
        .clamp(1.0, 4096.0);
    (cols as u16, rows as u16, cell_w_px as u16, cell_h_px as u16)
}

/// Files dropped on the chat paste their paths into the composer like the terminal view does.
fn route_dropped_files(ui: &Ui, term: &mut Terminal, response: &egui::Response) {
    let dropped =
        ui.input(|i| input::encode_dropped_paths(i.raw.dropped_files.iter().map(|f| f.path())));
    if dropped.is_empty() || term.input_held() {
        return;
    }
    let _ = response;
    match term.paste_bytes(&dropped) {
        Some(bytes) => term.write(&bytes),
        None => term.write(dropped.as_bytes()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    //! The chat view driven through egui with synthetic input events and a headless terminal whose
    //! child replays a recorded omp session, then records what reaches its input.

    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use deck_term::{SpawnSpec, TermOptions, Terminal};
    use egui::{Event, ImeEvent, Key, Modifiers, PointerButton, Pos2, Rect, Shape, pos2, vec2};

    use super::*;

    struct Harness {
        ctx: egui::Context,
        term: Terminal,
        dir: PathBuf,
        time: f64,
        id: egui::Id,
        /// What the view asked to open (links clicked).
        opened: Vec<String>,
        modifiers: Modifiers,
    }

    impl Harness {
        /// A terminal whose child prints the recorded session's frames, then copies its input
        /// (raw, no echo) to `typed`.
        fn new(name: &str) -> Harness {
            Harness::with_frames(name, &[])
        }

        /// [`Harness::new`], the child printing `frames` (JSON frame bodies of surface `s:1`,
        /// numbered after the recorded session's last) after the recorded session.
        fn with_frames(name: &str, frames: &[serde_json::Value]) -> Harness {
            Harness::with_frames_later(name, frames, &[])
        }

        /// [`Harness::with_frames`], and `later` frames (numbered after `frames`) that the child
        /// prints only once [`Harness::release_later`] is called.
        fn with_frames_later(
            name: &str,
            frames: &[serde_json::Value],
            later: &[serde_json::Value],
        ) -> Harness {
            let dir =
                std::env::temp_dir().join(format!("omp-deck-chat-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            // The recording starts after omp's `hello`: say it again (edit-capable, as 18.15.0).
            let hello = serde_json::json!({
                "q": "hello", "v": [1], "app": "omp", "ver": "18.15.0",
                "features": ["edit", "undo", "send"],
            });
            let mut wire = deck_tsp::stream::encode_message(
                deck_tsp::stream::Verb::Query,
                &[],
                &serde_json::to_vec(&hello).expect("hello JSON"),
            );
            wire.extend(snap::tests::fixture_wire(0..usize::MAX).concat());
            for (i, ops) in frames.iter().enumerate() {
                let body = serde_json::json!({"sf": "s:1", "s": 64 + i, "ops": ops});
                wire.extend(deck_tsp::stream::encode_message(
                    deck_tsp::stream::Verb::Frame,
                    &[],
                    &serde_json::to_vec(&body).expect("frame JSON"),
                ));
            }
            std::fs::write(dir.join("frames"), wire).expect("frames file");
            let mut after = Vec::new();
            for (i, ops) in later.iter().enumerate() {
                let body = serde_json::json!({"sf": "s:1", "s": 64 + frames.len() + i, "ops": ops});
                after.extend(deck_tsp::stream::encode_message(
                    deck_tsp::stream::Verb::Frame,
                    &[],
                    &serde_json::to_vec(&body).expect("frame JSON"),
                ));
            }
            std::fs::write(dir.join("later"), after).expect("later file");
            // The later frames wait for `go` (see `release_later`).
            let script = if later.is_empty() {
                "stty raw -echo; cat frames; cat > typed"
            } else {
                // The later frames wait for `go` (see `release_later`).
                "stty raw -echo; cat frames; while [ ! -e go ]; do sleep 0.02; done; \
                 cat later; cat > typed"
            };
            let ctx = egui::Context::default();
            crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
            ctx.run_ui(egui::RawInput::default(), |_| {})
                .textures_delta
                .clear();
            crate::ui_fonts::finalize(&ctx);
            let term = Terminal::spawn(
                SpawnSpec {
                    program: "sh".into(),
                    args: vec!["-c".into(), script.into()],
                    cwd: dir.clone(),
                    env: vec![("TERM".into(), "xterm-256color".into())],
                    tmux_client: false,
                },
                TermOptions::default(),
                ctx.clone(),
            )
            .expect("spawn sh");
            let mut h = Harness {
                ctx,
                term,
                dir,
                time: 1.0,
                id: egui::Id::NULL,
                opened: Vec::new(),
                modifiers: Modifiers::NONE,
            };
            let until = Instant::now() + Duration::from_secs(10);
            while !(h.term.tsp().live() && h.term.tsp().revision() > 0) {
                assert!(Instant::now() < until, "the surface never arrived");
                h.term.poll();
                std::thread::sleep(Duration::from_millis(20));
            }
            h
        }

        /// One frame with `events`; the shapes it painted.
        fn frame(&mut self, mut events: Vec<Event>) -> egui::FullOutput {
            if self.modifiers != Modifiers::NONE {
                events.insert(0, Event::ModifiersChanged(self.modifiers));
            }
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 700.0))),
                events,
                focused: true,
                time: Some(self.time),
                ..Default::default()
            };
            self.time += 0.05;
            let term = &mut self.term;
            let opened = &mut self.opened;
            let mut id = egui::Id::NULL;
            let mut out = self.ctx.run_ui(raw, |ui| {
                let shown = ChatView::new(term).id_salt(7).show(ui);
                id = shown.response.id;
                opened.extend(shown.open_url);
            });
            out.textures_delta.clear();
            self.id = id;
            out
        }

        /// Clicks the empty transcript: the chat takes the keyboard focus.
        fn focus(&mut self) {
            let at = pos2(450.0, 60.0);
            let button = |pressed| Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            self.frame(vec![Event::PointerMoved(at), button(true)]);
            self.frame(vec![button(false)]);
            self.frame(vec![]);
            assert!(self.term.tsp().live());
        }

        /// Runs `f` on the view's local state.
        fn local<R>(&self, f: impl FnOnce(&Local) -> R) -> R {
            let state = self
                .ctx
                .data(|d| d.get_temp::<Arc<Mutex<ChatState>>>(self.id.with("state")))
                .expect("chat state");
            let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
            f(&guard.loc)
        }

        /// Frames until the chat stops moving (scroll follow, fades): the painted text sits still.
        fn settle(&mut self) -> egui::FullOutput {
            let mut last = galleys(&self.frame(vec![]));
            for _ in 0..120 {
                let out = self.frame(vec![]);
                let now = galleys(&out);
                if now == last {
                    return out;
                }
                last = now;
            }
            panic!("the chat never settled");
        }

        fn caret(&self) -> Rect {
            let state = self
                .ctx
                .data(|d| d.get_temp::<Arc<Mutex<ChatState>>>(self.id.with("state")))
                .expect("chat state");
            let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
            guard.loc.caret.expect("the composer drew its caret")
        }

        /// Polls the terminal until `ok` holds on its document.
        fn wait_for(&mut self, what: &str, ok: impl Fn(&deck_tsp::document::Document) -> bool) {
            let until = Instant::now() + Duration::from_secs(5);
            while !self.term.tsp().with(&ok) {
                assert!(Instant::now() < until, "{what}");
                self.term.poll();
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// What the child's input holds so far, once it holds `want`.
        fn typed(&mut self, want: &[u8]) -> Vec<u8> {
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                self.term.poll();
                let got = std::fs::read(self.dir.join("typed")).unwrap_or_default();
                if got.windows(want.len().max(1)).any(|w| w == want) || Instant::now() > until {
                    return got;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Text shapes `text` among the painted shapes: their positions.
    fn text_at(out: &egui::FullOutput, text: &str) -> Vec<Pos2> {
        fn walk(shape: &Shape, text: &str, hits: &mut Vec<Pos2>) {
            match shape {
                Shape::Text(t) if t.galley.text() == text => hits.push(t.pos),
                Shape::Vec(v) => v.iter().for_each(|s| walk(s, text, hits)),
                _ => {}
            }
        }
        let mut hits = Vec::new();
        for clipped in &out.shapes {
            walk(&clipped.shape, text, &mut hits);
        }
        hits
    }

    fn ime(e: ImeEvent) -> Event {
        Event::Ime(e)
    }

    #[test]
    #[allow(deprecated)]
    fn ime_composition_shows_at_the_caret_and_commits_to_omp() {
        let mut h = Harness::new("ime");
        h.focus();
        let before = h.frame(vec![]);
        let caret = h.caret();
        let ime_out = before
            .platform_output
            .ime
            .expect("a focused composer tells the platform where its caret is");
        assert_eq!(ime_out.cursor_rect, caret);
        assert!(ime_out.rect.contains_rect(caret));

        // Enabled → Preedit: shown underlined at the caret, nothing sent.
        h.frame(vec![ime(ImeEvent::Enabled)]);
        let out = h.frame(vec![ime(ImeEvent::Preedit {
            text: "ni".into(),
            active_range_chars: Some(0..2),
        })]);
        assert_eq!(h.term.preedit(), "ni");
        let hits = text_at(&out, "ni");
        assert!(
            hits.iter().any(|p| (*p - caret.min).abs().max_elem() < 1.0),
            "the preedit is drawn at the caret {caret:?}: {hits:?}"
        );
        // The platform keeps following the caret while composing.
        assert!(out.platform_output.ime.is_some());

        // The candidate changes: the shown text follows.
        let out = h.frame(vec![ime(ImeEvent::Preedit {
            text: "nǐ".into(),
            active_range_chars: None,
        })]);
        assert!(!text_at(&out, "nǐ").is_empty() && text_at(&out, "ni").is_empty());

        // Commit: the text reaches omp's input, the preedit is gone.
        let out = h.frame(vec![ime(ImeEvent::Commit("你".into()))]);
        assert_eq!(h.term.preedit(), "");
        assert!(text_at(&out, "nǐ").is_empty());
        assert_eq!(h.typed("你".as_bytes()), "你".as_bytes());

        // A dismissed composition (empty preedit) leaves nothing behind and sends nothing.
        h.frame(vec![ime(ImeEvent::Preedit {
            text: "x".into(),
            active_range_chars: None,
        })]);
        let out = h.frame(vec![ime(ImeEvent::Preedit {
            text: String::new(),
            active_range_chars: None,
        })]);
        assert_eq!(h.term.preedit(), "");
        assert!(text_at(&out, "x").is_empty());
        // Plain typing still works next to it.
        h.frame(vec![
            Event::Key {
                key: Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            Event::Text("a".into()),
        ]);
        assert_eq!(h.typed("你a".as_bytes()), "你a".as_bytes());
    }

    /// Painted galleys: `(pos, size, text)` of every text shape.
    fn galleys(out: &egui::FullOutput) -> Vec<(Pos2, egui::Vec2, String)> {
        fn walk(shape: &Shape, hits: &mut Vec<(Pos2, egui::Vec2, String)>) {
            match shape {
                Shape::Text(t) => hits.push((t.pos, t.galley.size(), t.galley.text().to_owned())),
                Shape::Vec(v) => v.iter().for_each(|s| walk(s, hits)),
                _ => {}
            }
        }
        let mut hits = Vec::new();
        for clipped in &out.shapes {
            walk(&clipped.shape, &mut hits);
        }
        hits
    }

    fn copied(out: &egui::FullOutput) -> Vec<String> {
        out.platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    fn pointer(at: Pos2, pressed: Option<bool>) -> Vec<Event> {
        let mut events = vec![Event::PointerMoved(at)];
        if let Some(pressed) = pressed {
            events.push(Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            });
        }
        events
    }

    /// The `edit` events the child received, in order (`ESC _ tsp;e … ESC \`).
    fn edits(typed: &[u8]) -> Vec<serde_json::Value> {
        let text = String::from_utf8_lossy(typed);
        text.split("\u{1b}_tsp;e")
            .skip(1)
            .filter_map(|chunk| {
                let body = chunk.split("\u{1b}\\").next()?;
                serde_json::from_str::<serde_json::Value>(&body[body.find('{')?..]).ok()
            })
            .filter(|e| e["ev"] == "edit")
            .collect()
    }

    /// What the child's input holds besides TSP events and replies.
    fn plain_input(typed: &[u8]) -> String {
        let text = String::from_utf8_lossy(typed);
        let mut out = String::new();
        let mut rest = text.as_ref();
        while let Some(at) = rest.find("\u{1b}_tsp;") {
            out.push_str(&rest[..at]);
            rest = rest[at..].split_once("\u{1b}\\").map_or("", |(_, r)| r);
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn a_toast_shows_from_the_ring_once_after_omp_deleted_its_node() {
        let frames = [
            serde_json::json!([["add", "t1", "main", null, {"id": "t1", "k": "toast", "p": {"text": "Saved", "sub": "draft.md", "ttl": 30000}}]]),
            // omp deletes the toast with its next frame.
            serde_json::json!([["del", "t1"]]),
            serde_json::json!([["add", "t2", "main", null, {"id": "t2", "k": "toast", "p": {"text": "Copied", "ttl": 30000}}]]),
            serde_json::json!([["del", "t2"]]),
        ];
        let mut h = Harness::with_frames("toast", &frames);
        h.wait_for("both toasts reach the ring", |d| {
            d.toasts(Instant::now()).count() == 2
        });
        // (An area is laid out invisibly in its first frame.)
        h.frame(vec![]);
        let out = h.frame(vec![]);
        assert!(
            h.term
                .tsp()
                .with(|d| d.surfaces().iter().all(|s| s.node("t1").is_none())),
            "omp's node is gone from the tree"
        );
        let shown =
            |out: &egui::FullOutput, t: &str| galleys(out).iter().filter(|g| g.2 == t).count();
        assert_eq!(shown(&out, "Saved"), 1);
        assert_eq!(shown(&out, "draft.md"), 1);
        assert_eq!(shown(&out, "Copied"), 1);
        // The ring is read again every frame the document moves: no duplicates.
        h.frame(vec![]);
        let out = h.frame(vec![]);
        assert_eq!(shown(&out, "Saved"), 1);
        assert_eq!(
            h.local(|l| l.toasts.iter().map(|t| t.seq).collect::<Vec<_>>()),
            [1, 2]
        );
    }

    fn ctrl_c(pressed: bool) -> Event {
        Event::Key {
            key: Key::C,
            physical_key: Some(Key::C),
            pressed,
            repeat: false,
            modifiers: Modifiers {
                ctrl: true,
                command: true,
                ..Modifiers::NONE
            },
        }
    }

    fn esc(pressed: bool) -> Event {
        Event::Key {
            key: Key::Escape,
            physical_key: Some(Key::Escape),
            pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn esc_ends_a_selection_and_still_interrupts_a_running_turn() {
        let frames = [serde_json::json!([["add", "zwork", "dock", null, {
            "id": "zwork", "k": "row", "p": {"role": "omp.working"}
        }]])];
        let mut h = Harness::with_frames("esc-busy", &frames);
        h.wait_for("the working row arrives", |d| {
            d.surfaces().iter().any(|s| s.node("zwork").is_some())
        });
        h.focus();
        let out = h.settle();
        let (pos, size, _) = galleys(&out)
            .into_iter()
            .find(|g| g.2.starts_with("Streamed answer"))
            .expect("the conversation shows the answer");
        let (a, b) = (
            pos + vec2(3.0, size.y / 2.0),
            pos + vec2(110.0, size.y / 2.0),
        );
        h.frame(pointer(a, None));
        h.frame(pointer(a, Some(true)));
        h.frame(pointer(b, None));
        h.frame(pointer(b, Some(false)));
        assert!(h.local(|l| l.tsel.has_selection()));
        h.frame(vec![esc(true)]);
        h.frame(vec![esc(false)]);
        assert!(
            !h.local(|l| l.tsel.has_selection()),
            "Esc ended the selection"
        );
        assert!(
            h.typed(&[0x1b]).contains(&0x1b),
            "omp was busy: its Esc (the interrupt) was sent too"
        );
    }

    #[test]
    fn ctrl_c_copies_a_selection_and_only_without_one_interrupts_omp() {
        let mut h = Harness::new("ctrlc");
        h.focus();
        let out = h.settle();
        // A line of the conversation to select.
        let (pos, size, text) = galleys(&out)
            .into_iter()
            .find(|g| g.2.starts_with("Streamed answer"))
            .expect("the conversation shows the answer");
        let (a, b) = (
            pos + vec2(3.0, size.y / 2.0),
            pos + vec2(110.0, size.y / 2.0),
        );
        h.frame(pointer(a, None));
        h.frame(pointer(a, Some(true)));
        h.frame(pointer(b, None));
        h.frame(pointer(b, Some(false)));
        assert!(
            h.local(|l| l.tsel.has_selection()),
            "dragging over {text:?} selects it"
        );

        // Ctrl+C (the real chord: the app leaves it a key event while the chat has focus) with
        // the selection copies it and does not reach omp.
        let out = h.frame(vec![ctrl_c(true)]);
        let got = copied(&out);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            !got[0].is_empty() && text.contains(got[0].trim()),
            "{got:?}"
        );
        std::thread::sleep(Duration::from_millis(300));
        h.term.poll();
        assert!(h.typed(&[3]).is_empty(), "no interrupt reached omp");
        // The copy kept the selection: copying again gives the same text.
        h.frame(vec![ctrl_c(false)]);
        h.frame(vec![]);
        assert!(h.local(|l| l.tsel.has_selection()));
        let again = copied(&h.frame(vec![ctrl_c(true)]));
        assert_eq!(again, got, "a second copy of the same selection");
        h.frame(vec![ctrl_c(false)]);
        // Esc in the transcript ends it (and is not omp's); then Ctrl+C is omp's again.
        h.frame(vec![esc(true)]);
        h.frame(vec![esc(false)]);
        assert!(!h.local(|l| l.tsel.has_selection()));
        let out = h.frame(vec![ctrl_c(true)]);
        assert!(copied(&out).is_empty());
        assert_eq!(h.typed(&[3]), [3], "the plain 0x03, and no Esc");
    }

    #[test]
    fn the_menu_keeps_the_selection_painted_and_its_copy_reaches_the_output() {
        let mut h = Harness::new("menucopy");
        h.focus();
        let out = h.settle();
        let (pos, size, text) = galleys(&out)
            .into_iter()
            .find(|g| g.2.starts_with("Streamed answer"))
            .expect("the conversation shows the answer");
        let (a, b) = (
            pos + vec2(3.0, size.y / 2.0),
            pos + vec2(110.0, size.y / 2.0),
        );
        h.frame(pointer(a, None));
        h.frame(pointer(a, Some(true)));
        h.frame(pointer(b, None));
        h.frame(pointer(b, Some(false)));
        let secondary = |pressed| {
            vec![Event::PointerButton {
                pos: b,
                button: PointerButton::Secondary,
                pressed,
                modifiers: Modifiers::NONE,
            }]
        };
        h.frame(secondary(true));
        h.frame(secondary(false));
        h.frame(vec![]);
        assert!(
            h.local(|l| l.menu.is_some()),
            "the right click opened the menu"
        );
        assert!(
            h.local(|l| l.tsel.has_selection()),
            "the selection stays while the menu is open"
        );
        // Clicking "Copy" copies the selection (the entry is the menu's text "Copy").
        let out = h.frame(vec![]);
        let (pos, size, _) = galleys(&out)
            .into_iter()
            .find(|g| g.2 == "Copy")
            .expect("the menu offers Copy");
        let at = pos + size / 2.0;
        h.frame(pointer(at, None));
        h.frame(pointer(at, Some(true)));
        h.frame(pointer(at, Some(false)));
        let out = h.frame(vec![]);
        let got = copied(&out);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            !got[0].is_empty() && text.contains(got[0].trim()),
            "{got:?}"
        );
        assert!(h.local(|l| l.menu.is_none()), "the entry closed the menu");
    }

    #[test]
    fn dragging_in_the_composer_selects_and_typing_replaces_it_with_an_edit() {
        let frames =
            [serde_json::json!([["set", "3.line/input", {"text": "hello world", "cursor": 11}]])];
        let mut h = Harness::with_frames("composer", &frames);
        h.wait_for("the composer text", |d| {
            d.surfaces().iter().any(|s| {
                s.node("3.line/input")
                    .is_some_and(|n| n.props().get("text").is_some_and(|t| t == "hello world"))
            })
        });
        h.focus();
        let out = h.settle();
        let (pos, size, _) = galleys(&out)
            .into_iter()
            .find(|g| g.2 == "hello world")
            .expect("the composer draws its text");
        let y = pos.y + size.y / 2.0;
        // Drag from the start across the first word.
        h.frame(pointer(pos2(pos.x + 1.0, y), None));
        h.frame(pointer(pos2(pos.x + 1.0, y), Some(true)));
        h.frame(pointer(pos2(pos.x + size.x * 0.4, y), None));
        h.frame(pointer(pos2(pos.x + size.x * 0.4, y), Some(false)));
        let (from, to) = h
            .local(|l| l.sel.as_ref().map(|s| s.range()))
            .expect("the drag selected text");
        assert!(from == 0 && (3..=6).contains(&to), "{from}..{to}");
        // Releasing moved omp's caret to the selection's end.
        let typed = h.typed(b"\"ev\":\"edit\"");
        let moves = edits(&typed);
        assert_eq!(moves.len(), 1, "{moves:?}");
        assert_eq!(
            (moves[0]["from"].as_u64(), moves[0]["to"].as_u64()),
            (Some(to as u64), Some(to as u64))
        );

        // Typing replaces the selected range; neither the key nor its text reach omp as input.
        let key = |pressed| Event::Key {
            key: Key::Z,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        h.frame(vec![key(true), Event::Text("z".into()), key(false)]);
        let sent = loop {
            let sent = edits(&h.typed(b"\"text\":\"z\""));
            if sent.len() >= 2 {
                break sent;
            }
            h.term.poll();
        };
        let e = &sent[1];
        assert_eq!(
            (
                e["ev"].as_str(),
                e["from"].as_u64(),
                e["to"].as_u64(),
                e["text"].as_str(),
                e["len"].as_u64()
            ),
            (Some("edit"), Some(0), Some(to as u64), Some("z"), Some(11))
        );
        assert_eq!(e["cursor"].as_u64(), Some(1));
        assert!(
            !plain_input(&h.typed(b"z")).contains('z'),
            "the text travels in the edit only"
        );
        assert!(h.local(|l| l.sel.is_none()));
    }

    // ---- the transcript's text selection ------------------------------------------------------

    /// Turn `i` as the transcript paints it (its emphasis and link dropped to plain text).
    fn turn(i: usize) -> String {
        format!("Turn {i:02} opens with alpha{i:02}, then bold{i:02} and link{i:02} closes.")
    }

    /// One frame adding `n` conversation turns: `md` items with emphasis and a link.
    fn turns_frame(n: usize) -> serde_json::Value {
        let ops: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                let id = format!("t{i:02}");
                let text = format!(
                    "Turn {i:02} opens with alpha{i:02}, then **bold{i:02}** and \
                     [link{i:02}](https://example.com/{i:02}) closes."
                );
                serde_json::json!(["add", id, "main", null, {"id": id, "k": "md", "p": {"text": text}}])
            })
            .collect();
        serde_json::Value::Array(ops)
    }

    /// A chat showing `n` turns after the recorded session, following the end, focused.
    fn turns_harness(name: &str, n: usize, later: &[serde_json::Value]) -> Harness {
        let mut h = Harness::with_frames_later(name, &[turns_frame(n)], later);
        let last = format!("t{:02}", n - 1);
        h.wait_for("the turns arrive", |d| {
            d.surfaces().iter().any(|s| s.node(&last).is_some())
        });
        h.focus();
        h.settle();
        h
    }

    fn wheel(dy: f32) -> Event {
        Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, dy),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
            modifiers: Modifiers::NONE,
        }
    }

    /// Texts of the painted lines that carry a selection highlight.
    fn highlighted(out: &egui::FullOutput, bg: egui::Color32) -> Vec<String> {
        fn walk(shape: &Shape, bg: egui::Color32, hits: &mut Vec<String>) {
            match shape {
                Shape::Text(t)
                    if t.galley
                        .rows
                        .iter()
                        .any(|r| r.row.visuals.mesh.vertices.iter().any(|v| v.color == bg)) =>
                {
                    hits.push(t.galley.text().to_owned());
                }
                Shape::Vec(v) => v.iter().for_each(|s| walk(s, bg, hits)),
                _ => {}
            }
        }
        let mut hits = Vec::new();
        for clipped in &out.shapes {
            walk(&clipped.shape, bg, &mut hits);
        }
        hits
    }

    impl Harness {
        /// Lets the child print its later frames.
        fn release_later(&mut self) {
            std::fs::write(self.dir.join("go"), b"").expect("go file");
        }

        /// Where the transcript shows (the chat minus the dock).
        fn view(&self) -> Rect {
            self.local(|l| l.tsel.view)
                .expect("the transcript was drawn")
        }

        /// The painted line starting with `what`, if it lies well inside the transcript's view
        /// (clear of the bands in which a drag scrolls it).
        fn line_in_band(&self, out: &egui::FullOutput, what: &str) -> Option<(Pos2, egui::Vec2)> {
            let view = self.view();
            galleys(out)
                .into_iter()
                .find(|g| {
                    g.2.starts_with(what)
                        && g.0.y > view.top() + 36.0
                        && g.0.y + g.1.y < view.bottom() - 36.0
                })
                .map(|g| (g.0, g.1))
        }

        /// Wheel-scrolls (the content moves `dy` per frame) until the line starting with `what`
        /// rests in the view; its position.
        fn scroll_to(&mut self, what: &str, dy: f32) -> (Pos2, egui::Vec2) {
            let over = pos2(450.0, self.view().center().y);
            for _ in 0..300 {
                let out = self.frame(vec![Event::PointerMoved(over), wheel(dy)]);
                if self.line_in_band(&out, what).is_some() {
                    let out = self.settle();
                    if let Some(at) = self.line_in_band(&out, what) {
                        return at;
                    }
                }
            }
            let out = self.frame(vec![]);
            panic!(
                "{what:?} never came to rest in view (y={}, view={:?}): {:?}",
                self.local(|l| l.tsel.scroll_y),
                self.view(),
                galleys(&out)
                    .iter()
                    .filter(|g| g.2.starts_with("Turn"))
                    .map(|g| (g.0.y, &g.2))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// Drags from the start of turn `from` to the end of turn `to`, the way a reader does: the
    /// pointer rests at the transcript's bottom edge (which scrolls it) until `to` is in reach,
    /// then moves onto its end and lets go.
    fn drag_turns(h: &mut Harness, from: usize, to: usize) {
        let (pos, size) = h.scroll_to(&turn(from), 60.0);
        let press = pos + vec2(1.0, size.y / 2.0);
        h.frame(pointer(press, None));
        h.frame(pointer(press, Some(true)));
        let edge = pos2(450.0, h.view().bottom() - 3.0);
        let mut end = None;
        for _ in 0..600 {
            let out = h.frame(pointer(edge, None));
            if let Some((p, s)) = h.line_in_band(&out, &turn(to)) {
                end = Some(p + vec2(s.x + 2.0, s.y / 2.0));
                break;
            }
        }
        let end = end.expect("the view scrolled the last turn into reach");
        h.frame(pointer(end, None));
        h.frame(pointer(end, Some(false)));
    }

    /// The turns of a copy (`parts` are separated by a blank line).
    fn parts(copied: &str) -> Vec<&str> {
        copied.split("\n\n").collect()
    }

    /// Right click, then "Select all" of the menu.
    fn select_all_from_the_menu(h: &mut Harness) {
        h.settle();
        let at = pos2(450.0, h.view().bottom() - 6.0);
        let secondary = |pressed| {
            vec![Event::PointerButton {
                pos: at,
                button: PointerButton::Secondary,
                pressed,
                modifiers: Modifiers::NONE,
            }]
        };
        h.frame(pointer(at, None));
        h.frame(secondary(true));
        h.frame(secondary(false));
        let out = h.frame(vec![]);
        assert!(
            h.local(|l| l.menu.is_some()),
            "the right click opened the menu"
        );
        let (pos, size, _) = galleys(&out)
            .into_iter()
            .find(|g| g.2 == "Select all")
            .expect("the menu offers Select all");
        let to = pos + size / 2.0;
        h.frame(pointer(to, None));
        h.frame(pointer(to, Some(true)));
        h.frame(pointer(to, Some(false)));
        h.frame(vec![]);
        assert!(h.local(|l| l.tsel.has_selection()), "Select all selected");
    }

    #[test]
    fn a_drag_from_turn_3_to_turn_35_copies_every_turn_between_in_order() {
        let mut h = turns_harness("sel-drag", 40, &[]);
        // The view follows the end, so the turns near the start were never laid out.
        let out = h.settle();
        assert!(
            !galleys(&out).iter().any(|g| g.2.starts_with(&turn(10))),
            "turn 10 is far out of view"
        );
        drag_turns(&mut h, 3, 35);
        assert!(h.local(|l| l.tsel.has_selection()));

        let out = h.frame(vec![ctrl_c(true)]);
        let got = copied(&out);
        assert_eq!(got.len(), 1, "{got:?}");
        let p = parts(&got[0]);
        assert_eq!(p.len(), 33, "turns 3..=35: {:?}", got[0]);
        for (k, part) in p.iter().enumerate() {
            let want = turn(k + 3);
            if k + 3 == 35 {
                // The pointer let go at (or just before) the end of the last line.
                assert!(want.starts_with(part) && part.len() > 40, "{part:?}");
            } else {
                assert_eq!(*part, want);
            }
        }
        // Ctrl+C was the selection's: omp saw no interrupt, and the selection is still there.
        std::thread::sleep(Duration::from_millis(200));
        h.term.poll();
        assert!(h.typed(&[3]).is_empty(), "no interrupt reached omp");
        assert!(h.local(|l| l.tsel.has_selection()));
    }

    #[test]
    fn a_selection_survives_both_ends_scrolling_out_of_view_and_back() {
        let mut h = turns_harness("sel-scroll", 40, &[]);
        drag_turns(&mut h, 3, 35);
        let bg = h.ctx.global_style().visuals.selection.bg_fill;

        // Turn 19 is between the ends: neither is on screen, the selection still shows.
        h.scroll_to(&turn(19), 60.0);
        let out = h.settle();
        let shown = galleys(&out);
        assert!(!shown.iter().any(|g| g.2.starts_with(&turn(3))));
        assert!(!shown.iter().any(|g| g.2.starts_with(&turn(35))));
        assert!(
            h.local(|l| l.tsel.has_selection()),
            "the selection outlived its ends"
        );
        assert!(
            highlighted(&out, bg)
                .iter()
                .any(|t| t.starts_with(&turn(19))),
            "the turns between are highlighted"
        );

        // Back to the first end, then to the last: both are highlighted again.
        h.scroll_to(&turn(3), 60.0);
        let out = h.settle();
        assert!(
            highlighted(&out, bg)
                .iter()
                .any(|t| t.starts_with(&turn(3)))
        );
        h.scroll_to(&turn(35), -60.0);
        let out = h.settle();
        assert!(
            highlighted(&out, bg)
                .iter()
                .any(|t| t.starts_with(&turn(35)))
        );
        assert!(
            !highlighted(&out, bg)
                .iter()
                .any(|t| t.starts_with(&turn(36)))
        );

        let out = h.frame(vec![ctrl_c(true)]);
        let got = copied(&out);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(parts(&got[0]).len(), 33);
        assert_eq!(parts(&got[0])[0], turn(3));
    }

    fn diff_harness(name: &str, mode: &str) -> Harness {
        let frames = [serde_json::json!([["add", "zdiff", "main", null, {
            "id": "zdiff", "k": "diff",
            "p": {"mode": mode, "text":
                "@@ -1,3 +1,3 @@\n keep one\n-old two\n+new two\n keep three\n"}
        }]])];
        let mut h = Harness::with_frames(name, &frames);
        h.wait_for("the diff arrives", |d| {
            d.surfaces().iter().any(|s| s.node("zdiff").is_some())
        });
        h.focus();
        h.settle();
        h
    }

    #[test]
    fn a_unified_diff_copies_its_plus_minus_and_space_markers() {
        let mut h = diff_harness("sel-udiff", "unified");
        select_all_from_the_menu(&mut h);
        let got = copied(&h.frame(vec![ctrl_c(true)]));
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0].ends_with("@@ -1,3 +1,3 @@\n keep one\n-old two\n+new two\n keep three"),
            "{:?}",
            got[0]
        );
    }

    #[test]
    fn a_split_diff_copies_the_side_the_selection_covers_without_markers() {
        let mut h = diff_harness("sel-sdiff", "split");
        let out = h.settle();
        let left_of = |text: &str| {
            galleys(&out)
                .into_iter()
                .filter(|g| g.2 == text)
                .min_by(|a, b| a.0.x.total_cmp(&b.0.x))
                .unwrap_or_else(|| panic!("{text:?} is painted"))
        };
        let (from, fsize, _) = left_of("keep one");
        let (to, tsize, _) = left_of("keep three");
        let press = from + vec2(1.0, fsize.y / 2.0);
        let release = to + vec2(tsize.x + 2.0, tsize.y / 2.0);
        h.frame(pointer(press, None));
        h.frame(pointer(press, Some(true)));
        h.frame(pointer(release, None));
        h.frame(pointer(release, Some(false)));
        let got = copied(&h.frame(vec![ctrl_c(true)]));
        assert_eq!(got, ["keep one\nold two\nkeep three"], "the old side only");
        // And the right side alone.
        let right_of = |text: &str| {
            galleys(&out)
                .into_iter()
                .filter(|g| g.2 == text)
                .max_by(|a, b| a.0.x.total_cmp(&b.0.x))
                .unwrap_or_else(|| panic!("{text:?} is painted"))
        };
        let (from, fsize, _) = right_of("keep one");
        let (to, tsize, _) = right_of("keep three");
        let press = from + vec2(1.0, fsize.y / 2.0);
        let release = to + vec2(tsize.x + 2.0, tsize.y / 2.0);
        h.frame(vec![ctrl_c(false)]);
        h.frame(pointer(press, None));
        h.frame(pointer(press, Some(true)));
        h.frame(pointer(release, None));
        h.frame(pointer(release, Some(false)));
        let got = copied(&h.frame(vec![ctrl_c(true)]));
        assert_eq!(got, ["keep one\nnew two\nkeep three"], "the new side only");
    }

    #[test]
    fn select_all_from_the_menu_copies_the_whole_conversation_laid_out_or_not() {
        let mut h = turns_harness("sel-all", 40, &[]);
        let out = h.settle();
        assert!(
            !galleys(&out).iter().any(|g| g.2.starts_with(&turn(5))),
            "turn 5 is far out of view"
        );
        select_all_from_the_menu(&mut h);
        let out = h.frame(vec![ctrl_c(true)]);
        let got = copied(&out);
        assert_eq!(got.len(), 1, "{got:?}");
        let mut from = 0;
        for i in 0..40 {
            let at = got[0][from..]
                .find(&turn(i))
                .unwrap_or_else(|| panic!("turn {i} is in the copy, in order"));
            from += at + turn(i).len();
        }
        assert!(got[0].ends_with(&turn(39)));
        assert!(
            !got[0].contains("Pair up live") && !got[0].contains("v18.15.0"),
            "the welcome card is not part of the conversation: {:?}",
            &got[0][..80.min(got[0].len())]
        );
        assert!(
            got[0].find(&turn(0)).is_some_and(|at| at > 0),
            "the recorded conversation comes first"
        );
        assert!(
            h.local(|l| l.tsel.has_selection()),
            "copying keeps the selection"
        );
    }

    #[test]
    fn an_endpoint_deleted_by_a_frame_clamps_to_what_followed_it() {
        let later = [serde_json::json!([["del", "t03"], ["del", "t35"]])];
        let mut h = turns_harness("sel-del", 40, &later);
        drag_turns(&mut h, 3, 35);
        h.release_later();
        h.wait_for("both ends are deleted", |d| {
            d.surfaces()
                .iter()
                .all(|s| s.node("t35").is_none() && s.node("t03").is_none())
        });
        h.frame(vec![]);
        h.frame(vec![]);
        assert!(
            h.local(|l| l.tsel.has_selection()),
            "the selection clamped, not dropped"
        );
        let out = h.frame(vec![ctrl_c(true)]);
        let got = copied(&out);
        assert_eq!(got.len(), 1, "{got:?}");
        let p = parts(&got[0]);
        assert_eq!(p.len(), 31, "turns 4..=34: {:?}", got[0]);
        assert_eq!(p[0], turn(4), "the start moved to what followed turn 3");
        assert_eq!(p[30], turn(34), "the end stops where turn 35 was");
        assert!(!got[0].contains("Turn 03") && !got[0].contains("Turn 35"));
        assert!(!got[0].contains("Turn 36"));
    }

    #[test]
    fn shift_click_extends_the_selection_to_the_clicked_text() {
        let mut h = turns_harness("sel-shift", 40, &[]);
        let out = h.settle();
        let line = |t: usize| {
            galleys(&out)
                .into_iter()
                .find(|g| g.2.starts_with(&turn(t)))
                .unwrap_or_else(|| panic!("turn {t} is in view"))
        };
        let (p37, s37, _) = line(37);
        let (p38, s38, _) = line(38);
        // Drag over the first half of turn 37.
        let a = p37 + vec2(1.0, s37.y / 2.0);
        let b = p37 + vec2(s37.x / 2.0, s37.y / 2.0);
        h.frame(pointer(a, None));
        h.frame(pointer(a, Some(true)));
        h.frame(pointer(b, None));
        h.frame(pointer(b, Some(false)));
        // Shift+click past the end of turn 38: the selection grows to it.
        let to = p38 + vec2(s38.x + 2.0, s38.y / 2.0);
        h.modifiers = Modifiers::SHIFT;
        h.frame(pointer(to, None));
        h.frame(pointer(to, Some(true)));
        h.frame(pointer(to, Some(false)));
        h.modifiers = Modifiers::NONE;
        let out = h.frame(vec![ctrl_c(true)]);
        let got = copied(&out);
        assert_eq!(got, [format!("{}\n\n{}", turn(37), turn(38))]);
    }

    #[test]
    fn a_plain_click_sets_the_anchor_for_a_later_shift_click() {
        let mut h = turns_harness("sel-caret", 40, &[]);
        let out = h.settle();
        let line = |t: usize| {
            galleys(&out)
                .into_iter()
                .find(|g| g.2.starts_with(&turn(t)))
                .unwrap_or_else(|| panic!("turn {t} is in view"))
        };
        let (p37, s37, _) = line(37);
        let (p38, s38, _) = line(38);
        let at = p37 + vec2(1.0, s37.y / 2.0);
        h.frame(pointer(at, None));
        h.frame(pointer(at, Some(true)));
        h.frame(pointer(at, Some(false)));
        assert!(
            !h.local(|l| l.tsel.has_selection()),
            "a click alone selects nothing"
        );
        let to = p38 + vec2(s38.x + 2.0, s38.y / 2.0);
        h.modifiers = Modifiers::SHIFT;
        h.frame(pointer(to, None));
        h.frame(pointer(to, Some(true)));
        h.frame(pointer(to, Some(false)));
        h.modifiers = Modifiers::NONE;
        let got = copied(&h.frame(vec![ctrl_c(true)]));
        assert_eq!(got, [format!("{}\n\n{}", turn(37), turn(38))]);
    }

    #[test]
    fn a_selection_never_paints_or_copies_the_composer_chrome() {
        let mut h = turns_harness("sel-chrome", 40, &[]);
        let out = h.settle();
        let bg = h.ctx.global_style().visuals.selection.bg_fill;
        let (p38, s38, _) = galleys(&out)
            .into_iter()
            .find(|g| g.2.starts_with(&turn(38)))
            .expect("turn 38 is in view");
        let (model, msize, _) = galleys(&out)
            .into_iter()
            .find(|g| g.2 == "Fake One")
            .expect("the composer shows the model label");
        // Drag from turn 38 down into the composer's model label.
        let a = p38 + vec2(1.0, s38.y / 2.0);
        let b = model + msize / 2.0;
        h.frame(pointer(a, None));
        h.frame(pointer(a, Some(true)));
        h.frame(pointer(b, None));
        h.frame(pointer(b, Some(false)));
        let out = h.settle();
        assert!(h.local(|l| l.tsel.has_selection()));
        assert!(
            !highlighted(&out, bg).iter().any(|t| t.contains("Fake One")),
            "the model label is not painted selected"
        );
        let got = copied(&h.frame(vec![ctrl_c(true)]));
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(!got[0].contains("Fake One"), "{:?}", got[0]);
        assert!(got[0].starts_with(&turn(38)));
    }

    #[test]
    fn dragging_below_the_last_row_of_a_split_column_stays_in_that_column() {
        let mut h = diff_harness("sel-lane", "split");
        let out = h.settle();
        let at = |text: &str, right: bool| {
            let all = galleys(&out).into_iter().filter(|g| g.2 == text);
            if right {
                all.max_by(|a, b| a.0.x.total_cmp(&b.0.x))
            } else {
                all.min_by(|a, b| a.0.x.total_cmp(&b.0.x))
            }
            .unwrap_or_else(|| panic!("{text:?} is painted"))
        };
        let (from, fsize, _) = at("keep one", false);
        let (last, lsize, _) = at("keep three", false);
        let (right, _, _) = at("keep three", true);
        let press = from + vec2(1.0, fsize.y / 2.0);
        // Just below the last row, at the right pane's text column: closer to it than to the
        // left column's (short) text.
        let release = pos2(right.x - 2.0, last.y + lsize.y + 4.0);
        h.frame(pointer(press, None));
        h.frame(pointer(press, Some(true)));
        h.frame(pointer(release, None));
        h.frame(pointer(release, Some(false)));
        let got = copied(&h.frame(vec![ctrl_c(true)]));
        assert_eq!(
            got,
            ["keep one\nold two\nkeep three"],
            "the right side stays out"
        );
    }

    #[test]
    fn a_double_click_selects_a_word_and_a_triple_click_the_line() {
        let mut h = turns_harness("sel-multi", 40, &[]);
        let out = h.settle();
        let (pos, size, _) = galleys(&out)
            .into_iter()
            .find(|g| g.2.starts_with(&turn(38)))
            .expect("turn 38 is in view");
        let at = pos + vec2(6.0, size.y / 2.0);
        let press = |h: &mut Harness, pressed| {
            h.frame(pointer(at, Some(pressed)));
        };
        h.frame(pointer(at, None));
        press(&mut h, true);
        press(&mut h, false);
        press(&mut h, true);
        let got = copied(&h.frame(vec![ctrl_c(true)]));
        assert_eq!(
            got,
            ["Turn"],
            "the second press of a double click takes the word"
        );
        h.frame(vec![ctrl_c(false)]);
        press(&mut h, false);
        // A new series of presses, the third takes the line.
        for _ in 0..12 {
            h.frame(vec![]);
        }
        for pressed in [true, false, true, false, true] {
            press(&mut h, pressed);
        }
        let got = copied(&h.frame(vec![ctrl_c(true)]));
        assert_eq!(got, [turn(38)], "the third press takes the line");
    }

    #[test]
    fn clicking_a_link_or_a_fold_toggle_starts_no_selection() {
        let frames = [serde_json::json!([
            ["add", "lk", "main", null,
             {"id": "lk", "k": "md", "p": {"text": "[click me](https://example.com/click)"}}],
            ["add", "sec", "main", null,
             {"id": "sec", "k": "section", "p": {"head": "Fold me", "collapsible": true}}],
            ["add", "body", "sec", null,
             {"id": "body", "k": "md", "p": {"text": "Body of the fold"}}],
        ])];
        let mut h = Harness::with_frames("sel-click", &frames);
        h.wait_for("the nodes arrive", |d| {
            d.surfaces().iter().any(|s| s.node("body").is_some())
        });
        h.focus();
        let out = h.settle();
        let center = |out: &egui::FullOutput, what: &str| {
            let (pos, size, _) = galleys(out)
                .into_iter()
                .find(|g| g.2.starts_with(what))
                .unwrap_or_else(|| panic!("{what:?} is painted"));
            pos + size / 2.0
        };
        let click = |h: &mut Harness, at: Pos2| {
            h.frame(pointer(at, None));
            h.frame(pointer(at, Some(true)));
            h.frame(pointer(at, Some(false)));
        };

        let at = center(&out, "click me");
        click(&mut h, at);
        assert_eq!(h.opened, ["https://example.com/click"], "the link opened");
        assert!(
            !h.local(|l| l.tsel.has_selection()),
            "a link click selects nothing"
        );

        assert!(galleys(&out).iter().any(|g| g.2 == "Body of the fold"));
        let at = center(&out, "Fold me");
        click(&mut h, at);
        let out = h.settle();
        assert!(
            !galleys(&out).iter().any(|g| g.2 == "Body of the fold"),
            "the click folded the section"
        );
        assert!(
            !h.local(|l| l.tsel.has_selection()),
            "a fold click selects nothing"
        );
    }

    #[test]
    fn what_is_copied_is_the_text_that_is_painted() {
        let frames = [serde_json::json!([
            ["add", "zmd", "main", null, {"id": "zmd", "k": "md", "p": {"text":
                "Some *emphasis*, **strong**, `code span` and a [link](https://example.com/x).\n\n\
                 - first item\n- second **item**\n\n1. one\n2. two\n\n\
                 ```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n"}}],
            ["add", "zcode", "main", null, {"id": "zcode", "k": "code", "p":
                {"text": "let x = 1;\nlet y = 2;", "lang": "rust"}}],
            ["add", "zansi", "main", null, {"id": "zansi", "k": "ansi", "p":
                {"text": "\u{1b}[31mred\u{1b}[0m plain\nsecond line"}}],
        ])];
        let mut h = Harness::with_frames("sel-drift", &frames);
        h.wait_for("the nodes arrive", |d| {
            d.surfaces().iter().any(|s| s.node("zansi").is_some())
        });
        h.focus();
        let out = h.settle();
        // What each renderer paints, line by line.
        let painted = [
            "Some emphasis, strong, code span and a link.",
            "first item",
            "second item",
            "one",
            "two",
            "fn main() {\n    println!(\"hi\");\n}",
            "let x = 1;",
            "let y = 2;",
            "red plain\nsecond line",
        ];
        for want in painted {
            assert!(
                galleys(&out).iter().any(|g| g.2.trim_end() == want),
                "{want:?} is painted: {:?}",
                galleys(&out).iter().map(|g| &g.2).collect::<Vec<_>>()
            );
        }
        select_all_from_the_menu(&mut h);
        let out = h.frame(vec![ctrl_c(true)]);
        let got = copied(&out);
        assert_eq!(got.len(), 1, "{got:?}");
        let mut from = 0;
        for want in painted {
            let at = got[0][from..]
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is copied in order: {:?}", got[0]));
            from += at + want.len();
        }
        assert!(
            got[0].contains("• first item\n• second item"),
            "{:?}",
            got[0]
        );
        assert!(got[0].contains("1. one\n2. two"), "{:?}", got[0]);
    }
}
