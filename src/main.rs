use std::{
    env,
    error::Error,
    fs,
    io::{self, stdout},
    path::{Path, PathBuf},
    time::Duration,
};

use arboard::Clipboard; // クリップボード用
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
// pulldown_cmarkからhtmlモジュールをインポート
use pulldown_cmark::{html, Options, Parser as MarkdownParser};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use opener;

// --- 配色テーマ定義 ---
struct ColorScheme {
    bg: Color,
    fg: Color,
    selection_bg: Color,
    selection_fg: Color,
    comment: Color,
    link: Color,
}

const GITHUB_DARK_THEME: ColorScheme = ColorScheme {
    bg: Color::Rgb(13, 17, 23),          // #0d1117
    fg: Color::Rgb(201, 209, 217),       // #c9d1d9
    selection_bg: Color::Rgb(3, 34, 82), // A selection color
    selection_fg: Color::Rgb(201, 209, 217),
    comment: Color::Rgb(139, 148, 158),  // #8b949e
    link: Color::Rgb(88, 166, 255),      // #58a6ff
};

// --- アプリケーションの状態管理 ---

enum AppMode {
    Explorer,
    Preview,
}

struct ExplorerState {
    current_path: PathBuf,
    entries: Vec<PathBuf>,
    list_state: ListState,
    status_message: Option<String>, // エラーまたは成功メッセージ
    is_error: bool,                 // メッセージがエラーかどうか
    command_input: String,
    in_command_mode: bool,
}

impl ExplorerState {
    fn new() -> io::Result<Self> {
        let mut state = Self {
            current_path: env::current_dir()?,
            entries: Vec::new(),
            list_state: ListState::default(),
            status_message: None,
            is_error: false,
            command_input: String::new(),
            in_command_mode: false,
        };
        state.load_entries()?;
        Ok(state)
    }

    /// ディレクトリ読み込み時にカーソル位置を必ずリセットする
    fn load_entries(&mut self) -> io::Result<()> {
        let mut entries = fs::read_dir(&self.current_path)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();

        entries.sort_by(|a, b| {
            let a_is_dir = a.is_dir();
            let b_is_dir = b.is_dir();
            a_is_dir.cmp(&b_is_dir).reverse().then_with(|| a.cmp(b))
        });

        self.entries = entries;

        if !self.entries.is_empty() {
            self.list_state.select(Some(0));
        } else {
            self.list_state.select(None);
        }
        Ok(())
    }

    fn next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let i = self.list_state.selected().map_or(0, |i| {
            if i >= self.entries.len() - 1 {
                0
            } else {
                i + 1
            }
        });
        self.list_state.select(Some(i));
    }

    fn previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let i = self.list_state.selected().map_or(0, |i| {
            if i == 0 {
                self.entries.len() - 1
            } else {
                i - 1
            }
        });
        self.list_state.select(Some(i));
    }

    fn set_message(&mut self, message: String, is_error: bool) {
        self.status_message = Some(message);
        self.is_error = is_error;
    }

    fn clear_message(&mut self) {
        self.status_message = None;
        self.is_error = false;
    }
}

struct PreviewState {
    content: Text<'static>,
    original_text: String, // コピー用に原文を保持
    scroll: u16,
    title: String,
    char_count: usize,
    status_message: Option<String>, // "Copied!" などの一時メッセージ
    clipboard: Option<Clipboard>,   // Clipboardインスタンスを保持して早期Dropを防ぐ
}

impl PreviewState {
    // プレーンテキスト表示用
    fn new_text(file_path: &Path, content_str: String, theme: &ColorScheme) -> Self {
        let char_count = content_str.chars().count();
        let content = Text::styled(content_str.clone(), Style::default().fg(theme.fg));

        // Clipboardの初期化をここで行い、インスタンスを保持する
        let clipboard = Clipboard::new().ok();

        Self {
            content,
            original_text: content_str,
            scroll: 0,
            title: file_path.to_string_lossy().to_string(),
            char_count,
            status_message: None,
            clipboard,
        }
    }

