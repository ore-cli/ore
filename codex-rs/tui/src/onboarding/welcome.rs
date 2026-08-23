use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;
use std::cell::Cell;

use codex_ansi_escape::ansi_escape_line;

use crate::ascii_animation::AsciiAnimation;
use crate::key_hint::KeyBindingListExt;
use crate::onboarding::keys;
use crate::onboarding::onboarding_screen::KeyboardHandler;
use crate::onboarding::onboarding_screen::StepStateProvider;
use crate::tui::FrameRequester;

use super::onboarding_screen::StepState;

/// Rows kept clear beneath the crystal: the blank line and welcome text this
/// widget draws, plus headroom for the step that follows it. Without the
/// headroom a large crystal would claim the screen and push the provider picker
/// or login options out of view.
const RESERVED_ROWS_BELOW_ANIMATION: u16 = 16;

pub(crate) struct WelcomeWidget {
    pub is_logged_in: bool,
    animation: AsciiAnimation,
    animations_enabled: bool,
    animations_suppressed: Cell<bool>,
    layout_area: Cell<Option<Rect>>,
}

impl KeyboardHandler for WelcomeWidget {
    /// Rotate the welcome animation when the fixed toggle shortcut fires.
    ///
    /// The key list includes compatibility variants for terminals that report
    /// modifier bits differently.
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if !self.animations_enabled {
            return;
        }
        if key_event.kind == KeyEventKind::Press && keys::TOGGLE_ANIMATION.is_pressed(key_event) {
            tracing::warn!("Welcome background to press '.'");
            let _ = self.animation.pick_random_variant();
        }
    }
}

impl WelcomeWidget {
    pub(crate) fn new(
        is_logged_in: bool,
        request_frame: FrameRequester,
        animations_enabled: bool,
    ) -> Self {
        if animations_enabled {
            // The animation loop is self-sustaining -- each render schedules the
            // next frame -- but only once it has turned over. If the very first
            // request is dropped because the caller draws before subscribing to
            // the draw channel, the crystal stays frozen until an unrelated
            // event re-arms it. This kick costs one timer and removes the
            // dependency on that ordering.
            request_frame.schedule_frame_in(crate::frames::FRAME_TICK_DEFAULT);
        }
        Self {
            is_logged_in,
            animation: AsciiAnimation::new(request_frame),
            animations_enabled,
            animations_suppressed: Cell::new(false),
            layout_area: Cell::new(None),
        }
    }

    pub(crate) fn update_layout_area(&self, area: Rect) {
        self.layout_area.set(Some(area));
    }

    pub(crate) fn set_animations_suppressed(&self, suppressed: bool) {
        self.animations_suppressed.set(suppressed);
    }
}

impl WidgetRef for &WelcomeWidget {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        if self.animations_enabled && !self.animations_suppressed.get() {
            self.animation.schedule_next_frame();
        }

        let layout_area = self.layout_area.get().unwrap_or(area);
        // Pick the largest crystal that leaves room for the text and the next
        // step; `None` means even the small one would be clipped, so skip it.
        let variants = (self.animations_enabled && !self.animations_suppressed.get())
            .then(|| {
                crate::frames::variants_for_area(
                    layout_area.width,
                    layout_area.height,
                    RESERVED_ROWS_BELOW_ANIMATION,
                )
            })
            .flatten();

        // The crystal is rendered on its own, unwrapped. `Wrap` splits these
        // lines even when they fit, and by a different amount per frame, so
        // everything below them jittered as the crystal turned.
        let mut used = 0u16;
        if let Some(variants) = variants {
            let frame = self.animation.current_frame_in(variants);
            // The crystal frames carry 24-bit ANSI colour, so each row has to be
            // parsed rather than taken as literal text.
            let art: Vec<Line> = frame.lines().map(ansi_escape_line).collect();
            let rows = (art.len() as u16).min(area.height);
            Paragraph::new(art).render(
                Rect {
                    height: rows,
                    ..area
                },
                buf,
            );
            // The crystal's rows plus the blank line beneath it.
            used = rows.saturating_add(1);
        }

