use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::config::BastionSetting;
use crate::i18n::TuiMsg;
use crate::t;

use super::{App, AppState, CompletionState};
use crate::completion::CompletionKind;

impl App {
    /// パンくずリストの行を生成する
    ///
    /// 接続名 > DB名 > テーブル名 の形式で現在のナビゲーション位置を示す。
    /// 接続名が設定されていない場合（接続前）は None を返す。
    fn breadcrumb_line(&self) -> Option<Line<'static>> {
        let conn_name = self.connection_name.as_ref()?.clone();

        let separator_style = Style::default().fg(Color::DarkGray);
        let name_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let db_style = Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD);
        let table_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);

        let mut spans = vec![Span::styled(conn_name, name_style)];

        if let Some(ref db) = self.current_database {
            spans.push(Span::styled(" > ", separator_style));
            spans.push(Span::styled(db.clone(), db_style));

            if let Some(ref table) = self.current_table {
                spans.push(Span::styled(" > ", separator_style));
                spans.push(Span::styled(table.clone(), table_style));
            }
        }

        Some(Line::from(spans))
    }

    /// 画面描画
    pub(super) fn render(&self, frame: &mut Frame) {
        let size = frame.area();

        // 背景をクリア（画面遷移時のゴミを除去）
        frame.render_widget(Clear, size);

        match &self.state {
            AppState::Selecting {
                connections,
                selected_index,
            } => self.render_selecting(frame, size, connections, *selected_index),
            AppState::Connecting { connection_name, spinner_frame, .. } => {
                self.render_connecting(frame, size, connection_name, *spinner_frame)
            }
            AppState::Connected { .. } => self.render_connected(frame, size),
            AppState::Executing { query } => self.render_executing(frame, size, query),
            // ストリーミング待ち中はExecutingと同じ表示
            AppState::StreamingQuery { sql, .. } => self.render_executing(frame, size, sql),
            // カラム選択中はExecutingと同じ表示
            AppState::SelectingColumns { .. } => {
                self.render_executing(frame, size, &t!(TuiMsg::ColumnSelecting))
            }
            // ShowingResultはskimへの即遷移トリガーのみで使われるため、描画は不要
            AppState::ShowingResult { .. } => {}
            AppState::Error { message, .. } => self.render_error(frame, size, message),
        }
    }

    /// 接続先選択画面
    pub(super) fn render_selecting(
        &self,
        frame: &mut Frame,
        area: Rect,
        connections: &[crate::config::ConnectionConfig],
        selected_index: usize,
    ) {
        use ratatui::widgets::{List, ListItem};

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(area);

        // 接続先リスト
        let items: Vec<ListItem> = connections
            .iter()
            .enumerate()
            .map(|(i, conn)| {
                let bastion_info = if let Some(BastionSetting::Config(ref bastion)) = conn.bastion {
                    format!(" (via {}@{})", bastion.user, bastion.host)
                } else {
                    String::new()
                };

                let display = format!(
                    "{} - {}@{}:{}{}",
                    conn.name, conn.mysql.user, conn.mysql.host, conn.mysql.port, bastion_info
                );

                let style = if i == selected_index {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                ListItem::new(display).style(style)
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(t!(TuiMsg::SelectingTitle)),
        );

        frame.render_widget(list, chunks[0]);

        // ヘルプ
        let help_text = t!(TuiMsg::SelectingHelp);
        let help = Paragraph::new(help_text).style(Style::default().fg(Color::Gray));

        frame.render_widget(help, chunks[1]);
    }

    /// 接続中画面（スピナー表示）
    pub(super) fn render_connecting(
        &self,
        frame: &mut Frame,
        area: Rect,
        connection_name: &str,
        spinner_frame: u8,
    ) {
        const SPINNER_FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
        let spinner = SPINNER_FRAMES[(spinner_frame as usize) % SPINNER_FRAMES.len()];

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(area);

        let message = format!(
            " {} {}",
            spinner,
            t!(TuiMsg::ConnectingMessage { connection_name })
        );
        let paragraph = Paragraph::new(message)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(t!(TuiMsg::ConnectingTitle)),
            )
            .style(Style::default().fg(Color::Cyan));

        frame.render_widget(paragraph, chunks[0]);

        let help = Paragraph::new("Ctrl+C: quit")
            .style(Style::default().fg(Color::Gray));
        frame.render_widget(help, chunks[1]);
    }

    /// 接続済み画面（SQL入力）
    pub(super) fn render_connected(&self, frame: &mut Frame, area: Rect) {
        let has_record = self.selected_record.is_some();

        let chunks = if has_record {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),  // パンくずリスト
                    Constraint::Length(5),  // SQL入力エリア
                    Constraint::Min(5),     // 選択レコードプレビュー
                    Constraint::Length(3),  // ヘルプ
                ])
                .split(area)
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),  // パンくずリスト
                    Constraint::Length(7),  // SQL入力エリア
                    Constraint::Min(5),     // 接続情報・説明エリア
                    Constraint::Length(3),  // ヘルプ
                ])
                .split(area)
        };

        // パンくずリストを描画する
        if let Some(breadcrumb) = self.breadcrumb_line() {
            let breadcrumb_paragraph = Paragraph::new(breadcrumb);
            frame.render_widget(breadcrumb_paragraph, chunks[0]);
        }

        // SQL入力エリア（選択範囲がある場合はハイライト表示）
        let input_line = if let Some(sel_start) = self.selection_start {
            let start = sel_start.min(self.cursor_position);
            let end = sel_start.max(self.cursor_position);
            let byte_start = self.char_to_byte(start);
            let byte_end = self.char_to_byte(end);
            let before = &self.query_input[..byte_start];
            let selected = &self.query_input[byte_start..byte_end];
            let after = &self.query_input[byte_end..];
            Line::from(vec![
                Span::raw(before),
                Span::styled(
                    selected,
                    Style::default().bg(Color::Blue).fg(Color::White),
                ),
                Span::raw(after),
            ])
        } else {
            Line::from(self.query_input.as_str())
        };
        let input_text = Text::from(input_line);
        // readonlyモード時はタイトルの [READONLY] 部分を赤色+太字で目立たせる
        let input_title = if self.is_current_readonly() {
            Line::from(vec![
                Span::raw(format!("{} ", t!(TuiMsg::SqlInputTitle))),
                Span::styled(
                    t!(TuiMsg::SqlInputReadonlyLabel),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" {}", t!(TuiMsg::SqlInputTitleSuffix))),
            ])
        } else {
            Line::from(format!(
                "{} {}",
                t!(TuiMsg::SqlInputTitle),
                t!(TuiMsg::SqlInputTitleSuffix)
            ))
        };
        let input_paragraph = Paragraph::new(input_text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(input_title),
        );

        frame.render_widget(input_paragraph, chunks[1]);

        // カーソルを表示（入力エリア内の適切な位置）
        // cursor_positionはchar単位なので、表示幅（セル数）に変換してオフセットを計算する
        let cursor_display_offset: u16 = self
            .query_input
            .chars()
            .take(self.cursor_position)
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1) as u16)
            .sum();
        let cursor_x = chunks[1].x + 1 + cursor_display_offset;
        let cursor_y = chunks[1].y + 1;
        frame.set_cursor_position(ratatui::layout::Position { x: cursor_x, y: cursor_y });

        // 接続情報 or 選択レコードプレビュー
        let manager = match &self.state {
            AppState::Connected { manager } => manager,
            _ => {
                let empty = Paragraph::new("");
                frame.render_widget(empty, chunks[2]);
                let help = Paragraph::new(t!(TuiMsg::QueryHelp))
                    .style(Style::default().fg(Color::Gray));
                frame.render_widget(help, chunks[3]);
                return;
            }
        };

        if let Some(ref record) = self.selected_record {
            // 選択レコードプレビュー表示
            let mut preview_lines = Vec::new();
            for (col, val) in &record.columns {
                preview_lines.push(format!("{}: {}", col, val));
            }
            let preview_text = preview_lines.join("\n");

            let preview_paragraph = Paragraph::new(preview_text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(t!(TuiMsg::SelectedRecordTitle))
                        .style(Style::default().fg(Color::Green)),
                )
                .style(Style::default().fg(Color::White));

            frame.render_widget(preview_paragraph, chunks[2]);
        } else {
            // 通常の接続情報表示
            let conn_config = manager.config();
            let mut info_lines = format!(
                "{}: {}\n{}: {}:{}\n{}: {}",
                t!(TuiMsg::ConnectionTarget),
                conn_config.name,
                t!(TuiMsg::Host),
                conn_config.mysql.host,
                conn_config.mysql.port,
                t!(TuiMsg::Database),
                conn_config.mysql.database
            );
            // USEコマンドで切り替えた場合のみ選択データベースを追加表示する
            if let Some(ref db) = self.current_database {
                info_lines.push_str(&format!("\n{}: {}", t!(TuiMsg::SelectedDatabase), db));
            }
            info_lines.push_str(&format!("\n\n{}", t!(TuiMsg::SqlInputHint)));

            let info_paragraph = Paragraph::new(info_lines)
                .block(Block::default().borders(Borders::ALL).title(t!(TuiMsg::ConnectionInfo)))
                .style(Style::default().fg(Color::Cyan));

            frame.render_widget(info_paragraph, chunks[2]);
        }

        // ヘルプ
        let help_text = t!(TuiMsg::ConnectedHelp);
        let help = Paragraph::new(help_text).style(Style::default().fg(Color::Gray));

        frame.render_widget(help, chunks[3]);

        // 補完ポップアップを最後（最上層）に描画する
        // chunks[1] はSQL入力エリア（chunks[0] はパンくずリスト）
        if let Some(ref comp_state) = self.completion_state {
            if !comp_state.candidates.is_empty() {
                let popup_rect = completion_popup_rect(
                    chunks[1],
                    cursor_display_offset,
                    comp_state.candidates.len(),
                    frame.area(),
                );
                render_completion_popup(frame, popup_rect, comp_state);
            }
        }
    }

    /// クエリ実行中画面
    pub(super) fn render_executing(&self, frame: &mut Frame, area: Rect, query: &str) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),  // パンくずリスト
                Constraint::Length(5),  // クエリ表示
                Constraint::Min(3),     // ステータス
            ])
            .split(area);

        // パンくずリストを描画する
        if let Some(breadcrumb) = self.breadcrumb_line() {
            let breadcrumb_paragraph = Paragraph::new(breadcrumb);
            frame.render_widget(breadcrumb_paragraph, chunks[0]);
        }

        // 実行中のクエリを表示
        let query_paragraph = Paragraph::new(query).block(
            Block::default()
                .borders(Borders::ALL)
                .title(t!(TuiMsg::ExecutingQueryTitle)),
        );

        frame.render_widget(query_paragraph, chunks[1]);

        // 実行中表示
        let text = Text::from(t!(TuiMsg::ExecutingMessage));
        let paragraph = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(t!(TuiMsg::StatusTitle)))
            .style(Style::default().fg(Color::Yellow));

        frame.render_widget(paragraph, chunks[2]);
    }

    /// エラー表示画面
    pub(super) fn render_error(&self, frame: &mut Frame, area: Rect, message: &str) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),  // パンくずリスト
                Constraint::Min(5),     // エラーメッセージ
                Constraint::Length(3),  // ヘルプ
            ])
            .split(area);

        // パンくずリストを描画する
        if let Some(breadcrumb) = self.breadcrumb_line() {
            let breadcrumb_paragraph = Paragraph::new(breadcrumb);
            frame.render_widget(breadcrumb_paragraph, chunks[0]);
        }

        // エラーメッセージ
        let error_text = Text::from(message);
        let error_paragraph = Paragraph::new(error_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(t!(TuiMsg::ErrorTitle))
                    .style(Style::default().fg(Color::Red)),
            )
            .style(Style::default().fg(Color::Red));

        frame.render_widget(error_paragraph, chunks[1]);

        // ヘルプ
        let help_text = t!(TuiMsg::ErrorHelp);
        let help = Paragraph::new(help_text).style(Style::default().fg(Color::Gray));

        frame.render_widget(help, chunks[2]);
    }
}

