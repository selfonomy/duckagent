use crate::agent::{AgentEvent, AgentRuntime};
use crate::approval::{
    ApprovalDecision, ApprovalPrompt, ApprovalProvider, ApprovalResponse, RuleHit,
};
use crate::input::InputState;
use crate::markdown::{BLOCKQUOTE_MARKER, StreamRenderer, TABLE_MARKER};
use crate::profiles;
use crate::provider::{RuntimeProvider, get_model_capabilities, resolve_runtime_context_window};
use crate::session_control::{
    SessionControlCommand, SessionListItem, filter_session_items, format_rewind_list,
    format_session_list, paginate_session_items, parse_session_control_command,
    session_meta_to_list_item,
};
use crate::tools::{MessageType, UiMessage};
use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::style::{Color, Print, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, ScrollUp, disable_raw_mode, enable_raw_mode};
use crossterm::{ExecutableCommand, QueueableCommand};
use image::AnimationDecoder;
use image::DynamicImage;
use image::GrayImage;
use image::ImageFormat;
use image::ImageReader;
use image::codecs::gif::GifDecoder;
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::BufReader;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const POLL_IDLE_INTERVAL: Duration = Duration::from_millis(50);
const SPINNER_INTERVAL: Duration = Duration::from_millis(120);
const CHAT_CONTAINER_MAX_WIDTH_COLS: usize = 100;
const USER_BUBBLE_MAX_WIDTH_PERCENT: usize = 80;
const BUBBLE_HORIZONTAL_PADDING: usize = 2;
const SLASH_COMMANDS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        command: "/new",
        hint: "start a new session",
        insert_text: "/new",
    },
    SlashCommandSpec {
        command: "/resume",
        hint: "list or resume sessions",
        insert_text: "/resume ",
    },
    SlashCommandSpec {
        command: "/rewind",
        hint: "list or rewind turns",
        insert_text: "/rewind ",
    },
    SlashCommandSpec {
        command: "/model",
        hint: "manage models",
        insert_text: "/model",
    },
];
const STARTUP_AVATAR_FILENAMES: &[&str] = &[
    "avatar.png",
    "avatar.jpg",
    "avatar.jpeg",
    "avatar.webp",
    "avatar.gif",
];
const DEFAULT_AVATAR_DIR: &str = "src/default";

#[derive(Clone, Copy)]
struct Theme {
    app_bg: Color,
    user_bg: Color,
    user_fg: Color,
    ai_bg: Color,
    ai_fg: Color,
    input_fg: Color,
    accent: Color,
    cursor_cover_fg: Color,
    select_bg: Color,
    select_fg: Color,
    status_fg: Color,
    border_fg: Color,
    aux_fg: Color,
}

const THEME_GRUVBOX_DARK: Theme = Theme {
    app_bg: Color::Rgb {
        r: 40,
        g: 40,
        b: 40,
    },
    user_bg: Color::Rgb {
        r: 60,
        g: 56,
        b: 54,
    },
    user_fg: Color::Rgb {
        r: 235,
        g: 219,
        b: 178,
    },
    ai_bg: Color::Rgb {
        r: 50,
        g: 48,
        b: 47,
    },
    ai_fg: Color::Rgb {
        r: 235,
        g: 219,
        b: 178,
    },
    input_fg: Color::Rgb {
        r: 235,
        g: 219,
        b: 178,
    },
    accent: Color::Rgb {
        r: 215,
        g: 153,
        b: 33,
    },
    cursor_cover_fg: Color::Rgb {
        r: 29,
        g: 32,
        b: 33,
    },
    select_bg: Color::Rgb {
        r: 80,
        g: 73,
        b: 69,
    },
    select_fg: Color::Rgb {
        r: 251,
        g: 241,
        b: 199,
    },
    status_fg: Color::Rgb {
        r: 168,
        g: 153,
        b: 132,
    },
    border_fg: Color::Rgb {
        r: 102,
        g: 92,
        b: 84,
    },
    aux_fg: Color::Rgb {
        r: 189,
        g: 174,
        b: 147,
    },
};

const THEME_GRUVBOX_LIGHT: Theme = Theme {
    app_bg: Color::Rgb {
        r: 251,
        g: 241,
        b: 199,
    },
    user_bg: Color::Rgb {
        r: 242,
        g: 229,
        b: 188,
    },
    user_fg: Color::Rgb {
        r: 60,
        g: 56,
        b: 54,
    },
    ai_bg: Color::Rgb {
        r: 249,
        g: 245,
        b: 215,
    },
    ai_fg: Color::Rgb {
        r: 60,
        g: 56,
        b: 54,
    },
    input_fg: Color::Rgb {
        r: 60,
        g: 56,
        b: 54,
    },
    accent: Color::Rgb {
        r: 175,
        g: 58,
        b: 3,
    },
    cursor_cover_fg: Color::Rgb {
        r: 251,
        g: 241,
        b: 199,
    },
    select_bg: Color::Rgb {
        r: 213,
        g: 196,
        b: 161,
    },
    select_fg: Color::Rgb {
        r: 40,
        g: 40,
        b: 40,
    },
    status_fg: Color::Rgb {
        r: 124,
        g: 111,
        b: 100,
    },
    border_fg: Color::Rgb {
        r: 189,
        g: 174,
        b: 147,
    },
    aux_fg: Color::Rgb {
        r: 102,
        g: 92,
        b: 84,
    },
};

const THEME_GITHUB_DARK: Theme = Theme {
    app_bg: Color::Rgb {
        r: 13,
        g: 17,
        b: 23,
    },
    user_bg: Color::Rgb {
        r: 33,
        g: 38,
        b: 45,
    },
    user_fg: Color::Rgb {
        r: 230,
        g: 237,
        b: 243,
    },
    ai_bg: Color::Rgb {
        r: 22,
        g: 27,
        b: 34,
    },
    ai_fg: Color::Rgb {
        r: 230,
        g: 237,
        b: 243,
    },
    input_fg: Color::Rgb {
        r: 201,
        g: 209,
        b: 217,
    },
    accent: Color::Rgb {
        r: 47,
        g: 129,
        b: 247,
    },
    cursor_cover_fg: Color::Rgb {
        r: 13,
        g: 17,
        b: 23,
    },
    select_bg: Color::Rgb {
        r: 38,
        g: 79,
        b: 120,
    },
    select_fg: Color::Rgb {
        r: 230,
        g: 237,
        b: 243,
    },
    status_fg: Color::Rgb {
        r: 139,
        g: 148,
        b: 158,
    },
    border_fg: Color::Rgb {
        r: 48,
        g: 54,
        b: 61,
    },
    aux_fg: Color::Rgb {
        r: 157,
        g: 167,
        b: 179,
    },
};

const THEME_GITHUB_LIGHT: Theme = Theme {
    app_bg: Color::Rgb {
        r: 255,
        g: 255,
        b: 255,
    },
    user_bg: Color::Rgb {
        r: 246,
        g: 248,
        b: 250,
    },
    user_fg: Color::Rgb {
        r: 36,
        g: 41,
        b: 47,
    },
    ai_bg: Color::Rgb {
        r: 243,
        g: 244,
        b: 246,
    },
    ai_fg: Color::Rgb {
        r: 36,
        g: 41,
        b: 47,
    },
    input_fg: Color::Rgb {
        r: 36,
        g: 41,
        b: 47,
    },
    accent: Color::Rgb {
        r: 9,
        g: 105,
        b: 218,
    },
    cursor_cover_fg: Color::Rgb {
        r: 255,
        g: 255,
        b: 255,
    },
    select_bg: Color::Rgb {
        r: 219,
        g: 234,
        b: 254,
    },
    select_fg: Color::Rgb {
        r: 36,
        g: 41,
        b: 47,
    },
    status_fg: Color::Rgb {
        r: 87,
        g: 96,
        b: 106,
    },
    border_fg: Color::Rgb {
        r: 208,
        g: 215,
        b: 222,
    },
    aux_fg: Color::Rgb {
        r: 101,
        g: 109,
        b: 118,
    },
};

const THEME_ONE_DARK: Theme = Theme {
    app_bg: Color::Rgb {
        r: 40,
        g: 44,
        b: 52,
    },
    user_bg: Color::Rgb {
        r: 58,
        g: 63,
        b: 75,
    },
    user_fg: Color::Rgb {
        r: 171,
        g: 178,
        b: 191,
    },
    ai_bg: Color::Rgb {
        r: 50,
        g: 56,
        b: 66,
    },
    ai_fg: Color::Rgb {
        r: 171,
        g: 178,
        b: 191,
    },
    input_fg: Color::Rgb {
        r: 215,
        g: 218,
        b: 224,
    },
    accent: Color::Rgb {
        r: 97,
        g: 175,
        b: 239,
    },
    cursor_cover_fg: Color::Rgb {
        r: 30,
        g: 34,
        b: 42,
    },
    select_bg: Color::Rgb {
        r: 75,
        g: 83,
        b: 99,
    },
    select_fg: Color::Rgb {
        r: 230,
        g: 233,
        b: 239,
    },
    status_fg: Color::Rgb {
        r: 143,
        g: 152,
        b: 168,
    },
    border_fg: Color::Rgb {
        r: 75,
        g: 82,
        b: 99,
    },
    aux_fg: Color::Rgb {
        r: 170,
        g: 178,
        b: 191,
    },
};

const THEME_NORD_DARK: Theme = Theme {
    app_bg: Color::Rgb {
        r: 46,
        g: 52,
        b: 64,
    },
    user_bg: Color::Rgb {
        r: 59,
        g: 66,
        b: 82,
    },
    user_fg: Color::Rgb {
        r: 229,
        g: 233,
        b: 240,
    },
    ai_bg: Color::Rgb {
        r: 67,
        g: 76,
        b: 94,
    },
    ai_fg: Color::Rgb {
        r: 236,
        g: 239,
        b: 244,
    },
    input_fg: Color::Rgb {
        r: 229,
        g: 233,
        b: 240,
    },
    accent: Color::Rgb {
        r: 136,
        g: 192,
        b: 208,
    },
    cursor_cover_fg: Color::Rgb {
        r: 46,
        g: 52,
        b: 64,
    },
    select_bg: Color::Rgb {
        r: 76,
        g: 86,
        b: 106,
    },
    select_fg: Color::Rgb {
        r: 236,
        g: 239,
        b: 244,
    },
    status_fg: Color::Rgb {
        r: 216,
        g: 222,
        b: 233,
    },
    border_fg: Color::Rgb {
        r: 97,
        g: 110,
        b: 136,
    },
    aux_fg: Color::Rgb {
        r: 207,
        g: 215,
        b: 230,
    },
};

#[allow(dead_code)]
const BUILTIN_THEMES: &[(&str, Theme)] = &[
    ("gruvbox_dark", THEME_GRUVBOX_DARK),
    ("gruvbox_light", THEME_GRUVBOX_LIGHT),
    ("github_dark", THEME_GITHUB_DARK),
    ("github_light", THEME_GITHUB_LIGHT),
    ("one_dark", THEME_ONE_DARK),
    ("nord_dark", THEME_NORD_DARK),
];

const ACTIVE_THEME: Theme = THEME_GRUVBOX_DARK;

pub struct ChatUi {
    agent: AgentRuntime,
    session_id: String,
    session_title: String,
    model_name: String,
    context_tokens: usize,
    context_window: Option<u64>,
    context_window_rx: Receiver<Option<u64>>,
    input: InputState,
    status: String,
    loading_phase: usize,
    approval_rx: Receiver<ApprovalPrompt>,
    approval_provider: Arc<ChannelApprovalProvider>,
    viewport: ViewportState,
    pending_approval: Option<ApprovalUiState>,
    pending_history: Vec<HistoryEntry>,
    committed_history: Vec<HistoryEntry>,
    live_stream: Option<LiveStreamState>,
    rendered_history_entries: usize,
    live_render_kind: Option<LiveStreamKind>,
    live_render_lines: Vec<String>,
    pending_live_finalize: Option<PendingLiveFinalize>,
    live_preview_rendered: bool,
    event_rx: Receiver<AgentEvent>,
    main_agent_running: bool,
    next_spinner_tick: Option<Instant>,
    last_pending_approval: bool,
    last_footer_expanded: bool,
    history_fill_rows: u16,
    pending_preview_clear_lines: usize,
    startup_animation: Option<StartupAnimationState>,
    last_session_list: Vec<SessionListItem>,
    slash_picker: Option<SlashCommandPicker>,
    pending_revealed_footer_top: Option<u16>,
}

struct SlashCommandSpec {
    command: &'static str,
    hint: &'static str,
    insert_text: &'static str,
}

#[derive(Clone)]
struct SlashCommandPicker {
    selected_index: usize,
}

struct ApprovalUiState {
    command: String,
    options: [ApprovalDecision; 4],
    selected_index: usize,
    response_tx: mpsc::Sender<ApprovalResponse>,
}

#[derive(Clone)]
struct HistoryEntry {
    label: String,
    text: String,
    fg: Option<Color>,
    bg: Option<Color>,
    right_align: bool,
    bubble: bool,
    bubble_full_width: bool,
    no_wrap: bool,
    full_screen: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LiveStreamKind {
    Assistant,
    ToolCall,
}

struct LiveStreamState {
    kind: LiveStreamKind,
    text: String,
    raw_tool_args: String,
    consumed_bytes: usize,
    current_line: String,
    current_line_width: usize,
    started: bool,
    assistant_preview: Option<AssistantStreamPreview>,
}

struct AssistantStreamPreview {
    width: usize,
    renderer: StreamRenderer,
    committed_lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StreamPreviewOp {
    Append(String),
    RewriteRecent {
        offset_from_bottom: u16,
        line: String,
    },
}

struct PendingLiveFinalize {
    kind: LiveStreamKind,
    lines: Vec<String>,
    append_gap_after: bool,
}

struct RenderedHistoryLine {
    left: u16,
    text: String,
    fg: Option<Color>,
    bg: Option<Color>,
    styled_start: usize,
}

#[derive(Clone, Copy)]
struct ViewportState {
    cols: u16,
    rows: u16,
    footer_rows: u16,
    footer_top: u16,
    loading_row: u16,
    top_border_row: u16,
    input_row: u16,
    help_row: u16,
    bottom_row: u16,
}

#[derive(Clone, Copy)]
struct ChatContainerLayout {
    left: u16,
    width: usize,
}

struct BrailleLine {
    ansi_text: String,
    display_width: usize,
}

struct StartupFrame {
    lines: Vec<BrailleLine>,
    delay: Duration,
}

struct StartupAnimationState {
    frames: Vec<StartupFrame>,
    frame_index: usize,
    next_tick: Instant,
    image_line_count: usize,
    history_entry_count: usize,
}

impl ViewportState {
    fn from_size(cols: u16, rows: u16, expanded_footer: bool) -> Self {
        // Footer layout:
        // loading row
        // border
        // input/approval list
        // border
        // status
        // empty line
        // Approval and slash-command lists use 4 lines, input uses 1 line.
        let wanted_footer_rows = if expanded_footer { 9 } else { 6 };
        let footer_rows = rows.min(wanted_footer_rows);
        let footer_top = rows.saturating_sub(footer_rows);
        let top_border_row = footer_top.saturating_add(1);

        Self {
            cols,
            rows,
            footer_rows,
            footer_top,
            loading_row: footer_top,
            top_border_row,
            input_row: rows.saturating_sub(4),
            help_row: rows.saturating_sub(2),
            bottom_row: rows.saturating_sub(1),
        }
    }

    fn from_terminal(expanded_footer: bool) -> Self {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        Self::from_size(cols, rows, expanded_footer)
    }

    fn cols_usize(self) -> usize {
        self.cols as usize
    }

