# Vox design language (Rust / Slint rewrite)

The rewrite drops the Nothing-OS dot-matrix look of the Electron app. The new look takes its *vibe* from
`docs/reference-redesign.png` (a Figma "Manage Libraries" dialog on a dark silk backdrop). We do not copy that
dialog. We borrow its qualities:

- **Quiet, near-black surfaces.** One charcoal panel colour, hairline borders, and almost no fills. Hierarchy comes
  from type weight and grey levels, not from boxes inside boxes.
- **Monochrome UI, colour only in art.** Buttons, icons, charts and text are white or grey. The only saturated
  colour on screen comes from the abstract "silk" art thumbnails, which give each voice its own identity.
- **Pills.** Buttons, search fields, selected navigation items, segmented controls and badges are fully rounded.
  Primary actions are a solid near-white pill with dark text. Secondary actions are a dark pill with a hairline border.
- **Generous, calm spacing.** Big rows (thumbnail 110×82 in lists, 56×56 in compact rows), 24–40 px gutters,
  and section labels in muted grey sentence case ("Assets created in this file"), never ALL CAPS.
- **A deep backdrop.** Behind the panel sits a very dark navy backdrop with soft, out-of-focus silver "silk"
  curves. It shows around the main panel, on onboarding, and in empty states.

Measured from the reference (1200×1500 export):

| Sample | Colour |
|---|---|
| Panel fill (header, sidebar, content are all the same) | `#1a1a1a` |
| Panel outer border | `#262a2d` (a cool hairline that catches the backdrop) |
| Dividers (header rule, sidebar rule) | `#2d2d2d` / `#2f2f2f` |
| Search field fill / border | `#1b1b1b` / `#2c2c2c` (very subtle) |
| Keycap chip fill | `#262626` |
| Selected nav pill border | `#2f2f2f`, fill = panel |
| Primary pill ("Publish") | `#f9f9f9`, text `#0a0a0a` |
| Secondary pill ("Remove") | fill `#1b1b1b`, border `#2c2c2c`, text `#f2f2f2` |
| Section label text | about `#8a8a8a` |
| Row subtitle text | about `#9a9a9a` |
| Close glyph | `#c0c0c0` |
| Backdrop | `#010204` (top) → `#070b14` / `#0e1621` (blue-black), with grey silk highlights |

## Type

Windows 11 ships **Segoe UI Variable**. The Slint renderer can use it by family name, and it carries the same
crisp neo-grotesque feel as the reference's Inter. No font files need to be bundled.

| Role | Family | Size / weight |
|---|---|---|
| Panel header title ("History") | Segoe UI Variable Text | 15 / 600 |
| Page hero ("Good evening") | Segoe UI Variable Display | 26 / 600 |
| Big number (stat cards, time) | Segoe UI Variable Display | 34 / 600 |
| Section label | Segoe UI Variable Text | 13 / 500, `text-muted` |
| Row title | Segoe UI Variable Text | 14 / 600, `text` |
| Row subtitle | Segoe UI Variable Text | 13 / 500, `text-secondary` |
| Body | Segoe UI Variable Text | 14 / 400 |
| Button label | Segoe UI Variable Text | 14 / 600 (sm buttons 13 / 600) |
| Caption / meta | Segoe UI Variable Text | 12 / 500, `text-muted` |
| Widget sentence | Segoe UI Variable Display | 17 / 600 |

Fallbacks: `Segoe UI`, then the system default. Numbers that tick (times, counters) sit in fixed-width boxes
so the layout never jitters.

## Colour tokens

All colours live in one Slint `global Theme` with `dark` (default) and `light` ("Paper") variants. Components never
hard-code a colour.

| Token | Dark (Night) | Light (Paper) | Use |
|---|---|---|---|
| `backdrop` | `#05070b` + silk art | `#e9ecf1` + light silk | Window background around the panel |
| `panel` | `#1a1a1a` | `#ffffff` | The main panel, widget card, chip, dialogs |
| `panel-border` | `#262a2d` | `#dfe2e6` | Outer hairline of floating surfaces |
| `divider` | `#2d2d2d` | `#ececee` | Rules between header/sidebar/content |
| `field` | `#1d1d1d` | `#f6f6f7` | Inputs, search, text areas |
| `field-border` | `#2e2e2e` | `#e2e2e5` | Input / secondary-pill border |
| `field-border-focus` | `#5a5a5a` | `#9a9aa0` | Focused input border |
| `hover` | `#232323` | `#f2f2f3` | Row / ghost-button hover fill |
| `pressed` | `#2a2a2a` | `#e8e8ea` | Pressed fill |
| `chip` | `#262626` | `#efeff1` | Keycaps, small badges |
| `text` | `#f2f2f2` | `#111113` | Primary text and icons |
| `text-secondary` | `#9a9a9a` | `#5f6067` | Subtitles, secondary labels |
| `text-muted` | `#6f6f6f` | `#8d8e94` | Section labels, captions, placeholders, upcoming words |
| `primary` | `#f9f9f9` | `#111113` | Primary pill fill, active toggle track, progress fill |
| `primary-hover` | `#ffffff` | `#2a2a2e` | |
| `on-primary` | `#0a0a0a` | `#ffffff` | Text/icon on a primary pill |
| `danger` | `#ff6b5e` | `#d93a2b` | Destructive text, errors (the only non-art colour) |
| `success` | `#7ad69a` | `#1f9d55` | Small "connected / saved" dots only |
| `scrim` | `#000000a6` | `#0000004d` | Behind dialogs |