        let welcome = Line::from(vec![
            "  ".into(),
            "Welcome to ".into(),
            "Ore".bold(),
            ", a command-line coding agent".into(),
        ]);
        if used < area.height {
            Paragraph::new(vec![welcome])
                .wrap(Wrap { trim: false })
                .render(
                    Rect {
                        y: area.y.saturating_add(used),
                        height: area.height - used,
                        ..area
                    },
                    buf,
                );
        }
    }
}

impl StepStateProvider for WelcomeWidget {
    fn get_step_state(&self) -> StepState {
        match self.is_logged_in {
            true => StepState::Hidden,
            false => StepState::Complete,
        }
    }
}

#[cfg(test)]
mod tests {
    /// Comfortably fits the large crystal plus the reserved rows.
    const TEST_WIDTH: u16 = 80;
    const TEST_HEIGHT: u16 = 44;

    use super::*;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    static VARIANT_A: [&str; 1] = ["frame-a"];
    static VARIANT_B: [&str; 1] = ["frame-b"];
    static VARIANTS: [&[&str]; 2] = [&VARIANT_A, &VARIANT_B];

    fn row_containing(buf: &Buffer, needle: &str) -> Option<u16> {
        (0..buf.area.height).find(|&y| {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            row.contains(needle)
        })
    }

    /// The first frame the initial render schedules can be dropped when the
    /// caller draws before subscribing to the draw channel. Without this kick
    /// the crystal stays frozen until an unrelated event re-arms the loop.
    #[test]
    fn constructing_the_widget_kicks_the_animation_loop() {
        let (requester, mut scheduled) = FrameRequester::test_channel();
        let _widget = WelcomeWidget::new(
            /*is_logged_in*/ false, requester, /*animations_enabled*/ true,
        );

        let at = scheduled
            .try_recv()
            .expect("welcome should schedule a frame when it is constructed");
        assert!(
            at > std::time::Instant::now(),
            "the kick must be delayed so it lands after event_stream() subscribes"
        );
    }

    #[test]
    fn no_kick_when_animations_are_disabled() {
        let (requester, mut scheduled) = FrameRequester::test_channel();
        let _widget = WelcomeWidget::new(
            /*is_logged_in*/ false, requester, /*animations_enabled*/ false,
        );

        assert!(
            scheduled.try_recv().is_err(),
            "nothing should be scheduled when animations are off"
        );
    }

    #[test]
    fn welcome_renders_animation_on_first_draw() {
        let widget = WelcomeWidget::new(
            /*is_logged_in*/ false,
            FrameRequester::test_dummy(),
            /*animations_enabled*/ true,
        );
        let area = Rect::new(0, 0, TEST_WIDTH, TEST_HEIGHT);
        let mut buf = Buffer::empty(area);
        let variants = crate::frames::variants_for_area(
            area.width,
            area.height,
            RESERVED_ROWS_BELOW_ANIMATION,
        )
        .expect("a crystal should fit this area");
        let frame_lines = widget.animation.current_frame_in(variants).lines().count() as u16;
        (&widget).render_ref(area, &mut buf);

        let welcome_row = row_containing(&buf, "Welcome");
        assert_eq!(welcome_row, Some(frame_lines + 1));
    }

    /// The crystal frames carry 24-bit ANSI colour. A render path that treats a
    /// frame as literal text draws the escape sequences as visible garbage
    /// instead of colouring the cells.
    #[test]
    fn welcome_animation_renders_in_colour() {
        let widget = WelcomeWidget::new(
            /*is_logged_in*/ false,
            FrameRequester::test_dummy(),
            /*animations_enabled*/ true,
        );
        let area = Rect::new(0, 0, TEST_WIDTH, TEST_HEIGHT);
        let mut buf = Buffer::empty(area);
        (&widget).render_ref(area, &mut buf);

        let coloured = buf
            .content()
            .iter()
            .filter(|cell| cell.fg != ratatui::style::Color::Reset)
            .count();
        assert!(coloured > 0, "expected the crystal to render with colour");

        let has_escape = buf
            .content()
            .iter()
            .any(|cell| cell.symbol().contains('\u{1b}'));
        assert!(!has_escape, "ANSI escapes leaked into the rendered buffer");
    }