    fn chat_container_layout(self) -> ChatContainerLayout {
        let cols = self.cols_usize();
        let width = cols.min(CHAT_CONTAINER_MAX_WIDTH_COLS).max(1);
        let left = cols.saturating_sub(width) / 2;
        ChatContainerLayout {
            left: left as u16,
            width,
        }
    }
}

#[derive(Clone)]
struct ChannelApprovalProvider {
    prompt_tx: mpsc::Sender<ApprovalPrompt>,
}

impl ApprovalProvider for ChannelApprovalProvider {
    fn request_approval(
        &self,
        command: &str,
        _rule_hits: &[RuleHit],
        options: [ApprovalDecision; 4],
    ) -> Option<ApprovalResponse> {
        let (response_tx, response_rx) = mpsc::channel();
        let prompt = ApprovalPrompt {
            command: command.to_string(),
            options,
            response_tx,
        };
        self.prompt_tx.send(prompt).ok()?;
        response_rx.recv().ok()
    }
}

impl ChatUi {
    pub fn new(agent: AgentRuntime, session_id: String) -> Self {
        let (approval_tx, approval_rx) = mpsc::channel();
        let approval_provider = Arc::new(ChannelApprovalProvider {
            prompt_tx: approval_tx,
        });
        let runtime = agent.runtime().clone();
        let model_name = runtime.model.clone();
        let session_title = agent
            .session_title(&session_id)
            .unwrap_or_else(|| "Untitled session".to_string());
        let context_tokens = agent.main_context_tokens(&session_id).unwrap_or(0);
        let context_window = Some(resolve_runtime_context_window(&runtime));
        let (context_window_tx, context_window_rx) = mpsc::channel();
        let provider_name = runtime.provider.as_str().to_string();
        let model_for_metadata = runtime.model.clone();
        let runtime_for_metadata = runtime.clone();
        thread::spawn(move || {
            let resolved = get_model_capabilities(&provider_name, &model_for_metadata)
                .ok()
                .flatten()
                .and_then(|capabilities| capabilities.context_window)
                .unwrap_or_else(|| resolve_runtime_context_window(&runtime_for_metadata));
            let _ = context_window_tx.send(Some(resolved));
        });
        let event_rx = agent.subscribe();
        let viewport = ViewportState::from_terminal(false);
        let mut ui = Self {
            agent,
            session_id,
            session_title,
            model_name,
            context_tokens,
            context_window,
            context_window_rx,
            input: InputState::new(),
            status: "Ready".to_string(),
            loading_phase: 0,
            approval_rx,
            approval_provider,
            viewport,
            pending_approval: None,
            pending_history: Vec::new(),
            committed_history: Vec::new(),
            live_stream: None,
            rendered_history_entries: 0,
            live_render_kind: None,
            live_render_lines: Vec::new(),
            pending_live_finalize: None,
            live_preview_rendered: false,
            event_rx,
            main_agent_running: false,
            next_spinner_tick: None,
            last_pending_approval: false,
            last_footer_expanded: false,
            history_fill_rows: 0,
            pending_preview_clear_lines: 0,
            startup_animation: None,
            last_session_list: Vec::new(),
            slash_picker: None,
            pending_revealed_footer_top: None,
        };
        let (startup_entries, startup_animation) = ui.render_startup_braille_entries();
        ui.startup_animation = startup_animation;
        ui.queue_history_entries(startup_entries);
        ui.flush_pending_history();
        ui
    }

    pub fn run(&mut self) -> Result<()> {
        let mut stdout = io::stdout();
        enable_raw_mode().context("failed to enable raw mode")?;
        stdout.execute(Hide)?;
        stdout
            .execute(EnableBracketedPaste)
            .context("failed to enable bracketed paste")?;

        let mut guard = TerminalGuard;
        self.redraw(&mut stdout)?;

        loop {
            let mut needs_redraw = false;
            needs_redraw |= self.drain_approval_requests();
            needs_redraw |= self.drain_agent_events();
            needs_redraw |= self.flush_pending_history();

            if needs_redraw {
                self.redraw(&mut stdout)?;
            }

            let poll_timeout = self.next_poll_timeout();
            if event::poll(poll_timeout)? {
                match event::read()? {
                    Event::Resize(cols, rows) => {
                        self.recompute_viewport(Some((cols, rows)));
                        self.rebuild_startup_on_resize();
                        self.redraw(&mut stdout)?;
                    }
                    Event::Key(key) => {
                        if self.pending_approval.is_some() {
                            if self.handle_approval_key(key, &mut stdout)? {
                                break;
                            }
                            self.redraw(&mut stdout)?;
                        } else if self.handle_key(key, &mut stdout)? {
                            break;
                        }
                    }
                    Event::Paste(text) => {
                        self.input.insert_str(&text);
                        if self.update_slash_picker_state() {
                            self.redraw_history_and_footer(&mut stdout)?;
                        } else {
                            self.redraw_footer(&mut stdout)?;
                        }
                    }
                    _ => {}
                }
            }

            self.tick_spinner(&mut stdout)?;
        }

        guard.release(&mut stdout)?;
        Ok(())
    }

    fn viewport_state(&self) -> ViewportState {
        self.viewport
    }

    fn recompute_viewport(&mut self, terminal_size: Option<(u16, u16)>) -> bool {
        let (cols, rows) = terminal_size.unwrap_or_else(|| terminal::size().unwrap_or((80, 24)));
        let next = ViewportState::from_size(cols, rows, self.footer_expanded());
        let changed = self.viewport.cols != next.cols
            || self.viewport.rows != next.rows
            || self.viewport.footer_rows != next.footer_rows;
        self.viewport = next;
        if changed {
            self.live_render_kind = None;
            self.live_render_lines.clear();
            self.pending_live_finalize = None;
        }
        changed
    }

    fn queue_history_entries(&mut self, entries: Vec<HistoryEntry>) {
        if !entries.is_empty() {
            self.pending_history.extend(entries);
        }
    }

    fn footer_expanded(&self) -> bool {
        self.pending_approval.is_some() || self.slash_picker.is_some()
    }

    fn flush_pending_history(&mut self) -> bool {
        if self.pending_history.is_empty() {
            return false;
        }
        let entries = std::mem::take(&mut self.pending_history);
        self.committed_history.extend(entries);
        true
    }

    fn handle_key(&mut self, key: KeyEvent, stdout: &mut io::Stdout) -> Result<bool> {
        if self.handle_slash_picker_key(key, stdout)? {
            return Ok(false);
        }
        match key {
            KeyEvent {
                code: KeyCode::Esc, ..
            }
            | KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => return Ok(true),

            KeyEvent {
                code: KeyCode::Enter,
                ..
            } => {
                self.slash_picker = None;
                self.recompute_viewport(None);
                self.submit_input_as_user_turn(stdout)?;
            }

            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } => {
                self.input.backspace();
                if self.update_slash_picker_state() {
                    self.redraw_history_and_footer(stdout)?;
                } else {
                    self.redraw_footer(stdout)?;
                }
            }

            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            } if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                self.input.insert_char(c);
                if self.update_slash_picker_state() {
                    self.redraw_history_and_footer(stdout)?;
                } else {
                    self.redraw_footer(stdout)?;
                }
            }

            KeyEvent {
                code: KeyCode::Delete,
                ..
            } => {
                self.input.delete_forward();
                if self.update_slash_picker_state() {
                    self.redraw_history_and_footer(stdout)?;
                } else {
                    self.redraw_footer(stdout)?;
                }
            }

            KeyEvent {
                code: KeyCode::Left,
                modifiers,
                ..
            } if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                self.input
                    .move_left(modifiers.contains(KeyModifiers::SHIFT));
                if self.update_slash_picker_state() {
                    self.redraw_history_and_footer(stdout)?;
                } else {
                    self.redraw_footer(stdout)?;
                }
            }

