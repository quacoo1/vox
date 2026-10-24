//! Locale conventions for display formatting (BRIEF.md Product rulings: "Formatting follows the Windows user locale
//! for dates and numbers where the old app used `Intl` defaults"; docs/ARCHITECTURE.md §8 D-FMT-1).
//!
//! The copy is English, so names (weekdays, months, "Today", "at") stay English. What follows the user's locale is
//! the *conventions*: decimal and grouping separators, 12- vs 24-hour clock, and whether the day or the month comes
//! first in dates. vox-core stays free of Windows APIs: `vox_win::system::user_locale()` reads the conventions
//! (`GetLocaleInfoEx`) and vox-app installs them once at startup (and again on `WM_SETTINGCHANGE("intl")`) with
//! [`set_process_locale`]. Until then, and in every test, the conventions are [`Locale::EN_US`].
//!
//! Strings the old app formatted with an explicit `'en-US'` (the reader's `Reading the first 20,000 characters`
//! toast) keep en-US through [`Locale::EN_US`] directly.

use std::cell::RefCell;
use std::sync::RwLock;

/// Day/month order in dates with names (`October 17` vs `17 October`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DateOrder {
    /// `Friday, October 17` / `Oct 8, 2026` (en-US and other month-first locales).
    #[default]
    MonthFirst,
    /// `Friday 17 October` / `8 Oct 2026` (day-first and year-first locales).
    DayFirst,
}

/// Number and date conventions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Locale {
    /// Decimal separator (`.` en-US, `,` de-DE).
    pub decimal: String,
    /// Thousands separator (`,` en-US, `.` de-DE, U+202F/U+00A0 fr-FR, `'` de-CH). May be empty (no grouping).
    pub group: String,
    /// 12-hour clock with AM/PM (`9:05 PM`) vs 24-hour (`21:05`).
    pub hour12: bool,
    pub date_order: DateOrder,
}

impl Locale {
    /// The default and the test locale.
    pub const EN_US: Locale = Locale { decimal: String::new(), group: String::new(), hour12: true, date_order: DateOrder::MonthFirst };

    /// `"."`/`","` with en-US defaults for empty fields (`EN_US` is `const`, so its separators are stored empty and
    /// resolved here).
    pub fn decimal_sep(&self) -> &str {
        if self.decimal.is_empty() {
            "."
        } else {
            &self.decimal
        }
    }

    /// The grouping separator; en-US `,` when unset. A locale without grouping uses [`Locale::NO_GROUPING`].
    pub fn group_sep(&self) -> &str {
        if self.group.is_empty() {
            ","
        } else if self.group == Self::NO_GROUPING {
            ""
        } else {
            &self.group
        }
    }

    /// Marker for "no thousands separator" in [`Locale::group`].
    pub const NO_GROUPING: &'static str = "\u{0}";

    /// A locale from raw conventions (what `vox_win::system::user_locale` returns). Empty `group` = no grouping.
    pub fn new(decimal: &str, group: &str, hour12: bool, date_order: DateOrder) -> Locale {
        Locale {
            decimal: if decimal.is_empty() { ".".into() } else { decimal.into() },
            group: if group.is_empty() { Self::NO_GROUPING.into() } else { group.into() },
            hour12,
            date_order,
        }
    }

    /// Integer digits with grouping: `1234567 → "1,234,567"` (en-US) / `"1.234.567"` (de-DE). `digits` must be ASCII
    /// digits (no sign).
    pub fn group_digits(&self, digits: &str) -> String {
        let sep = self.group_sep();
        let mut out = String::with_capacity(digits.len() + digits.len() / 3 * sep.len());
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                out.push_str(sep);
            }
            out.push(c);
        }
        out
    }

    /// Replaces the `.` of an ASCII decimal string (`"38.8"`) with this locale's decimal separator.
    pub fn localize_decimal(&self, ascii: &str) -> String {
        ascii.replacen('.', self.decimal_sep(), 1)
    }
}

impl Default for Locale {
    fn default() -> Self {
        Locale::EN_US
    }
}

static PROCESS_LOCALE: RwLock<Option<Locale>> = RwLock::new(None);

thread_local! {
    static OVERRIDE: RefCell<Option<Locale>> = const { RefCell::new(None) };
}

/// Installs the user's conventions for the whole process (vox-app at startup and on locale change).
pub fn set_process_locale(locale: Locale) {
    if let Ok(mut g) = PROCESS_LOCALE.write() {
        *g = Some(locale);
    }
}

/// The conventions in effect on this thread: a [`with_locale`] override, else the process locale, else en-US.
pub fn current() -> Locale {
    if let Some(l) = OVERRIDE.with(|o| o.borrow().clone()) {
        return l;
    }
    PROCESS_LOCALE.read().ok().and_then(|g| g.clone()).unwrap_or_default()
}

/// Runs `f` with `locale` as this thread's conventions (tests; deterministic regardless of other threads).
pub fn with_locale<R>(locale: Locale, f: impl FnOnce() -> R) -> R {
    let prev = OVERRIDE.with(|o| o.replace(Some(locale)));
    let out = f();
    OVERRIDE.with(|o| *o.borrow_mut() = prev);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separators() {
        let us = Locale::EN_US;
        assert_eq!(us.group_digits("1234567"), "1,234,567");
        assert_eq!(us.localize_decimal("38.8"), "38.8");
        let de = Locale::new(",", ".", false, DateOrder::DayFirst);
        assert_eq!(de.group_digits("1234567"), "1.234.567");
        assert_eq!(de.localize_decimal("38.8"), "38,8");
        let none = Locale::new(".", "", false, DateOrder::DayFirst);
        assert_eq!(none.group_digits("1234567"), "1234567");
        assert_eq!(current(), Locale::EN_US);
        assert_eq!(with_locale(de.clone(), current), de);
        assert_eq!(current(), Locale::EN_US);
    }
}
