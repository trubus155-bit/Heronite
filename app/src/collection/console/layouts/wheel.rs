//! Wheel: a vertical list of the tab's games on the left, EmulationStation
//! style. The focused row stays centred and the others fade and bend away
//! with their distance; the rows are placed from the list's scroll position,
//! so a [`Glide`] moves them all smoothly. The games are the tab's as the
//! Filters panel (Y) leaves them, as a grid's would be: Show, Favourites,
//! Status, Category, the search and the sort; ◀ ▶ switch the chooser in the
//! header over those (All games, Installed, Favourites and metadata facets). On the right:
//! the focused game's art with its logo over it, its facts and overview.

use std::collections::BTreeMap;

use gpui_kit::prelude::*;
use gpui_kit::*;
use heronite_data::console_theme::{HeroTitle, TabSpec, WheelItems};
use heronite_data::organize::release_year;
use heronite_data::{CollectionGame, FacetKind};
use heronite_i18n::{tr, trn};

use super::super::super::CollectionView;
use super::super::hero::{clip, played_time};
use super::grid::fact_cell;
use crate::console::kit::{self, u};
use crate::console::look;
use crate::console::motion::{self, Axis, Glide, Goal};
use crate::console::pointer::{self, Clickable};
use crate::console::{hint, Btn, Hint, Pad};
use crate::icons::Icon;
use crate::theme;

const SIDE: f32 = 46.;
/// Distance between two rows, design px.
const ROW: f32 = 62.;
/// Height of a row's pill (the rest is the gap).
const ROW_H: f32 = 54.;
/// Height of the header with the filter.
const HEAD: f32 = 46.;
/// What the game's text under its art takes at most (the name without a
/// logo, two lines of facts, four of overview), design px.
const PANEL_TEXT: f32 = 262.;

/// Height of the game's art in the panel `w` wide: 16:9, as far as the
/// body's height `body_h` leaves room for the text under it.
fn wheel_art_h(w: f32, body_h: f32) -> f32 {
    (w * 9. / 16.).min(body_h - HEAD - 6. - PANEL_TEXT).max(120.).round()
}
/// Rows drawn on each side of the centre (further ones are invisible).
const REACH: isize = 7;
/// LT / RT jump this many games.
const JUMP: usize = 10;
/// Moves longer than this many rows jump instead of gliding past every cover.
const GLIDE_MAX: usize = 12;

pub(crate) struct Nav {
    /// Index in the filters (see [`CollectionView::wheel_data`]).
    filter: usize,
    index: usize,
    scroll: Glide,
    /// The filter changed: back to the top at once.
    restart: bool,
}

impl Default for Nav {
    fn default() -> Self {
        Self { filter: 0, index: 0, scroll: Glide::new(Axis::Y), restart: false }
    }
}

impl Nav {
    /// The Filters panel changed the list: back to its first game, at once.
    pub(crate) fn restart_list(&mut self) {
        self.index = 0;
        self.restart = true;
    }

    pub(crate) fn reset_filter(&mut self) {
        self.filter = 0;
        self.index = 0;
        self.restart = true;
    }
}

#[derive(Clone, PartialEq)]
enum Filter {
    All,
    Installed,
    Favourites,
    Facet(FacetKind, String),
}

/// What the wheel shows now: the filters, the active one and its games.
struct Data {
    filters: Vec<(Filter, SharedString)>,
    /// Index of the active filter.
    active: usize,
    keys: Vec<SharedString>,
    count: usize,
}

/// Opacity of a row `d` rows from the centre.
fn fade(d: f32) -> f32 {
    (1. - d.abs() * 0.16).max(0.)
}

/// How far (design px) a row `d` rows from the centre bends to the right.
fn bend(d: f32) -> f32 {
    d * d * 2.4
}

/// A game of the wheel, as its quick-filter chooser sees it.
struct Entry {
    key: SharedString,
    installed: bool,
    favourite: bool,
    facets: Vec<(FacetKind, String)>,
}