            KeyEvent {
                code: KeyCode::Right,
                modifiers,
                ..
            } if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                self.input
                    .move_right(modifiers.contains(KeyModifiers::SHIFT));
                if self.update_slash_picker_state() {
                    self.redraw_history_and_footer(stdout)?;
                } else {
                    self.redraw_footer(stdout)?;
                }
            }

            _ => {}
        }
        Ok(false)
    }

    fn handle_slash_picker_key(&mut self, key: KeyEvent, stdout: &mut io::Stdout) -> Result<bool> {
        let Some(picker) = self.slash_picker.as_mut() else {
            return Ok(false);
        };
        match key.code {
            KeyCode::Up => {
                let matches = matching_slash_commands(&self.input.text);
                if !matches.is_empty() {
                    picker.selected_index = if picker.selected_index == 0 {
                        matches.len().saturating_sub(1)
                    } else {
                        picker.selected_index.saturating_sub(1)
                    };
                }
                self.redraw_footer(stdout)?;
                Ok(true)
            }
            KeyCode::Down => {
                let matches = matching_slash_commands(&self.input.text);
                if !matches.is_empty() {
                    picker.selected_index = (picker.selected_index + 1) % matches.len();
                }
                self.redraw_footer(stdout)?;
                Ok(true)
            }
            KeyCode::Enter => {
                let matches = matching_slash_commands(&self.input.text);
                if let Some(spec) = matches.get(picker.selected_index).copied() {
                    self.input.clear();
                    self.input.insert_str(spec.insert_text);
                }
                self.close_slash_picker_and_redraw(stdout)?;
                Ok(true)
            }
            KeyCode::Esc => {
                self.close_slash_picker_and_redraw(stdout)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn update_slash_picker_state(&mut self) -> bool {
        let matches = matching_slash_commands(&self.input.text);
        let should_show = slash_picker_should_show(&self.input) && !matches.is_empty();
        match (&mut self.slash_picker, should_show) {
            (Some(picker), true) => {
                if picker.selected_index >= matches.len() {
                    picker.selected_index = 0;
                }
                false
            }
            (None, true) => {
                self.slash_picker = Some(SlashCommandPicker { selected_index: 0 });
                self.recompute_viewport(None);
                false
            }
            (Some(_), false) => {
                let previous_footer_top = self.viewport.footer_top;
                self.slash_picker = None;
                if self.recompute_viewport(None) {
                    self.pending_revealed_footer_top = Some(previous_footer_top);
                    true
                } else {
                    false
                }
            }
            (None, false) => false,
        }
    }

    fn close_slash_picker_and_redraw(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        let previous_footer_top = self.viewport.footer_top;
        self.slash_picker = None;
        if self.recompute_viewport(None) {
            self.pending_revealed_footer_top = Some(previous_footer_top);
        }
        self.redraw_history_and_footer(stdout)
    }

    fn redraw_history_and_footer(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        if let Some(previous_footer_top) = self.pending_revealed_footer_top.take() {
            self.restore_revealed_history_rows(stdout, previous_footer_top)?;
        }
        self.last_footer_expanded = self.footer_expanded();
        self.redraw_footer(stdout)
    }

    fn submit_input_as_user_turn(&mut self, stdout: &mut io::Stdout) -> Result<bool> {
        let user_text_trimmed = self.input.text.trim();
        if user_text_trimmed.is_empty() {
            return Ok(false);
        }
        let user_text = user_text_trimmed.to_string();
        self.input.clear();

        self.queue_history_entries(self.with_message_gap(self.render_user_entries(&user_text)));
        self.flush_pending_history();

        if user_text == "/model" {
            self.handle_model_manager_command(stdout)?;
            return Ok(true);
        }

        if let Some(command) = parse_session_control_command(&user_text) {
            self.handle_session_control_command(command, stdout)?;
            return Ok(true);
        }

        self.agent.submit_user_turn(
            self.session_id.clone(),
            user_text,
            self.approval_provider.clone(),
        );
        if self.main_agent_running {
            self.arm_spinner_if_needed();
            self.status = "Agent is thinking...".to_string();
        }
        self.redraw(stdout)?;
        Ok(true)
    }

    fn handle_model_manager_command(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        if self.agent.has_pending_or_running_work(&self.session_id) {
            self.queue_history_entries(self.with_message_gap(self.render_assistant_entries(
                "Model management is available after the current turn finishes.",
            )));
            self.flush_pending_history();
            self.redraw(stdout)?;
            return Ok(());
        }

        let manager_result = self.run_model_manager_outside_chat(stdout);
        self.restore_chat_terminal(stdout)?;
        let selected_runtime = manager_result?;
        if let Some(runtime) = selected_runtime {
            self.apply_selected_runtime(runtime)?;
        }
        self.redraw(stdout)?;
        Ok(())
    }

    fn run_model_manager_outside_chat(
        &mut self,
        stdout: &mut io::Stdout,
    ) -> Result<Option<RuntimeProvider>> {
        stdout
            .execute(DisableBracketedPaste)
            .context("failed to disable bracketed paste before model manager")?;
        stdout.execute(Show)?;
        disable_raw_mode().context("failed to disable raw mode before model manager")?;
        crate::model_manager::run_model_manager()
    }

    fn restore_chat_terminal(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        enable_raw_mode().context("failed to re-enable raw mode after model manager")?;
        stdout.execute(Hide)?;
        stdout
            .execute(EnableBracketedPaste)
            .context("failed to re-enable bracketed paste after model manager")?;
        self.recompute_viewport(None);
        Ok(())
    }

    fn apply_selected_runtime(&mut self, runtime: RuntimeProvider) -> Result<()> {
        let old_session_id = self.session_id.clone();
        self.agent.clear_session_runtime_state(&old_session_id);
        self.agent
            .update_session_runtime(&self.session_id, runtime.session_config())?;
        let next_agent = self.agent.with_runtime(runtime.clone())?;
        self.agent = next_agent;
        self.event_rx = self.agent.subscribe();
        self.model_name = runtime.model.clone();
        self.context_window = Some(resolve_runtime_context_window(&runtime));
        let (context_window_tx, context_window_rx) = mpsc::channel();
        let provider_name = runtime.provider.as_str().to_string();
        let model_for_metadata = runtime.model.clone();
        let runtime_for_metadata = runtime.clone();
        thread::spawn(move || {
            let resolved = get_model_capabilities(&provider_name, &model_for_metadata)
                .ok()
                .flatten()
                .and_then(|capabilities| capabilities.context_window)
                .unwrap_or_else(|| resolve_runtime_context_window(&runtime_for_metadata));
            let _ = context_window_tx.send(Some(resolved));
        });
        self.context_window_rx = context_window_rx;
        self.status = format!("Using {}.", self.model_name);
        self.queue_history_entries(
            self.with_message_gap(self.render_assistant_entries(&format!(
                "Using model: {} / {}.",
                runtime.provider.as_str(),
                runtime.model
            ))),
        );
        self.flush_pending_history();
        Ok(())
    }

    fn handle_session_control_command(
        &mut self,
        command: SessionControlCommand,
        stdout: &mut io::Stdout,
    ) -> Result<()> {
        match command {
            SessionControlCommand::New => {
                let old_session_id = self.session_id.clone();
                self.agent.clear_session_runtime_state(&old_session_id);
                let session_id = self
                    .agent
                    .create_session_with_title(None, "tui")?;
                self.session_id = session_id;
                self.session_title = "Untitled session".to_string();
                self.context_tokens = 0;
                self.status = "Started a new session.".to_string();
                self.last_session_list.clear();
                self.clear_live_state();
                self.committed_history.clear();
                self.pending_history.clear();
                self.rendered_history_entries = 0;
                self.queue_history_entries(self.with_message_gap(
                    self.render_assistant_entries("Started a new session."),
                ));
            }
            SessionControlCommand::Resume { index: None } => {
                let items = self.tui_session_items(None)?;
                let (page_items, page, total_pages) = paginate_session_items(&items, 1);
                self.last_session_list = page_items.to_vec();
                let text = format_session_list(page_items, page, total_pages);
                self.queue_history_entries(
                    self.with_message_gap(self.render_assistant_entries(&text)),
                );
            }
            SessionControlCommand::Resume { index: Some(index) } => {
                let Some(item) = self.last_session_list.get(index.saturating_sub(1)).cloned()
                else {
                    self.queue_history_entries(self.with_message_gap(
                        self.render_assistant_entries(
                            "That session number is not in the latest `/resume` list.",
                        ),
                    ));
                    self.flush_pending_history();
                    self.redraw(stdout)?;
                    return Ok(());
                };
                let old_session_id = std::mem::replace(&mut self.session_id, item.session_id);
                self.agent.clear_session_runtime_state(&old_session_id);
                self.session_title = item.title.clone();
                self.context_tokens = self
                    .agent
                    .main_context_tokens(&self.session_id)
                    .unwrap_or_default();
                self.status = "Resumed session.".to_string();
                self.clear_live_state();
                self.committed_history.clear();
                self.pending_history.clear();
                self.rendered_history_entries = 0;
                self.queue_history_entries(self.with_message_gap(
                    self.render_assistant_entries(&format!("Resumed session: {}.", item.title)),
                ));
            }
            SessionControlCommand::Rewind { index: None } => {
                let items = self.agent.rewind_list(&self.session_id)?;
                let text = format_rewind_list(&items);
                self.queue_history_entries(
                    self.with_message_gap(self.render_assistant_entries(&text)),
                );
            }
            SessionControlCommand::Rewind { index: Some(index) } => {
                if self.agent.has_pending_or_running_work(&self.session_id) {
                    self.queue_history_entries(self.with_message_gap(
                        self.render_assistant_entries(
                            "Rewind is available after the current turn finishes.",
                        ),
                    ));
                    self.flush_pending_history();
                    self.redraw(stdout)?;
                    return Ok(());
                }
                let result = self.agent.rewind_session(&self.session_id, index)?;
                self.context_tokens = self
                    .agent
                    .main_context_tokens(&self.session_id)
                    .unwrap_or_default();
                self.status = "Rewound session.".to_string();
                self.clear_live_state();
                self.committed_history.clear();
                self.pending_history.clear();
                self.rendered_history_entries = 0;
                let mut text = format!(
                    "Rewound to before #{}: {}.\nRestored file changes: {}.",
                    result.target.index,
                    result.target.preview,
                    result.restored_files.len()
                );
                if !result.warnings.is_empty() {
                    text.push_str("\n\nWarnings:\n");
                    for warning in &result.warnings {
                        text.push_str("- ");
                        text.push_str(warning);
                        text.push('\n');
                    }
                }
                self.queue_history_entries(
                    self.with_message_gap(self.render_assistant_entries(text.trim_end())),
                );
            }
            SessionControlCommand::Invalid { message } => {
                self.queue_history_entries(
                    self.with_message_gap(self.render_assistant_entries(&message)),
                );
            }
        }
        self.flush_pending_history();
        self.redraw(stdout)?;
        Ok(())
    }

    fn tui_session_items(&self, query: Option<&str>) -> Result<Vec<SessionListItem>> {
        let items = self
            .agent
            .list_main_session_metas()?
            .into_iter()
            .filter(|meta| meta.created_by == "tui")
            .map(|meta| session_meta_to_list_item(&meta, "tui"))
            .collect::<Vec<_>>();
        Ok(filter_session_items(items, query))
    }

    fn clear_live_state(&mut self) {
        self.live_stream = None;
        self.live_render_kind = None;
        self.live_render_lines.clear();
        self.pending_live_finalize = None;
        self.live_preview_rendered = false;
        self.pending_preview_clear_lines = 0;
        self.main_agent_running = false;
        self.next_spinner_tick = None;
    }

    fn next_poll_timeout(&self) -> Duration {
        let now = Instant::now();
        let mut next_deadline: Option<Instant> = self.next_spinner_tick;
        if let Some(anim) = self.startup_animation.as_ref() {
            next_deadline = match next_deadline {
                Some(existing) => Some(existing.min(anim.next_tick)),
                None => Some(anim.next_tick),
            };
        }
        let Some(next) = next_deadline else {
            return POLL_IDLE_INTERVAL;
        };
        if now >= next {
            Duration::ZERO
        } else {
            next.saturating_duration_since(now).min(POLL_IDLE_INTERVAL)
        }
    }

    fn arm_spinner_if_needed(&mut self) {
        if self.main_agent_running && self.next_spinner_tick.is_none() {
            self.next_spinner_tick = Some(Instant::now() + SPINNER_INTERVAL);
        }
    }

    fn tick_spinner(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        self.tick_startup_animation(stdout)?;
        if !self.main_agent_running {
            self.next_spinner_tick = None;
            return Ok(());
        }
        self.arm_spinner_if_needed();
        let now = Instant::now();
        let Some(next) = self.next_spinner_tick else {
            return Ok(());
        };
        if now < next {
            return Ok(());
        }

        self.loading_phase = (self.loading_phase + 1) % 10;
        self.draw_loading_line(stdout, self.viewport_state())?;
        stdout.flush()?;
        self.next_spinner_tick = Some(now + SPINNER_INTERVAL);
        Ok(())
    }

    fn tick_startup_animation(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        let Some(anim) = self.startup_animation.as_mut() else {
            return Ok(());
        };
        let startup_only = self.committed_history.len() == anim.history_entry_count
            && self.pending_history.is_empty()
            && self.live_stream.is_none()
            && self.pending_approval.is_none();
        if !startup_only || anim.frames.len() <= 1 {
            self.startup_animation = None;
            return Ok(());
        }

        let now = Instant::now();
        if now < anim.next_tick {
            return Ok(());
        }

        anim.frame_index = (anim.frame_index + 1) % anim.frames.len();
        let frame_index = anim.frame_index;
        let next_delay = anim.frames[frame_index].delay;
        let lines: Vec<String> = anim.frames[frame_index]
            .lines
            .iter()
            .map(|l| l.ansi_text.clone())
            .collect();
        let line_count = anim.image_line_count;
        anim.next_tick = now + next_delay;

        for (line_idx, line) in lines.iter().enumerate().take(line_count) {
            // History tail is a trailing blank line, so image rows start one line above it.
            let offset_from_bottom = (line_count.saturating_sub(line_idx)) as u16;
            self.rewrite_recent_history_line(stdout, 0, offset_from_bottom, line, None, None)?;
        }
        stdout.flush()?;
        Ok(())
    }

    fn drain_agent_events(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.event_rx.try_recv() {
                Ok(event) => {
                    changed = true;
                    match event {
                        AgentEvent::StatusChanged { session_id, status } => {
                            if session_id != self.session_id {
                                continue;
                            }
                            self.status = status;
                        }
                        AgentEvent::SessionTitleUpdated { session_id, title } => {
                            if session_id != self.session_id {
                                continue;
                            }
                            self.session_title = title;
                        }
                        AgentEvent::Message {
                            session_id,
                            message,
                        } => {
                            if session_id != self.session_id {
                                continue;
                            }
                            // User messages are already echoed immediately in handle_key()
                            // so we skip the duplicated user event from AgentRuntime.
                            if message.msg_type != MessageType::User {
                                let suppress_final = self.finish_live_stream_for_message_type(
                                    message.msg_type.clone(),
                                    Some(&message.content),
                                );
                                if !suppress_final {
                                    self.queue_history_entries(self.render_ui_entries(&message));
                                }
                            }
                        }
                        AgentEvent::StreamDelta { session_id, delta } => {
                            if session_id != self.session_id {
                                continue;
                            }
                            if self.pending_approval.is_none() {
                                self.push_assistant_stream_delta(&delta);
                            }
                        }
                        AgentEvent::StreamToolCallDelta { session_id, delta } => {
                            if session_id != self.session_id {
                                continue;
                            }
                            if self.pending_approval.is_none() {
                                self.push_tool_call_stream_delta(&delta);
                            }
                        }
                        AgentEvent::ApprovalRequested {
                            session_id,
                            command,
                            rule_hits,
                        } => {
                            if session_id != self.session_id {
                                continue;
                            }
                            self.status = format!(
                                "Approval required for: {} ({} rules)",
                                command,
                                rule_hits.len()
                            );
                        }
                        AgentEvent::ApprovalDecided {
                            session_id,
                            command,
                            decision,
                            approved,
                        } => {
                            if session_id != self.session_id {
                                continue;
                            }
                            self.status = if approved {
                                format!("Approval granted ({decision}) for: {command}")
                            } else {
                                format!("Approval denied ({decision}) for: {command}")
                            };
                        }
                        AgentEvent::Error {
                            session_id,
                            message,
                        } => {
                            if session_id != self.session_id {
                                continue;
                            }
                            self.live_stream = None;
                            self.live_render_kind = None;
                            self.live_render_lines.clear();
                            self.pending_live_finalize = None;
                            self.live_preview_rendered = false;
                            let err_lines = self.with_message_gap(
                                self.render_assistant_entries(&format!("Error: {message}")),
                            );
                            self.queue_history_entries(err_lines);
                            self.status = "Ready".to_string();
                        }
                        AgentEvent::MainTurnStarted { session_id } => {
                            if session_id != self.session_id {
                                continue;
                            }
                            self.main_agent_running = true;
                            self.arm_spinner_if_needed();
                            self.status = "Agent is thinking...".to_string();
                        }
                        AgentEvent::MainTurnFinished { session_id } => {
                            if session_id != self.session_id {
                                continue;
                            }
                            self.main_agent_running = false;
                            self.next_spinner_tick = None;
                            self.status = "Ready".to_string();
                        }
                        AgentEvent::CronRunFinished { .. } => {}
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }

        changed
    }

    fn refresh_context_tokens(&mut self) {
        if let Some(tokens) = self.agent.main_context_tokens(&self.session_id) {
            self.context_tokens = tokens;
        }
        while let Ok(context_window) = self.context_window_rx.try_recv() {
            self.context_window = context_window;
        }
    }

    fn drain_approval_requests(&mut self) -> bool {
        let mut changed = false;
        while let Ok(prompt) = self.approval_rx.try_recv() {
            changed = true;
            self.queue_history_entries(self.render_approval_prompt_entries(&prompt.command));
            self.flush_pending_history();
            self.live_stream = None;
            self.live_render_kind = None;
            self.live_render_lines.clear();
            self.pending_live_finalize = None;
            self.live_preview_rendered = false;
            self.pending_approval = Some(ApprovalUiState {
                command: prompt.command,
                options: prompt.options,
                selected_index: 0,
                response_tx: prompt.response_tx,
            });
            self.recompute_viewport(None);
        }
        changed
    }

    fn handle_approval_key(&mut self, key: KeyEvent, stdout: &mut io::Stdout) -> Result<bool> {
        match key.code {
            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                self.deny_all_pending_approvals(stdout)?;
                return Ok(true);
            }
            KeyCode::Up => {
                if let Some(approval) = self.pending_approval.as_mut() {
                    if approval.selected_index == 0 {
                        approval.selected_index = approval.options.len().saturating_sub(1);
                    } else {
                        approval.selected_index -= 1;
                    }
                }
            }
            KeyCode::Down => {
                if let Some(approval) = self.pending_approval.as_mut() {
                    approval.selected_index =
                        (approval.selected_index + 1) % approval.options.len();
                }
            }
            KeyCode::Char('1') if self.input.text.is_empty() => {
                if let Some(approval) = self.pending_approval.as_mut() {
                    approval.selected_index = 0;
                }
            }
            KeyCode::Char('2') if self.input.text.is_empty() => {
                if let Some(approval) = self.pending_approval.as_mut() {
                    approval.selected_index = 1;
                }
            }
            KeyCode::Char('3') if self.input.text.is_empty() => {
                if let Some(approval) = self.pending_approval.as_mut() {
                    approval.selected_index = 2;
                }
            }
            KeyCode::Char('4') if self.input.text.is_empty() => {
                if let Some(approval) = self.pending_approval.as_mut() {
                    approval.selected_index = 3;
                }
            }
            KeyCode::Esc | KeyCode::Enter => {
                if key.code == KeyCode::Enter && !self.input.text.trim().is_empty() {
                    self.deny_all_pending_approvals(stdout)?;
                    self.submit_input_as_user_turn(stdout)?;
                    return Ok(false);
                }
                let Some(approval) = self.pending_approval.as_ref() else {
                    return Ok(false);
                };
                let (selected_index, decision) = if key.code == KeyCode::Esc {
                    (3usize, ApprovalDecision::Forbidden)
                } else {
                    (
                        approval.selected_index,
                        approval.options[approval.selected_index],
                    )
                };
                let command = approval.command.clone();
                let response_tx = approval.response_tx.clone();

                // Exit approval mode first so footer switch creates reclaimable rows.
                self.pending_approval = None;
                self.recompute_viewport(None);
                self.redraw_footer(stdout)?;

                // Immediately fill reclaimed rows with approval snapshot lines.
                self.queue_history_entries(self.render_approval_decision_entries(
                    &command,
                    selected_index,
                    decision,
                ));
                self.flush_pending_history();
                self.redraw_history(stdout)?;

                let _ = response_tx.send(ApprovalResponse { decision });
            }
            KeyCode::Backspace => {
                self.input.backspace();
                self.redraw_footer(stdout)?;
            }
            KeyCode::Delete => {
                self.input.delete_forward();
                self.redraw_footer(stdout)?;
            }
            KeyCode::Left => {
                self.input
                    .move_left(key.modifiers.contains(KeyModifiers::SHIFT));
                self.redraw_footer(stdout)?;
            }
            KeyCode::Right => {
                self.input
                    .move_right(key.modifiers.contains(KeyModifiers::SHIFT));
                self.redraw_footer(stdout)?;
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input.insert_char(c);
                self.redraw_footer(stdout)?;
            }
            _ => {}
        }
        Ok(false)
    }

    fn deny_all_pending_approvals(&mut self, stdout: &mut io::Stdout) -> Result<bool> {
        let mut prompts = Vec::new();
        if let Some(approval) = self.pending_approval.take() {
            prompts.push((approval.command, approval.response_tx));
        }
        while let Ok(prompt) = self.approval_rx.try_recv() {
            prompts.push((prompt.command, prompt.response_tx));
        }
        if prompts.is_empty() {
            return Ok(false);
        }

        self.recompute_viewport(None);
        self.redraw_footer(stdout)?;

        let count = prompts.len();
        for (command, response_tx) in prompts {
            self.queue_history_entries(self.render_approval_decision_entries(
                &command,
                3,
                ApprovalDecision::Forbidden,
            ));
            let _ = response_tx.send(ApprovalResponse {
                decision: ApprovalDecision::Forbidden,
            });
        }
        self.flush_pending_history();
        self.redraw_history(stdout)?;
        self.status = if count == 1 {
            "Denied pending approval.".to_string()
        } else {
            format!("Denied {count} pending approvals.")
        };
        Ok(true)
    }

    fn redraw(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        self.refresh_context_tokens();
        self.redraw_history(stdout)?;
        self.redraw_footer(stdout)?;
        Ok(())
    }

    fn redraw_footer(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        let viewport = self.viewport_state();
        if viewport.cols == 0 || viewport.rows == 0 {
            return Ok(());
        }
        let layout = viewport.chat_container_layout();
        let footer_left = layout.left;
        let footer_width = layout.width;
        let full_width = viewport.cols_usize();

        let expanded_footer = self.footer_expanded();
        // When closing an expanded footer (approval or slash picker), the new
        // compact footer starts lower. Clear the previous taller block first so
        // option rows never remain in the chat area as visual residue.
        if !expanded_footer && self.last_footer_expanded {
            let previous_footer_rows = viewport.rows.min(9);
            let previous_footer_top = viewport.rows.saturating_sub(previous_footer_rows);
            let fill_rows = viewport.footer_top.saturating_sub(previous_footer_top);
            for row in previous_footer_top..viewport.rows {
                stdout.queue(MoveTo(0, row))?;
                fill_container_bg(stdout, row, 0, full_width, ACTIVE_THEME.app_bg)?;
            }
            self.history_fill_rows = fill_rows;
        }

        for row in viewport.footer_top..viewport.rows {
            stdout.queue(MoveTo(0, row))?;
            fill_container_bg(stdout, row, 0, full_width, ACTIVE_THEME.app_bg)?;
        }

        self.draw_loading_line(stdout, viewport)?;

        stdout.queue(MoveTo(footer_left, viewport.top_border_row))?;
        stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
        stdout.queue(SetForegroundColor(ACTIVE_THEME.border_fg))?;
        stdout.queue(Print("─".repeat(footer_width)))?;
        stdout.queue(SetForegroundColor(Color::Reset))?;
        stdout.queue(SetBackgroundColor(Color::Reset))?;

        let bottom_border_row = if self.pending_approval.is_some() {
            viewport.top_border_row.saturating_add(5)
        } else {
            viewport.rows.saturating_sub(3)
        };
        stdout.queue(MoveTo(footer_left, bottom_border_row))?;
        stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
        stdout.queue(SetForegroundColor(ACTIVE_THEME.border_fg))?;
        stdout.queue(Print("─".repeat(footer_width)))?;
        stdout.queue(SetForegroundColor(Color::Reset))?;
        stdout.queue(SetBackgroundColor(Color::Reset))?;

        self.draw_input_line(stdout, viewport)?;
        self.last_pending_approval = self.pending_approval.is_some();
        self.last_footer_expanded = expanded_footer;
        stdout.flush()?;
        Ok(())
    }

    fn draw_loading_line(&self, stdout: &mut io::Stdout, viewport: ViewportState) -> Result<()> {
        let layout = viewport.chat_container_layout();
        let full_width = viewport.cols_usize();
        stdout.queue(MoveTo(0, viewport.loading_row))?;
        fill_container_bg(
            stdout,
            viewport.loading_row,
            0,
            full_width,
            ACTIVE_THEME.app_bg,
        )?;

        if self.main_agent_running {
            let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let spinner_char = spinner[self.loading_phase % spinner.len()];
            stdout.queue(MoveTo(layout.left, viewport.loading_row))?;
            stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
            stdout.queue(SetForegroundColor(ACTIVE_THEME.aux_fg))?;
            stdout.queue(Print(format!("{spinner_char} Agent is thinking...")))?;
            stdout.queue(SetForegroundColor(Color::Reset))?;
            stdout.queue(SetBackgroundColor(Color::Reset))?;
        }

        Ok(())
    }

    fn redraw_history(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        let viewport = self.viewport_state();
        if viewport.cols == 0 || viewport.rows == 0 {
            return Ok(());
        }

        if viewport.footer_top == 0 {
            return Ok(());
        }

        if self.rendered_history_entries > self.committed_history.len() {
            self.rendered_history_entries = self.committed_history.len();
        }
        if self.pending_preview_clear_lines > 0 {
            let layout = viewport.chat_container_layout();
            let clear_count = self.pending_preview_clear_lines;
            for offset in 0..clear_count {
                self.rewrite_recent_history_line(
                    stdout,
                    layout.left,
                    offset as u16,
                    "",
                    None,
                    None,
                )?;
            }
            self.pending_preview_clear_lines = 0;
        }
        if self.rendered_history_entries < self.committed_history.len() {
            let new_entries = self.committed_history[self.rendered_history_entries..].to_vec();
            for entry in new_entries {
                for line in self.render_history_entry_lines(&entry, viewport) {
                    self.append_history_line(
                        stdout,
                        line.left,
                        &line.text,
                        line.fg,
                        line.bg,
                        line.styled_start,
                    )?;
                }
            }
            self.rendered_history_entries = self.committed_history.len();
        }

        self.redraw_live_stream(stdout)?;
        stdout.flush()?;
        Ok(())
    }

    fn restore_revealed_history_rows(
        &self,
        stdout: &mut io::Stdout,
        previous_footer_top: u16,
    ) -> Result<()> {
        let viewport = self.viewport_state();
        if viewport.cols == 0 || viewport.rows == 0 || previous_footer_top >= viewport.footer_top {
            return Ok(());
        }

        let history_lines = self.render_history_lines_for_viewport(viewport);
        for row in previous_footer_top..viewport.footer_top {
            let line = visible_history_line_index(row, viewport.footer_top, history_lines.len())
                .and_then(|line_index| history_lines.get(line_index));
            if let Some(line) = line {
                self.draw_history_line_at(stdout, row, line)?;
            } else {
                stdout.queue(MoveTo(0, row))?;
                fill_container_bg(stdout, row, 0, viewport.cols_usize(), ACTIVE_THEME.app_bg)?;
            }
        }
        Ok(())
    }

    fn render_history_lines_for_viewport(
        &self,
        viewport: ViewportState,
    ) -> Vec<RenderedHistoryLine> {
        let mut lines = Vec::new();
        for entry in &self.committed_history {
            lines.extend(self.render_history_entry_lines(entry, viewport));
        }
        lines
    }

    fn render_history_entry_lines(
        &self,
        entry: &HistoryEntry,
        viewport: ViewportState,
    ) -> Vec<RenderedHistoryLine> {
        let layout = viewport.chat_container_layout();
        let entry_width = if entry.full_screen {
            viewport.cols_usize().max(1)
        } else {
            layout.width
        };
        let entry_left = if entry.full_screen { 0 } else { layout.left };
        let lines = if entry.bubble {
            let full_inner_width = entry_width
                .saturating_sub(BUBBLE_HORIZONTAL_PADDING.saturating_mul(2))
                .max(1);
            let inner_width = if entry.right_align {
                let min_total_width = BUBBLE_HORIZONTAL_PADDING
                    .saturating_mul(2)
                    .saturating_add(1);
                let user_max_total_width =
                    entry_width.saturating_mul(USER_BUBBLE_MAX_WIDTH_PERCENT) / 100;
                let user_max_inner_width = user_max_total_width
                    .max(min_total_width)
                    .saturating_sub(BUBBLE_HORIZONTAL_PADDING.saturating_mul(2))
                    .max(1);
                full_inner_width.min(user_max_inner_width)
            } else {
                full_inner_width
            };
            if entry.bubble_full_width {
                build_assistant_bubble_from_rendered_text(&entry.text, inner_width)
            } else {
                let mut content = wrap_text_display_width(&entry.text, inner_width);
                if content.is_empty() {
                    content.push(String::new());
                }
                let content_width = content
                    .iter()
                    .map(|line| display_width(line))
                    .max()
                    .unwrap_or(0)
                    .min(inner_width);
                let mut bubble_lines = Vec::with_capacity(content.len() + 2);
                bubble_lines.push(" ".repeat(content_width + BUBBLE_HORIZONTAL_PADDING * 2));
                for line in content {
                    bubble_lines.push(render_bubble_content_line(&line, content_width));
                }
                bubble_lines.push(" ".repeat(content_width + BUBBLE_HORIZONTAL_PADDING * 2));
                bubble_lines
            }
        } else {
            let plain = format!("{}{}", entry.label, entry.text);
            if entry.no_wrap {
                vec![plain]
            } else {
                wrap_text_display_width(&plain, entry_width.saturating_sub(1))
            }
        };

        lines
            .into_iter()
            .map(|line| {
                let (text, styled_start) = if entry.right_align {
                    let pad = entry_width.saturating_sub(display_width(&line));
                    (format!("{}{}", " ".repeat(pad), line), pad)
                } else {
                    (line, 0)
                };
                RenderedHistoryLine {
                    left: entry_left,
                    text,
                    fg: entry.fg,
                    bg: entry.bg,
                    styled_start,
                }
            })
            .collect()
    }

    fn draw_history_line_at(
        &self,
        stdout: &mut io::Stdout,
        row: u16,
        line: &RenderedHistoryLine,
    ) -> Result<()> {
        let viewport = self.viewport_state();
        stdout.queue(MoveTo(0, row))?;
        fill_container_bg(stdout, row, 0, viewport.cols_usize(), ACTIVE_THEME.app_bg)?;
        stdout.queue(MoveTo(line.left, row))?;
        let (prefix, styled) = if line.styled_start > 0 && line.styled_start <= line.text.len() {
            line.text.split_at(line.styled_start)
        } else {
            ("", line.text.as_str())
        };
        stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
        if !prefix.is_empty() {
            stdout.queue(Print(prefix))?;
        }
        let effective_bg = line.bg.unwrap_or(ACTIVE_THEME.app_bg);
        stdout.queue(SetBackgroundColor(effective_bg))?;
        if let Some(color) = line.fg {
            stdout.queue(SetForegroundColor(color))?;
            stdout.queue(Print(styled))?;
            stdout.queue(SetForegroundColor(Color::Reset))?;
        } else {
            stdout.queue(Print(styled))?;
        }
        stdout.queue(SetBackgroundColor(Color::Reset))?;
        Ok(())
    }

    fn redraw_live_stream(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        let viewport = self.viewport_state();
        if viewport.cols == 0 || viewport.footer_top == 0 {
            return Ok(());
        }

        let Some(mut stream) = self.live_stream.take() else {
            if let Some(pending) = self.pending_live_finalize.take() {
                let layout = viewport.chat_container_layout();
                let (fg, bg) = match pending.kind {
                    LiveStreamKind::Assistant => {
                        (Some(ACTIVE_THEME.ai_fg), Some(ACTIVE_THEME.ai_bg))
                    }
                    LiveStreamKind::ToolCall => (Some(ACTIVE_THEME.aux_fg), None),
                };
                let ops = apply_live_line_updates(&mut self.live_render_lines, &pending.lines);
                let assistant_blank_row = " ".repeat(layout.width.max(1));
                for op in ops {
                    match op {
                        StreamPreviewOp::Append(line) => {
                            self.append_history_line(stdout, layout.left, &line, fg, bg, 0)?;
                        }
                        StreamPreviewOp::RewriteRecent {
                            offset_from_bottom,
                            line,
                        } => {
                            let line = if matches!(pending.kind, LiveStreamKind::Assistant)
                                && line.is_empty()
                            {
                                assistant_blank_row.clone()
                            } else {
                                line
                            };
                            self.rewrite_recent_history_line(
                                stdout,
                                layout.left,
                                offset_from_bottom,
                                &line,
                                fg,
                                bg,
                            )?;
                        }
                    }
                }
                self.live_render_kind = None;
                self.live_render_lines.clear();
                if pending.append_gap_after {
                    self.append_history_line(stdout, layout.left, "", None, None, 0)?;
                }
                return Ok(());
            }
            self.live_render_kind = None;
            self.live_render_lines.clear();
            return Ok(());
        };
        let stream_kind = stream.kind;
        let layout = viewport.chat_container_layout();

        if self.live_render_kind != Some(stream_kind) {
            self.live_render_kind = Some(stream_kind);
            self.live_render_lines.clear();
            self.live_preview_rendered = false;
            stream.consumed_bytes = 0;
            stream.current_line.clear();
            stream.current_line_width = 0;
            stream.started = false;
            stream.assistant_preview = None;
        }

        let (max_width, fg, bg) = match stream_kind {
            LiveStreamKind::Assistant => (
                layout
                    .width
                    .saturating_sub(BUBBLE_HORIZONTAL_PADDING.saturating_mul(2))
                    .max(1),
                Some(ACTIVE_THEME.ai_fg),
                Some(ACTIVE_THEME.ai_bg),
            ),
            LiveStreamKind::ToolCall => (
                layout.width.saturating_sub(1),
                Some(ACTIVE_THEME.aux_fg),
                None,
            ),
        };

        match stream_kind {
            LiveStreamKind::Assistant => {
                let rendered = stream_render_assistant_markdown_lines(&mut stream, max_width);
                let ops = apply_live_line_updates(&mut self.live_render_lines, &rendered);
                let assistant_blank_row = " ".repeat(layout.width.max(1));
                for op in ops {
                    match op {
                        StreamPreviewOp::Append(line) => {
                            self.append_history_line(stdout, layout.left, &line, fg, bg, 0)?;
                        }
                        StreamPreviewOp::RewriteRecent {
                            offset_from_bottom,
                            line,
                        } => {
                            let line = if line.is_empty() {
                                assistant_blank_row.clone()
                            } else {
                                line
                            };
                            self.rewrite_recent_history_line(
                                stdout,
                                layout.left,
                                offset_from_bottom,
                                &line,
                                fg,
                                bg,
                            )?;
                        }
                    }
                }
                self.live_preview_rendered = rendered.iter().any(|line| !line.trim().is_empty());
            }
            LiveStreamKind::ToolCall => {
                if stream.text.is_empty() || stream.consumed_bytes >= stream.text.len() {
                    self.live_stream = Some(stream);
                    return Ok(());
                }
                let delta_owned = stream.text[stream.consumed_bytes..].to_string();
                stream.consumed_bytes = stream.text.len();
                let ops = stream_preview_apply_delta(
                    stream_kind,
                    max_width,
                    &delta_owned,
                    &mut stream,
                    &mut self.live_render_lines,
                );
                for op in ops {
                    match op {
                        StreamPreviewOp::Append(line) => {
                            self.append_history_line(stdout, layout.left, &line, fg, bg, 0)?;
                            if !line.is_empty() {
                                self.live_preview_rendered = true;
                            }
                        }
                        StreamPreviewOp::RewriteRecent {
                            offset_from_bottom,
                            line,
                        } => {
                            self.rewrite_recent_history_line(
                                stdout,
                                layout.left,
                                offset_from_bottom,
                                &line,
                                fg,
                                bg,
                            )?;
                            if !line.is_empty() {
                                self.live_preview_rendered = true;
                            }
                        }
                    }
                }
            }
        }
        self.live_stream = Some(stream);
        Ok(())
    }

    fn append_history_line(
        &mut self,
        stdout: &mut io::Stdout,
        left: u16,
        line: &str,
        fg: Option<Color>,
        bg: Option<Color>,
        styled_start: usize,
    ) -> Result<()> {
        let viewport = self.viewport_state();
        if viewport.footer_top == 0 {
            return Ok(());
        }
        let full_width = viewport.cols_usize();
        let history_row = viewport.footer_top.saturating_sub(1);
        let target_row = if self.history_fill_rows > 0 && self.pending_approval.is_none() {
            let row = history_row
                .saturating_sub(self.history_fill_rows)
                .saturating_add(1);
            self.history_fill_rows = self.history_fill_rows.saturating_sub(1);
            row
        } else {
            stdout.queue(ScrollUp(1))?;
            history_row
        };
        stdout.queue(MoveTo(0, target_row))?;
        fill_container_bg(stdout, target_row, 0, full_width, ACTIVE_THEME.app_bg)?;
        stdout.queue(MoveTo(left, target_row))?;
        let (prefix, styled) = if styled_start > 0 && styled_start <= line.len() {
            line.split_at(styled_start)
        } else {
            ("", line)
        };
        stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
        if !prefix.is_empty() {
            stdout.queue(Print(prefix))?;
        }
        let effective_bg = bg.unwrap_or(ACTIVE_THEME.app_bg);
        stdout.queue(SetBackgroundColor(effective_bg))?;
        if let Some(color) = fg {
            stdout.queue(SetForegroundColor(color))?;
            stdout.queue(Print(styled))?;
            stdout.queue(SetForegroundColor(Color::Reset))?;
        } else {
            stdout.queue(Print(styled))?;
        }
        stdout.queue(SetBackgroundColor(Color::Reset))?;
        Ok(())
    }

    fn rewrite_recent_history_line(
        &self,
        stdout: &mut io::Stdout,
        left: u16,
        offset_from_bottom: u16,
        line: &str,
        fg: Option<Color>,
        bg: Option<Color>,
    ) -> Result<()> {
        let viewport = self.viewport_state();
        if viewport.footer_top == 0 {
            return Ok(());
        }
        let full_width = viewport.cols_usize();
        let history_row = viewport
            .footer_top
            .saturating_sub(1)
            .saturating_sub(offset_from_bottom);
        stdout.queue(MoveTo(0, history_row))?;
        fill_container_bg(stdout, history_row, 0, full_width, ACTIVE_THEME.app_bg)?;
        stdout.queue(MoveTo(left, history_row))?;
        let effective_bg = bg.unwrap_or(ACTIVE_THEME.app_bg);
        stdout.queue(SetBackgroundColor(effective_bg))?;
        if let Some(color) = fg {
            stdout.queue(SetForegroundColor(color))?;
            stdout.queue(Print(line))?;
            stdout.queue(SetForegroundColor(Color::Reset))?;
        } else {
            stdout.queue(Print(line))?;
        }
        stdout.queue(SetBackgroundColor(Color::Reset))?;
        Ok(())
    }

    fn draw_input_line(&self, stdout: &mut io::Stdout, viewport: ViewportState) -> Result<()> {
        let layout = viewport.chat_container_layout();
        let cols_usize = layout.width;
        let full_width = viewport.cols_usize();
        let left = layout.left;

        if let Some(approval) = &self.pending_approval {
            let option_lines = [
                "1. once - allow only this command",
                "2. session - allow matching rules for this session",
                "3. always - allow matching rules permanently",
                "4. forbidden - block this command",
            ];

            let options_start_row = viewport.top_border_row.saturating_add(1);
            for (idx, line) in option_lines.iter().enumerate() {
                let row = options_start_row.saturating_add(idx as u16);
                if row >= viewport.top_border_row.saturating_add(5) {
                    break;
                }
                stdout.queue(MoveTo(left, row))?;
                fill_container_bg(stdout, row, 0, full_width, ACTIVE_THEME.app_bg)?;
                stdout.queue(MoveTo(left, row))?;
                if approval.selected_index == idx {
                    stdout.queue(SetBackgroundColor(ACTIVE_THEME.select_bg))?;
                    stdout.queue(SetForegroundColor(ACTIVE_THEME.select_fg))?;
                    stdout.queue(Print(truncate_line_display_width(
                        &format!("➤ {line}"),
                        cols_usize,
                    )))?;
                    stdout.queue(SetBackgroundColor(Color::Reset))?;
                    stdout.queue(SetForegroundColor(Color::Reset))?;
                } else {
                    stdout.queue(SetForegroundColor(ACTIVE_THEME.aux_fg))?;
                    stdout.queue(Print(truncate_line_display_width(
                        &format!("  {line}"),
                        cols_usize,
                    )))?;
                    stdout.queue(SetForegroundColor(Color::Reset))?;
                }
            }

            let status_row = viewport.top_border_row.saturating_add(6);
            let empty_row = viewport.top_border_row.saturating_add(7);
            stdout.queue(MoveTo(left, status_row))?;
            fill_container_bg(stdout, status_row, 0, full_width, ACTIVE_THEME.app_bg)?;
            stdout.queue(MoveTo(left, status_row))?;
            stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
            stdout.queue(SetForegroundColor(ACTIVE_THEME.status_fg))?;
            let help = self.status_bar_text();
            stdout.queue(Print(truncate_line_display_width(&help, cols_usize)))?;
            stdout.queue(SetForegroundColor(Color::Reset))?;
            stdout.queue(SetBackgroundColor(Color::Reset))?;
            stdout.queue(MoveTo(left, empty_row))?;
            fill_container_bg(stdout, empty_row, 0, full_width, ACTIVE_THEME.app_bg)?;
            stdout.queue(MoveTo(left, empty_row))?;
            stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
            stdout.queue(SetForegroundColor(ACTIVE_THEME.accent))?;
            stdout.queue(Print("❯ "))?;
            stdout.queue(SetForegroundColor(ACTIVE_THEME.input_fg))?;
            let available_width = cols_usize.saturating_sub(3);
            let layout = self.input.visible_layout(available_width);
            for (idx, ch) in self.input.text.chars().enumerate() {
                if idx < layout.start || idx >= layout.end {
                    continue;
                }
                if idx == self.input.cursor {
                    draw_cursor_cell(stdout, ch)?;
                } else {
                    stdout.queue(Print(ch))?;
                }
            }
            if self.input.cursor == self.input.text.chars().count() {
                draw_cursor_cell(stdout, ' ')?;
            }
            stdout.queue(SetForegroundColor(Color::Reset))?;
            stdout.queue(SetBackgroundColor(Color::Reset))?;
            return Ok(());
        }

        if let Some(picker) = &self.slash_picker {
            let matches = matching_slash_commands(&self.input.text);
            let start_row = viewport.top_border_row.saturating_add(1);
            for idx in 0..SLASH_COMMANDS.len() {
                let row = start_row.saturating_add(idx as u16);
                if row >= viewport.input_row {
                    break;
                }
                stdout.queue(MoveTo(left, row))?;
                fill_container_bg(stdout, row, 0, full_width, ACTIVE_THEME.app_bg)?;
                if let Some(spec) = matches.get(idx) {
                    let line = format!("{}  {}", spec.command, spec.hint);
                    if picker.selected_index == idx {
                        fill_container_bg(stdout, row, left, cols_usize, ACTIVE_THEME.select_bg)?;
                        stdout.queue(MoveTo(left, row))?;
                        stdout.queue(SetBackgroundColor(ACTIVE_THEME.select_bg))?;
                        stdout.queue(SetForegroundColor(ACTIVE_THEME.select_fg))?;
                        stdout.queue(Print(truncate_line_display_width(
                            &format!("➤ {line}"),
                            cols_usize,
                        )))?;
                        stdout.queue(SetBackgroundColor(Color::Reset))?;
                        stdout.queue(SetForegroundColor(Color::Reset))?;
                    } else {
                        fill_container_bg(stdout, row, left, cols_usize, ACTIVE_THEME.ai_bg)?;
                        stdout.queue(MoveTo(left, row))?;
                        stdout.queue(SetBackgroundColor(ACTIVE_THEME.ai_bg))?;
                        stdout.queue(SetForegroundColor(ACTIVE_THEME.aux_fg))?;
                        stdout.queue(Print(truncate_line_display_width(
                            &format!("  {line}"),
                            cols_usize,
                        )))?;
                        stdout.queue(SetForegroundColor(Color::Reset))?;
                        stdout.queue(SetBackgroundColor(Color::Reset))?;
                    }
                }
            }
        }

        stdout.queue(MoveTo(left, viewport.input_row))?;
        fill_container_bg(
            stdout,
            viewport.input_row,
            0,
            full_width,
            ACTIVE_THEME.app_bg,
        )?;
        stdout.queue(MoveTo(left, viewport.input_row))?;
        stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
        stdout.queue(SetForegroundColor(ACTIVE_THEME.input_fg))?;

        let prefix_width = 2usize;
        stdout.queue(SetForegroundColor(ACTIVE_THEME.accent))?;
        stdout.queue(Print("❯ "))?;
        stdout.queue(SetForegroundColor(ACTIVE_THEME.input_fg))?;

        let available_width = cols_usize.saturating_sub(prefix_width + 1);
        let layout = self.input.visible_layout(available_width);
        let selection = self.input.selection_range();
        let mut cursor_x = prefix_width;

        for (idx, ch) in self.input.text.chars().enumerate() {
            if idx < layout.start || idx >= layout.end {
                continue;
            }

            if idx == self.input.cursor {
                draw_cursor_cell(stdout, ch)?;
            } else if selection.as_ref().is_some_and(|range| range.contains(&idx)) {
                stdout.queue(SetBackgroundColor(ACTIVE_THEME.select_bg))?;
                stdout.queue(SetForegroundColor(ACTIVE_THEME.select_fg))?;
                stdout.queue(Print(ch))?;
                stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
                stdout.queue(SetForegroundColor(ACTIVE_THEME.input_fg))?;
            } else {
                stdout.queue(Print(ch))?;
            }

            if idx < self.input.cursor {
                cursor_x += char_display_width(ch);
            }
        }

        if self.input.cursor == self.input.text.chars().count() {
            draw_cursor_cell(stdout, ' ')?;
        }

        stdout.queue(MoveTo(left, viewport.help_row))?;
        fill_container_bg(
            stdout,
            viewport.help_row,
            0,
            full_width,
            ACTIVE_THEME.app_bg,
        )?;
        stdout.queue(MoveTo(left, viewport.help_row))?;
        stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
        stdout.queue(SetForegroundColor(ACTIVE_THEME.status_fg))?;

        let help = self.status_bar_text();
        stdout.queue(Print(truncate_line_display_width(&help, cols_usize)))?;
        stdout.queue(SetForegroundColor(Color::Reset))?;
        stdout.queue(SetBackgroundColor(Color::Reset))?;

        stdout.queue(MoveTo(left, viewport.bottom_row))?;
        fill_container_bg(
            stdout,
            viewport.bottom_row,
            0,
            full_width,
            ACTIVE_THEME.app_bg,
        )?;
        stdout.queue(SetBackgroundColor(Color::Reset))?;
        stdout.queue(MoveTo(
            left.saturating_add(cursor_x as u16),
            viewport.input_row,
        ))?;

        Ok(())
    }

    fn status_bar_text(&self) -> String {
        let context = match self.context_window {
            Some(total) => format!(
                "context: {}/{}",
                self.context_tokens,
                format_compact_u64(total)
            ),
            None => format!("context: {}/?", self.context_tokens),
        };
        format!(
            "{} | model: {} | {}",
            self.session_title, self.model_name, context
        )
    }

    fn render_ui_entries(&self, message: &UiMessage) -> Vec<HistoryEntry> {
        match message.msg_type {
            MessageType::User => self.with_message_gap(self.render_user_entries(&message.content)),
            MessageType::Assistant => {
                self.with_message_gap(self.render_assistant_entries(&message.content))
            }
            MessageType::ToolCall => {
                self.wrap_history_entry("🔧 ", &message.content, Some(ACTIVE_THEME.aux_fg), false)
            }
            MessageType::ToolResult => {
                self.wrap_history_entry("  ", &message.content, Some(ACTIVE_THEME.aux_fg), false)
            }
        }
    }

    fn render_user_entries(&self, text: &str) -> Vec<HistoryEntry> {
        vec![HistoryEntry {
            label: String::new(),
            text: text.to_string(),
            fg: Some(ACTIVE_THEME.user_fg),
            bg: Some(ACTIVE_THEME.user_bg),
            right_align: true,
            bubble: true,
            bubble_full_width: false,
            no_wrap: false,
            full_screen: false,
        }]
    }

    fn render_assistant_entries(&self, text: &str) -> Vec<HistoryEntry> {
        let inner_width = self
            .viewport_state()
            .chat_container_layout()
            .width
            .saturating_sub(BUBBLE_HORIZONTAL_PADDING.saturating_mul(2))
            .max(1);
        let rendered = render_assistant_markdown(text, inner_width);
        vec![HistoryEntry {
            label: String::new(),
            text: rendered,
            fg: Some(ACTIVE_THEME.ai_fg),
            bg: Some(ACTIVE_THEME.ai_bg),
            right_align: false,
            bubble: true,
            bubble_full_width: true,
            no_wrap: false,
            full_screen: false,
        }]
    }

    fn render_approval_prompt_entries(&self, command: &str) -> Vec<HistoryEntry> {
        vec![HistoryEntry {
            label: "Approval: ".to_string(),
            text: format!("Command requires confirmation: {command}"),
            fg: Some(ACTIVE_THEME.aux_fg),
            bg: None,
            right_align: false,
            bubble: false,
            bubble_full_width: false,
            no_wrap: false,
            full_screen: false,
        }]
    }

    fn render_approval_decision_entries(
        &self,
        _command: &str,
        selected_index: usize,
        _decision: ApprovalDecision,
    ) -> Vec<HistoryEntry> {
        let option_lines = [
            "1. once - allow only this command",
            "2. session - allow matching rules for this session",
            "3. always - allow matching rules permanently",
            "4. forbidden - block this command",
        ];
        let mut entries = Vec::new();
        entries.push(HistoryEntry {
            label: String::new(),
            text: String::new(),
            fg: None,
            bg: None,
            right_align: false,
            bubble: false,
            bubble_full_width: false,
            no_wrap: false,
            full_screen: false,
        });
        for (idx, line) in option_lines.iter().enumerate() {
            let selected = idx == selected_index;
            entries.push(HistoryEntry {
                label: if selected {
                    "➤ ".to_string()
                } else {
                    "  ".to_string()
                },
                text: (*line).to_string(),
                fg: if selected {
                    Some(ACTIVE_THEME.select_fg)
                } else {
                    Some(ACTIVE_THEME.aux_fg)
                },
                bg: if selected {
                    Some(ACTIVE_THEME.select_bg)
                } else {
                    None
                },
                right_align: false,
                bubble: false,
                bubble_full_width: false,
                no_wrap: false,
                full_screen: false,
            });
        }
        entries.push(HistoryEntry {
            label: String::new(),
            text: String::new(),
            fg: None,
            bg: None,
            right_align: false,
            bubble: false,
            bubble_full_width: false,
            no_wrap: false,
            full_screen: false,
        });
        entries
    }

    fn push_assistant_stream_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let stream = self.ensure_live_stream(LiveStreamKind::Assistant);
        stream.text.push_str(delta);
    }

    fn push_tool_call_stream_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let stream = self.ensure_live_stream(LiveStreamKind::ToolCall);
        stream.raw_tool_args.push_str(delta);
        stream.text = preview_tool_command(&stream.raw_tool_args);
    }

    fn ensure_live_stream(&mut self, kind: LiveStreamKind) -> &mut LiveStreamState {
        let needs_reset = self
            .live_stream
            .as_ref()
            .is_none_or(|stream| stream.kind != kind);
        if needs_reset {
            self.live_stream = Some(LiveStreamState {
                kind,
                text: String::new(),
                raw_tool_args: String::new(),
                consumed_bytes: 0,
                current_line: String::new(),
                current_line_width: 0,
                started: false,
                assistant_preview: None,
            });
            self.live_render_kind = None;
            self.live_render_lines.clear();
            self.pending_live_finalize = None;
            self.live_preview_rendered = false;
        }
        self.live_stream
            .as_mut()
            .expect("live stream just initialized")
    }

    fn finish_live_stream_for_message_type(
        &mut self,
        msg_type: MessageType,
        final_message_content: Option<&str>,
    ) -> bool {
        let Some(stream) = self.live_stream.as_ref() else {
            return false;
        };
        let matches = matches!(
            (stream.kind, msg_type),
            (LiveStreamKind::Assistant, MessageType::Assistant)
                | (LiveStreamKind::ToolCall, MessageType::ToolCall)
        );
        if matches {
            let stream_kind = stream.kind;
            let has_live_lines = !self.live_render_lines.is_empty();
            let previous_committed_len = self.committed_history.len();
            let suppress_final = should_suppress_final_message(
                stream_kind,
                self.live_preview_rendered,
                has_live_lines,
                !stream.text.is_empty(),
            );
            if !suppress_final && has_live_lines {
                self.pending_preview_clear_lines = self.live_render_lines.len();
            }
            if suppress_final {
                match stream_kind {
                    LiveStreamKind::Assistant => {
                        let inner_width = self
                            .viewport_state()
                            .chat_container_layout()
                            .width
                            .saturating_sub(BUBBLE_HORIZONTAL_PADDING.saturating_mul(2))
                            .max(1);
                        let final_text = choose_assistant_commit_text(
                            &stream.text,
                            final_message_content.unwrap_or(""),
                        );
                        let rendered_text = render_assistant_markdown(final_text, inner_width);
                        let final_lines =
                            build_assistant_bubble_from_rendered_text(&rendered_text, inner_width);
                        self.committed_history.push(HistoryEntry {
                            label: String::new(),
                            text: rendered_text,
                            fg: Some(ACTIVE_THEME.ai_fg),
                            bg: Some(ACTIVE_THEME.ai_bg),
                            right_align: false,
                            bubble: true,
                            bubble_full_width: true,
                            no_wrap: false,
                            full_screen: false,
                        });
                        self.committed_history.push(self.blank_line_entry());
                        self.pending_live_finalize = Some(PendingLiveFinalize {
                            kind: LiveStreamKind::Assistant,
                            lines: final_lines,
                            append_gap_after: true,
                        });
                    }
                    LiveStreamKind::ToolCall => {
                        if let Some(entry) = self.live_stream_history_entry() {
                            self.committed_history.push(entry);
                        }
                    }
                }
                self.rendered_history_entries = if matches!(stream_kind, LiveStreamKind::Assistant)
                {
                    self.committed_history.len()
                } else {
                    previous_committed_len
                };
            }
            self.live_stream = None;
            self.live_render_kind = None;
            if !suppress_final {
                self.live_render_lines.clear();
                self.pending_live_finalize = None;
            }
            self.live_preview_rendered = false;
            return suppress_final;
        }
        false
    }

    fn wrap_history_entry(
        &self,
        label: &str,
        text: &str,
        fg: Option<Color>,
        right_align: bool,
    ) -> Vec<HistoryEntry> {
        vec![HistoryEntry {
            label: label.to_string(),
            text: text.to_string(),
            fg,
            bg: None,
            right_align,
            bubble: false,
            bubble_full_width: false,
            no_wrap: false,
            full_screen: false,
        }]
    }

    fn with_message_gap(&self, mut entries: Vec<HistoryEntry>) -> Vec<HistoryEntry> {
        entries.push(self.blank_line_entry());
        entries
    }

    fn blank_line_entry(&self) -> HistoryEntry {
        HistoryEntry {
            label: String::new(),
            text: String::new(),
            fg: None,
            bg: None,
            right_align: false,
            bubble: false,
            bubble_full_width: false,
            no_wrap: false,
            full_screen: false,
        }
    }

    fn live_stream_history_entry(&self) -> Option<HistoryEntry> {
        let stream = self.live_stream.as_ref()?;
        match stream.kind {
            LiveStreamKind::Assistant => Some(HistoryEntry {
                label: String::new(),
                text: finalized_assistant_stream_text(
                    stream,
                    self.viewport_state()
                        .chat_container_layout()
                        .width
                        .saturating_sub(BUBBLE_HORIZONTAL_PADDING.saturating_mul(2))
                        .max(1),
                ),
                fg: Some(ACTIVE_THEME.ai_fg),
                bg: Some(ACTIVE_THEME.ai_bg),
                right_align: false,
                bubble: true,
                bubble_full_width: true,
                no_wrap: false,
                full_screen: false,
            }),
            LiveStreamKind::ToolCall => Some(HistoryEntry {
                label: "🔧 $ ".to_string(),
                text: stream.text.clone(),
                fg: Some(ACTIVE_THEME.aux_fg),
                bg: None,
                right_align: false,
                bubble: false,
                bubble_full_width: false,
                no_wrap: false,
                full_screen: false,
            }),
        }
    }

    fn render_startup_braille_entries(&self) -> (Vec<HistoryEntry>, Option<StartupAnimationState>) {
        let viewport = self.viewport_state();
        let screen_width = viewport.cols_usize().saturating_sub(1).max(1);
        let image_rows = viewport.rows.saturating_sub(6).max(1);
        let mut frames = load_startup_image_braille_frames(image_rows, viewport.cols);
        if frames.is_empty() {
            return (Vec::new(), None);
        }

        for frame in &mut frames {
            for line in &mut frame.lines {
                if screen_width > line.display_width {
                    let pad = (screen_width - line.display_width) / 2;
                    line.ansi_text = format!("{}{}", " ".repeat(pad), line.ansi_text);
                }
            }
        }

        let first_lines = frames
            .first()
            .map(|f| {
                f.lines
                    .iter()
                    .map(|l| l.ansi_text.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut entries = Vec::with_capacity(first_lines.len() + 1);
        for line in first_lines {
            entries.push(HistoryEntry {
                label: String::new(),
                text: line,
                fg: None,
                bg: None,
                right_align: false,
                bubble: false,
                bubble_full_width: false,
                no_wrap: true,
                full_screen: true,
            });
        }
        entries.push(self.blank_line_entry());
        let image_line_count = entries.len().saturating_sub(1);
        let animation = if frames.len() > 1 && image_line_count > 0 {
            Some(StartupAnimationState {
                frames,
                frame_index: 0,
                next_tick: Instant::now() + Duration::from_millis(120),
                image_line_count,
                history_entry_count: entries.len(),
            })
        } else {
            None
        };
        (entries, animation)
    }

    fn rebuild_startup_on_resize(&mut self) {
        let Some(anim) = self.startup_animation.as_ref() else {
            return;
        };
        let startup_only = self.committed_history.len() == anim.history_entry_count
            && self.pending_history.is_empty()
            && self.live_stream.is_none()
            && self.pending_approval.is_none();
        if !startup_only {
            self.startup_animation = None;
            return;
        }

        let (entries, animation) = self.render_startup_braille_entries();
        if entries.is_empty() {
            self.startup_animation = None;
            return;
        }
        self.committed_history = entries;
        self.pending_history.clear();
        self.rendered_history_entries = 0;
        self.startup_animation = animation;
    }
}

fn preview_tool_command(raw_args: &str) -> String {
    extract_partial_command_value(raw_args).unwrap_or_default()
}

fn slash_picker_should_show(input: &InputState) -> bool {
    input.selection_range().is_none()
        && input.cursor == input.text.chars().count()
        && input.text.starts_with('/')
        && !input.text.chars().any(char::is_whitespace)
}

fn matching_slash_commands(input: &str) -> Vec<&'static SlashCommandSpec> {
    if !input.starts_with('/') || input.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .filter(|spec| spec.command.starts_with(input))
        .collect()
}

fn visible_history_line_index(row: u16, visible_rows: u16, line_count: usize) -> Option<usize> {
    let visible_rows = visible_rows as usize;
    let row = row as usize;
    if row >= visible_rows {
        return None;
    }

    let visible_line_count = line_count.min(visible_rows);
    let first_visible = line_count.saturating_sub(visible_line_count);
    let blank_top_rows = visible_rows.saturating_sub(visible_line_count);
    row.checked_sub(blank_top_rows)
        .filter(|offset| *offset < visible_line_count)
        .map(|offset| first_visible + offset)
}

fn extract_partial_command_value(raw_args: &str) -> Option<String> {
    let marker = "\"command\":\"";
    let start = raw_args.find(marker)? + marker.len();
    let mut escaped = false;
    let mut out = String::new();

    for ch in raw_args[start..].chars() {
        if escaped {
            match ch {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                other => out.push(other),
            }
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => break,
            other => out.push(other),
        }
    }

    Some(out)
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(strip_sgr_ansi(s).as_str())
}

fn char_display_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WrapTokenKind {
    Word,
    Space,
    Other,
}

struct WrapToken {
    text: String,
    width: usize,
    kind: WrapTokenKind,
}

fn is_non_breaking_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '\''
}

fn tokenize_wrappable_line(raw_line: &str) -> Vec<WrapToken> {
    let mut tokens = Vec::new();
    let mut current_text = String::new();
    let mut current_width = 0usize;
    let mut current_kind: Option<WrapTokenKind> = None;
    let mut pending_prefix_ansi = String::new();

    let mut i = 0usize;
    while i < raw_line.len() {
        if let Some(end) = take_ansi_escape_end(raw_line, i) {
            if current_kind.is_some() {
                current_text.push_str(&raw_line[i..end]);
            } else {
                pending_prefix_ansi.push_str(&raw_line[i..end]);
            }
            i = end;
            continue;
        }

        let Some(ch) = raw_line[i..].chars().next() else {
            break;
        };
        let ch_width = char_display_width(ch);
        let kind = if ch.is_whitespace() {
            WrapTokenKind::Space
        } else if is_non_breaking_word_char(ch) {
            WrapTokenKind::Word
        } else {
            WrapTokenKind::Other
        };

        match current_kind {
            None => {
                current_text.push_str(&pending_prefix_ansi);
                pending_prefix_ansi.clear();
                current_text.push(ch);
                current_width = ch_width;
                current_kind = Some(kind);
            }
            Some(existing) if existing == kind => {
                current_text.push(ch);
                current_width = current_width.saturating_add(ch_width);
            }
            Some(existing) => {
                tokens.push(WrapToken {
                    text: std::mem::take(&mut current_text),
                    width: current_width,
                    kind: existing,
                });
                current_text.push_str(&pending_prefix_ansi);
                pending_prefix_ansi.clear();
                current_text.push(ch);
                current_width = ch_width;
                current_kind = Some(kind);
            }
        }
        i += ch.len_utf8();
    }

    if let Some(kind) = current_kind {
        tokens.push(WrapToken {
            text: current_text,
            width: current_width,
            kind,
        });
    }

    tokens
}

fn wrap_hard_by_display_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut i = 0usize;

    while i < text.len() {
        if let Some(end) = take_ansi_escape_end(text, i) {
            current.push_str(&text[i..end]);
            i = end;
            continue;
        }

        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        let ch_width = char_display_width(ch);
        if current_width + ch_width > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width = current_width.saturating_add(ch_width);
        i += ch.len_utf8();
    }

    if current.is_empty() {
        out.push(String::new());
    } else {
        out.push(current);
    }

    out
}

fn wrap_text_display_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut out = Vec::new();

    for raw_line in text.lines() {
        if raw_line.is_empty() {
            out.push(String::new());
            continue;
        }

        let tokens = tokenize_wrappable_line(raw_line);
        let mut current = String::new();
        let mut current_width = 0usize;

        for token in tokens {
            if current_width + token.width <= width {
                current.push_str(&token.text);
                current_width = current_width.saturating_add(token.width);
                continue;
            }

            if current.is_empty() {
                if token.width > width {
                    let mut pieces = wrap_hard_by_display_width(&token.text, width);
                    let last = pieces.pop().unwrap_or_default();
                    out.extend(pieces);
                    current_width = display_width(&last);
                    current = last;
                } else {
                    current.push_str(&token.text);
                    current_width = token.width;
                }
                continue;
            }

            if token.kind == WrapTokenKind::Space {
                out.push(std::mem::take(&mut current));
                current_width = 0;
                continue;
            }

            out.push(std::mem::take(&mut current));
            if token.width > width {
                let mut pieces = wrap_hard_by_display_width(&token.text, width);
                let last = pieces.pop().unwrap_or_default();
                out.extend(pieces);
                current_width = display_width(&last);
                current = last;
            } else {
                current.push_str(&token.text);
                current_width = token.width;
            }
        }

        if current.is_empty() {
            out.push(String::new());
        } else {
            out.push(current);
        }
    }

    if out.is_empty() {
        out.push(String::new());
    }

    out
}

