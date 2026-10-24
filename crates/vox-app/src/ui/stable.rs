//! Stable model identity for the structs `AppUi` pushes into Slint (ARCHITECTURE.md §7.1; audit finding "rows are
//! re-created several times a second").
//!
//! Slint's repeater drops **every** row instance when the identity of its model changes (`old_model != m` in
//! `i-slint-core` `Repeater::model`), and `ModelRc` compares by pointer. The controllers build their page structs with
//! fresh `ModelRc::new(VecModel::from(..))` lists on every `build`, and the 30 Hz poll rebuilds the sidebar and Home
//! while a read plays — without this module every nav item, Recent row and toast would be destroyed and re-created
//! ~10 times a second (lost clicks, lost focus, hover flicker).
//!
//! [`stabilize`] runs between `build` and the property setter: it points every list field of the fresh struct at the
//! list that is already shown and updates that list **in place** (`set_row_data` for changed rows, `insert` / `remove`
//! for count changes — never `set_vec`, which also resets), then reports whether anything changed at all, so an
//! unchanged struct is not set again.
//!
//! Rules:
//! - Top-level lists (fields of a page struct, or of a struct field of it) are synced in place, but only into lists
//!   this module created ([`StableModel`]); a list a controller keeps and syncs itself (History rows, Voices rows) is
//!   left alone when the controller hands back the same model. The first push keeps the controller's model; a later
//!   push of a different throwaway model replaces it once with a `StableModel`, which then stays.
//! - Rows with an identity ([`Stable::KEYED`], e.g. toasts, Recent rows, choice items) are matched by key: a new key
//!   is inserted, a vanished one removed, a matched one updated in place, so a row keeps its instance while it exists.
//!   Other rows are positional.
//! - Lists inside rows are never mutated in place (a controller may share one list between rows): a row's nested list
//!   keeps the shown identity when its contents are equal, else the row gets the new list.
//! - Images compare by buffer identity (the art cache hands out clones, so they compare equal), but an **empty**
//!   `slint::Image` never equals another one (Slint 1.18.1 `ImageInner`'s `PartialEq` has no `None` arm): a struct
//!   holding one is always set again and a row holding one is re-set in place — its instance is kept either way.

use slint::{Model, ModelRc, ModelTracker, SharedString, VecModel};

use super::generated::*;

/// A row identity for keyed list diffs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    Str(SharedString),
    Int(i64),
}

/// A value `AppUi` pushes into Slint (a page struct, a nested struct, a list row or a list item).
pub trait Stable: Clone + PartialEq + 'static {
    /// Rows of this type have an identity ([`Stable::row_key`]).
    const KEYED: bool = false;
    /// The row's identity (only read when [`Stable::KEYED`]).
    fn row_key(&self) -> Option<RowKey> {
        None
    }
    /// Top level: points this value's lists at `shown`'s lists, syncing them in place (module docs).
    fn adopt(&mut self, _shown: &Self) {}
    /// Inside a row: points this value's lists at `shown`'s lists where their contents are equal.
    fn share_equal(&mut self, _shown: &Self) {}
}

macro_rules! leaf {
    ($($ty:ty),* $(,)?) => {
        $(impl Stable for $ty {})*
    };
}

macro_rules! keyed {
    ($ty:ty, |$r:ident| $key:expr) => {
        impl Stable for $ty {
            const KEYED: bool = true;
            fn row_key(&self) -> Option<RowKey> {
                let $r = self;
                Some($key)
            }
        }
    };
}

/// A struct with list fields (`models`) and struct fields that contain lists (`structs`).
macro_rules! composite {
    ($ty:ty { models: [$($m:ident),* $(,)?] $(, structs: [$($s:ident),* $(,)?])? } $(, key: |$r:ident| $key:expr)?) => {
        impl Stable for $ty {
            $(
                const KEYED: bool = true;
                fn row_key(&self) -> Option<RowKey> {
                    let $r = self;
                    Some($key)
                }
            )?
            fn adopt(&mut self, shown: &Self) {
                $( self.$m = sync_model(&shown.$m, &self.$m); )*
                $($( self.$s.adopt(&shown.$s); )*)?
            }
            fn share_equal(&mut self, shown: &Self) {
                $(
                    if models_equal(&self.$m, &shown.$m) {
                        self.$m = shown.$m.clone();
                    }
                )*
                $($( self.$s.share_equal(&shown.$s); )*)?
            }
        }
    };
}

