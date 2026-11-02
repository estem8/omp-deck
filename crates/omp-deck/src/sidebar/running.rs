//! "Running sessions": every session still alive in the session backends (tmux or the OmpDeck
//! host), with or without an open tab. Pinned under "Open tabs" in its own capped scroll area.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use deck_core::mux::session_name_for;
use deck_core::sessions::SessionEntry;
use deck_service::backend::kind_label;
use deck_service::status::TabStatus;
use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, CursorIcon, Sense, Stroke, Ui, pos2, vec2};

use super::{
    STATUS_SIZE, SidebarAction, SidebarInput, SidebarOutput, paint_row_bg, row_h, section_header,
    session_menu, tab_menu,
};
use crate::badge::{self, Hue};
use crate::folder_scope;
use crate::icons::{ICON_APP_WINDOW, ICON_CIRCLE_STOP, ICON_PLAY, ICON_TERMINAL};
use crate::tabs;
use crate::theme::{self, Palette};
use crate::widgets::{menu_item, paint_icon};
use crate::{card, tips};

/// Most rows the list shows before it scrolls.
const MAX_ROWS: f32 = 6.0;

/// One live session of the section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    /// The backend session name.
    name: String,
    /// Strip index of the tab showing it.
    tab: Option<usize>,
    /// `(group, session)` in the sessions snapshot: the session file it holds open.
    loc: Option<(usize, usize)>,
    /// That file's mtime.
    modified: Option<SystemTime>,
}

/// Sessions with a tab first (strip order), then the rest newest-first by session file; sessions
/// without a listed file last, by name.
fn order(rows: &mut [Row]) {
    rows.sort_by(|a, b| match (a.tab, b.tab) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => match (a.modified, b.modified) {
            (Some(x), Some(y)) => y.cmp(&x).then_with(|| a.name.cmp(&b.name)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.name.cmp(&b.name),
        },
    });
}

/// `(sessions snapshot, scope, live sessions + tabs)`.
type Key = ((u64, usize), Option<PathBuf>, u64);

/// The rows, valid for one [`Key`] (the path lookups run once per change, not per frame).
#[derive(Default)]
pub(super) struct RunningCache {
    key: Option<Key>,
    rows: Vec<Row>,
}

impl RunningCache {
    fn sync(&mut self, input: &SidebarInput<'_>) -> &[Row] {
        let key = (
            (
                input.generation,
                input.groups.map_or(0, |g| g.as_ptr() as usize),
            ),
            input.scope.map(Path::to_path_buf),
            fingerprint(input),
        );
        if self.key.as_ref() != Some(&key) {
            self.rows = build(input);
            self.key = Some(key);
        }
        &self.rows
    }
}

/// What the rows depend on besides the sessions snapshot: which sessions live and hold which
/// files (order independent), and the tab strip's session names and folders.
fn fingerprint(input: &SidebarInput<'_>) -> u64 {
    let snap = input.running;
    let live = snap.alive.iter().fold(0u64, |acc, name| {
        let mut h = DefaultHasher::new();
        name.hash(&mut h);
        snap.files_of(name).hash(&mut h);
        acc.wrapping_add(h.finish())
    });
    let mut h = DefaultHasher::new();
    (live, snap.blind, input.tabs.len()).hash(&mut h);
    for t in input.tabs {
        t.mux_name().hash(&mut h);
        t.cwd.as_path().hash(&mut h);
    }
    h.finish()
}