/// The chooser over the tab's filtered games: basic filters followed by the
/// metadata values actually present in the filtered collection.
fn wheel_pick(games: &[Entry], want: usize) -> Data {
    let mut filters = vec![
        (Filter::All, SharedString::from(tr!("All games"))),
        (Filter::Installed, SharedString::from(tr!("Installed"))),
        (Filter::Favourites, SharedString::from(tr!("Favourites"))),
    ];
    for kind in [FacetKind::Platform, FacetKind::Genre, FacetKind::GameMode, FacetKind::Tag] {
        let mut values = BTreeMap::new();
        for game in games {
            for (facet, value) in &game.facets {
                if *facet == kind {
                    values.entry(value.as_str()).or_insert_with(|| value.clone());
                }
            }
        }
        let label = match kind {
            FacetKind::Platform => tr!("Platform"),
            FacetKind::Genre => tr!("Genre"),
            FacetKind::GameMode => tr!("Game mode"),
            FacetKind::Tag => tr!("Steam tag"),
            _ => continue,
        };
        filters.extend(
            values.into_values().map(|value| (Filter::Facet(kind, value.clone()), SharedString::from(format!("{label}: {value}")))),
        );
    }
    let active = want.min(filters.len().saturating_sub(1));
    let keys: Vec<SharedString> = games
        .iter()
        .filter(|g| match &filters[active].0 {
            Filter::All => true,
            Filter::Installed => g.installed,
            Filter::Favourites => g.favourite,
            Filter::Facet(kind, value) => g.facets.iter().any(|(facet, item)| facet == kind && item == value),
        })
        .map(|g| g.key.clone())
        .collect();
    let count = keys.len();
    Data { keys, filters, active, count }
}

impl CollectionView {
    /// The tab's games as the Filters panel leaves them (show, favourites,
    /// status, category or smart group, search and sort: the Grid's own
    /// filtering, `grid_keys`, with this tab's filters), then the ‹ ›
    /// chooser over them ([`wheel_pick`]).
    fn wheel_data(&mut self, tab: usize, spec: &TabSpec, want: usize) -> Data {
        let keys = self.grid_keys(tab, spec);
        let Some(lib) = self.lib.clone() else { return wheel_pick(&[], want) };
        let games: Vec<Entry> = keys
            .into_iter()
            .filter_map(|key| {
                let g = lib.get(&key)?;
                let facets = [FacetKind::Platform, FacetKind::Genre, FacetKind::GameMode, FacetKind::Tag]
                    .into_iter()
                    .flat_map(|kind| g.facets[kind.index()].iter().map(move |item| (kind, item.value.clone())))
                    .collect();
                Some(Entry { installed: g.installed, favourite: g.favourite, facets, key })
            })
            .collect();
        wheel_pick(&games, want)
    }

    pub(super) fn render_wheel(&mut self, tab: usize, spec: &TabSpec, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let want = self.tab_nav(tab).wheel.filter;
        let data = self.wheel_data(tab, spec, want);
        let n = data.keys.len();
        let (_, label) = &data.filters[data.active];
        let head = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(u(12.))
            .h(u(HEAD))
            .px(u(SIDE))
            .child(div().child(crate::ui::icon(Icon::ChevronLeft, u(22.), theme::text_muted())).on_pads([Pad::Left]))
            .child(kit::title(label.clone(), 22.))
            .child(div().child(crate::ui::icon(Icon::ChevronRight, u(22.), theme::text_muted())).on_pads([Pad::Right]))
            .child(div().text_size(u(15.)).text_color(theme::text_muted()).child(trn!(data.count, "{n} game", "{n} games")));
        if n == 0 {
            return div()
                .flex()
                .size_full()
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .w(u(SIDE + 420.))
                        .h_full()
                        .child(head)
                        .child(div().flex_1().flex().items_center().justify_center().child(super::no_games())),
                )
                .into_any_element();
        }
        let nav = &mut self.tab_nav(tab).wheel;
        nav.filter = data.active;
        nav.index = nav.index.min(n - 1);
        if std::mem::take(&mut nav.restart) {
            nav.scroll.reset();
        }
        let (index, glide) = (nav.index, nav.scroll.clone());
        let focused = data.keys[index].clone();
        self.console_focus_game(Some(focused.clone()), cx);