leaf!(SharedString, f32, bool, i32, Fact, BadgeData, BarData, NavItemData, HeatCellData, CreditsData, MiniPlayerData, StatBlockData);
keyed!(ChoiceItem, |r| RowKey::Str(r.value.clone()));
keyed!(ToastData, |r| RowKey::Int(i64::from(r.key)));
keyed!(RecentRow, |r| RowKey::Str(r.id.clone()));
keyed!(ShareRow, |r| RowKey::Str(r.key.clone()));
composite!(KeyGroup { models: [keys] });
composite!(SidebarData { models: [nav, hint_keys] });
composite!(NowPlayingData { models: [badges, idle_keys] });
composite!(WeekData { models: [bars] });
composite!(WayRow { models: [key_groups] });
composite!(HomeData { models: [recent, ways], structs: [now, week] });
composite!(HistoryRow { models: [badges, notes, facts, details] }, key: |r| RowKey::Str(r.id.clone()));
composite!(HistoryData { models: [source_options, state_keys, rows] });
composite!(KpiData { models: [spark, dots] });
composite!(AnalyticsData {
    models: [ranges, kpis, daily, heat, heat_day_labels, heat_hour_labels, sources, top_apps, top_voices]
});
composite!(VoiceRow { models: [tags] }, key: |r| RowKey::Str(r.id.clone()));
composite!(VoicesData { models: [gender_options, accent_options, category_options, voices] });
composite!(VoiceModelData { models: [voice_tags, model_options, model_badges, language_options, quality_options] });
composite!(PlaybackData { models: [rate_options, max_chars_options] });
composite!(HotkeyRowData { models: [keys, preview] }, key: |r| RowKey::Str(r.action.clone()));
composite!(TriggersData { models: [hotkeys, excluded_apps] });
composite!(WidgetSettingsData { models: [position_options] });
composite!(AppearanceData { models: [theme_options, theme_notes] });
composite!(LibraryData { models: [retention_options] });
composite!(SettingsData {
    models: [sections],
    structs: [voice, playback, triggers, widget, appearance, library]
});
composite!(VoiceStepData { models: [items] });
composite!(TryStepData { models: [keys] });
composite!(OnboardingData { models: [step_labels, tips], structs: [voices, try_it] });

/// Prepares `fresh` for a push over `shown` (the value the property holds now): lists adopted (module docs). `None`
/// when nothing changed, so the caller skips the setter.
pub fn stabilize<T: Stable>(mut fresh: T, shown: &T) -> Option<T> {
    fresh.adopt(shown);
    (fresh != *shown).then_some(fresh)
}

/// The same for a bare list property (the toast stack): the model to set, `None` when the shown one was updated in
/// place (or is already `fresh`).
pub fn stabilize_model<T: Stable>(fresh: &ModelRc<T>, shown: &ModelRc<T>) -> Option<ModelRc<T>> {
    let next = sync_model(shown, fresh);
    (next != *shown).then_some(next)
}

/// A list this module owns and updates in place (a `VecModel` with its own type, so a list a controller keeps is never
/// mistaken for one of ours).
pub struct StableModel<T> {
    rows: VecModel<T>,
}

impl<T: Clone + 'static> StableModel<T> {
    fn new(rows: Vec<T>) -> Self {
        StableModel { rows: VecModel::from(rows) }
    }
}