## Shape and spacing

- Radii: panel 22, card 16, thumbnail 10 (12 for 110-px art), field 12 (search field fully round), pill = height/2,
  keycap 7 (or a circle for single glyphs), widget card 20, chip window 17 (= height/2).
- Spacing scale: 4, 8, 12, 16, 20, 24, 32, 40. Panel header height 64. Sidebar width 260 (collapses to icons
  below 900 px window width).
- Hairlines are always 1 px (physical pixels may round). No drop shadows inside the panel. The main panel itself
  gets one soft shadow (`#00000099`, blur 40) against the backdrop.
- Motion: 120 ms ease-out for hover/press colour, 180 ms for toggles/segmented, 220 ms for page fades. Nothing
  bounces.

## Components (in `ui/design/`)

| Component | Spec |
|---|---|
| `Button` | `kind`: primary / secondary / ghost / danger; `size`: sm (32 h, 14 px side padding) / md (40 h, 22 px). Optional leading icon (16 px). Primary = `primary` fill + `on-primary` text. Secondary = `field` fill + `field-border` hairline. Ghost = no fill, `hover` on hover. Danger = secondary with `danger` text. Disabled = 40 % opacity. Focus ring = 2 px `text` at 25 % alpha, outside the pill. |
| `IconButton` | 36 (or 28 small) circle, ghost by default; `primary` variant = white circle with dark glyph (used for Play). |
| `SearchField` | Fully round, 44 h, `field` fill, `field-border`, magnifier icon left, placeholder in `text-muted`, optional keycaps at the right (`Ctrl` `K`). |
| `Keycap` | 22 h, min 22 w, radius 7 (circle for 1 glyph), `chip` fill, 1 px `field-border`, 11 / 600 text. |
| `NavItem` | 44 h row: 20 px icon + 14 / 600 label. Selected = pill with 1 px `divider`-grey border (reference "This File"); hover = `hover` fill pill. |
| `SectionLabel` | 13 / 500 `text-muted`, sentence case, 0 letter-spacing. Optional trailing link ("All history →"). |
| `ListRow` | Thumbnail (art), title, subtitle, trailing slot (buttons/meta). Sizes: `large` (110×82 art, 24 gap), `compact` (44×44 art, 16 gap). Hover = `hover` fill with radius 14 behind the whole row. |
| `Card` | `panel` fill on the backdrop, or a hairline-bordered (`divider`) area inside the panel. Radius 16, padding 20. Used sparingly. |
| `Toggle` | 40×24 pill. Off: `chip` track + `text-muted` knob. On: `primary` track + `on-primary` knob. |
| `Segmented` | Pill container (`field` fill, hairline) with a sliding selected pill (`primary` fill, `on-primary` text). |
| `Slider` | 4 px track (`chip`), `primary` fill, 16 px `primary` knob with a 1 px `panel` ring; value label at right in `text-secondary`. |
| `TextField` / `TextArea` | `field` fill, `field-border`, radius 12, 40 h (area: min 96), focus border `field-border-focus`. |
| `Select` | Looks like a secondary pill with a chevron; opens a `panel` popup list with hairline border, radius 14, row hover `hover`, check mark on the selected row. |
| `Badge` | 22 h pill, `chip` fill, 12 / 600; `outline` variant = hairline only. |
| `Toast` | Bottom-centre pill on the panel: `panel` fill, `panel-border`, icon + text + optional action (ghost sm button). Error toasts use a `danger` icon, not a red fill. |
| `Dialog` | Exactly the reference's shape: 64-px header with 15/600 title and a close glyph, divider, body, footer with right-aligned pills. Over a `scrim`. |
| `EmptyState` | Large art thumbnail (110×82), title 15/600, body `text-secondary`, optional primary pill. |
| `ProgressLine` | 3 px rounded track (`chip`), buffered segment (`text-muted` at 50 %), played segment (`primary`); on hover it grows to 5 px and shows a 12 px knob. |
| `Skeleton` | `hover`-coloured rounded bars with a slow (1.4 s) opacity pulse. |

## Art ("silk" thumbnails)

The reference's thumbnails are glossy, out-of-focus 3-D ribbons in vivid gradients. Vox generates these
procedurally at runtime (a tiny-skia renderer in Rust, deterministic from a seed string) and caches them:

- **Voice art**: seed = voice id. Every voice always has the same art, in Voices, Settings, the widget, History
  rows and Home.
