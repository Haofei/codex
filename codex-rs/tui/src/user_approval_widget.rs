//! A modal widget that prompts the user to approve or deny an action
//! requested by the agent.
//!
//! This is a (very) rough port of
//! `src/components/chat/terminal-chat-command-review.tsx` from the TypeScript
//! UI to Rust using [`ratatui`]. The goal is feature‑parity for the keyboard
//! driven workflow – a fully‑fledged visual match is not required.

use std::path::PathBuf;
use std::sync::LazyLock;

use codex_core::protocol::Op;
use codex_core::protocol::ReviewDecision;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::exec_command::relativize_to_home;
use crate::exec_command::strip_bash_lc_and_escape;

/// Request coming from the agent that needs user approval.
pub(crate) enum ApprovalRequest {
    Exec {
        id: String,
        command: Vec<String>,
        cwd: PathBuf,
        reason: Option<String>,
    },
    ApplyPatch {
        id: String,
        reason: Option<String>,
        grant_root: Option<PathBuf>,
    },
}

/// Options displayed in the *select* mode.
struct SelectOption {
    label: Line<'static>,
    description: &'static str,
    key: KeyCode,
    decision: ReviewDecision,
}

static COMMAND_SELECT_OPTIONS: LazyLock<Vec<SelectOption>> = LazyLock::new(|| {
    vec![
        SelectOption {
            label: Line::from(vec!["Y".underlined(), "es".into()]),
            description: "Approve and run the command",
            key: KeyCode::Char('y'),
            decision: ReviewDecision::Approved,
        },
        SelectOption {
            label: Line::from(vec!["A".underlined(), "lways".into()]),
            description: "Approve the command for the remainder of this session",
            key: KeyCode::Char('a'),
            decision: ReviewDecision::ApprovedForSession,
        },
        SelectOption {
            label: Line::from(vec!["N".underlined(), "o".into()]),
            description: "Do not run the command",
            key: KeyCode::Char('n'),
            decision: ReviewDecision::Denied,
        },
    ]
});

static PATCH_SELECT_OPTIONS: LazyLock<Vec<SelectOption>> = LazyLock::new(|| {
    vec![
        SelectOption {
            label: Line::from(vec!["Y".underlined(), "es".into()]),
            description: "Approve and apply the changes",
            key: KeyCode::Char('y'),
            decision: ReviewDecision::Approved,
        },
        SelectOption {
            label: Line::from(vec!["N".underlined(), "o".into()]),
            description: "Do not apply the changes",
            key: KeyCode::Char('n'),
            decision: ReviewDecision::Denied,
        },
    ]
});

/// A modal prompting the user to approve or deny the pending request.
pub(crate) struct UserApprovalWidget<'a> {
    approval_request: ApprovalRequest,
    app_event_tx: AppEventSender,
    confirmation_prompt: Paragraph<'a>,
    select_options: &'a Vec<SelectOption>,

    /// Currently selected index in *select* mode.
    selected_option: usize,

    /// Set to `true` once a decision has been sent – the parent view can then
    /// remove this widget from its queue.
    done: bool,
}