impl<T: Clone + 'static> Model for StableModel<T> {
    type Data = T;
    fn row_count(&self) -> usize {
        self.rows.row_count()
    }
    fn row_data(&self, row: usize) -> Option<T> {
        self.rows.row_data(row)
    }
    fn set_row_data(&self, row: usize, data: T) {
        self.rows.set_row_data(row, data)
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        self.rows.model_tracker()
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl<T: Stable> StableModel<T> {
    /// Updates the rows in place to `next` (module docs: keyed or positional).
    fn sync(&self, next: Vec<T>) {
        if T::KEYED {
            self.sync_keyed(next);
        } else {
            self.sync_positional(next);
        }
    }

    /// Row `i` becomes `row` (only when it differs; nested lists keep their identity when equal).
    fn update(&self, i: usize, mut row: T) {
        let Some(shown) = self.rows.row_data(i) else { return };
        row.share_equal(&shown);
        if row != shown {
            self.rows.set_row_data(i, row);
        }
    }

    fn truncate(&self, len: usize) {
        while self.rows.row_count() > len {
            self.rows.remove(self.rows.row_count() - 1);
        }
    }

    fn sync_positional(&self, next: Vec<T>) {
        let len = next.len();
        for (i, row) in next.into_iter().enumerate() {
            if i < self.rows.row_count() {
                self.update(i, row);
            } else {
                self.rows.push(row);
            }
        }
        self.truncate(len);
    }

    /// After step `i`, rows `0..=i` equal `next[0..=i]` (so the result is always exactly `next`).
    fn sync_keyed(&self, next: Vec<T>) {
        let key_at = |i: usize| self.rows.row_data(i).and_then(|r| r.row_key());
        let len = next.len();
        for (i, row) in next.into_iter().enumerate() {
            let key = row.row_key();
            let count = self.rows.row_count();
            if i < count && key_at(i) == key {
                self.update(i, row);
                continue;
            }
            match (i + 1..count).find(|&j| key_at(j) == key) {
                Some(j) => {
                    // Rows i..j vanished (or moved later): drop them; row j moves up to i.
                    for _ in i..j {
                        self.rows.remove(i);
                    }
                    self.update(i, row);
                }
                None => self.rows.insert(i.min(count), row),
            }
        }
        self.truncate(len);
    }
}

/// Whether `m` is the "no list" default (`ModelRc::default()`), whose `as_any` is `()`.
fn is_default<T>(m: &ModelRc<T>) -> bool {
    m.row_count() == 0 && m.as_any().is::<()>()
}

/// The list to set for a top-level field (module docs).
fn sync_model<T: Stable>(shown: &ModelRc<T>, fresh: &ModelRc<T>) -> ModelRc<T> {
    if shown == fresh {
        return fresh.clone();
    }
    if let Some(ours) = shown.as_any().downcast_ref::<StableModel<T>>() {
        ours.sync(fresh.iter().collect());
        return shown.clone();
    }
    if is_default(shown) {
        return fresh.clone();
    }
    ModelRc::new(StableModel::new(fresh.iter().collect()))
}

/// Deep equality of two lists (nested lists compared by contents).
fn models_equal<T: Stable>(a: &ModelRc<T>, b: &ModelRc<T>) -> bool {
    if a == b {
        return true;
    }
    if a.row_count() != b.row_count() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(mut x, y)| {
        x.share_equal(&y);
        x == y
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn list<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
        ModelRc::new(VecModel::from(v))
    }

    fn strings(v: &[&str]) -> ModelRc<SharedString> {
        list(v.iter().map(|s| SharedString::from(*s)).collect())
    }

    fn nav(labels: &[&str]) -> ModelRc<NavItemData> {
        list(labels.iter().map(|l| NavItemData { label: (*l).into(), ..NavItemData::default() }).collect())
    }

    thread_local! {
        /// One cached art image (the art cache hands out clones of one buffer; an empty image never compares equal).
        static ART: slint::Image = crate::ui::convert::rgba_image(&[1, 2, 3, 255].repeat(4), 2, 2).unwrap();
    }

    fn sidebar(labels: &[&str], status: &str) -> SidebarData {
        SidebarData {
            nav: nav(labels),
            hint_keys: strings(&["Ctrl", "Alt", "R"]),
            now: MiniPlayerData { status: status.into(), art: ART.with(Clone::clone), ..MiniPlayerData::default() },
            ..SidebarData::default()
        }
    }

    fn contents<T: Clone + 'static>(m: &ModelRc<T>) -> Vec<T> {
        m.iter().collect()
    }

    /// The identity Slint's repeater compares (a changed identity drops every row instance).
    fn row_identity<T: Clone + 'static>(m: &ModelRc<T>) -> *const () {
        m.as_any() as *const dyn core::any::Any as *const ()
    }

    #[test]
    fn a_pushed_struct_keeps_its_list_identity() {
        let first = sidebar(&["Home", "History"], "Playing");
        let shown = stabilize(first, &SidebarData::default()).expect("first push sets");
        // The second push (poll: only the mini player changed) brings brand-new lists.
        let next = stabilize(sidebar(&["Home", "History"], "Paused"), &shown).expect("status changed");
        assert_eq!(next.now.status, "Paused");
        // From the second push on, the lists are ours and keep their identity.
        let third = stabilize(sidebar(&["Home", "History"], "Playing"), &next).expect("status changed");
        assert!(third.nav == next.nav, "nav keeps its identity");
        assert!(third.hint_keys == next.hint_keys);
        // Nothing changed at all → no set.
        assert!(stabilize(sidebar(&["Home", "History"], "Playing"), &third).is_none());
        // (An empty image never compares equal: such a struct is set again, its lists still keep their identity.)
        let mut blank = sidebar(&["Home", "History"], "Playing");
        blank.now.art = slint::Image::default();
        let again = stabilize(blank, &third).expect("an empty image is never equal");
        assert!(again.nav == third.nav);
        // A list change alone is applied in place: the shown list now holds the fresh rows, nothing to set.
        assert!(stabilize(sidebar(&["Home", "History", "Analytics"], "Playing"), &third).is_none(), "updated in place");
        assert_eq!(contents(&third.nav).iter().map(|n| n.label.to_string()).collect::<Vec<_>>(), ["Home", "History", "Analytics"]);
    }

    #[test]
    fn a_controller_owned_list_is_left_alone() {
        let owned = Rc::new(VecModel::from(vec![SharedString::from("a")]));
        let data = |m: &Rc<VecModel<SharedString>>| KeyGroup { label: "x".into(), keys: ModelRc::from(m.clone()) };
        let shown = stabilize(data(&owned), &KeyGroup::default()).unwrap();
        assert!(shown.keys == ModelRc::from(owned.clone()), "the first push keeps the controller's model");
        owned.push("b".into());
        // The controller hands back the same model: nothing to sync, nothing to set (the model notified on its own).
        assert!(stabilize(data(&owned), &shown).is_none());
        assert_eq!(contents(&shown.keys).len(), 2);
    }

    #[test]
    fn keyed_rows_keep_their_place_and_identity() {
        let toast = |id: &str, key: i32, leaving: bool| ToastData { id: id.into(), key, leaving, ..ToastData::default() };
        let shown = list(vec![toast("a", 1, false), toast("b", 2, false)]);
        // First sync: a fresh list over a controller-built one → ours from now on.
        let ours = stabilize_model(&list(vec![toast("a", 1, false), toast("b", 2, false)]), &shown).unwrap();
        let id = row_identity(&ours);
        // A third toast arrives, the first starts leaving.
        assert!(stabilize_model(&list(vec![toast("a", 1, true), toast("b", 2, false), toast("c", 3, false)]), &ours).is_none());
        assert_eq!(row_identity(&ours), id);
        assert_eq!(contents(&ours).iter().map(|t| (t.key, t.leaving)).collect::<Vec<_>>(), [(1, true), (2, false), (3, false)]);
        // The first is removed, a new one is pushed in front.
        assert!(stabilize_model(&list(vec![toast("d", 4, false), toast("b", 2, false), toast("c", 3, false)]), &ours).is_none());
        assert_eq!(contents(&ours).iter().map(|t| t.key).collect::<Vec<_>>(), [4, 2, 3]);
        // A reorder and a shrink still end exactly at the fresh list.
        assert!(stabilize_model(&list(vec![toast("c", 3, false), toast("d", 4, false)]), &ours).is_none());
        assert_eq!(contents(&ours).iter().map(|t| t.key).collect::<Vec<_>>(), [3, 4]);
        assert!(stabilize_model(&list(Vec::<ToastData>::new()), &ours).is_none());
        assert_eq!(ours.row_count(), 0);
    }

    #[test]
    fn nested_lists_keep_their_identity_when_equal() {
        let way = |title: &str, keys: &[&str]| WayRow {
            title: title.into(),
            key_groups: list(vec![KeyGroup { label: "k".into(), keys: strings(keys) }]),
            ..WayRow::default()
        };
        let home = |ways: Vec<WayRow>, title: &str| HomeData {
            ways: list(ways),
            now: NowPlayingData { title: title.into(), badges: strings(&["Cached"]), ..NowPlayingData::default() },
            ..HomeData::default()
        };
        let shown = stabilize(home(vec![way("Hotkey", &["Ctrl", "R"])], "one"), &HomeData::default()).unwrap();
        let shown = stabilize(home(vec![way("Hotkey", &["Ctrl", "R"])], "two"), &shown).unwrap();
        let groups = contents(&shown.ways)[0].key_groups.clone();
        // Only the now-playing title changed: the Ways list, its row and the row's nested list are untouched.
        let next = stabilize(home(vec![way("Hotkey", &["Ctrl", "R"])], "three"), &shown).unwrap();
        assert!(next.ways == shown.ways);
        assert!(contents(&next.ways)[0].key_groups == groups, "nested list identity kept");
        assert!(next.now.badges == shown.now.badges);
        // A real change in the nested list replaces that row's nested list only.
        let next = stabilize(home(vec![way("Hotkey", &["Ctrl", "Alt", "R"])], "three"), &next).unwrap();
        assert!(next.ways == shown.ways);
        assert!(contents(&next.ways)[0].key_groups != groups);
        assert_eq!(contents(&contents(&contents(&next.ways)[0].key_groups)[0].keys).len(), 3);
    }

    #[test]
    fn deep_equality_ignores_list_identity() {
        let a = list(vec![KeyGroup { label: "x".into(), keys: strings(&["A"]) }]);
        let b = list(vec![KeyGroup { label: "x".into(), keys: strings(&["A"]) }]);
        let c = list(vec![KeyGroup { label: "x".into(), keys: strings(&["B"]) }]);
        assert!(models_equal(&a, &b));
        assert!(!models_equal(&a, &c));
    }
}