fn truncate_line_display_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    let mut out = String::new();
    let mut used = 0usize;

    let mut i = 0usize;
    while i < text.len() {
        if let Some(end) = take_ansi_escape_end(text, i) {
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }
        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        let w = char_display_width(ch);
        if used + w > max_width {
            break;
        }
        out.push(ch);
        used += w;
        i += ch.len_utf8();
    }

    out
}

fn take_sgr_escape_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if start + 2 >= bytes.len() || bytes[start] != 0x1B || bytes[start + 1] != b'[' {
        return None;
    }
    let mut i = start + 2;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'm' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn take_osc_escape_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if start + 2 >= bytes.len() || bytes[start] != 0x1B || bytes[start + 1] != b']' {
        return None;
    }
    let mut i = start + 2;
    while i < bytes.len() {
        if bytes[i] == 0x07 {
            return Some(i + 1);
        }
        if i + 1 < bytes.len() && bytes[i] == 0x1B && bytes[i + 1] == b'\\' {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

fn take_ansi_escape_end(text: &str, start: usize) -> Option<usize> {
    take_sgr_escape_end(text, start).or_else(|| take_osc_escape_end(text, start))
}

fn strip_sgr_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        if let Some(end) = take_ansi_escape_end(text, i) {
            i = end;
            continue;
        }
        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn draw_cursor_cell(stdout: &mut io::Stdout, ch: char) -> Result<()> {
    stdout.queue(SetBackgroundColor(ACTIVE_THEME.accent))?;
    stdout.queue(SetForegroundColor(ACTIVE_THEME.cursor_cover_fg))?;
    stdout.queue(Print(ch))?;
    stdout.queue(SetBackgroundColor(ACTIVE_THEME.app_bg))?;
    stdout.queue(SetForegroundColor(ACTIVE_THEME.input_fg))?;
    Ok(())
}

fn render_assistant_markdown(markdown: &str, width: usize) -> String {
    render_message_markdown(markdown, width, ACTIVE_THEME.ai_bg, ACTIVE_THEME.ai_fg)
}

fn render_message_markdown(markdown: &str, width: usize, bg: Color, fg: Color) -> String {
    let bg_rgb = color_to_rgb(bg);
    let fg_rgb = color_to_rgb(fg);
    crate::markdown::render_assistant_markdown_with_mode(
        markdown,
        width,
        is_light_color(bg),
        bg_rgb,
        fg_rgb,
    )
}

fn should_suppress_final_message(
    kind: LiveStreamKind,
    live_preview_rendered: bool,
    has_live_lines: bool,
    has_stream_text: bool,
) -> bool {
    let has_any_live_signal = live_preview_rendered || has_live_lines || has_stream_text;
    match kind {
        LiveStreamKind::Assistant => has_any_live_signal,
        LiveStreamKind::ToolCall => has_any_live_signal,
    }
}

fn choose_assistant_commit_text<'a>(
    stream_text: &'a str,
    final_message_content: &'a str,
) -> &'a str {
    if final_message_content.is_empty() {
        stream_text
    } else {
        final_message_content
    }
}