- **Source art** (history rows when no voice art fits, e.g. a source filter chip): seed = app name.
- **Backdrop**: one large dark silk image (navy/charcoal palette, low contrast) for the window backdrop, onboarding
  and empty states.

Palettes, picked by seed (taken from the reference): ember (orange → peach → cyan), opal (lavender → teal →
graphite), dusk (pink → navy → orange), crimson (red → oxblood → black), steel (navy → charcoal → silver), plus a
few more in the same spirit (moss, glacier, saffron). Each image: 2–4 bezier ribbons with gradient fills, a
Gaussian blur, a soft specular streak, and a gentle vignette. Sizes are rendered at 2× for crisp display.

## Screens

The main window shows the **backdrop**, with the **panel** inset by 14 px on every side (radius 22, one soft
shadow). The native title bar is kept (resize, snap and keyboard behave normally) and coloured to match the
backdrop with DWM, so it reads as one surface.

Inside the panel (reference layout):

- **Header row** (64 h): current page title (15 / 600), and page actions on the right (for example Export on
  History). Divider below.
- **Sidebar** (260 w, divider on the right): `SearchField` ("Search history", `Ctrl` `K`), divider, primary nav
  (Home, History, Analytics, Voices) as `NavItem`s, divider, then a muted section label ("Library" style) for
  secondary entries (Settings), and at the bottom a compact **now playing** row (voice art, sentence, play/pause)
  plus the credits line.
- **Content**: scrolls; 40 px side padding; sections start with a `SectionLabel`; lists use `ListRow`.

Page notes (the behaviour comes from the port specs in `docs/port/`; this table is only the look):

| Page | Look |
|---|---|
| Home | Greeting hero with the date. A "Now playing" row (large art, sentence, transport, Replay last as a secondary pill). Three quiet stat blocks in a row (Today vs goal, Credits, This week) as big Display numbers with a thin line meter, separated by vertical dividers, no boxes. Quick read (TextArea + primary "Read" pill + secondary "Read clipboard"). "Recent" list of compact ListRows (voice art, text excerpt, "Outlook · George · 10 h ago", duration, play button). "Ways to read" as compact rows with icons. |
| History | Search + filter pills (All, sources…, Starred) under the header. Day groups with a section label ("Today", "Yesterday", "Mon 6 Oct"). Large ListRows: voice art, excerpt title, meta subtitle, trailing "Replay" secondary pill (or "Free replay" badge) and an overflow (⋯) for Copy / Star / Delete. Expanding a row shows the full text and details in an inset area. |
| Analytics | Range segmented control (7D, 30D, 90D, 1Y, All). Hero number (hours listened) + small stats row. Daily columns chart: rounded white bars on a hairline grid, muted for zero days, hover tooltip pill. Heatmap: 7×24 rounded cells in white at 6 opacity steps. Top apps / top voices as compact ListRows with a thin share meter. Credits block. |
| Voices | Search + filter pills. Large ListRows exactly like the reference: voice art, name, "Male · American · Narration", trailing "Preview" secondary pill and "Use voice" primary pill (the voice in use shows a "In use" outline badge instead). |
| Settings | A second-level list on the left inside the content (Account, Voice & model, Playback, Triggers, Widget, Appearance, Library & privacy, Integrations, About) as small NavItems; the right side is a column of setting rows: title 14 / 600, help text 13 `text-secondary`, control on the right; dividers between rows. |
| Onboarding | Full backdrop, a centred 560-px panel (dialog shape) with a step indicator (dots), large art per step, primary "Continue" pill. |

## Floating widget (player)

A frameless, transparent, always-on-top window that is exactly the card (no shadow margin; only the rounded corners
are transparent). It never takes focus.

```
┌──────────────────────────────────────────────────────┐  380 w, radius 20, panel fill at 96 % alpha,
│ [art]  Chrome · George                     (1×)   ✕  │  1 px panel-border
│                                                      │
│ The quiet revolution in personal computing was       │  17 / 600 Display: spoken words `text`,
│ never about faster chips.                            │  current word `text` + subtle underline,
│                                                      │  upcoming words `text-muted`
│ ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━──────────────────  │  ProgressLine (seek by click/drag)
│ Reading                         ⏮   ( ▶ )   ⏭   -0:17│  status label 12/500 muted; white play circle
└──────────────────────────────────────────────────────┘
```

- The speed pill is a secondary sm pill ("1×", "1.25×"…). Close is a ghost glyph.
- States (starting, synthesising, reading, paused, finished, stopped early, error, toast) change the status label
  and body. Errors show the message in `text` with a `danger` icon. No red fills.
- **Compact pill** variant (one line, 48 h, radius 24): art dot, play/pause, sentence excerpt with the current word
  bright, time, close.

## "Read with Vox" chip

A 34-px pill window: `panel` fill, `panel-border` hairline, a 16-px voice-art dot, then "Read with Vox" (13 / 600,
`text`). Hover = `hover` fill. The selection-popup variant is the same pill with just "Read".

## Tray

Native Windows tray menu (muda). Tray icon: a white rounded "V" wave mark on transparent; the active variant adds a
small dot.
