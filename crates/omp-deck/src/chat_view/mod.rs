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
mod layer;
mod md;
mod paint;
mod render;
mod snap;
mod style;
mod text;
mod tools;
mod transcript;

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

        // Keys to omp (before drawing: the echo of this frame's typing shows next frame).
        let mut copy_label_selection = false;
        if focused && exit.is_none() {
            let events: Vec<Event> = ui.input(|i| i.events.clone());
            copy_label_selection = events.iter().any(|e| {
                matches!(e, Event::Key { key: egui::Key::C, pressed: true, modifiers, .. }
                    if modifiers.ctrl && modifiers.shift && !modifiers.alt)
            });
            let modifiers = ui.input(|i| i.modifiers);
            term.route_keyboard(&events, modifiers);
            route_dropped_files(ui, term, &response);
        }
        if copy_label_selection {
            // Labels copy their selection when they see the event while drawing.
            ctx.input_mut(|i| i.events.push(Event::Copy));
        }

        // The surface, synced only when the document moved.
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
        if !snap.is_current(rev, epoch)
            && handle.with_stamp(|doc, rev, epoch| snap.sync(doc, rev, epoch))
        {
            layer::collect_toasts(snap, now, &mut loc.toasts);
            loc.prune(snap, now);
        } else if !loc.toasts.is_empty() {
            loc.toasts.retain(|t| t.until > now);
        }

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
                refused: None,
            };
        }

        let cx = Cx::new(snap, &ink, loc, now, focused, snap.can_edit);

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

        let (out, animate) = cx.into_out();
        if let Some(blob) = out.zoom {
            loc.zoomed = Some(blob);
        }
        if let Some(text) = out.copy {
            ctx.copy_text(text);
        }
        let mut refused = None;
        for e in out.events.iter().filter(|_| exit.is_none()) {
            if let Err(r) = term.send_tsp(e) {
                refused.get_or_insert(r);
            }
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