fn build(input: &SidebarInput<'_>) -> Vec<Row> {
    let snap = input.running;
    let mut tab_of: HashMap<&str, usize> = HashMap::new();
    for (i, t) in input.tabs.iter().enumerate() {
        if let Some(name) = t.mux_name() {
            tab_of.entry(name).or_insert(i);
        }
    }
    let mut rows: Vec<Row> = snap
        .alive
        .iter()
        .map(|name| Row {
            name: name.clone(),
            tab: tab_of.get(name.as_str()).copied(),
            loc: None,
            modified: None,
        })
        .collect();
    if let Some(groups) = input.groups {
        // A session holds the files it has open; one that holds none (a fresh omp) is the
        // session of the id its canonical name is made from.
        let mut by_path: HashMap<&Path, usize> = HashMap::new();
        let mut by_name: HashMap<&str, usize> = HashMap::new();
        for (k, row) in rows.iter().enumerate() {
            let files = snap.files_of(&row.name);
            if files.is_empty() {
                by_name.insert(row.name.as_str(), k);
            }
            for f in files {
                by_path.entry(f.as_path()).or_insert(k);
            }
        }
        let mut found: Vec<Option<((usize, usize), SystemTime)>> = vec![None; rows.len()];
        for (gi, group) in groups.iter().enumerate() {
            for (si, e) in group.sessions.iter().enumerate() {
                let k = by_path.get(e.path.as_path()).copied().or_else(|| {
                    (!by_name.is_empty())
                        .then(|| by_name.get(session_name_for(&e.id).as_str()).copied())
                        .flatten()
                });
                if let Some(k) = k
                    && found[k].is_none_or(|(_, m)| e.modified > m)
                {
                    found[k] = Some(((gi, si), e.modified));
                }
            }
        }
        for (row, f) in rows.iter_mut().zip(found) {
            if let Some((loc, modified)) = f {
                row.loc = Some(loc);
                row.modified = Some(modified);
            }
        }
    }
    if let Some(scope) = input.scope {
        rows.retain(|r| {
            let cwd = match (r.loc, input.groups) {
                (Some((g, s)), Some(groups)) => groups
                    .get(g)
                    .and_then(|g| g.sessions.get(s))
                    .map(|e| e.cwd.as_path()),
                _ => r
                    .tab
                    .and_then(|i| input.tabs.get(i))
                    .map(|t| t.cwd.as_path()),
            };
            cwd.is_some_and(|c| folder_scope::contains(scope, c))
        });
    }
    order(&mut rows);
    rows
}

/// The live state of a row: its tab's, else what the session's terminal title says.
fn status_of(input: &SidebarInput<'_>, row: &Row) -> TabStatus {
    match row.tab.and_then(|i| input.tabs.get(i).map(|t| (i, t))) {
        Some((i, tab)) => tab.status(i == input.active_tab),
        None => TabStatus::of(None, input.running.activity_of(&row.name), false),
    }
}

pub(super) fn section(
    ui: &mut Ui,
    input: &SidebarInput<'_>,
    cache: &mut RunningCache,
    out: &mut SidebarOutput,
) {
    if input.running.blind || input.running.alive.is_empty() {
        return;
    }
    let rows = cache.sync(input);
    if rows.is_empty() {
        return;
    }
    let pal = Palette::of_ui(ui);
    let waiting = rows
        .iter()
        .filter(|r| status_of(input, r) == TabStatus::Attention)
        .count();
    let mut state = section_header(
        ui,
        out,
        ("running-sessions", "Running sessions"),
        rows.len(),
        waiting,
        true,
    );
    state.show_body_unindented(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt("running-sessions-list")
            .max_height((row_h() + 2.0) * MAX_ROWS)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for row in rows {
                    running_row(ui, input, row, out);
                }
            });
    });
    ui.add_space(theme::dens(6.0, 8.0));
    let sep = ui.available_rect_before_wrap();
    ui.painter()
        .hline(sep.x_range(), sep.top(), Stroke::new(1.0, pal.stroke));
    ui.add_space(theme::dens(6.0, 8.0));
}

