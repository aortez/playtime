use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use playtime_chat::console::{
    Composer, MAX_DRAFT_BYTES, Observation, Presentation, markdown::Markdown,
};
use ratatui::{Terminal, backend::TestBackend, style::Modifier};
use serde_json::json;

fn render(composer: &Composer, width: u16) {
    let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
    terminal
        .draw(|f| f.render_widget(&composer.editor, f.area()))
        .unwrap();
}
fn key(composer: &mut Composer, code: KeyCode) {
    composer.key(KeyEvent::new(code, KeyModifiers::NONE));
}
#[test]
fn composer_wraps_unicode_and_preserves_draft_history() {
    let mut composer = Composer::default();
    composer.paste("a long wrapped line 界 e\u{301} 👩\u{200d}💻");
    render(&composer, 12);
    key(&mut composer, KeyCode::Backspace);
    assert_eq!(composer.text(), "a long wrapped line 界 e\u{301} ");
    key(&mut composer, KeyCode::Backspace);
    key(&mut composer, KeyCode::Backspace);
    assert_eq!(composer.text(), "a long wrapped line 界 ");
    let submitted = composer.take();
    composer.paste("pending\ncode");
    composer.history(true);
    assert_eq!(composer.text(), submitted);
    composer.history(false);
    assert_eq!(composer.text(), "pending\ncode");
    render(&composer, 60);
    render(&composer, 4);
    assert_eq!(composer.text(), "pending\ncode");
}
#[test]
fn delete_at_end_preserves_text_and_does_not_leave_a_selection() {
    for text in ["", "abc", "first\n", "first\ne\u{301} 👩\u{200d}💻"] {
        let mut composer = Composer::default();
        composer.paste(text);
        key(&mut composer, KeyCode::Delete);
        assert_eq!(composer.text(), text);
        assert!(composer.editor.selection_range().is_none());
        composer.paste("x");
        assert_eq!(composer.text(), format!("{text}x"));
    }
    let mut composer = Composer::default();
    composer.paste("ab\n👩\u{200d}💻");
    key(&mut composer, KeyCode::Home);
    key(&mut composer, KeyCode::Left);
    key(&mut composer, KeyCode::Delete);
    assert_eq!(composer.text(), "ab👩\u{200d}💻");
    key(&mut composer, KeyCode::Delete);
    assert_eq!(composer.text(), "ab");
    key(&mut composer, KeyCode::Delete);
    assert_eq!(composer.text(), "ab");
}
#[test]
fn plain_arrows_end_shift_selection_before_typing() {
    for direction in [KeyCode::Left, KeyCode::Right] {
        let mut composer = Composer::default();
        composer.paste("abc");
        composer.key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        key(&mut composer, direction);
        composer.paste("X");
        assert_eq!(
            composer.text(),
            if direction == KeyCode::Left {
                "aXbc"
            } else {
                "abcX"
            }
        );
        assert!(composer.editor.selection_range().is_none());
    }
}
#[test]
fn paste_is_atomic_bounded_and_escapes_controls() {
    let mut composer = Composer::default();
    assert!(composer.paste("/quit\r\n\u{1b}[2J"));
    assert_eq!(composer.text(), "/quit\n\\u{1b}[2J");
    assert!(!composer.paste(&"x".repeat(MAX_DRAFT_BYTES)));
    assert_eq!(composer.text(), "/quit\n\\u{1b}[2J");
}
#[test]
fn streaming_markdown_handles_split_fences_and_partial_lines() {
    let mut md = Markdown::default();
    assert!(md.push("## Ti").is_empty());
    assert_eq!(md.preview().to_string(), "## Ti");
    let lines = md.push("tle\n- **bold** and `code`\n``");
    assert_eq!(lines[0].to_string(), "Title");
    assert!(
        lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD)
    );
    assert_eq!(lines[1].to_string(), "- bold and code");
    assert!(md.push("`rust\n")[0].to_string().contains("```rust"));
    assert_eq!(
        md.push("    **literal**\n")[0].to_string(),
        "    **literal**"
    );
    md.push("```\n");
    assert_eq!(
        md.push("[docs](https://example.test)\n")[0].to_string(),
        "docs (https://example.test)"
    );
    md.push("unterminated **");
    assert_eq!(md.flush()[0].to_string(), "unterminated **");
}
#[test]
fn markdown_character_references_cannot_reintroduce_terminal_controls() {
    let mut md = Markdown::default();
    let lines = md.push("&#27;]52;c;payload&#7; [link](https://example.test/&#27;)\n");
    let rendered = lines[0].to_string();
    assert!(!rendered.chars().any(|c| c.is_control()));
    assert!(rendered.contains("payload"));
}
#[test]
fn transcript_retains_raw_tools_and_distinguishes_two_kinds_of_omission() {
    let mut view = Presentation::default();
    let value = json!({"stderr":"failure\n".repeat(10),"output_truncated":true,"exit_code":2});
    let rendered = view
        .observe(Observation::ToolResult {
            value: value.clone(),
        })
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("exit 2"));
    assert!(rendered.contains("omitted from preview"));
    assert!(rendered.contains("truncated upstream"));
    let raw = view.raw(Some(1));
    let evidence = raw
        .iter()
        .skip(1)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&evidence).unwrap(),
        value
    );
    for _ in 0..17 {
        view.observe(Observation::ToolResult {
            value: json!({"content":"new"}),
        });
    }
    assert!(
        view.raw(Some(1))[0]
            .to_string()
            .contains("not in the bounded preview cache")
    );
}
#[test]
fn long_partial_text_is_bounded_and_compaction_does_not_erase_printed_content() {
    let mut view = Presentation::default();
    let lines = view.observe(Observation::Delta {
        text: "界".repeat(20_000),
        thinking: false,
    });
    assert!(view.preview().to_string().len() <= 4096);
    let count: usize = lines
        .iter()
        .map(|line| line.to_string().matches('界').count())
        .sum();
    let flushed = view.observe(Observation::Phase {
        text: "Compaction · input bound 100 tokens".into(),
    });
    let remaining: usize = flushed
        .iter()
        .map(|line| line.to_string().matches('界').count())
        .sum();
    assert_eq!(count + remaining, 20_000);
    let lines = view.observe(Observation::Delta {
        text: "\u{1b}]52;c;payload\u{7}\n".into(),
        thinking: false,
    });
    assert!(!lines.iter().any(|line| line.to_string().contains('\u{1b}')));
}
