// Sommelier IME Test Application
//
// An egui-based GUI for interactively debugging IME and keyboard event flow
// through the sommelier-rs Wayland proxy. Captures and logs all input events
// to diagnose issues in the IME v3→v1 bridge translation.
//
// Usage:
//   cargo run -p sommelier-test-gui
//   cargo run -p sommelier-test-gui -- --auto-exit   # CI mode: exits after 2 s
//   cargo test -p sommelier-test-gui                  # run tests

use eframe::egui;
use egui::{Event, ImeEvent, RawInput, Rgba, TextEdit, Ui, ViewportBuilder, ViewportCommand};
use std::time::{Duration, Instant};

const WINDOW_WIDTH: f32 = 800.0;
const WINDOW_HEIGHT: f32 = 600.0;
const AUTO_EXIT_SECS: u64 = 2;
const BG_DARK: f32 = 0.12;
const TEXT_EDIT_ROWS: usize = 10;
const UI_SPACING: f32 = 8.0;
const TEXT_EDIT_ID: &str = "ime-text-field";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!("Starting Sommelier IME Test Application...");

    let auto_exit = std::env::args().any(|a| a == "--auto-exit");
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "Sommelier IME Test Application\n\
             \n\
             Usage: sommelier-test-gui [OPTIONS]\n\
             \n\
             Options:\n  --auto-exit   Exit automatically after {} s (useful for CI)\n  --help        Print this help message",
            AUTO_EXIT_SECS
        );
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default().with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT]),
        ..Default::default()
    };

    eframe::run_native(
        "Sommelier IME Test",
        options,
        Box::new(|_cc| {
            Ok(Box::new(App {
                text: String::new(),
                auto_exit,
                started: Instant::now(),
                has_focused: false,
            }))
        }),
    )?;

    Ok(())
}

struct App {
    text: String,
    auto_exit: bool,
    started: Instant,
    has_focused: bool,
}

impl App {
    /// Log a single event at the appropriate level.
    ///
    /// IME, Text, Key, Focus, Scroll, and clipboard events are logged at `info!`;
    /// pointer motion and unhandled events are at `trace!`.
    fn log_event(event: &Event) {
        match event {
            Event::Ime(ime) => match ime {
                ImeEvent::Enabled => log::info!("[IME] Enabled"),
                ImeEvent::Preedit(text) => log::info!("[IME] Preedit: {:?}", text),
                ImeEvent::Commit(text) => log::info!("[IME] Commit: {:?}", text),
                ImeEvent::Disabled => log::info!("[IME] Disabled"),
            },
            Event::Text(text) => log::info!("[Text] {:?}", text),
            Event::Key {
                key,
                physical_key,
                pressed,
                repeat,
                modifiers,
            } => {
                log::info!(
                    "[Key] key={:?} physical={:?} pressed={} repeat={} mods={:?}",
                    key,
                    physical_key,
                    pressed,
                    repeat,
                    modifiers
                );
            }
            Event::Copy => log::info!("[Clipboard] Copy"),
            Event::Cut => log::info!("[Clipboard] Cut"),
            Event::Paste(text) => log::info!("[Clipboard] Paste: {:?}", text),
            Event::PointerMoved(pos) => log::trace!("[Pointer] Moved: {:?}", pos),
            Event::PointerButton {
                pos,
                button,
                pressed,
                ..
            } => {
                log::info!(
                    "[Pointer] Button={:?} pos={:?} pressed={}",
                    button,
                    pos,
                    pressed
                );
            }
            Event::PointerGone => log::trace!("[Pointer] Gone"),
            Event::WindowFocused(focused) => {
                log::info!("[Focus] WindowFocused={}", focused);
            }
            Event::MouseWheel { delta, .. } => log::info!("[Scroll] delta={:?}", delta),
            _ => log::trace!("[Event] {:?}", event),
        }
    }
}

impl eframe::App for App {
    /// Called before egui processes input each frame.
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut RawInput) {
        for event in &raw_input.events {
            Self::log_event(event);
        }
    }

    /// Called each frame to render the UI.
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show_ui(ui);
    }

    /// Opaque background (avoids a transparent window on compositors).
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Rgba::from_rgb(BG_DARK, BG_DARK, BG_DARK).to_array()
    }

    /// Log final text buffer on close.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        log::info!(
            "[OnExit] Application closing. Final text buffer: {:?}",
            self.text
        );
    }
}