fn fill_container_bg(
    stdout: &mut io::Stdout,
    row: u16,
    left: u16,
    width: usize,
    bg: Color,
) -> Result<()> {
    if width == 0 {
        return Ok(());
    }
    stdout.queue(SetBackgroundColor(bg))?;
    stdout.queue(MoveTo(left, row))?;
    // Explicitly paint the row cells instead of relying on terminal-specific
    // clear semantics, so blank areas always use the same background.
    stdout.queue(Print(" ".repeat(width.saturating_sub(1))))?;
    stdout.queue(MoveTo(
        left.saturating_add(width.saturating_sub(1) as u16),
        row,
    ))?;
    stdout.queue(Print(" "))?;
    stdout.queue(SetBackgroundColor(Color::Reset))?;
    Ok(())
}

fn load_startup_image_braille_frames(target_rows: u16, max_visible_cols: u16) -> Vec<StartupFrame> {
    if target_rows == 0 || max_visible_cols == 0 {
        return Vec::new();
    }

    let candidates = startup_image_candidate_paths();
    load_startup_image_braille_frames_from_candidates(target_rows, max_visible_cols, &candidates)
}

fn load_startup_image_braille_frames_from_candidates(
    target_rows: u16,
    max_visible_cols: u16,
    candidates: &[PathBuf],
) -> Vec<StartupFrame> {
    let Some(source) = decode_first_existing_startup_image_source(candidates) else {
        return Vec::new();
    };
    match source {
        StartupImageSource::Static(img) => vec![StartupFrame {
            lines: render_braille_lines_for_image(&img, target_rows, max_visible_cols),
            delay: Duration::from_millis(120),
        }],
        StartupImageSource::AnimatedGif(frames) => frames
            .into_iter()
            .map(|(img, delay)| StartupFrame {
                lines: render_braille_lines_for_image(&img, target_rows, max_visible_cols),
                delay,
            })
            .collect(),
    }
}