        let scale = kit::scale();
        let area = crate::ui::content_size(window);
        let (vw, vh) = (f32::from(area.width) / scale, f32::from(area.height) / scale);
        let list_w = (vw * 0.37).round().min(470.);
        let handle = glide.handle().clone();
        let row_px = f32::from(u(ROW));
        // Before the first layout the list's height is a guess.
        let view_px = match f32::from(handle.bounds().size.height) {
            h if h > 1. => h,
            _ => (vh - 76. - HEAD) * scale,
        };
        let pos = -f32::from(handle.offset().y) / row_px;
        let mid = (view_px - row_px) / 2.;
        let centre = pos.round() as isize;
        let (first, last) = ((centre - REACH).max(0) as usize, (centre + REACH).clamp(0, n as isize - 1) as usize);
        // Tall enough to scroll the last game to the middle.
        let mut rows = div().relative().w_full().h(px(n as f32 * row_px + view_px));
        for (i, key) in data.keys.iter().enumerate().take(last + 1).skip(first) {
            let pads = pointer::focus_or_activate(pointer::along(index, i, Pad::Up, Pad::Down));
            rows = rows.children(self.wheel_row(key, i, i as f32 - pos, i == index, list_w, mid, window).map(|r| r.on_pads(pads)));
        }
        let list =
            div().id(("console-wheel", tab)).flex_1().min_h_0().overflow_y_scroll().track_scroll(&handle).child(rows).child(glide.driver());

