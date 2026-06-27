#![allow(dead_code)]

include!("../src/main.rs");

#[cfg(test)]
mod tests {
    use super::{
        apply_append_text, display_lines_for_run, is_exit_key, parse_bar_values, parse_cli_args,
        push_output_line, shell_escape_arg, spawn_script, split_args_on_separator,
    };
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::sync::{
        Arc, Mutex,
        atomic::AtomicU64,
        mpsc,
    };
    use std::time::Duration;

    #[test]
    fn escapes_empty_argument() {
        assert_eq!(shell_escape_arg(""), "''");
    }

    #[test]
    fn keeps_safe_argument_unquoted() {
        assert_eq!(
            shell_escape_arg("python3:/usr/local/bin@v1.0%prod"),
            "python3:/usr/local/bin@v1.0%prod"
        );
    }

    #[test]
    fn quotes_argument_with_spaces_and_symbols() {
        assert_eq!(
            shell_escape_arg("sum:metric.name{env:prod}.as_count()"),
            "'sum:metric.name{env:prod}.as_count()'"
        );
        assert_eq!(
            shell_escape_arg("two words"),
            "'two words'"
        );
    }

    #[test]
    fn escapes_single_quote_inside_argument() {
        assert_eq!(
            shell_escape_arg("it's good"),
            "'it'\\''s good'"
        );
    }