    // HTMLソース表示用（簡易ハイライト付き）
    fn new_html(file_path: &Path, html_source: String, theme: &ColorScheme) -> Self {
        let char_count = html_source.chars().count();
        // ハイライト処理
        let content = highlight_html(&html_source, theme);

        // Clipboardの初期化をここで行い、インスタンスを保持する
        let clipboard = Clipboard::new().ok();

        Self {
            content,
            original_text: html_source,
            scroll: 0,
            title: file_path.to_string_lossy().to_string(),
            char_count,
            status_message: None,
            clipboard,
        }
    }

    fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    fn scroll_down(&mut self) {
        let max_scroll = self.content.height().saturating_sub(1) as u16;
        if self.scroll < max_scroll {
            self.scroll = self.scroll.saturating_add(1);
        }
    }

    fn copy_to_clipboard(&mut self) {
        // 保持しているインスタンスを使用する
        // インスタンスがない場合（初期化失敗時など）は再作成を試みる
        if self.clipboard.is_none() {
            self.clipboard = Clipboard::new().ok();
        }

        if let Some(clipboard) = &mut self.clipboard {
            if let Err(e) = clipboard.set_text(&self.original_text) {
                self.status_message = Some(format!("Copy failed: {}", e));
            } else {
                // 成功時はメッセージを表示しない
                self.status_message = None;
            }
        } else {
            self.status_message = Some("Clipboard not available".to_string());
        }
    }
}

// 簡易HTMLハイライト関数
fn highlight_html(html_source: &str, theme: &ColorScheme) -> Text<'static> {
    let mut lines = Vec::new();

    for line in html_source.lines() {
        let mut spans = Vec::new();
        let mut current_text = String::new();
        let mut in_tag = false;

        for c in line.chars() {
            if c == '<' {
                // タグ開始前までのテキストをプッシュ
                if !current_text.is_empty() {
                    spans.push(Span::styled(
                        current_text.clone(),
                        Style::default().fg(if in_tag { theme.comment } else { theme.fg }),
                    ));
                    current_text.clear();
                }
                in_tag = true;
                current_text.push(c);
            } else if c == '>' {
                // タグ終了
                current_text.push(c);
                spans.push(Span::styled(
                    current_text.clone(),
                    Style::default().fg(theme.comment), // タグの色（コメント色と同じにして目立たなくする）
                ));
                current_text.clear();
                in_tag = false;
            } else {
                current_text.push(c);
            }
        }
        
        // 行末に残ったテキストをプッシュ
        if !current_text.is_empty() {
             spans.push(Span::styled(
                current_text,
                Style::default().fg(if in_tag { theme.comment } else { theme.fg }),
            ));
        }

        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

// --- メインロジック ---

fn main() -> Result<(), Box<dyn Error>> {
    // TUIモードの起動
    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal);
    restore_terminal()?;

    if let Err(err) = result {
        // "quit"エラーはユーザーによる正常終了なので、エラーメッセージは表示しない
        if err.to_string() != "quit" {
            println!("エラーが発生しました: {}", err);
        }
    }
    Ok(())
}