fn running_row(ui: &mut Ui, input: &SidebarInput<'_>, row: &Row, out: &mut SidebarOutput) {
    let pal = Palette::of_ui(ui);
    let entry: Option<&SessionEntry> = row.loc.and_then(|(g, s)| {
        input
            .groups
            .and_then(|gs| gs.get(g))
            .and_then(|g| g.sessions.get(s))
    });
    let tab = row.tab.and_then(|i| input.tabs.get(i));
    let status = status_of(input, row);
    let label = match (entry, tab) {
        (Some(e), _) => e.label(),
        (None, Some(t)) => t.label(input.labels),
        (None, None) => row.name.as_str(),
    };
    let cwd = entry
        .map(|e| e.cwd.as_path())
        .or_else(|| tab.map(|t| t.cwd.as_path()));
    let folder = cwd.and_then(Path::file_name).and_then(|n| n.to_str());
    let active = row.tab == Some(input.active_tab);
    let neutral = theme::flat();

    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h()), Sense::click());
    let (view, view_label) = super::view_action(input.default_view, tab.map(tabs::Tab::view));
    let view_why = tab.and_then(|t| super::view_switch_reason(t, view, input.host_available));
    let view_rect =
        egui::Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(22.0, 22.0));
    let view_button = tab.is_some().then(|| {
        ui.interact(view_rect, resp.id.with("session-view"), Sense::click())
            .on_hover_text(view_why.unwrap_or(view_label))
    });
    let over_view = view_button.as_ref().is_some_and(egui::Response::hovered);
    let ctx = ui.ctx().clone();
    let hov = ctx.animate_bool_with_time(
        resp.id.with("hov"),
        resp.hovered() || over_view || resp.context_menu_opened(),
        0.12,
    );
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), active, 0.18);
    if ui.is_rect_visible(rect) {
        let pill = rect.shrink2(vec2(2.0, 0.0));
        let p = ui.painter();
        paint_row_bg(p, pal, pill, (hov, sel));
        if status == TabStatus::Attention {
            tabs::paint_attention_pill(p, pal, pill, if neutral { 6 } else { 10 });
        }
        let ic = pos2(
            pill.left() + if neutral { 15.0 } else { 19.0 },
            pill.center().y,
        );
        if !tabs::paint_status(ui, ic, STATUS_SIZE, status) {
            if neutral {
                p.circle_filled(ic, 3.0, pal.ok);
            } else {
                theme::glow_dot(p, ic, 3.5, pal.ok, theme::GLOW_REST);
            }
        }
        // Marks a session that already has a tab.
        let mut text_right = pill.right() - 8.0;
        if view_button.is_some() {
            let c = view_rect.center();
            let icon = if hov > 0.01 {
                super::view_icon(view)
            } else {
                ICON_APP_WINDOW
            };
            paint_icon(
                p,
                c,
                Align2::CENTER_CENTER,
                icon,
                14.0,
                theme::mix(pal.weak, pal.accent, 0.5 + 0.5 * sel),
            );
            text_right = c.x - 12.0;
        }
        let color = theme::mix(pal.text, pal.text_strong, sel.max(hov * 0.6));
        let mut job = LayoutJob::default();
        job.append(
            label,
            0.0,
            TextFormat {
                // One step below the section header's 13/14 px.
                font_id: theme::font(if neutral { 12.0 } else { 13.0 }),
                color,
                ..Default::default()
            },
        );
        if let Some(folder) = folder {
            job.append(
                &format!("  {folder}"),
                0.0,
                TextFormat {
                    font_id: theme::font(if neutral { 10.5 } else { 11.0 }),
                    color: pal.weak,
                    ..Default::default()
                },
            );
        }
        let text_x = pill.left() + if neutral { 28.0 } else { 32.0 };
        job.wrap.max_width = (text_right - text_x).max(10.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let galley = p.layout_job(job);
        p.galley(
            pos2(text_x, pill.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if view_button.as_ref().is_some_and(egui::Response::clicked) {
        if view_why.is_none() {
            if let Some(t) = tab {
                out.actions
                    .push(SidebarAction::SwitchView { uid: t.uid, view });
            }
            out.interacted = true;
        }
    } else if resp.clicked() {
        out.actions.push(match (row.tab, entry) {
            (Some(i), _) => SidebarAction::ActivateTab(i),
            (None, Some(e)) => SidebarAction::Open(e.clone()),
            (None, None) => SidebarAction::AttachRunning {
                name: row.name.clone(),
                title: label.to_owned(),
            },
        });
        out.interacted = true;
    }
    let env = input.menu_env();
    theme::context_menu(&resp, 230.0, |ui| match (row.tab.zip(tab), entry) {
        (Some((i, tab)), _) => tab_menu(ui, i, tab, &env, out),
        (None, Some(e)) => session_menu(ui, e, true, true, &env, out),
        (None, None) => {
            if menu_item(ui, ICON_PLAY, "Open", None, true, false).clicked() {
                out.actions.push(SidebarAction::AttachRunning {
                    name: row.name.clone(),
                    title: label.to_owned(),
                });
                out.interacted = true;
                ui.close();
            }
            ui.separator();
            if menu_item(
                ui,
                ICON_CIRCLE_STOP,
                "Stop session process",
                None,
                true,
                true,
            )
            .clicked()
            {
                out.actions.push(SidebarAction::StopRunning {
                    name: row.name.clone(),
                    label: label.to_owned(),
                });
                ui.close();
            }
        }
    });
    hover_card(&resp, pal, row, label, status, entry, cwd, input);
}

#[expect(
    clippy::too_many_arguments,
    reason = "one card, its facts are separate"
)]
fn hover_card(
    resp: &egui::Response,
    pal: &Palette,
    row: &Row,
    label: &str,
    status: TabStatus,
    entry: Option<&SessionEntry>,
    cwd: Option<&Path>,
    input: &SidebarInput<'_>,
) {
    const KEY_W: f32 = 76.0;
    let mut head = card::Header::new(ICON_TERMINAL, label).tone(tips::status_tone(pal, status));
    if let Some(hint) = status.hint() {
        head = head.subtitle(hint);
    }
    let backend = input.running.owner_of(&row.name).map(kind_label);
    let open_tab = row.tab.is_some();
    card::hover(
        resp,
        390.0,
        false,
        &head,
        |ui| tips::status_badge(ui, status, None),
        |ui| {
            if let Some(dir) = cwd {
                card::kv(ui, KEY_W, "Folder", |ui| tips::path(ui, dir));
            }
            if let Some(e) = entry {
                card::kv(ui, KEY_W, "File", |ui| tips::path(ui, &e.path));
            }
            card::kv(ui, KEY_W, "Session", |ui| tips::mono(ui, &row.name));
            if let Some(b) = backend {
                card::kv(ui, KEY_W, "Backend", |ui| {
                    badge::show(ui, None, b, Hue::Orange, tips::VALUE);
                });
            }
            if open_tab {
                card::kv(ui, KEY_W, "Tab", |ui| {
                    badge::show(ui, Some(ICON_APP_WINDOW), "open", Hue::Blue, tips::VALUE);
                });
            }
            card::footer_hints(
                ui,
                &[
                    ("click", if open_tab { "focus" } else { "open" }),
                    ("right-click", "actions"),
                ],
            );
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn row(name: &str, tab: Option<usize>, age: Option<u64>) -> Row {
        Row {
            name: name.to_owned(),
            tab,
            loc: age.map(|_| (0, 0)),
            modified: age.map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s)),
        }
    }

    #[test]
    fn tabs_first_in_strip_order_then_newest_file_then_unmapped_by_name() {
        let mut rows = vec![
            row("omp-z", None, None),
            row("omp-old", None, Some(10)),
            row("omp-b", Some(2), Some(1)),
            row("omp-new", None, Some(99)),
            row("omp-a", Some(0), None),
            row("omp-c", None, None),
        ];
        order(&mut rows);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["omp-a", "omp-b", "omp-new", "omp-old", "omp-c", "omp-z"]
        );
    }

    #[test]
    fn equal_mtimes_fall_back_to_the_name() {
        let mut rows = vec![row("omp-b", None, Some(5)), row("omp-a", None, Some(5))];
        order(&mut rows);
        assert_eq!(rows[0].name, "omp-a");
    }
}