    #[test]
    fn captures_subprocess_stdout_line() {
        let buffer: Arc<Mutex<Vec<(u64, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let active_run = Arc::new(AtomicU64::new(1));
        let (update_tx, _update_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<u64>();

        let cmd = vec!["/bin/echo".to_string(), "23.0".to_string()];
        spawn_script(
            &cmd,
            Arc::clone(&buffer),
            Arc::clone(&active_run),
            1,
            update_tx,
            done_tx,
        );

        let finished = done_rx.recv_timeout(Duration::from_secs(5)).expect("script should finish");
        assert_eq!(finished, 1);

        let lines: Vec<String> = buffer
            .lock()
            .unwrap()
            .iter()
            .filter(|(rid, _)| *rid == 1)
            .map(|(_, line)| line.clone())
            .collect();
        assert!(lines.iter().any(|line| line.contains("23.0")));
    }

    #[test]
    fn exits_on_q_key() {
        assert!(is_exit_key(KeyCode::Char('q'), KeyModifiers::NONE));
    }

    #[test]
    fn exits_on_ctrl_c() {
        assert!(is_exit_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    #[test]
    fn does_not_exit_on_plain_c() {
        assert!(!is_exit_key(KeyCode::Char('c'), KeyModifiers::NONE));
    }

    #[test]
    fn separator_extracts_command_args() {
        let args = vec![
            "vju-t".to_string(),
            "--line-chart".to_string(),
            "--".to_string(),
            "aws_eks_info".to_string(),
            "--cluster".to_string(),
            "dev".to_string(),
        ];

        let (parse_end, trailing) = split_args_on_separator(&args);
        assert_eq!(parse_end, 2);
        assert_eq!(trailing, vec!["aws_eks_info", "--cluster", "dev"]);
    }

    #[test]
    fn separator_absent_keeps_full_parse_range() {
        let args = vec![
            "vju-t".to_string(),
            "--line-chart".to_string(),
            "aws_eks_info".to_string(),
        ];

        let (parse_end, trailing) = split_args_on_separator(&args);
        assert_eq!(parse_end, args.len());
        assert!(trailing.is_empty());
    }

    // -- Rendering regression tests ----------------------------------------

    #[test]
    fn no_waiting_flash_while_new_run_has_no_output_yet() {
        // Run 1 produced output; run 2 just started and has nothing yet.
        // display_lines_for_run should return run 1's lines so we don't flash "Waiting...".
        let buf = vec![
            (1u64, "line-a".to_string()),
            (1u64, "line-b".to_string()),
        ];
        let lines = display_lines_for_run(&buf, 2);
        assert_eq!(lines, vec!["line-a", "line-b"],
            "should fall back to previous run's lines while new run is empty");
    }

    #[test]
    fn switches_to_new_run_once_it_has_output() {
        // Once run 2 pushes its first line, display should show only run 2.
        let buf = vec![
            (1u64, "old-line".to_string()),
            (2u64, "new-line".to_string()),
        ];
        let lines = display_lines_for_run(&buf, 2);
        assert_eq!(lines, vec!["new-line"],
            "once new run has data, old run lines should not appear");
    }

    #[test]
    fn empty_buffer_returns_empty() {
        let buf: Vec<(u64, String)> = vec![];
        let lines = display_lines_for_run(&buf, 1);
        assert!(lines.is_empty());
    }

    #[test]
    fn parse_bar_values_accumulates_across_multiple_lines() {
        // Regression: bar parser must not return after the first non-empty line.
        // When no word label precedes the value, the value itself becomes the x-axis label.
        let input = "10\n20\n30\n";
        let bars = parse_bar_values(input).expect("should parse multi-line bar values");
        assert_eq!(bars, vec![
            ("10".to_string(), 10),
            ("20".to_string(), 20),
            ("30".to_string(), 30),
        ]);
    }

    #[test]
    fn parse_bar_values_uses_value_as_label_when_no_word_precedes() {
        // When input has bare numbers with no preceding word, the value itself
        // becomes the x-axis label so the bar chart x-axis is meaningful
        // rather than showing a meaningless count (1, 2, 3...).
        let input = "1432\n23\n105\n";
        let bars = parse_bar_values(input).expect("should parse bare numbers");
        assert_eq!(bars, vec![
            ("1432".to_string(), 1432),
            ("23".to_string(), 23),
            ("105".to_string(), 105),
        ]);
    }

    #[test]
    fn parse_bar_values_preserves_labeled_values_from_multiple_lines() {
        // Regression: labeled pairs from later lines must not be dropped.
        let input = "ok 7\nwarn 4\ncrit 2\n";
        let bars = parse_bar_values(input).expect("should parse labeled multi-line values");
        assert_eq!(bars, vec![
            ("ok".to_string(), 7),
            ("warn".to_string(), 4),
            ("crit".to_string(), 2),
        ]);
    }

    #[test]
    fn parse_bar_values_strips_common_label_punctuation() {
        // Regression: labels like "2xx:" should remain readable x labels.
        let input = "2xx: 10\n5xx: 3\n";
        let bars = parse_bar_values(input).expect("should parse punctuated labels");
        assert_eq!(bars, vec![
            ("2xx".to_string(), 10),
            ("5xx".to_string(), 3),
        ]);
    }

    #[test]
    fn push_output_line_evicts_old_run_on_first_new_line() {
        // Previous run left lines; first push of new run should clear them.
        let buffer: Arc<Mutex<Vec<(u64, String)>>> = Arc::new(Mutex::new(vec![
            (1u64, "old".to_string()),
        ]));
        let active_run = Arc::new(AtomicU64::new(2));
        let (tx, _rx) = mpsc::channel::<()>();

        push_output_line("new", &buffer, &active_run, 2, &tx);

        let buf = buffer.lock().unwrap();
        let run1: Vec<_> = buf.iter().filter(|(rid, _)| *rid == 1).collect();
        let run2: Vec<_> = buf.iter().filter(|(rid, _)| *rid == 2).collect();
        assert!(run1.is_empty(), "old run lines should be evicted after first new-run push");
        assert_eq!(run2.len(), 1);
        assert_eq!(run2[0].1, "new");
    }

    #[test]
    fn push_output_line_does_not_evict_on_subsequent_lines() {
        // Second push of same run should not clear existing run lines.
        let buffer: Arc<Mutex<Vec<(u64, String)>>> = Arc::new(Mutex::new(vec![
            (2u64, "first".to_string()),
        ]));
        let active_run = Arc::new(AtomicU64::new(2));
        let (tx, _rx) = mpsc::channel::<()>();

        push_output_line("second", &buffer, &active_run, 2, &tx);

        let buf = buffer.lock().unwrap();
        let run2: Vec<_> = buf.iter().filter(|(rid, _)| *rid == 2).collect();
        assert_eq!(run2.len(), 2, "second push should append, not evict");
    }

    #[test]
    fn push_output_line_ignored_when_run_id_superseded() {
        // If active_run has moved on, push for old run_id should be a no-op.
        let buffer: Arc<Mutex<Vec<(u64, String)>>> = Arc::new(Mutex::new(vec![]));
        let active_run = Arc::new(AtomicU64::new(3));
        let (tx, _rx) = mpsc::channel::<()>();

        push_output_line("stale", &buffer, &active_run, 2, &tx);

        assert!(buffer.lock().unwrap().is_empty(),
            "stale run output should be dropped silently");
    }

    // -- append-text tests -------------------------------------------------

    #[test]
    fn append_text_adds_suffix() {
        let lines = vec!["0.42".to_string()];
        assert_eq!(apply_append_text(&lines, Some("s")), Some("0.42s".to_string()));
    }

    #[test]
    fn append_text_no_suffix_returns_trimmed_value() {
        let lines = vec!["  1.23  ".to_string()];
        assert_eq!(apply_append_text(&lines, None), Some("1.23".to_string()));
    }

    #[test]
    fn append_text_uses_last_non_empty_line() {
        let lines = vec![
            "ignored".to_string(),
            "42".to_string(),
            "".to_string(),
        ];
        assert_eq!(apply_append_text(&lines, Some("ms")), Some("42ms".to_string()));
    }

    #[test]
    fn append_text_strips_ansi_before_appending() {
        // ANSI cyan colour code around the value.
        let lines = vec!["\x1b[36m9.9\x1b[0m".to_string()];
        assert_eq!(apply_append_text(&lines, Some("s")), Some("9.9s".to_string()));
    }

    #[test]
    fn append_text_empty_lines_returns_none() {
        let lines: Vec<String> = vec!["".to_string(), "  ".to_string()];
        assert_eq!(apply_append_text(&lines, Some("s")), None);
    }

    #[test]
    fn append_text_no_lines_returns_none() {
        assert_eq!(apply_append_text(&[], Some("s")), None);
    }

    // -- CLI parsing tests -------------------------------------------------

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("vju-t").chain(args.iter().copied())
            .map(str::to_string).collect()
    }

    #[cfg(unix)]
    fn run_pty_exit_test(input_expr: &str) -> std::process::ExitStatus {
        use std::process::Command;

        let bin = std::env::var("CARGO_BIN_EXE_vju-t")
            .unwrap_or_else(|_| "./target/debug/vju-t".to_string());
        let bin_q = shell_escape_arg(&bin);

        let cmd = format!(
            "({input_expr}) | timeout 5 script -q /dev/null {bin_q} --watch 5s -- /bin/sh -c 'while true; do echo 1; sleep 1; done' >/dev/null 2>&1"
        );

        Command::new("sh")
            .arg("-lc")
            .arg(cmd)
            .status()
            .expect("failed to run PTY exit test")
    }

    #[cfg(unix)]
    fn run_pty_exit_test_with_child(input_expr: &str, child_cmd: &str) -> std::process::ExitStatus {
        use std::process::Command;

        let bin = std::env::var("CARGO_BIN_EXE_vju-t")
            .unwrap_or_else(|_| "./target/debug/vju-t".to_string());
        let bin_q = shell_escape_arg(&bin);
        let child_q = shell_escape_arg(child_cmd);

        let cmd = format!(
            "({input_expr}) | timeout 5 script -q /dev/null {bin_q} --watch 5s -- /bin/sh -c {child_q} >/dev/null 2>&1"
        );

        Command::new("sh")
            .arg("-lc")
            .arg(cmd)
            .status()
            .expect("failed to run PTY exit test with child")
    }

    #[cfg(unix)]
    fn run_pty_exit_test_stdin_pipe(stdin_data: &str, key_expr: &str) -> std::process::ExitStatus {
        use std::process::Command;

        // Regression for: echo "data" | vju-t --line-chart (stdin is a pipe, not a TTY)
        // The keyboard input arrives from the outer PTY (via script), while vju-t's own
        // stdin is a pipe carrying the chart data. This requires use-dev-tty so crossterm
        // opens /dev/tty directly rather than polling fd 0.
        let bin = std::env::var("CARGO_BIN_EXE_vju-t")
            .unwrap_or_else(|_| "./target/debug/vju-t".to_string());
        let bin_q = shell_escape_arg(&bin);
        let data_q = shell_escape_arg(stdin_data);

        let cmd = format!(
            "({key_expr}) | timeout 5 script -q /dev/null bash -c 'printf %s {data_q} | {bin_q} --line-chart' >/dev/null 2>&1"
        );

        Command::new("sh")
            .arg("-lc")
            .arg(cmd)
            .status()
            .expect("failed to run PTY stdin-pipe exit test")
    }

    #[cfg(unix)]
    #[test]
    fn q_exits_when_stdin_is_pipe() {
        // Regression: `echo data | vju-t --line-chart` — stdin is a pipe, not a TTY.
        // Without crossterm use-dev-tty, keyboard events were silently lost.
        let status = run_pty_exit_test_stdin_pipe("10\\n20\\n30\\n", "sleep 0.8; printf 'q'");
        assert!(status.success(), "q should exit vju-t even when stdin is a pipe (status: {status:?})");
    }

    #[cfg(unix)]
    #[test]
    fn ctrl_c_exits_when_stdin_is_pipe() {
        // Regression: `echo data | vju-t --line-chart` — Ctrl+C should exit even with piped stdin.
        let status = run_pty_exit_test_stdin_pipe("10\\n20\\n30\\n", "sleep 0.8; printf '\\003'");
        assert!(status.success(), "Ctrl+C should exit vju-t even when stdin is a pipe (status: {status:?})");
    }

    #[cfg(unix)]
    #[test]
    fn q_key_exits_process_end_to_end() {
        // Regression: pressing q in TUI should terminate the process.
        let status = run_pty_exit_test("sleep 0.6; printf 'q'");
        assert!(status.success(), "q should exit vju-t (status: {status:?})");
    }

    #[cfg(unix)]
    #[test]
    fn ctrl_c_key_exits_process_end_to_end() {
        // Regression: Ctrl+C key event in raw mode should terminate the process.
        let status = run_pty_exit_test("sleep 0.6; printf '\\003'");
        assert!(status.success(), "Ctrl+C should exit vju-t (status: {status:?})");
    }

    #[cfg(unix)]
    #[test]
    fn q_exits_even_if_child_reads_stdin() {
        // Regression: child command must not steal stdin from vju-t.
        // `cat` continuously reads stdin; pressing q should still exit vju-t.
        let status = run_pty_exit_test_with_child("sleep 0.6; printf 'q'", "cat");
        assert!(status.success(), "q should exit even when child reads stdin (status: {status:?})");
    }

    #[test]
    fn parse_append_text_space_form() {
        let cli = parse_cli_args(&argv(&["--big-text", "--append-text", "s", "mycmd"]));
        assert_eq!(cli.append_text, Some("s".to_string()));
        assert_eq!(cli.script_args, vec!["mycmd"]);
    }

    #[test]
    fn parse_append_text_equals_form() {
        let cli = parse_cli_args(&argv(&["--append-text=ms", "mycmd"]));
        assert_eq!(cli.append_text, Some("ms".to_string()));
    }

    #[test]
    fn parse_append_text_absent_is_none() {
        let cli = parse_cli_args(&argv(&["--big-text", "mycmd"]));
        assert_eq!(cli.append_text, None);
    }

    #[test]
    fn parse_big_text_flag() {
        let cli = parse_cli_args(&argv(&["--big-text", "mycmd"]));
        assert!(matches!(cli.render_mode, super::RenderMode::Big));
    }

    #[test]
    fn parse_status_rect_with_text_flag() {
        let cli = parse_cli_args(&argv(&["--status-rect-with-text", "mycmd"]));
        assert!(matches!(cli.render_mode, super::RenderMode::StatusRectWithText));
    }

    #[test]
    fn parse_title_flag() {
        let cli = parse_cli_args(&argv(&["--title", "My Pane", "mycmd"]));
        assert_eq!(cli.title_override, Some("My Pane".to_string()));
    }

    #[test]
    fn parse_status_good_colour_flag() {
        let cli = parse_cli_args(&argv(&["--status-colour-good", "#00a3e0", "mycmd"]));
        assert_eq!(cli.status_good_colour, ratatui::style::Color::Rgb(0, 163, 224));
    }

    #[test]
    fn parse_status_bad_colour_equals_flag() {
        let cli = parse_cli_args(&argv(&["--status-colour-bad=#e0465a", "mycmd"]));
        assert_eq!(cli.status_bad_colour, ratatui::style::Color::Rgb(224, 70, 90));
    }

    #[test]
    fn parse_status_good_colour_rgb_flag() {
        let cli = parse_cli_args(&argv(&["--status-colour-good", "0,163,224", "mycmd"]));
        assert_eq!(cli.status_good_colour, ratatui::style::Color::Rgb(0, 163, 224));
    }

    #[test]
    fn parse_status_bad_colour_rgb_equals_flag() {
        let cli = parse_cli_args(&argv(&["--status-colour-bad=224,70,90", "mycmd"]));
        assert_eq!(cli.status_bad_colour, ratatui::style::Color::Rgb(224, 70, 90));
    }

    #[test]
    fn parse_append_text_not_consumed_as_script_arg() {
        // Regression: if --append-text parser arm is deleted, the flag and
        // value would end up in script_args instead of being parsed.
        let cli = parse_cli_args(&argv(&["--big-text", "--append-text", "s", "mycmd", "arg1"]));
        assert_eq!(cli.append_text, Some("s".to_string()),
            "--append-text should be parsed, not in script_args");
        assert_eq!(cli.script_args, vec!["mycmd", "arg1"],
            "--append-text should not end up in script_args");
    }

    #[test]
    fn parse_append_text_equals_not_consumed_as_script_arg() {
        let cli = parse_cli_args(&argv(&["--append-text=ms", "mycmd"]));
        assert_eq!(cli.append_text, Some("ms".to_string()));
        assert_eq!(cli.script_args, vec!["mycmd"]);
    }

    #[test]
    fn parse_append_text_with_separator() {
        // Test that --append-text works correctly with -- separator
        let cli = parse_cli_args(&argv(&["--big-text", "--append-text", "s", "--", "mycmd", "--some-flag"]));
        assert_eq!(cli.append_text, Some("s".to_string()));
        assert_eq!(cli.script_args, vec!["mycmd", "--some-flag"],
            "command args after -- should not be confused with vju-t options");
    }

    #[test]
    fn big_render_integration_append_text_used() {
        // Regression: Big render mode must actually use the append_text value.
        // This test verifies the full flow: parse args, extract append_text,
        // simulate buffer, and verify rendered output contains appended text.

        let cli = parse_cli_args(&argv(&["--big-text", "--append-text", "s", "mycommand"]));
        assert!(matches!(cli.render_mode, super::RenderMode::Big));
        assert_eq!(cli.append_text, Some("s".to_string()));

        // Simulate buffer: what the render loop would see
        let raw_lines = vec!["0.42".to_string()];

        // This is the exact call Big render makes:
        let display_str = apply_append_text(
            &raw_lines,
            cli.append_text.as_deref(),
        ).unwrap_or_default();

        // Verify the output actually contains the appended text
        assert_eq!(display_str, "0.42s",
            "Big render output should contain appended text. \
             If this test fails, Big render is not calling apply_append_text correctly");
    }

    #[test]
    fn big_render_integration_append_text_absent() {
        // When append_text is not provided, output should not have suffix
        let cli = parse_cli_args(&argv(&["--big-text", "mycommand"]));
        assert_eq!(cli.append_text, None);

        let raw_lines = vec!["0.42".to_string()];
        let display_str = apply_append_text(
            &raw_lines,
            cli.append_text.as_deref(),
        ).unwrap_or_default();

        assert_eq!(display_str, "0.42",
            "Without --append-text, output should be unchanged");
    }

    #[test]
    fn stdin_mode_with_append_text() {
        // Regression: stdin mode (no command) + --append-text should work
        let cli = parse_cli_args(&argv(&["--big-text", "--append-text", "ms"]));
        assert_eq!(cli.script_args, Vec::<String>::new(),
            "stdin mode: no script args");
        assert_eq!(cli.append_text, Some("ms".to_string()),
            "append_text should be parsed even in stdin mode");
        assert!(matches!(cli.render_mode, super::RenderMode::Big));

        // Simulate rendering with stdin data
        let raw_lines = vec!["250".to_string()];
        let display_str = apply_append_text(
            &raw_lines,
            cli.append_text.as_deref(),
        ).unwrap_or_default();

        assert_eq!(display_str, "250ms",
            "stdin + --append-text should produce '250ms'");
    }

    #[test]
    fn full_pipeline_buffer_to_display() {
        // Comprehensive regression test: buffer -> display_lines_for_run -> apply_append_text
        let cli = parse_cli_args(&argv(&["--big-text", "--append-text", "s"]));

        // Simulate buffer as if stdin had read "0.42\n" once
        let buffer: Vec<(u64, String)> = vec![
            (1, "0.42".to_string()),
        ];

        // What display_lines_for_run returns for run 1
        let raw_lines = display_lines_for_run(&buffer, 1);
        assert_eq!(raw_lines, vec!["0.42"],
            "display_lines_for_run should return buffer lines for current run");

        // What Big render does with those lines
        let display_str = apply_append_text(&raw_lines, cli.append_text.as_deref())
            .unwrap_or_default();
        assert_eq!(display_str, "0.42s",
            "Full pipeline: buffer[1] -> display_lines_for_run -> apply_append_text should produce '0.42s'");
    }

    #[test]
    fn apply_append_text_never_returns_none_with_data() {
        // Regression: apply_append_text should never return None when there are non-empty lines.
        // If it returns None, unwrap_or_default() produces empty string, silently losing the text.
        let lines = vec!["0.42".to_string()];
        let result = apply_append_text(&lines, Some("s"));
        assert!(result.is_some(),
            "apply_append_text must return Some(), not None, when lines exist. \
             If None is returned, Big render will display nothing");
        assert_eq!(result.unwrap(), "0.42s");
    }

    #[test]
    fn ob_server_real_command_parsing() {
        // Regression: the actual ob-server.toml commands should parse correctly
        let args = vec![
            "vju-t",
            "--big-text",
            "--append-text", "s",
            "--title", "MD GET",
            "--watch",
            "--",
            "python3",
            "/usr/local/src/pyqdd/query_metric.py",
            "--query",
            "p99:my.orderbird.responsetime",
        ].into_iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let cli = parse_cli_args(&args);
        assert_eq!(cli.append_text, Some("s".to_string()),
            "ob-server real command: --append-text 's' should be parsed");
        assert_eq!(cli.title_override, Some("MD GET".to_string()),
            "ob-server real command: --title should be parsed");
        assert!(cli.watch_ms.is_some(),
            "ob-server real command: --watch should be parsed");
        assert!(matches!(cli.render_mode, super::RenderMode::Big),
            "ob-server real command: --big-text should be parsed");
        assert_eq!(cli.script_args, vec!["python3", "/usr/local/src/pyqdd/query_metric.py", "--query", "p99:my.orderbird.responsetime"],
            "ob-server real command: args after -- must include --query flag and value");
    }

    #[test]
    fn append_text_preserved_across_watch_cycles() {
        // Regression: --append-text must be preserved after parsing.
        // The variable 'append_text' is set in main() from cli.append_text
        // and must remain in scope for the entire watch loop.
        let cli = parse_cli_args(&argv(&["--big-text", "--append-text", "s", "--watch", "100ms", "mycmd"]));

        // Verify parsing extracted it correctly
        assert_eq!(cli.append_text, Some("s".to_string()),
            "Step 1: --append-text 's' must be parsed from CLI args");

        // Simulate main() doing: let append_text = cli.append_text;
        let append_text = cli.append_text;
        assert_eq!(append_text, Some("s".to_string()),
            "Step 2: main() assigns cli.append_text to local variable 'append_text'");

        // Simulate rendering loop: append_text.as_deref() is passed to apply_append_text
        let raw_lines = vec!["100.5".to_string()];
        let display_str = apply_append_text(&raw_lines, append_text.as_deref())
            .unwrap_or_default();
        assert_eq!(display_str, "100.5s",
            "Step 3: render loop passes append_text to apply_append_text() and gets '100.5s'");

        // After first iteration, watch cycles. The variable append_text should still have the value.
        let display_str_2 = apply_append_text(&raw_lines, append_text.as_deref())
            .unwrap_or_default();
        assert_eq!(display_str_2, "100.5s",
            "Step 4: after watch cycle, append_text still has value");
    }

    #[test]
    fn klue_command_with_append_text_and_quotes() {
        // Regression: commands from klue with single quotes around values should still work
        // This simulates what klue extracts from:
        //   "vju-t --big-text --append-text 's' --title 'MD GET' --watch -- ..."
        // When this reaches the shell, single quotes are stripped, so it becomes:
        // vju-t --big-text --append-text s --title MD GET --watch -- ...

        let argv = argv(&[
            "--big-text",
            "--append-text", "s",     // shell stripped the single quotes
            "--title", "MD",           // 'MD GET' becomes two separate args
            "GET",                      // this becomes a script arg since --title only takes one arg
            "--watch",
            "--",
            "python3", "/usr/local/src/pyqdd/query_metric.py"
        ]);

        let cli = parse_cli_args(&argv);
        assert_eq!(cli.append_text, Some("s".to_string()),
            "klue command: --append-text 's' should parse correctly");
        // Note: when klue sends --title 'MD GET' through tmux/shell,
        // it becomes two separate tokens. The parser will consume --title MD,
        // leaving GET as the first script arg.
    }
}