fn startup_image_candidate_paths() -> Vec<PathBuf> {
    startup_image_candidate_paths_for_profile_dir(
        profiles::active_profile_dir().ok(),
        Path::new(DEFAULT_AVATAR_DIR),
    )
}

fn startup_image_candidate_paths_for_profile_dir(
    profile_dir: Option<PathBuf>,
    default_dir: &Path,
) -> Vec<PathBuf> {
    let profile_count = if profile_dir.is_some() {
        STARTUP_AVATAR_FILENAMES.len()
    } else {
        0
    };
    let mut candidates = Vec::with_capacity(STARTUP_AVATAR_FILENAMES.len() + profile_count);
    if let Some(dir) = profile_dir {
        candidates.extend(STARTUP_AVATAR_FILENAMES.iter().map(|file| dir.join(file)));
    }
    candidates.extend(
        STARTUP_AVATAR_FILENAMES
            .iter()
            .map(|file| default_dir.join(file)),
    );
    candidates
}

fn render_braille_lines_for_image(
    img: &DynamicImage,
    target_rows: u16,
    max_visible_cols: u16,
) -> Vec<BrailleLine> {
    let (width, height) = compute_startup_braille_size_from_height(
        img.width().max(1),
        img.height().max(1),
        target_rows as u32,
    );

    let dots_w = width.saturating_mul(2).max(2);
    let dots_h = height.saturating_mul(4).max(4);
    let img_dots_rgb = img
        .resize_exact(dots_w, dots_h, image::imageops::FilterType::Lanczos3)
        .to_rgb8();
    let img_dots_gray = image::DynamicImage::ImageRgb8(img_dots_rgb.clone()).to_luma8();
    let threshold = adaptive_luma_threshold(&img_dots_gray);

    let max_visible = max_visible_cols as usize;
    let mut out = Vec::new();
    let mut y = 0;
    while y + 3 < dots_h {
        let mut x = 0;
        let mut cells = Vec::new();
        while x + 1 < dots_w {
            let mut bits = [false; 8];
            let mut idx = 0usize;
            let mut r_sum: u32 = 0;
            let mut g_sum: u32 = 0;
            let mut b_sum: u32 = 0;
            for dy in 0..4 {
                for dx in 0..2 {
                    let luma = img_dots_gray.get_pixel(x + dx, y + dy)[0];
                    bits[idx] = luma > threshold;
                    let rgb = img_dots_rgb.get_pixel(x + dx, y + dy).0;
                    r_sum += rgb[0] as u32;
                    g_sum += rgb[1] as u32;
                    b_sum += rgb[2] as u32;
                    idx += 1;
                }
            }
            let ch = braille_char_from_bits(bits);
            let draw_ch = if ch == '\u{2800}' { ' ' } else { ch };
            let avg_r = (r_sum / 8) as u8;
            let avg_g = (g_sum / 8) as u8;
            let avg_b = (b_sum / 8) as u8;
            cells.push((avg_r, avg_g, avg_b, draw_ch));
            x += 2;
        }
        let (crop_start, crop_len) = centered_crop_range(cells.len(), max_visible);
        let mut line = String::new();
        for (r, g, b, ch) in cells.into_iter().skip(crop_start).take(crop_len) {
            let _ = write!(&mut line, "\x1b[38;2;{};{};{}m{}", r, g, b, ch);
        }
        line.push_str("\x1b[0m");
        out.push(BrailleLine {
            ansi_text: line,
            display_width: crop_len,
        });
        y += 4;
    }
    out
}

fn centered_crop_range(total: usize, max_visible: usize) -> (usize, usize) {
    if max_visible == 0 || total == 0 {
        return (0, 0);
    }
    let keep = total.min(max_visible);
    let start = (total.saturating_sub(keep)) / 2;
    (start, keep)
}

fn compute_startup_braille_size_from_height(src_w: u32, src_h: u32, target_h: u32) -> (u32, u32) {
    let src_w = src_w.max(1) as f32;
    let src_h = src_h.max(1) as f32;
    let height = target_h.max(1) as f32;
    // Braille cell packs 2x4 subpixels, so cell-space width needs a 4/2 factor
    // to preserve source aspect after rasterization into braille dots.
    let width = (height * (src_w / src_h) * 2.0).round().max(1.0) as u32;
    (width, height as u32)
}

fn braille_char_from_bits(bits: [bool; 8]) -> char {
    let dot_map = [0x01u32, 0x08, 0x02, 0x10, 0x04, 0x20, 0x40, 0x80];
    let mut code = 0u32;
    for (idx, on) in bits.iter().enumerate() {
        if *on {
            code |= dot_map[idx];
        }
    }
    char::from_u32(0x2800 + code).unwrap_or(' ')
}

enum StartupImageSource {
    Static(DynamicImage),
    AnimatedGif(Vec<(DynamicImage, Duration)>),
}

#[cfg(test)]
fn first_existing_path<'a>(candidates: &'a [&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|path| Path::new(path).exists())
}

#[cfg(test)]
fn decode_image_from_path(path: &str) -> Option<DynamicImage> {
    let file = File::open(path).ok()?;
    let reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .ok()?;
    reader.decode().ok()
}

fn decode_first_existing_startup_image_source(
    candidates: &[PathBuf],
) -> Option<StartupImageSource> {
    for path in candidates {
        if !path.exists() {
            continue;
        }
        if let Some(source) = decode_startup_image_source(path) {
            return Some(source);
        }
    }
    None
}

fn decode_startup_image_source(path: &Path) -> Option<StartupImageSource> {
    let file = File::open(path).ok()?;
    let reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .ok()?;
    if reader.format() == Some(ImageFormat::Gif) {
        let gif_frames = decode_gif_frames_from_path(path)?;
        if gif_frames.is_empty() {
            return None;
        }
        if gif_frames.len() == 1 {
            return Some(StartupImageSource::Static(gif_frames[0].0.clone()));
        }
        return Some(StartupImageSource::AnimatedGif(gif_frames));
    }
    reader.decode().ok().map(StartupImageSource::Static)
}

fn decode_gif_frames_from_path(path: &Path) -> Option<Vec<(DynamicImage, Duration)>> {
    let file = File::open(path).ok()?;
    let decoder = GifDecoder::new(BufReader::new(file)).ok()?;
    let frames = decoder.into_frames().collect_frames().ok()?;
    let mut out = Vec::with_capacity(frames.len());
    for frame in frames {
        let delay = frame.delay();
        let (num_ms, den_ms) = delay.numer_denom_ms();
        let den = den_ms.max(1);
        let ms = ((num_ms as f32) / (den as f32)).round().max(30.0) as u64;
        out.push((
            DynamicImage::ImageRgba8(frame.into_buffer()),
            Duration::from_millis(ms),
        ));
    }
    Some(out)
}

fn adaptive_luma_threshold(img_gray: &GrayImage) -> u8 {
    let pixels = img_gray.as_raw();
    if pixels.is_empty() {
        return 128;
    }
    let sum: u64 = pixels.iter().map(|&v| v as u64).sum();
    let mean = (sum as f32) / (pixels.len() as f32);
    // Mean-based adaptive threshold for dark/bright images, with guard rails.
    // Dark images get lower threshold (more details kept), bright images higher.
    mean.round().clamp(32.0, 192.0) as u8
}

fn render_stream_line(
    kind: LiveStreamKind,
    content: &str,
    content_width: usize,
    max_width: usize,
) -> String {
    match kind {
        LiveStreamKind::Assistant => {
            let fill = max_width.saturating_sub(content_width);
            format!(
                "{}{}{}{}",
                " ".repeat(BUBBLE_HORIZONTAL_PADDING),
                content,
                " ".repeat(fill),
                " ".repeat(BUBBLE_HORIZONTAL_PADDING),
            )
        }
        LiveStreamKind::ToolCall => content.to_string(),
    }
}