impl UserApprovalWidget<'_> {
    pub(crate) fn new(approval_request: ApprovalRequest, app_event_tx: AppEventSender) -> Self {
        let confirmation_prompt = match &approval_request {
            ApprovalRequest::Exec {
                command,
                cwd,
                reason,
                ..
            } => {
                let cmd = strip_bash_lc_and_escape(command);
                // Maybe try to relativize to the cwd of this process first?
                // Will make cwd_str shorter in the common case.
                let cwd_str = match relativize_to_home(cwd) {
                    Some(rel) => format!("~/{}", rel.display()),
                    None => cwd.display().to_string(),
                };
                let mut contents: Vec<Line> = vec![
                    Line::from(vec!["codex".bold().magenta(), " wants to run:".into()]),
                    Line::from(vec![cwd_str.dim(), "$".into(), format!(" {cmd}").into()]),
                    Line::from(""),
                ];
                if let Some(reason) = reason {
                    contents.push(Line::from(reason.clone().italic()));
                    contents.push(Line::from(""));
                }
                Paragraph::new(contents).wrap(Wrap { trim: false })
            }
            ApprovalRequest::ApplyPatch {
                reason, grant_root, ..
            } => {
                let mut contents: Vec<Line> = vec![];

                if let Some(r) = reason {
                    contents.push(Line::from(r.clone().italic()));
                    contents.push(Line::from(""));
                }

                if let Some(root) = grant_root {
                    contents.push(Line::from(format!(
                        "This will grant write access to {} for the remainder of this session.",
                        root.display()
                    )));
                    contents.push(Line::from(""));
                }

                Paragraph::new(contents).wrap(Wrap { trim: false })
            }
        };

        Self {
            select_options: match &approval_request {
                ApprovalRequest::Exec { .. } => &COMMAND_SELECT_OPTIONS,
                ApprovalRequest::ApplyPatch { .. } => &PATCH_SELECT_OPTIONS,
            },
            approval_request,
            app_event_tx,
            confirmation_prompt,
            selected_option: 0,
            done: false,
        }
    }

    fn get_confirmation_prompt_height(&self, width: u16) -> u16 {
        // Should cache this for last value of width.
        self.confirmation_prompt.line_count(width) as u16
    }

    /// Process a `KeyEvent` coming from crossterm. Always consumes the event
    /// while the modal is visible.
    /// Process a key event originating from crossterm. As the modal fully
    /// captures input while visible, we don’t need to report whether the event
    /// was consumed—callers can assume it always is.
    pub(crate) fn handle_key_event(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press {
            self.handle_select_key(key);
        }
    }

    /// Handle Ctrl-C pressed by the user while the modal is visible.
    /// Behaves like pressing Escape: abort the request and close the modal.
    pub(crate) fn on_ctrl_c(&mut self) {
        self.send_decision(ReviewDecision::Abort);
    }

    fn handle_select_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Left => {
                self.selected_option = (self.selected_option + self.select_options.len() - 1)
                    % self.select_options.len();
            }
            KeyCode::Right => {
                self.selected_option = (self.selected_option + 1) % self.select_options.len();
            }
            KeyCode::Enter => {
                let opt = &self.select_options[self.selected_option];
                self.send_decision(opt.decision);
            }
            KeyCode::Esc => {
                self.send_decision(ReviewDecision::Abort);
            }
            other => {
                if let Some(opt) = self.select_options.iter().find(|opt| opt.key == other) {
                    self.send_decision(opt.decision);
                }
            }
        }
    }

    fn send_decision(&mut self, decision: ReviewDecision) {
        self.send_decision_with_feedback(decision, String::new())
    }

    fn send_decision_with_feedback(&mut self, decision: ReviewDecision, feedback: String) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        match &self.approval_request {
            ApprovalRequest::Exec { command, .. } => {
                let cmd = strip_bash_lc_and_escape(command);
                lines.push(Line::from("approval decision"));
                lines.push(Line::from(format!("$ {cmd}")));
                lines.push(Line::from(format!("decision: {decision:?}")));
            }
            ApprovalRequest::ApplyPatch { .. } => {
                lines.push(Line::from(format!("patch approval decision: {decision:?}")));
            }
        }
        if !feedback.trim().is_empty() {
            lines.push(Line::from("feedback:"));
            for l in feedback.lines() {
                lines.push(Line::from(l.to_string()));
            }
        }
        lines.push(Line::from(""));
        self.app_event_tx.send(AppEvent::InsertHistory(lines));

        let op = match &self.approval_request {
            ApprovalRequest::Exec { id, .. } => Op::ExecApproval {
                id: id.clone(),
                decision,
            },
            ApprovalRequest::ApplyPatch { id, .. } => Op::PatchApproval {
                id: id.clone(),
                decision,
            },
        };

        self.app_event_tx.send(AppEvent::CodexOp(op));
        self.done = true;
    }

    /// Returns `true` once the user has made a decision and the widget no
    /// longer needs to be displayed.
    pub(crate) fn is_complete(&self) -> bool {
        self.done
    }

    pub(crate) fn desired_height(&self, width: u16) -> u16 {
        self.get_confirmation_prompt_height(width) + self.select_options.len() as u16
    }
}

impl WidgetRef for &UserApprovalWidget<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let prompt_height = self.get_confirmation_prompt_height(area.width);
        let [prompt_chunk, response_chunk] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(prompt_height), Constraint::Min(0)])
            .areas(area);

        let lines: Vec<Line> = self
            .select_options
            .iter()
            .enumerate()
            .map(|(idx, opt)| {
                let style = if idx == self.selected_option {
                    Style::new().bg(Color::Cyan).fg(Color::Black)
                } else {
                    Style::new().bg(Color::DarkGray)
                };
                opt.label.clone().alignment(Alignment::Center).style(style)
            })
            .collect();

        let [title_area, button_area, description_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(response_chunk.inner(Margin::new(1, 0)));
        let title = match &self.approval_request {
            ApprovalRequest::Exec { .. } => "Allow command?",
            ApprovalRequest::ApplyPatch { .. } => "Apply changes?",
        };
        Line::from(title).render(title_area, buf);

        self.confirmation_prompt.clone().render(prompt_chunk, buf);
        let areas = Layout::horizontal(
            lines
                .iter()
                .map(|l| Constraint::Length(l.width() as u16 + 2)),
        )
        .spacing(1)
        .split(button_area);
        for (idx, area) in areas.iter().enumerate() {
            let line = &lines[idx];
            line.render(*area, buf);
        }

        Line::from(self.select_options[self.selected_option].description)
            .style(Style::new().italic().fg(Color::DarkGray))
            .render(description_area.inner(Margin::new(1, 0)), buf);

        Block::bordered()
            .border_type(BorderType::QuadrantOutside)
            .border_style(Style::default().fg(Color::Cyan))
            .borders(Borders::LEFT)
            .render_ref(
                Rect::new(0, response_chunk.y, 1, response_chunk.height),
                buf,
            );
    }
}