        let art_l = SIDE + list_w + 40.;
        let panel = self.lib.as_ref().and_then(|l| l.get(&focused)).cloned().map(|g| {
            let panel_w = vw - art_l - SIDE;
            let body_h = super::body_height(window, self.hints_visible());
            self.wheel_panel(&g, panel_w, body_h, window)
        });
        div()
            .flex()
            .size_full()
            .child(div().flex_none().flex().flex_col().w(u(SIDE + list_w + 40.)).h_full().child(head).child(list))
            .child(div().flex_1().min_w_0().h_full().overflow_hidden().pt(u(HEAD + 6.)).pr(u(SIDE)).children(panel))
            .into_any_element()
    }

    /// One row of the list, `d` rows from the centre.
    fn wheel_row(&mut self, key: &SharedString, i: usize, d: f32, focused: bool, list_w: f32, mid: f32, window: &Window) -> Option<Div> {
        let alpha = fade(d);
        if alpha <= 0.01 && !focused {
            return None;
        }
        let g = self.lib.as_ref()?.get(key)?.clone();
        let logo =
            (look::theme().layouts.wheel_items == WheelItems::Logo).then(|| self.console_logo(key, 200., 34., focused, window)).flatten();
        let what = match logo {
            // Kept to its box: logos come in every shape.
            Some(src) => div()
                .flex_1()
                .min_w_0()
                .h(u(34.))
                .flex()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .flex_none()
                        .w(u(200.))
                        .h(u(34.))
                        .overflow_hidden()
                        .child(img(src).w(u(200.)).h(u(34.)).object_fit(ObjectFit::Contain)),
                )
                .into_any_element(),
            None => {
                let cover = self.cover(key, 30. * kit::scale(), 42. * kit::scale(), window.scale_factor(), if focused { 120 } else { 40 });
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(u(14.))
                    .child(
                        div()
                            .flex_none()
                            .w(u(30.))
                            .h(u(42.))
                            .rounded(u(4.))
                            .overflow_hidden()
                            .bg(kit::ink(0.1))
                            .children(cover.map(|s| img(s).size_full().object_fit(ObjectFit::Cover))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(u(19.))
                            .when(focused, |d| d.font_weight(theme::W_BOLD))
                            .child(g.title.clone()),
                    )
                    .into_any_element()
            }
        };
        let pill = kit::row_base(focused).h(u(ROW_H)).gap(u(14.)).child(what);
        Some(
            div()
                .absolute()
                .top(px(mid + i as f32 * f32::from(u(ROW))))
                .left(u(SIDE - 22. + bend(d)))
                .w(u(list_w))
                .h(u(ROW))
                .flex()
                .items_center()
                .opacity(alpha)
                .child(div().w_full().child(pill)),
        )
    }

    /// The focused game on the right: art in a rounded frame with its logo
    /// over it, six facts and the overview, in the body's height `body_h`
    /// (the art gives up what the text under it needs).
    fn wheel_panel(&mut self, g: &CollectionGame, w: f32, body_h: f32, window: &Window) -> AnyElement {
        let theme = look::theme();
        let key = SharedString::from(g.key.clone());
        let art_h = wheel_art_h(w, body_h);
        let art = self.console_wide_art(&key, w, art_h, 200, window);
        let logo = (theme.hero.title == HeroTitle::Logo).then(|| self.console_logo(&key, w * 0.55, art_h * 0.34, true, window)).flatten();
        let no_logo = logo.is_none();
        let dash = || "\u{2014}".to_string();
        let status = self.console_status(g);
        let value = |text: String| div().truncate().text_size(u(15.)).text_color(theme::text_primary()).child(text);
        let row = || div().flex().gap(u(18.));
        let facts = div()
            .flex()
            .flex_col()
            .gap(u(12.))
            .mt(u(16.))
            .child(
                row()
                    .child(fact_cell(tr!("Release year"), value(release_year(g).map_or_else(dash, |y| y.to_string()))))
                    .child(fact_cell(tr!("Genre"), value(g.details.genres.first().cloned().unwrap_or_else(dash))))
                    .child(fact_cell(
                        tr!("Platform"),
                        value(
                            g.details
                                .platforms
                                .iter()
                                .map(|platform| platform.trim())
                                .find(|platform| !platform.is_empty())
                                .map_or_else(|| "-".to_string(), str::to_owned),
                        ),
                    )),
            )
            .child(
                row()
                    .child(fact_cell(tr!("Playtime"), value(played_time(g).unwrap_or_else(dash))))
                    .child(fact_cell(
                        tr!("Status"),
                        match status {
                            Some((label, colour)) => value(label).text_color(kit::tint(colour)),
                            None => value(dash()),
                        },
                    ))
                    .child(fact_cell(tr!("Developer"), value(g.details.developers.first().cloned().unwrap_or_else(dash)))),
            );
        let overview = theme.hero.overview.then(|| self.console_overview(&g.key)).flatten();
        let frame = div()
            .relative()
            .flex_none()
            .w(u(w))
            .h(u(art_h))
            .rounded(u(16.))
            .overflow_hidden()
            .border_1()
            .border_color(kit::ink(0.08))
            .bg(kit::ink(0.07))
            .shadow(kit::shadow(0.55, 18., 44.))
            .children(art.map(|s| img(s).size_full().object_fit(ObjectFit::Cover)))
            .child(div().absolute().inset_0().bg(linear_gradient(
                0.,
                linear_color_stop(theme::black_a(0.55), 0.),
                linear_color_stop(theme::black_a(0.), 0.45),
            )))
            .children(logo.map(|l| {
                div()
                    .absolute()
                    .left(u(20.))
                    .bottom(u(16.))
                    .flex()
                    .items_end()
                    .w(u(w * 0.55))
                    .h(u(art_h * 0.34))
                    .child(img(l).max_w(u(w * 0.55)).max_h(u(art_h * 0.34)).object_fit(ObjectFit::Contain))
            }));
        let panel = div()
            .relative()
            .w(u(w))
            .flex()
            .flex_col()
            .child(frame)
            .when(no_logo, |d| d.child(kit::title(g.title.clone(), 26.).mt(u(14.))))
            .when(theme.hero.facts, |d| d.child(facts))
            .children(overview.map(|o| {
                motion::fade_in(
                    SharedString::from(format!("wheel-about-{}", g.key)),
                    kit::body_text(clip(&o, 240)).mt(u(10.)).line_clamp(4),
                    260,
                )
            }));
        motion::enter_from(SharedString::from(format!("wheel-panel-{}", g.key)), panel, 280, 0., 10., 0.3)
    }

    pub(super) fn wheel_pad(&mut self, tab: usize, spec: &TabSpec, pad: Pad, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let want = self.tab_nav(tab).wheel.filter;
        let data = self.wheel_data(tab, spec, want);
        let n = data.keys.len();
        let nav = &mut self.tab_nav(tab).wheel;
        if matches!(pad, Pad::Left | Pad::Right) {
            nav.filter = super::step(data.active, data.filters.len(), pad.dir(), true);
            nav.index = 0;
            nav.restart = true;
            return true;
        }
        if n == 0 {
            return false;
        }
        let i = nav.index.min(n - 1);
        let to = match pad {
            Pad::Up => super::step(i, n, -1, true),
            Pad::Down => super::step(i, n, 1, true),
            Pad::LT => i.saturating_sub(JUMP),
            Pad::RT => (i + JUMP).min(n - 1),
            Pad::A | Pad::X => {
                self.console_activate(data.keys[i].clone(), pad, window, cx);
                return true;
            }
            Pad::Y => {
                self.open_console_filters(window, cx);
                return true;
            }
            _ => return false,
        };
        nav.index = to;
        let goal = Goal::Offset(to as f32 * f32::from(u(ROW)));
        if to.abs_diff(i) > GLIDE_MAX {
            nav.scroll.jump(goal);
        } else {
            nav.scroll.go(goal);
        }
        true
    }

    pub(super) fn wheel_hints(&self, _tab: usize, _spec: &TabSpec) -> Vec<Hint> {
        let mut h = self.console_cover_hints();
        h.insert(0, hint(Btn::LeftRight, tr!("Filter")));
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` above brings gpui's own `test` macro.
    use core::prelude::v1::test;

    fn entry(key: &str, installed: bool, favourite: bool, facets: &[(FacetKind, &str)]) -> Entry {
        Entry { key: key.into(), installed, favourite, facets: facets.iter().map(|(kind, value)| (*kind, (*value).into())).collect() }
    }

    #[test]
    fn the_chooser_cycles_basic_and_metadata_filters_over_the_tab_games() {
        let games = [
            entry("c", true, true, &[(FacetKind::Platform, "PC"), (FacetKind::Genre, "Action")]),
            entry("a", false, false, &[(FacetKind::Platform, "PS2"), (FacetKind::Genre, "Puzzle")]),
            entry("b", true, false, &[(FacetKind::Platform, "PC"), (FacetKind::Genre, "Action")]),
        ];
        let all = wheel_pick(&games, 0);
        assert_eq!(all.keys, ["c", "a", "b"]);
        assert_eq!(all.count, 3);
        let installed = wheel_pick(&games, 1);
        assert_eq!(installed.keys, ["c", "b"]);
        let favourites = wheel_pick(&games, 2);
        assert_eq!(favourites.keys, ["c"]);
        let pc = wheel_pick(&games, 3);
        assert_eq!(pc.keys, ["c", "b"]);
        let action = wheel_pick(&games, 5);
        assert_eq!(action.keys, ["c", "b"]);
        assert_eq!(wheel_pick(&games, 9).active, 6);
        assert!(wheel_pick(&[], 0).keys.is_empty());
    }

    #[test]
    fn the_art_leaves_room_for_the_text_under_it() {
        // Wide panel, the body under a hint bar: the height caps it.
        let h = wheel_art_h(1200., 664.);
        assert!(h + HEAD + 6. + PANEL_TEXT <= 664.5, "{h}");
        // A narrow panel keeps 16:9.
        assert_eq!(wheel_art_h(400., 724.), 225.);
    }

    #[test]
    fn rows_fade_and_bend_with_distance() {
        assert_eq!(fade(0.), 1.);
        assert!(fade(2.) < fade(1.));
        assert_eq!(fade(-3.), fade(3.));
        assert_eq!(fade(7.), 0.);
        assert_eq!(bend(0.), 0.);
        assert_eq!(bend(-2.), bend(2.));
    }
}