    #[test]
    fn welcome_skips_animation_below_height_breakpoint() {
        let widget = WelcomeWidget::new(
            /*is_logged_in*/ false,
            FrameRequester::test_dummy(),
            /*animations_enabled*/ true,
        );
        // Too short for even the small crystal plus the reserved rows.
        let area = Rect::new(
            0,
            0,
            TEST_WIDTH,
            crate::frames::SIZE_SMALL.1 + RESERVED_ROWS_BELOW_ANIMATION - 1,
        );
        let mut buf = Buffer::empty(area);
        (&widget).render_ref(area, &mut buf);

        let welcome_row = row_containing(&buf, "Welcome");
        assert_eq!(welcome_row, Some(0));
    }

    #[test]
    fn ctrl_dot_changes_animation_variant() {
        let mut widget = WelcomeWidget {
            is_logged_in: false,
            animation: AsciiAnimation::with_variants(
                FrameRequester::test_dummy(),
                &VARIANTS,
                /*variant_idx*/ 0,
            ),
            animations_enabled: true,
            animations_suppressed: Cell::new(false),
            layout_area: Cell::new(None),
        };

        let before = widget.animation.current_frame_in(&VARIANTS);
        widget.handle_key_event(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::CONTROL));
        let after = widget.animation.current_frame_in(&VARIANTS);

        assert_ne!(
            before, after,
            "expected ctrl+. to switch welcome animation variant"
        );
    }

    #[test]
    fn ctrl_shift_dot_changes_animation_variant() {
        let mut widget = WelcomeWidget {
            is_logged_in: false,
            animation: AsciiAnimation::with_variants(
                FrameRequester::test_dummy(),
                &VARIANTS,
                /*variant_idx*/ 0,
            ),
            animations_enabled: true,
            animations_suppressed: Cell::new(false),
            layout_area: Cell::new(None),
        };

        let before = widget.animation.current_frame_in(&VARIANTS);
        widget.handle_key_event(KeyEvent::new(
            KeyCode::Char('.'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        let after = widget.animation.current_frame_in(&VARIANTS);

        assert_ne!(
            before, after,
            "expected ctrl+shift+. to switch welcome animation variant"
        );
    }

    /// Every frame must place the text on the same row. `Wrap` used to split
    /// the crystal's lines by a differing amount per frame, so the step's
    /// measured height changed as it turned and the menu below it jittered.
    #[test]
    fn text_row_is_identical_for_every_frame() {
        for (name, variants) in [
            ("small", crate::frames::VARIANTS_SMALL),
            ("medium", crate::frames::VARIANTS_MEDIUM),
            ("large", crate::frames::VARIANTS_LARGE),
        ] {
            for (vi, frames) in variants.iter().enumerate() {
                let mut rows = std::collections::BTreeSet::new();
                for frame in frames.iter() {
                    let area = Rect::new(0, 0, TEST_WIDTH, TEST_HEIGHT);
                    let mut buf = Buffer::empty(area);
                    let art: Vec<Line> = frame.lines().map(ansi_escape_line).collect();
                    let art_rows = art.len() as u16;
                    Paragraph::new(art).render(
                        Rect {
                            height: art_rows,
                            ..area
                        },
                        &mut buf,
                    );
                    Paragraph::new(vec![Line::from("Welcome to")])
                        .wrap(Wrap { trim: false })
                        .render(
                            Rect {
                                y: art_rows + 1,
                                height: area.height - art_rows - 1,
                                ..area
                            },
                            &mut buf,
                        );
                    rows.insert(row_containing(&buf, "Welcome to"));
                }
                assert_eq!(
                    rows.len(),
                    1,
                    "{name} variant {vi}: text row moves across frames: {rows:?}"
                );
            }
        }
    }
}