fn build_assistant_bubble_lines(content_lines: &[String], max_width: usize) -> Vec<String> {
    let mut expanded = Vec::new();
    for line in content_lines {
        if is_code_decorated_line(line) {
            expanded.push(line.clone());
            continue;
        }
        if line.starts_with(TABLE_MARKER) {
            expanded.push(line.clone());
            continue;
        }
        if let Some(quote_text) = line.strip_prefix(BLOCKQUOTE_MARKER) {
            let wrapped = wrap_text_display_width(quote_text, max_width);
            for seg in wrapped {
                expanded.push(format!("{BLOCKQUOTE_MARKER}{seg}"));
            }
            continue;
        }
        expanded.extend(wrap_text_display_width(line, max_width));
    }
    if expanded.is_empty() {
        expanded.push(String::new());
    }

    let content_width = max_width.max(1);

    let mut lines = Vec::with_capacity(content_lines.len() + 2);
    let row_width = content_width + BUBBLE_HORIZONTAL_PADDING * 2;
    lines.push(" ".repeat(row_width));

    if expanded.is_empty() {
        lines.push(" ".repeat(row_width));
    } else {
        for line in &expanded {
            lines.push(render_bubble_content_line(line, content_width));
        }
    }

    lines.push(" ".repeat(row_width));
    lines
}

fn build_assistant_bubble_from_rendered_text(rendered_text: &str, max_width: usize) -> Vec<String> {
    let mut content_lines: Vec<String> =
        rendered_text.lines().map(|line| line.to_string()).collect();
    if content_lines.is_empty() {
        content_lines.push(String::new());
    }
    build_assistant_bubble_lines(&content_lines, max_width)
}

fn render_bubble_content_line(line: &str, content_width: usize) -> String {
    let line = line.strip_prefix(TABLE_MARKER).unwrap_or(line);
    if is_blockquote_decorated_line(line) {
        let quote_text = line.strip_prefix(BLOCKQUOTE_MARKER).unwrap_or(line);
        let fill = content_width.saturating_sub(display_width(quote_text));
        let quote_bg = quote_bg_sgr();
        let quoted = format!("{quote_bg}{quote_text}\x1b[49m");
        let suffix = if fill == 0 {
            String::new()
        } else {
            format!("{quote_bg}{}\x1b[49m", " ".repeat(fill))
        };
        let right_padding = format!(
            "{}{}\x1b[49m",
            ai_bubble_bg_sgr(),
            " ".repeat(BUBBLE_HORIZONTAL_PADDING)
        );
        return format!(
            "{}{}{}{}",
            " ".repeat(BUBBLE_HORIZONTAL_PADDING),
            quoted,
            suffix,
            right_padding,
        );
    }

    let fill = content_width.saturating_sub(display_width(line));
    let suffix = if fill == 0 {
        String::new()
    } else if is_code_decorated_line(line) {
        let bg = extract_first_background_sgr(line).unwrap_or_else(|| "\x1b[49m".to_string());
        format!("{bg}{}\x1b[49m", " ".repeat(fill))
    } else {
        " ".repeat(fill)
    };
    let right_padding = if is_code_decorated_line(line) {
        format!(
            "{}{}\x1b[49m",
            ai_bubble_bg_sgr(),
            " ".repeat(BUBBLE_HORIZONTAL_PADDING)
        )
    } else {
        " ".repeat(BUBBLE_HORIZONTAL_PADDING)
    };
    format!(
        "{}{}{}{}",
        " ".repeat(BUBBLE_HORIZONTAL_PADDING),
        line,
        suffix,
        right_padding,
    )
}

fn is_code_decorated_line(line: &str) -> bool {
    line.contains("\x1b[48;5;") || line.contains("\x1b[48;2;")
}

fn is_blockquote_decorated_line(line: &str) -> bool {
    line.starts_with(BLOCKQUOTE_MARKER)
}

fn extract_first_background_sgr(line: &str) -> Option<String> {
    let idx = line.find("\x1b[48;")?;
    let rest = &line[idx..];
    let end = rest.find('m')?;
    Some(rest[..=end].to_string())
}

fn quote_bg_sgr() -> String {
    let ai_bg = color_to_rgb(ACTIVE_THEME.ai_bg);
    let accent = color_to_rgb(ACTIVE_THEME.accent);
    let light = is_light_color(ACTIVE_THEME.ai_bg);
    let base = shift_rgb(ai_bg, if light { -22 } else { 18 });
    let mixed = blend_rgb(base, accent, if light { 12 } else { 18 });
    format!("\x1b[48;2;{};{};{}m", mixed.0, mixed.1, mixed.2)
}

fn ai_bubble_bg_sgr() -> String {
    match ACTIVE_THEME.ai_bg {
        Color::Rgb { r, g, b } => format!("\x1b[48;2;{r};{g};{b}m"),
        Color::AnsiValue(v) => format!("\x1b[48;5;{v}m"),
        _ => "\x1b[49m".to_string(),
    }
}

fn format_compact_u64(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{}m", value / 1_000_000)
    } else if value >= 1_000 {
        format!("{}k", value / 1_000)
    } else {
        value.to_string()
    }
}

fn is_light_color(color: Color) -> bool {
    let (r, g, b) = color_to_rgb(color);
    let luma = 0.299f32 * (r as f32) + 0.587f32 * (g as f32) + 0.114f32 * (b as f32);
    luma >= 140.0
}

fn color_to_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb { r, g, b } => (r, g, b),
        Color::AnsiValue(v) => (v, v, v),
        _ => (128, 128, 128),
    }
}

fn shift_rgb((r, g, b): (u8, u8, u8), delta: i16) -> (u8, u8, u8) {
    let shift = |v: u8| -> u8 { ((v as i16) + delta).clamp(0, 255) as u8 };
    (shift(r), shift(g), shift(b))
}

fn blend_rgb(base: (u8, u8, u8), tint: (u8, u8, u8), tint_weight_percent: u8) -> (u8, u8, u8) {
    let w = tint_weight_percent.min(100) as u16;
    let inv = 100u16.saturating_sub(w);
    let mix = |b: u8, t: u8| -> u8 { (((b as u16) * inv + (t as u16) * w) / 100) as u8 };
    (
        mix(base.0, tint.0),
        mix(base.1, tint.1),
        mix(base.2, tint.2),
    )
}

fn finalized_assistant_stream_text(stream: &LiveStreamState, inner_width: usize) -> String {
    if let Some(preview) = stream.assistant_preview.as_ref() {
        let mut lines = preview.committed_lines.clone();
        if let Some(line) = preview.renderer.preview_incomplete_line() {
            lines.push(line);
        }
        if !lines.is_empty() {
            return lines.join("\n");
        }
    }
    render_assistant_markdown(&stream.text, inner_width)
}

fn stream_render_assistant_markdown_lines(
    stream: &mut LiveStreamState,
    max_width: usize,
) -> Vec<String> {
    let needs_reset = stream
        .assistant_preview
        .as_ref()
        .is_none_or(|state| state.width != max_width);
    if needs_reset {
        let bg = match stream.kind {
            LiveStreamKind::Assistant => ACTIVE_THEME.ai_bg,
            LiveStreamKind::ToolCall => ACTIVE_THEME.app_bg,
        };
        let fg = match stream.kind {
            LiveStreamKind::Assistant => ACTIVE_THEME.ai_fg,
            LiveStreamKind::ToolCall => ACTIVE_THEME.aux_fg,
        };
        stream.assistant_preview = Some(AssistantStreamPreview {
            width: max_width,
            renderer: StreamRenderer::new(
                max_width,
                is_light_color(bg),
                color_to_rgb(bg),
                color_to_rgb(fg),
            ),
            committed_lines: Vec::new(),
        });
        stream.consumed_bytes = 0;
    }

    let preview = stream
        .assistant_preview
        .as_mut()
        .expect("assistant preview is initialized");
    if stream.consumed_bytes < stream.text.len() {
        let delta = &stream.text[stream.consumed_bytes..];
        preview.renderer.push_delta(delta);
        stream.consumed_bytes = stream.text.len();
        preview.committed_lines = preview.renderer.commit_complete_lines();
    }

    let mut content = preview.committed_lines.clone();
    if let Some(line) = preview.renderer.preview_incomplete_line() {
        content.push(line);
    }

    build_assistant_bubble_lines(&content, max_width)
}

fn apply_live_line_updates(
    live_lines: &mut Vec<String>,
    next_lines: &[String],
) -> Vec<StreamPreviewOp> {
    let mut ops = Vec::new();
    for (idx, line) in next_lines.iter().enumerate() {
        if idx < live_lines.len() {
            if live_lines[idx] == *line {
                continue;
            }
            let offset = (live_lines.len().saturating_sub(1).saturating_sub(idx)) as u16;
            ops.push(StreamPreviewOp::RewriteRecent {
                offset_from_bottom: offset,
                line: line.clone(),
            });
        } else {
            ops.push(StreamPreviewOp::Append(line.clone()));
        }
    }

    if next_lines.len() < live_lines.len() {
        for idx in next_lines.len()..live_lines.len() {
            let offset = (live_lines.len().saturating_sub(1).saturating_sub(idx)) as u16;
            ops.push(StreamPreviewOp::RewriteRecent {
                offset_from_bottom: offset,
                line: String::new(),
            });
        }
    }

    *live_lines = next_lines.to_vec();
    ops
}

fn stream_preview_apply_delta(
    kind: LiveStreamKind,
    max_width: usize,
    delta: &str,
    stream: &mut LiveStreamState,
    live_lines: &mut Vec<String>,
) -> Vec<StreamPreviewOp> {
    let mut ops = Vec::new();

    if !stream.started {
        match kind {
            LiveStreamKind::Assistant => {
                let top = render_stream_line(kind, "", 0, max_width);
                let content = render_stream_line(kind, "", 0, max_width);
                let bottom = render_stream_line(kind, "", 0, max_width);
                live_lines.push(top.clone());
                live_lines.push(content.clone());
                live_lines.push(bottom.clone());
                ops.push(StreamPreviewOp::Append(top));
                ops.push(StreamPreviewOp::Append(content));
                ops.push(StreamPreviewOp::Append(bottom));
            }
            LiveStreamKind::ToolCall => {
                stream.current_line.push_str("🔧 $ ");
                stream.current_line_width = display_width(&stream.current_line);
                let initial = render_stream_line(
                    kind,
                    &stream.current_line,
                    stream.current_line_width,
                    max_width,
                );
                live_lines.push(initial.clone());
                ops.push(StreamPreviewOp::Append(initial));
            }
        }
        stream.started = true;
    }

    for ch in delta.chars() {
        match kind {
            LiveStreamKind::Assistant => {
                if ch == '\n' {
                    stream.current_line.clear();
                    stream.current_line_width = 0;
                    // Convert current bottom padding row into a new content row.
                    let new_content = render_stream_line(kind, "", 0, max_width);
                    if let Some(last) = live_lines.last_mut() {
                        *last = new_content.clone();
                    }
                    ops.push(StreamPreviewOp::RewriteRecent {
                        offset_from_bottom: 0,
                        line: new_content,
                    });
                    let bottom = render_stream_line(kind, "", 0, max_width);
                    live_lines.push(bottom.clone());
                    ops.push(StreamPreviewOp::Append(bottom));
                    continue;
                }

                let w = char_display_width(ch);
                if stream.current_line_width + w > max_width && !stream.current_line.is_empty() {
                    stream.current_line.clear();
                    stream.current_line_width = 0;
                    let new_content = render_stream_line(kind, "", 0, max_width);
                    if let Some(last) = live_lines.last_mut() {
                        *last = new_content.clone();
                    }
                    ops.push(StreamPreviewOp::RewriteRecent {
                        offset_from_bottom: 0,
                        line: new_content,
                    });
                    let bottom = render_stream_line(kind, "", 0, max_width);
                    live_lines.push(bottom.clone());
                    ops.push(StreamPreviewOp::Append(bottom));
                }

                stream.current_line.push(ch);
                stream.current_line_width = stream.current_line_width.saturating_add(w);
                let updated = render_stream_line(
                    kind,
                    &stream.current_line,
                    stream.current_line_width,
                    max_width,
                );
                if live_lines.len() >= 2 {
                    let idx = live_lines.len() - 2;
                    live_lines[idx] = updated.clone();
                }
                ops.push(StreamPreviewOp::RewriteRecent {
                    offset_from_bottom: 1,
                    line: updated,
                });
            }
            LiveStreamKind::ToolCall => {
                if ch == '\n' {
                    stream.current_line.clear();
                    stream.current_line_width = 0;
                    let next_line = render_stream_line(kind, "", 0, max_width);
                    live_lines.push(next_line.clone());
                    ops.push(StreamPreviewOp::Append(next_line));
                    continue;
                }

                let w = char_display_width(ch);
                if stream.current_line_width + w > max_width && !stream.current_line.is_empty() {
                    stream.current_line.clear();
                    stream.current_line_width = 0;
                    let next_line = render_stream_line(kind, "", 0, max_width);
                    live_lines.push(next_line.clone());
                    ops.push(StreamPreviewOp::Append(next_line));
                }

                stream.current_line.push(ch);
                stream.current_line_width = stream.current_line_width.saturating_add(w);
                let updated = render_stream_line(
                    kind,
                    &stream.current_line,
                    stream.current_line_width,
                    max_width,
                );
                if let Some(last) = live_lines.last_mut() {
                    *last = updated.clone();
                }
                ops.push(StreamPreviewOp::RewriteRecent {
                    offset_from_bottom: 0,
                    line: updated,
                });
            }
        }
    }

    ops
}

struct TerminalGuard;

impl TerminalGuard {
    fn release(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        stdout
            .execute(DisableBracketedPaste)
            .context("failed to disable bracketed paste")?;
        stdout.execute(Show)?;
        disable_raw_mode().context("failed to disable raw mode")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat};
    use std::fs;

    fn new_stream(kind: LiveStreamKind) -> LiveStreamState {
        LiveStreamState {
            kind,
            text: String::new(),
            raw_tool_args: String::new(),
            consumed_bytes: 0,
            current_line: String::new(),
            current_line_width: 0,
            started: false,
            assistant_preview: None,
        }
    }

    fn strip_padding_and_fill(line: &str) -> String {
        let chars: Vec<char> = line.chars().collect();
        if chars.len() <= BUBBLE_HORIZONTAL_PADDING.saturating_mul(2) {
            return String::new();
        }
        let start = BUBBLE_HORIZONTAL_PADDING;
        let end = chars.len() - BUBBLE_HORIZONTAL_PADDING;
        chars[start..end]
            .iter()
            .collect::<String>()
            .trim_end_matches(' ')
            .to_string()
    }