fn run<B: Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    let mut mode = AppMode::Explorer;
    let mut explorer_state = ExplorerState::new()?;
    let mut preview_state: Option<PreviewState> = None;
    let theme = &GITHUB_DARK_THEME;

    loop {
        terminal.draw(|f| match mode {
            AppMode::Explorer => ui_explorer(f, &mut explorer_state, theme),
            AppMode::Preview => {
                if let Some(state) = &mut preview_state {
                    ui_preview(f, state, theme);
                }
            }
        })?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match mode {
                AppMode::Preview => {
                    if let Some(state) = &mut preview_state {
                        match key.code {
                            KeyCode::Char('q') => {
                                preview_state = None;
                                mode = AppMode::Explorer;
                            }
                            KeyCode::Up | KeyCode::Char('k') => state.scroll_up(),
                            KeyCode::Down | KeyCode::Char('j') => state.scroll_down(),
                            KeyCode::Char('y') => state.copy_to_clipboard(), // 'y'でコピー
                            _ => {}
                        }
                    }
                }
                AppMode::Explorer => {
                    if explorer_state.in_command_mode {
                        match key.code {
                            KeyCode::Enter => {
                                let command_text = explorer_state.command_input.trim().to_string();
                                explorer_state.command_input.clear();
                                explorer_state.in_command_mode = false;
                                explorer_state.clear_message();

                                let parts: Vec<&str> = command_text.split_whitespace().collect();

                                match parts.as_slice() {
                                    ["q"] => {
                                        return Err(io::Error::new(io::ErrorKind::Other, "quit"));
                                    }
                                    // :hp コマンドは削除されました
                                    ["cat", filename] => {
                                        let file_path = explorer_state.current_path.join(filename);
                                        if !file_path.is_file() {
                                            explorer_state.set_message(
                                                format!("ファイルが見つかりません: {}", filename),
                                                true,
                                            );
                                            continue;
                                        }

                                        match fs::read_to_string(&file_path) {
                                            Ok(file_content) => {
                                                preview_state = Some(PreviewState::new_text(
                                                    &file_path,
                                                    file_content,
                                                    theme,
                                                ));
                                                mode = AppMode::Preview;
                                            }
                                            Err(e) => {
                                                explorer_state.set_message(
                                                    format!("ファイル読み込みエラー: {}", e),
                                                    true,
                                                );
                                            }
                                        }
                                    }
                                    ["ob", filename] => {
                                        let file_path = explorer_state.current_path.join(filename);

                                        // ファイルの存在と拡張子をチェック
                                        if !file_path.is_file() {
                                            explorer_state.set_message(
                                                format!("ファイルが見つかりません: {}", filename),
                                                true,
                                            );
                                        } else if file_path.extension().and_then(|s| s.to_str())
                                            != Some("html")
                                        {
                                            explorer_state.set_message(
                                                "HTMLファイルのみ開けます。".to_string(),
                                                true,
                                            );
                                        } else {
                                            // ブラウザで開く
                                            if let Err(e) = opener::open(&file_path) {
                                                explorer_state.set_message(
                                                    format!("ブラウザで開けませんでした: {}", e),
                                                    true,
                                                );
                                            } else {
                                                explorer_state.set_message(
                                                    format!("ブラウザで開きました: {}", filename),
                                                    false,
                                                );
                                            }
                                        }
                                    }
                                    [] => {} // 空のコマンドは無視
                                    _ => {
                                        explorer_state.set_message(
                                            format!("不明なコマンドです: {}", command_text),
                                            true,
                                        );
                                    }
                                }
                            }
                            KeyCode::Char(c) => explorer_state.command_input.push(c),
                            KeyCode::Backspace => {
                                explorer_state.command_input.pop();
                            }
                            KeyCode::Esc => {
                                explorer_state.command_input.clear();
                                explorer_state.in_command_mode = false;
                            }
                            _ => {}
                        }
                    } else {
                        explorer_state.clear_message(); // 操作時にメッセージをクリア
                        match key.code {
                            KeyCode::Char(':') => {
                                explorer_state.in_command_mode = true;
                            }
                            KeyCode::Down | KeyCode::Char('j') => explorer_state.next(),
                            KeyCode::Up | KeyCode::Char('k') => explorer_state.previous(),
                            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                                if let Some(parent) = explorer_state.current_path.parent() {
                                    explorer_state.current_path = parent.to_path_buf();
                                    explorer_state.load_entries()?;
                                }
                            }
                            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                                if let Some(selected_index) = explorer_state.list_state.selected() {
                                    if let Some(selected_path) =
                                        explorer_state.entries.get(selected_index)
                                    {
                                        let selected_path = selected_path.clone();
                                        if selected_path.is_dir() {
                                            // ディレクトリなら移動
                                            explorer_state.current_path =
                                                dunce::canonicalize(selected_path)?;
                                            explorer_state.load_entries()?;
                                        } else {
                                            // ファイルの場合
                                            if selected_path.extension().and_then(|s| s.to_str())
                                                == Some("md")
                                            {
                                                // .mdファイルならHTMLに変換してプレビュー画面で表示する
                                                match fs::read_to_string(&selected_path) {
                                                    Ok(markdown_input) => {
                                                        let parser = MarkdownParser::new_ext(
                                                            &markdown_input,
                                                            Options::all(),
                                                        );
                                                        let mut html_output = String::new();
                                                        html::push_html(&mut html_output, parser);

                                                        preview_state =
                                                            Some(PreviewState::new_html(
                                                                &selected_path,
                                                                html_output,
                                                                theme,
                                                            ));
                                                        mode = AppMode::Preview;
                                                    }
                                                    Err(e) => {
                                                        explorer_state.set_message(
                                                            format!("ファイル読み込みエラー: {}", e),
                                                            true,
                                                        );
                                                    }
                                                }
                                            } else {
                                                // .md以外のファイルはプレーンテキストとして開く
                                                match fs::read_to_string(&selected_path) {
                                                    Ok(file_content) => {
                                                        preview_state = Some(PreviewState::new_text(
                                                            &selected_path,
                                                            file_content,
                                                            theme,
                                                        ));
                                                        mode = AppMode::Preview;
                                                    }
                                                    Err(e) => {
                                                        explorer_state.set_message(
                                                            format!("ファイル読み込みエラー: {}", e),
                                                            true,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

// UI描画

fn ui_explorer(f: &mut Frame, state: &mut ExplorerState, theme: &ColorScheme) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(2)].as_ref())
        .split(f.size());

    let items: Vec<ListItem> = state
        .entries
        .iter()
        .map(|path| {
            let file_name = path
                .file_name()
                .map_or_else(|| "..".into(), |s| s.to_string_lossy());

            let display_name = if path.is_dir() {
                format!("{}/", file_name)
            } else {
                file_name.to_string()
            };

            let style = if path.is_dir() {
                Style::default().fg(theme.link)
            } else {
                Style::default().fg(theme.fg)
            };
            ListItem::new(Span::styled(display_name, style))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(state.current_path.to_string_lossy().to_string())
                .style(Style::default().fg(theme.fg).bg(theme.bg)),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    f.render_stateful_widget(list, chunks[0], &mut state.list_state);

    let status_bar_style = Style::default().fg(theme.fg).bg(theme.bg);
    let status_text = if state.in_command_mode {
        format!(":{}", state.command_input)
    } else if let Some(msg) = &state.status_message {
        msg.clone()
    } else {
        "j/k: Move | Enter: View HTML Source | :<cmd>: Command (:cat, :ob, :q)".to_string()
    };
    
    let status_color = if state.is_error {
        Color::Red
    } else if state.status_message.is_some() {
        Color::Green // 成功メッセージなどは緑などにする
    } else {
        theme.fg
    };

    let status_bar = Paragraph::new(status_text).style(status_bar_style.fg(status_color));

    f.render_widget(status_bar, chunks[1]);
}

fn ui_preview(f: &mut Frame, state: &mut PreviewState, theme: &ColorScheme) {
    // Create a layout with a main area and a footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),    // Main content
            Constraint::Length(1), // Footer
        ])
        .split(f.size());

    // Main content paragraph without a block/border
    let paragraph = Paragraph::new(state.content.clone())
        .style(Style::default().fg(theme.fg).bg(theme.bg))
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));
    f.render_widget(paragraph, chunks[0]);

    // Footer
    let msg = state.status_message.as_deref().unwrap_or("Press 'q' to close | 'y' to copy");
    let footer_text = format!(
        "{} | {} chars | {}",
        state.title, state.char_count, msg
    );
    let footer = Paragraph::new(footer_text)
        .style(Style::default().fg(theme.comment).bg(theme.bg))
        .alignment(Alignment::Right);
    f.render_widget(footer, chunks[1]);
}

// --- ターミナル設定 ---
fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, Box<dyn Error>> {
    let mut stdout = stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal() -> Result<(), Box<dyn Error>> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}