/// 補完ポップアップ用の描画領域を計算する
///
/// カーソル位置の右下（入力行の1行下、カーソルのx位置）に表示する。
/// Clearウィジェットが背景を上書きするため、入力エリア外にはみ出しても問題ない。
/// 候補数が多い場合は最大 MAX_POPUP_HEIGHT 行に制限する。
pub(super) fn completion_popup_rect(
    input_area: Rect,
    cursor_x_offset: u16,
    candidate_count: usize,
    terminal_area: Rect,
) -> Rect {
    const MAX_POPUP_HEIGHT: u16 = 8;
    const MAX_POPUP_WIDTH: u16 = 40;

    // Borders::ALL による上枠・下枠の2行分を加算して内側に候補が表示されるようにする
    let popup_height = (candidate_count as u16 + 2).min(MAX_POPUP_HEIGHT);
    // カーソル行(input_area.y + 1)の1行下に表示する
    let popup_y = input_area.y + 2;
    // カーソルのx位置（枠の内側1セル + カーソルオフセット）に揃える
    let popup_x = input_area.x + 1 + cursor_x_offset;

    // 画面下端を超えないように調整（入力欄の上に移動）
    let popup_y = if popup_y + popup_height > terminal_area.height {
        input_area.y.saturating_sub(popup_height)
    } else {
        popup_y
    };

    // 画面右端を超えないように調整
    let popup_x = if popup_x + MAX_POPUP_WIDTH > terminal_area.width {
        terminal_area.width.saturating_sub(MAX_POPUP_WIDTH)
    } else {
        popup_x
    };

    Rect::new(popup_x, popup_y, MAX_POPUP_WIDTH, popup_height)
}

/// 補完ポップアップを描画する
pub(super) fn render_completion_popup(frame: &mut Frame, popup_rect: Rect, state: &CompletionState) {
    frame.render_widget(Clear, popup_rect);

    let items: Vec<ListItem> = state
        .candidates
        .iter()
        .map(|item| {
            let style = match item.kind {
                CompletionKind::Keyword => Style::default().fg(Color::Cyan),
                CompletionKind::Table => Style::default().fg(Color::Green),
                CompletionKind::Column { .. } => Style::default().fg(Color::Yellow),
                CompletionKind::Database => Style::default().fg(Color::Magenta),
            };
            ListItem::new(item.text.clone()).style(style)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

    let mut list_state = ListState::default();
    list_state.select(Some(state.selected_index));

    frame.render_stateful_widget(list, popup_rect, &mut list_state);
}