    fn decode_assistant_preview(lines: &[String]) -> String {
        if lines.len() < 3 {
            return String::new();
        }
        lines[1..lines.len() - 1]
            .iter()
            .map(|line| strip_padding_and_fill(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn slash_picker_matches_known_control_commands() {
        assert!(matching_slash_commands("/s").is_empty());
        let commands = matching_slash_commands("/r")
            .into_iter()
            .map(|spec| spec.command)
            .collect::<Vec<_>>();
        assert_eq!(commands, vec!["/resume", "/rewind"]);
        assert_eq!(
            matching_slash_commands("/m")
                .into_iter()
                .map(|spec| spec.command)
                .collect::<Vec<_>>(),
            vec!["/model"]
        );
        assert!(matching_slash_commands("/x").is_empty());
        assert!(matching_slash_commands("/new title").is_empty());
    }

    #[test]
    fn visible_history_line_index_bottom_aligns_short_history() {
        assert_eq!(visible_history_line_index(6, 10, 3), None);
        assert_eq!(visible_history_line_index(7, 10, 3), Some(0));
        assert_eq!(visible_history_line_index(8, 10, 3), Some(1));
        assert_eq!(visible_history_line_index(9, 10, 3), Some(2));
    }

    #[test]
    fn visible_history_line_index_tails_long_history() {
        assert_eq!(visible_history_line_index(0, 10, 12), Some(2));
        assert_eq!(visible_history_line_index(9, 10, 12), Some(11));
        assert_eq!(visible_history_line_index(10, 10, 12), None);
    }

    #[test]
    fn assistant_preview_preserves_text_single_char_chunks() {
        let text = "Hello! What task can I help with? (such as shell commands or file search) `ok`";
        let mut stream = new_stream(LiveStreamKind::Assistant);
        let mut lines = Vec::new();

        for ch in text.chars() {
            let delta = ch.to_string();
            let _ = stream_preview_apply_delta(
                LiveStreamKind::Assistant,
                240,
                &delta,
                &mut stream,
                &mut lines,
            );
        }

        assert_eq!(decode_assistant_preview(&lines), text);
    }

    #[test]
    fn assistant_preview_preserves_text_mixed_chunk_sizes() {
        let text = "I am `duck`, a powerful command-line AI agent. (shell/search)";
        let mut stream = new_stream(LiveStreamKind::Assistant);
        let mut lines = Vec::new();
        let chunks = [
            "I am `du",
            "ck",
            "`, a ",
            "powerful command-line ",
            "AI agent. (shell/search)",
        ];

        for chunk in chunks {
            let _ = stream_preview_apply_delta(
                LiveStreamKind::Assistant,
                240,
                chunk,
                &mut stream,
                &mut lines,
            );
        }

        assert_eq!(decode_assistant_preview(&lines), text);
    }

    #[test]
    fn assistant_preview_preserves_explicit_newlines() {
        let text = "First line\n\nThird line (with `code`)\nFourth line";
        let mut stream = new_stream(LiveStreamKind::Assistant);
        let mut lines = Vec::new();
        let _ = stream_preview_apply_delta(
            LiveStreamKind::Assistant,
            240,
            text,
            &mut stream,
            &mut lines,
        );

        assert_eq!(decode_assistant_preview(&lines), text);
    }

    #[test]
    fn assistant_preview_wrap_keeps_all_characters() {
        let text = "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890";
        let mut stream = new_stream(LiveStreamKind::Assistant);
        let mut lines = Vec::new();
        let _ =
            stream_preview_apply_delta(LiveStreamKind::Assistant, 8, text, &mut stream, &mut lines);

        assert_eq!(decode_assistant_preview(&lines).replace('\n', ""), text);
    }

    #[test]
    fn assistant_preview_starts_with_top_content_bottom_padding() {
        let mut stream = new_stream(LiveStreamKind::Assistant);
        let mut lines = Vec::new();
        let _ =
            stream_preview_apply_delta(LiveStreamKind::Assistant, 20, "", &mut stream, &mut lines);

        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn toolcall_preview_keeps_prefix_and_content() {
        let mut stream = new_stream(LiveStreamKind::ToolCall);
        let mut lines = Vec::new();
        let _ = stream_preview_apply_delta(
            LiveStreamKind::ToolCall,
            240,
            "echo hello",
            &mut stream,
            &mut lines,
        );

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "🔧 $ echo hello");
    }

    #[test]
    fn braille_dot_mapping_matches_python_reference() {
        // dot_map = [0x01, 0x08, 0x02, 0x10, 0x04, 0x20, 0x40, 0x80]
        let ch = braille_char_from_bits([true, false, false, false, false, false, false, false]);
        assert_eq!(ch as u32, 0x2801);

        let ch = braille_char_from_bits([false, true, false, false, false, false, false, false]);
        assert_eq!(ch as u32, 0x2808);

        let ch = braille_char_from_bits([true, true, true, true, true, true, true, true]);
        assert_eq!(ch as u32, 0x28ff);
    }

    #[test]
    fn braille_from_single_2x4_cell_uses_expected_order() {
        // 2x4 cell bits in scan order:
        // (0,0) (1,0)
        // (0,1) (1,1)
        // (0,2) (1,2)
        // (0,3) (1,3)
        // Light only (0,0) and (1,2).
        let ch = braille_char_from_bits([
            true, false, // row 0
            false, false, // row 1
            false, true, // row 2
            false, false, // row 3
        ]);
        // dot1 (0x01) + dot6 (0x20)
        assert_eq!(ch as u32, 0x2821);
    }

    #[test]
    fn startup_braille_uses_truecolor_escape() {
        let candidates =
            startup_image_candidate_paths_for_profile_dir(None, Path::new(DEFAULT_AVATAR_DIR));
        let frames = load_startup_image_braille_frames_from_candidates(8, 80, &candidates);
        if frames.is_empty() || frames[0].lines.is_empty() {
            return;
        }
        assert!(frames[0].lines[0].ansi_text.contains("\x1b[38;2;"));
    }

    #[test]
    fn startup_image_candidates_prefer_profile_avatar_files() {
        let profile_dir = PathBuf::from("/tmp/profile");
        let candidates = startup_image_candidate_paths_for_profile_dir(
            Some(profile_dir),
            Path::new("src/default"),
        );
        assert_eq!(candidates[0], PathBuf::from("/tmp/profile/avatar.png"));
        assert_eq!(candidates[1], PathBuf::from("/tmp/profile/avatar.jpg"));
        assert_eq!(candidates[2], PathBuf::from("/tmp/profile/avatar.jpeg"));
        assert_eq!(candidates[3], PathBuf::from("/tmp/profile/avatar.webp"));
        assert_eq!(candidates[4], PathBuf::from("/tmp/profile/avatar.gif"));
        assert_eq!(candidates[5], PathBuf::from("src/default/avatar.png"));
    }

    #[test]
    fn startup_image_candidates_use_defaults_without_avatar() {
        let candidates =
            startup_image_candidate_paths_for_profile_dir(None, Path::new("src/default"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("src/default/avatar.png"),
                PathBuf::from("src/default/avatar.jpg"),
                PathBuf::from("src/default/avatar.jpeg"),
                PathBuf::from("src/default/avatar.webp"),
                PathBuf::from("src/default/avatar.gif"),
            ]
        );
    }

    #[test]
    fn centered_crop_range_crops_both_sides() {
        let (start, keep) = centered_crop_range(120, 80);
        assert_eq!(start, 20);
        assert_eq!(keep, 80);
    }

    #[test]
    fn startup_braille_size_uses_target_height() {
        let (w, h) = compute_startup_braille_size_from_height(784, 1168, 24);
        assert_eq!(h, 24);
        assert_eq!(w, 32);
    }

    #[test]
    fn startup_braille_size_scales_width_from_ratio() {
        let (w, h) = compute_startup_braille_size_from_height(1200, 400, 24);
        assert_eq!(h, 24);
        assert_eq!(w, 144);
    }

    #[test]
    fn adaptive_threshold_is_lower_for_dark_images() {
        let img = GrayImage::from_raw(4, 2, vec![20, 30, 40, 50, 35, 45, 55, 65]).unwrap();
        let t = adaptive_luma_threshold(&img);
        assert!(t < 128);
    }

    #[test]
    fn adaptive_threshold_is_higher_for_bright_images() {
        let img = GrayImage::from_raw(4, 2, vec![180, 190, 200, 210, 220, 230, 240, 250]).unwrap();
        let t = adaptive_luma_threshold(&img);
        assert!(t > 128);
    }

    #[test]
    fn first_existing_path_returns_first_hit_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("image.png");
        let p2 = dir.path().join("image.jpg");
        fs::write(&p2, b"dummy").unwrap();

        let candidates = vec![
            p1.to_string_lossy().to_string(),
            p2.to_string_lossy().to_string(),
        ];
        let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
        let found = first_existing_path(&refs).unwrap();
        assert_eq!(found, refs[1]);
    }

    #[test]
    fn decode_image_from_path_works_when_extension_mismatches_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.jpg");
        let img = DynamicImage::new_rgb8(1, 1);
        img.save_with_format(&path, ImageFormat::Png).unwrap();

        let decoded = decode_image_from_path(path.to_string_lossy().as_ref());
        assert!(decoded.is_some());
        let decoded = decoded.unwrap();
        assert_eq!(decoded.width(), 1);
        assert_eq!(decoded.height(), 1);
    }

    #[test]
    fn render_assistant_markdown_renders_list_and_heading() {
        let markdown = "# title\n- one\n- two";
        let got = render_assistant_markdown(markdown, 80);
        assert!(got.contains("title"));
        assert!(!got.contains("# title"));
        assert!(got.contains("• one"));
        assert!(got.contains("• two"));
    }

    #[test]
    fn render_assistant_markdown_renders_table() {
        let markdown = "| Name | Age |\n| --- | --- |\n| Alice | 30 |";
        let got = render_assistant_markdown(markdown, 80);
        assert!(got.contains("\x1b[48;2;"));
        assert!(got.contains("Alice"));
        assert!(!got.contains("┌"));
        assert!(!got.contains("┘"));
    }

    #[test]
    fn assistant_stream_suppresses_final_when_preview_rendered() {
        assert!(!should_suppress_final_message(
            LiveStreamKind::Assistant,
            false,
            false,
            false,
        ));
        assert!(should_suppress_final_message(
            LiveStreamKind::Assistant,
            true,
            false,
            false,
        ));
    }

    #[test]
    fn assistant_stream_suppresses_final_when_live_lines_exist_even_if_flag_is_false() {
        assert!(should_suppress_final_message(
            LiveStreamKind::Assistant,
            false,
            true,
            false,
        ));
    }

    #[test]
    fn assistant_stream_suppresses_final_when_stream_text_exists_even_if_not_rendered_yet() {
        assert!(should_suppress_final_message(
            LiveStreamKind::Assistant,
            false,
            false,
            true,
        ));
    }

    #[test]
    fn choose_assistant_commit_text_prefers_final_message_when_present() {
        let got = choose_assistant_commit_text("stream prefix", "final full message");
        assert_eq!(got, "final full message");
    }

    #[test]
    fn choose_assistant_commit_text_falls_back_to_stream_when_final_empty() {
        let got = choose_assistant_commit_text("stream only", "");
        assert_eq!(got, "stream only");
    }

    #[test]
    fn tool_stream_suppression_follows_any_live_signal() {
        assert!(!should_suppress_final_message(
            LiveStreamKind::ToolCall,
            false,
            false,
            false,
        ));
        assert!(should_suppress_final_message(
            LiveStreamKind::ToolCall,
            true,
            false,
            false,
        ));
    }

    #[test]
    fn display_width_ignores_sgr_sequences() {
        let plain = "abc";
        let colored = "\x1b[38;5;215mabc\x1b[39m";
        assert_eq!(display_width(plain), 3);
        assert_eq!(display_width(colored), 3);
    }

    #[test]
    fn display_width_ignores_osc8_hyperlink_sequences() {
        let osc = "\x1b]8;;https://example.com\x1b\\abc\x1b]8;;\x1b\\";
        assert_eq!(display_width(osc), 3);
    }

    #[test]
    fn render_assistant_markdown_highlights_code() {
        let md = "inline `let x = 1;`\n```rust\nlet y = 2;\n```";
        let got = render_assistant_markdown(md, 80);
        assert!(got.contains("\x1b[38;5;215mlet x = 1;"));
        assert!(got.contains("\x1b[48;"));
        assert!(!got.contains("  1   "));
        assert!(got.contains("\x1b["));
    }

    #[test]
    fn code_line_bubble_fill_uses_code_background() {
        let line = "\x1b[48;2;52;61;70mlet x = 1;\x1b[49m";
        let content_width = 40usize;
        let fill = content_width.saturating_sub(display_width(line));
        assert!(fill > 0);
        let got = render_bubble_content_line(line, content_width);
        let colored_fill = format!("\x1b[48;2;52;61;70m{}\x1b[49m", " ".repeat(fill));
        assert!(got.contains(&colored_fill));
        let bubble_pad = format!(
            "{}{}\x1b[49m",
            ai_bubble_bg_sgr(),
            " ".repeat(BUBBLE_HORIZONTAL_PADDING)
        );
        assert!(got.contains(&bubble_pad));
    }

    #[test]
    fn blockquote_line_bubble_fill_uses_quote_background() {
        let line = format!("{BLOCKQUOTE_MARKER}quoted");
        let content_width = 24usize;
        let fill = content_width.saturating_sub(display_width("quoted"));
        assert!(fill > 0);
        let got = render_bubble_content_line(&line, content_width);
        let bg = quote_bg_sgr();
        let expected_fill = format!("{bg}{}\x1b[49m", " ".repeat(fill));
        assert!(got.contains(&expected_fill));
        let bubble_pad = format!(
            "{}{}\x1b[49m",
            ai_bubble_bg_sgr(),
            " ".repeat(BUBBLE_HORIZONTAL_PADDING)
        );
        assert!(got.contains(&bubble_pad));
    }

    #[test]
    fn build_assistant_bubble_lines_keeps_quote_background_on_all_wrapped_segments() {
        let long_quote = format!(
            "{BLOCKQUOTE_MARKER}Programming language design is not only about making machines execute instructions, but about expressing human intent clearly to machines. Adapted from ideas associated with Alfred Aho, Hal Abelson, and other computer scientists."
        );
        let lines = build_assistant_bubble_lines(&[long_quote], 28);
        assert!(lines.len() > 3);
        let quote_bg = quote_bg_sgr();
        for line in lines.iter().skip(1).take(lines.len().saturating_sub(2)) {
            assert!(line.contains(&quote_bg));
        }
    }

    #[test]
    fn assistant_bubble_keeps_quote_and_footnote_definition_backgrounds_separate() {
        let rendered =
            format!("{BLOCKQUOTE_MARKER}Quoted content[^1]\n\n[^1]: Footnote definition");
        let lines = build_assistant_bubble_from_rendered_text(&rendered, 32);
        let quote_bg = quote_bg_sgr();
        let content_lines = lines.iter().skip(1).take(lines.len().saturating_sub(2));
        let collected = content_lines
            .map(|line| strip_sgr_ansi(line))
            .collect::<Vec<_>>();
        assert!(
            collected
                .iter()
                .any(|line| line.contains("Quoted content[^1]"))
        );
        assert!(
            collected
                .iter()
                .any(|line| line.contains("[^1]: Footnote definition"))
        );
        let quote_line = lines
            .iter()
            .find(|line| strip_sgr_ansi(line).contains("Quoted content[^1]"))
            .expect("quote line exists");
        let footnote_line = lines
            .iter()
            .find(|line| strip_sgr_ansi(line).contains("[^1]: Footnote definition"))
            .expect("footnote line exists");
        assert!(quote_line.contains(&quote_bg));
        assert!(!footnote_line.contains(&quote_bg));
    }

    #[test]
    fn linked_line_bubble_fill_preserves_right_padding() {
        let line = "\x1b]8;;https://example.com\x1b\\Google\x1b]8;;\x1b\\";
        let content_width = 20usize;
        let fill = content_width.saturating_sub(display_width(line));
        assert!(fill > 0);
        let got = render_bubble_content_line(line, content_width);
        assert!(got.ends_with(&" ".repeat(BUBBLE_HORIZONTAL_PADDING)));
        assert!(got.contains(&" ".repeat(fill)));
    }

    #[test]
    fn table_marker_line_is_not_rewrapped_by_assistant_bubble_builder() {
        let line = format!("{TABLE_MARKER}\x1b[48;2;60;60;60m Bob \x1b[49m");
        let lines = build_assistant_bubble_lines(&[line], 8);
        assert!(lines.len() >= 3);
        assert_eq!(display_width(&lines[1]), 8 + BUBBLE_HORIZONTAL_PADDING * 2);
    }

    #[test]
    fn assistant_stream_bubble_uses_full_inner_width() {
        let lines = build_assistant_bubble_lines(&[String::from("short")], 32);
        assert!(!lines.is_empty());
        assert_eq!(display_width(&lines[0]), 32 + BUBBLE_HORIZONTAL_PADDING * 2);
        assert_eq!(display_width(&lines[1]), 32 + BUBBLE_HORIZONTAL_PADDING * 2);
    }

    #[test]
    fn apply_live_line_updates_shrink_for_assistant_can_use_non_empty_blank_row() {
        let mut live = vec!["line1".to_string(), "line2".to_string()];
        let next = vec!["line1".to_string()];
        let ops = apply_live_line_updates(&mut live, &next);
        assert!(ops.iter().any(|op| matches!(
            op,
            StreamPreviewOp::RewriteRecent {
                offset_from_bottom: 0,
                line
            } if line.is_empty()
        )));
    }

    #[test]
    fn wrap_text_keeps_english_word_intact() {
        let got = wrap_text_display_width("hello world", 7);
        assert_eq!(got, vec!["hello ".to_string(), "world".to_string()]);
    }

    #[test]
    fn wrap_text_splits_only_when_single_word_exceeds_width() {
        let text = "supercalifragilistic";
        let got = wrap_text_display_width(text, 6);
        assert!(got.len() > 1);
        assert_eq!(got.concat(), text);
    }

    #[test]
    fn wrap_text_mixed_text_keeps_english_token_whole() {
        let got = wrap_text_display_width("hello markdown world", 8);
        let plain_lines: Vec<String> = got.iter().map(|s| strip_sgr_ansi(s)).collect();
        assert!(plain_lines.iter().any(|l| l == "markdown"));
        assert!(
            !plain_lines
                .iter()
                .any(|l| l.contains("markd") && l != "markdown")
        );
    }

    #[test]
    fn wrap_text_splits_long_unspaced_text() {
        let text = "abcdefghijklmnopqrstuvwxyz0123456789";
        let got = wrap_text_display_width(text, 12);
        assert!(got.len() > 1);
        assert!(got.iter().all(|line| display_width(line) <= 12));
        assert_eq!(got.concat(), text);
    }

    #[test]
    fn wrap_text_preserves_sgr_and_keeps_word_whole() {
        let text = "\x1b[1mhello\x1b[22m world";
        let got = wrap_text_display_width(text, 7);
        let plain = got
            .iter()
            .map(|s| strip_sgr_ansi(s))
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(plain, "hello world");
        assert_eq!(got.len(), 2);
    }
}