impl App {
    /// Render the UI body: heading + multiline TextEdit + auto-focus logic.
    fn show_ui(&mut self, ui: &mut Ui) {
        ui.heading("Sommelier IME Test");
        ui.add_space(UI_SPACING);

        let text_edit_id = egui::Id::new(TEXT_EDIT_ID);
        let response = TextEdit::multiline(&mut self.text)
            .id(text_edit_id)
            .desired_rows(TEXT_EDIT_ROWS)
            .desired_width(f32::INFINITY)
            .show(ui);

        // Auto-focus the text field on first frame so the user can type
        // immediately without clicking.
        if !self.has_focused {
            ui.memory_mut(|mem| mem.request_focus(response.response.id));
            self.has_focused = true;
        }

        if self.auto_exit && self.started.elapsed() > Duration::from_secs(AUTO_EXIT_SECS) {
            log::info!("Auto-exit timeout reached. Exiting application.");
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Key, Modifiers};

    fn setup_app() -> (egui::Context, App) {
        let ctx = egui::Context::default();
        ctx.options_mut(|o| o.max_passes = 1.try_into().unwrap());
        let app = App {
            text: String::new(),
            auto_exit: false,
            started: Instant::now(),
            has_focused: false,
        };
        (ctx, app)
    }

    /// Run a single frame with the raw events that a compositor would deliver.
    fn run_frame(ctx: &egui::Context, app: &mut App, events: Vec<Event>) {
        let input = RawInput {
            events,
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| app.show_ui(ui));
    }

    /// Helper to build a Key event for a given logical key.
    fn key_event_with_repeat(key: Key, pressed: bool, repeat: bool) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat,
            modifiers: Modifiers::default(),
        }
    }

    fn key_event(key: Key, pressed: bool) -> Event {
        key_event_with_repeat(key, pressed, false)
    }

    // ── IME Commit flows ────────────────────────────────────────────────

    /// Five IME commits with proper `Enabled` before each.  Well-behaved path.
    #[test]
    fn test_ime_commit_appends_multiple_characters() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);
        assert!(
            app.has_focused,
            "TextEdit should be focused after first frame"
        );

        for i in 0..5 {
            run_frame(
                &ctx,
                &mut app,
                vec![
                    Event::Ime(ImeEvent::Enabled),
                    Event::Ime(ImeEvent::Commit("f".to_owned())),
                ],
            );
            assert_eq!(
                app.text.len(),
                i + 1,
                "After {} commit(s), got {:?}",
                i + 1,
                app.text
            );
        }

        assert_eq!(app.text, "fffff", "Expected 5 f's but got: {:?}", app.text);
    }

    /// Korean IME flow: `Enabled → Preedit → Commit`.
    #[test]
    fn test_ime_commit_with_preedit_appends() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);

        run_frame(
            &ctx,
            &mut app,
            vec![
                Event::Ime(ImeEvent::Enabled),
                Event::Ime(ImeEvent::Preedit("한".to_owned())),
                Event::Ime(ImeEvent::Commit("한".to_owned())),
            ],
        );
        assert_eq!(app.text, "한", "Expected '한' but got: {:?}", app.text);
    }

    /// Empty string commit should be a no-op.
    #[test]
    fn test_ime_empty_commit_is_noop() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);

        run_frame(
            &ctx,
            &mut app,
            vec![
                Event::Ime(ImeEvent::Enabled),
                Event::Ime(ImeEvent::Commit(String::new())),
            ],
        );
        assert_eq!(app.text, "", "Empty commit should not insert anything");
    }

    /// `Disabled` without a following `Commit` should be harmless.
    #[test]
    fn test_ime_disabled_without_commit() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);

        run_frame(&ctx, &mut app, vec![Event::Ime(ImeEvent::Disabled)]);
        assert_eq!(app.text, "", "Just Disabled should not modify text");

        // Subsequent well-behaved commit should still work.
        run_frame(
            &ctx,
            &mut app,
            vec![
                Event::Ime(ImeEvent::Enabled),
                Event::Ime(ImeEvent::Commit("f".to_owned())),
            ],
        );
        assert_eq!(app.text, "f", "Commit after Disabled should work");
    }

    /// `Enabled → Disabled → Enabled → Commit` rapid toggling should work.
    #[test]
    fn test_ime_toggle_then_commit() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);

        run_frame(
            &ctx,
            &mut app,
            vec![
                Event::Ime(ImeEvent::Enabled),
                Event::Ime(ImeEvent::Disabled),
                Event::Ime(ImeEvent::Enabled),
                Event::Ime(ImeEvent::Commit("f".to_owned())),
            ],
        );
        assert_eq!(
            app.text, "f",
            "Enabled→Disabled→Enabled→Commit should insert 'f', got {:?}",
            app.text
        );
    }

    // ── Non-IME event flows ─────────────────────────────────────────────

    /// Direct `Text` events (non-IME path).
    #[test]
    fn test_text_event_appends_multiple() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);

        run_frame(&ctx, &mut app, vec![Event::Text("abc".to_owned())]);
        assert_eq!(app.text, "abc", "Expected 'abc' but got: {:?}", app.text);
    }

    /// Simulates pressing 'f' twice via the normal keyboard event path
    /// (`Key` + `Text`), across separate frames.
    #[test]
    fn test_type_f_twice_end_to_end() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);

        run_frame(
            &ctx,
            &mut app,
            vec![
                key_event(Key::F, true),
                Event::Text("f".to_owned()),
                key_event(Key::F, false),
            ],
        );
        assert_eq!(app.text, "f", "After first f, got: {:?}", app.text);

        run_frame(
            &ctx,
            &mut app,
            vec![
                key_event(Key::F, true),
                Event::Text("f".to_owned()),
                key_event(Key::F, false),
            ],
        );
        assert_eq!(app.text, "ff", "After second f, got: {:?}", app.text);
    }

    /// Multiple characters in a single frame (batch of `Key` + `Text` events).
    #[test]
    fn test_type_three_chars_in_one_frame() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);

        run_frame(
            &ctx,
            &mut app,
            vec![
                key_event(Key::F, true),
                Event::Text("f".to_owned()),
                key_event(Key::O, true),
                Event::Text("o".to_owned()),
                Event::Text("o".to_owned()),
            ],
        );
        assert_eq!(app.text, "foo", "Expected 'foo' but got: {:?}", app.text);
    }

    /// Backspace via Key event should delete the last character.
    #[test]
    fn test_backspace_deletes_last_char() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);

        run_frame(
            &ctx,
            &mut app,
            vec![key_event(Key::F, true), Event::Text("f".to_owned())],
        );
        assert_eq!(app.text, "f");

        run_frame(&ctx, &mut app, vec![key_event(Key::Backspace, true)]);
        assert_eq!(
            app.text, "",
            "Backspace should delete the 'f', got {:?}",
            app.text
        );
    }

    /// Reproduces the Crostini regression: repeated Backspace events after a
    /// Korean commit must continue past the final preedit syllable.
    #[test]
    fn test_held_backspace_deletes_all_korean_syllables() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);
        run_frame(&ctx, &mut app, vec![Event::Text("가나다라".to_owned())]);
        assert_eq!(app.text, "가나다라");

        run_frame(&ctx, &mut app, vec![key_event(Key::Backspace, true)]);
        assert_eq!(app.text, "가나다");

        for expected in ["가나", "가", ""] {
            run_frame(
                &ctx,
                &mut app,
                vec![key_event_with_repeat(Key::Backspace, true, true)],
            );
            assert_eq!(app.text, expected);
        }

        run_frame(&ctx, &mut app, vec![key_event(Key::Backspace, false)]);
        assert!(app.text.is_empty());
    }

    // ── Clipboard events ────────────────────────────────────────────────

    /// Paste should insert clipboard content.
    #[test]
    fn test_paste_inserts_text() {
        let (ctx, mut app) = setup_app();
        run_frame(&ctx, &mut app, vec![]);

        run_frame(&ctx, &mut app, vec![Event::Paste("hello".to_owned())]);
        assert_eq!(
            app.text, "hello",
            "Paste 'hello' should insert 'hello', got {:?}",
            app.text
        );
    }
}
