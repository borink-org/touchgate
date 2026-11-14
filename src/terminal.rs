//! The terminal of `touchgate release`: raw input, keys and full-screen
//! drawing with escape sequences.
//!
//! It runs on Linux in a modern terminal such as Ghostty, and asks nothing of
//! terminfo. Keys come through the kitty keyboard protocol where the terminal
//! has it, which tells Esc apart from the start of a sequence. Each frame is
//! drawn as one synchronized update, so the screen does not flicker.

use std::io::Write;
use std::sync::{Once, OnceLock};

use rustix::termios::{self, OptionalActions, SpecialCodeIndex, Termios};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Switches to the alternate screen, hides the cursor and asks for the kitty
/// keyboard protocol with escape codes disambiguated.
const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[>1u";

/// Undoes [`ENTER`] and clears the progress in the tab.
const LEAVE: &str = "\x1b[<u\x1b[?25h\x1b[?1049l\x1b]9;4;0\x1b\\";

/// The terminal settings from before [`RawTerminal::enter`], which the panic
/// hook restores.
static ORIGINAL: OnceLock<Termios> = OnceLock::new();

/// A key that the person pressed. Keys that `touchgate release` does not use
/// are left out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Key {
    Char(char),
    /// A letter pressed with Ctrl, in lowercase.
    Ctrl(char),
    Enter,
    Escape,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

/// How a span of text looks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Style {
    Plain,
    Bold,
    Dim,
    /// Something that passed.
    Passed,
    /// Something that failed.
    Failed,
    /// Something under way.
    Running,
    /// What the screen is about, such as a version or a commit.
    Accent,
}

impl Style {
    /// Returns the SGR sequence that turns the style on.
    fn sgr(self) -> &'static str {
        match self {
            Self::Plain => "",
            Self::Bold => "\x1b[1m",
            Self::Dim => "\x1b[2m",
            Self::Passed => "\x1b[32m",
            Self::Failed => "\x1b[31m",
            Self::Running => "\x1b[33m",
            Self::Accent => "\x1b[36m",
        }
    }
}

/// A run of text in one style, which may link to a URL.
pub struct Span {
    pub text: String,
    pub style: Style,
    pub link: Option<String>,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Self::styled(text, Style::Plain)
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            link: None,
        }
    }

    /// Makes the span a link to `url`, which a click in the terminal opens.
    pub fn linked(mut self, url: impl Into<String>) -> Self {
        self.link = Some(url.into());
        self
    }
}

/// One line of the screen.
pub type Line = Vec<Span>;

/// The progress the terminal shows in its tab or title bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TabProgress {
    Hidden,
    /// Under way, for an unknown time.
    Busy,
    /// Waiting for the person to do something.
    WaitingOnYou,
    Failed,
}

impl TabProgress {
    /// Returns the OSC 9;4 sequence that shows the progress.
    fn sequence(self) -> &'static str {
        match self {
            Self::Hidden => "\x1b]9;4;0\x1b\\",
            Self::Busy => "\x1b]9;4;3\x1b\\",
            Self::WaitingOnYou => "\x1b]9;4;4;100\x1b\\",
            Self::Failed => "\x1b]9;4;2;100\x1b\\",
        }
    }
}

/// The terminal in raw mode on the alternate screen. Dropping it, or a
/// panic, restores the terminal as it was.
pub struct RawTerminal {
    original: Termios,
    /// Bytes read that do not make a whole key yet.
    pending: Vec<u8>,
    progress: TabProgress,
}

impl RawTerminal {
    /// Puts the terminal in raw mode on the alternate screen. Standard input
    /// and output must both be the terminal.
    pub fn enter() -> Result<Self, String> {
        if !termios::isatty(std::io::stdin()) || !termios::isatty(std::io::stdout()) {
            return Err("touchgate release needs a terminal on standard input and output".into());
        }
        let original =
            termios::tcgetattr(std::io::stdin()).map_err(|error| format!("terminal: {error}"))?;
        let _ = ORIGINAL.set(original.clone());
        install_panic_hook();
        let terminal = Self {
            original,
            pending: Vec::new(),
            progress: TabProgress::Hidden,
        };
        terminal.activate()?;
        Ok(terminal)
    }

    /// Turns raw mode on and enters the alternate screen. A read waits at
    /// most a tenth of a second for input, so the screen can redraw while the
    /// person does nothing.
    fn activate(&self) -> Result<(), String> {
        let mut raw = self.original.clone();
        raw.make_raw();
        raw.special_codes[SpecialCodeIndex::VMIN] = 0;
        raw.special_codes[SpecialCodeIndex::VTIME] = 1;
        termios::tcsetattr(std::io::stdin(), OptionalActions::Now, &raw)
            .map_err(|error| format!("terminal: {error}"))?;
        write_out(ENTER)
    }

    /// Leaves the alternate screen and restores the settings from before.
    fn deactivate(&self) {
        let _ = write_out(LEAVE);
        let _ = termios::tcsetattr(std::io::stdin(), OptionalActions::Now, &self.original);
    }

    /// Runs `action` on the terminal as it was before raw mode, such as for
    /// a program that asks for a PIN, and comes back afterwards.
    pub fn suspended<T>(&mut self, action: impl FnOnce() -> T) -> Result<T, String> {
        self.deactivate();
        let result = action();
        self.pending.clear();
        self.activate()?;
        let progress = self.progress;
        self.progress = TabProgress::Hidden;
        self.show_progress(progress)?;
        Ok(result)
    }

    /// Returns the size of the terminal as columns and rows.
    pub fn size(&self) -> (usize, usize) {
        match termios::tcgetwinsize(std::io::stdout()) {
            Ok(size) if size.ws_col > 0 && size.ws_row > 0 => {
                (size.ws_col as usize, size.ws_row as usize)
            }
            _ => (80, 24),
        }
    }

    /// Returns the keys pressed, waiting at most a tenth of a second for the
    /// first. Returns no keys if none came.
    pub fn read_keys(&mut self) -> Result<Vec<Key>, String> {
        let mut buffer = [0; 256];
        let read = match rustix::io::read(std::io::stdin(), &mut buffer[..]) {
            Ok(read) => read,
            Err(rustix::io::Errno::INTR) => 0,
            Err(error) => return Err(format!("terminal: {error}")),
        };
        if read == 0 {
            // Nothing followed a lone Esc within the wait, so it was the key.
            let keys = if self.pending == [0x1b] {
                vec![Key::Escape]
            } else {
                Vec::new()
            };
            self.pending.clear();
            return Ok(keys);
        }
        self.pending.extend_from_slice(&buffer[..read]);
        let (keys, used) = parse_keys(&self.pending);
        self.pending.drain(..used);
        Ok(keys)
    }

    /// Draws `lines` over the whole screen, cut to its size.
    pub fn draw(&mut self, lines: &[Line]) -> Result<(), String> {
        let (columns, rows) = self.size();
        write_out(&render_frame(lines, columns, rows))
    }

    /// Shows `progress` in the terminal's tab, if it changed.
    pub fn show_progress(&mut self, progress: TabProgress) -> Result<(), String> {
        if progress == self.progress {
            return Ok(());
        }
        self.progress = progress;
        write_out(progress.sequence())
    }
}

impl RawTerminal {
    /// Asks the terminal to show a desktop notification with `title` and
    /// `body`, through OSC 777. Ghostty shows it.
    pub fn notify(&mut self, title: &str, body: &str) -> Result<(), String> {
        write_out(&notification(title, body))
    }
}

/// Returns the OSC 777 sequence of a notification. Control characters would
/// end the sequence, and a `;` in the title would end the title, so both
/// become spaces.
fn notification(title: &str, body: &str) -> String {
    let clean = |text: &str, also: &[char]| -> String {
        text.chars()
            .map(|c| {
                if c.is_control() || also.contains(&c) {
                    ' '
                } else {
                    c
                }
            })
            .collect()
    };
    format!(
        "\x1b]777;notify;{};{}\x1b\\",
        clean(title, &[';']),
        clean(body, &[])
    )
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        self.deactivate();
    }
}

/// Makes a panic restore the terminal before it prints its message, which
/// would otherwise land on the alternate screen and vanish.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = write_out(LEAVE);
            if let Some(original) = ORIGINAL.get() {
                let _ = termios::tcsetattr(std::io::stdin(), OptionalActions::Now, original);
            }
            default(info);
        }));
    });
}

fn write_out(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| format!("terminal: {error}"))
}

/// Returns the escape sequences that draw `lines` on a screen of `columns`
/// by `rows`, as one synchronized update. Lines past the last row are left
/// out, and text past the last column is cut.
fn render_frame(lines: &[Line], columns: usize, rows: usize) -> String {
    let mut frame = String::from("\x1b[?2026h");
    for (row, line) in lines.iter().take(rows).enumerate() {
        frame.push_str(&format!("\x1b[{};1H", row + 1));
        let mut left = columns;
        for span in line {
            let (text, width) = cut_to_width(&span.text, left);
            if text.is_empty() {
                break;
            }
            left -= width;
            if let Some(url) = &span.link {
                frame.push_str(&format!("\x1b]8;;{url}\x1b\\"));
            }
            frame.push_str(span.style.sgr());
            frame.push_str(text);
            if span.style != Style::Plain {
                frame.push_str("\x1b[0m");
            }
            if span.link.is_some() {
                frame.push_str("\x1b]8;;\x1b\\");
            }
        }
        frame.push_str("\x1b[K");
    }
    let drawn = lines.len().min(rows);
    if drawn < rows {
        frame.push_str(&format!("\x1b[{};1H\x1b[J", drawn + 1));
    }
    frame.push_str("\x1b[?2026l");
    frame
}

/// Returns the longest start of `text` that is at most `columns` wide, and
/// its width. Control characters are dropped from the count, as from the
/// screen.
fn cut_to_width(text: &str, columns: usize) -> (&str, usize) {
    let mut width = 0;
    for (index, character) in text.char_indices() {
        let next = width + character.width().unwrap_or(0);
        if next > columns {
            return (&text[..index], width);
        }
        width = next;
    }
    (text, width)
}

/// Returns `text` broken into lines at most `columns` wide, at spaces. A word
/// wider than `columns` gets a line of its own, which drawing cuts.
pub fn wrap_to_width(text: &str, columns: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() > columns {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Reads the keys at the start of `bytes`, and returns them with the number
/// of bytes they took. Bytes at the end that do not make a whole key yet are
/// left for the next read. Sequences of keys that [`Key`] does not name are
/// skipped.
///
/// A lone Esc at the end counts as unfinished: it may start a sequence whose
/// rest has not arrived. With the kitty keyboard protocol, the Esc key comes
/// as `CSI 27 u` instead.
pub fn parse_keys(bytes: &[u8]) -> (Vec<Key>, usize) {
    let mut keys = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let (key, length) = match parse_key(&bytes[at..]) {
            Parsed::Key(key, length) => (Some(key), length),
            Parsed::Skip(length) => (None, length),
            Parsed::Unfinished => break,
        };
        keys.extend(key);
        at += length;
    }
    (keys, at)
}

/// What the start of some input holds.
enum Parsed {
    /// A key, which took this many bytes.
    Key(Key, usize),
    /// Bytes of no key that [`Key`] names.
    Skip(usize),
    /// The start of a key whose rest has not arrived.
    Unfinished,
}

fn parse_key(bytes: &[u8]) -> Parsed {
    match bytes {
        [] => Parsed::Unfinished,
        [0x1b] => Parsed::Unfinished,
        [0x1b, b'[', rest @ ..] => parse_csi(rest),
        [0x1b, b'O'] => Parsed::Unfinished,
        [0x1b, b'O', last, ..] => match final_key(*last) {
            Some(key) => Parsed::Key(key, 3),
            None => Parsed::Skip(3),
        },
        // An Esc that starts no sequence: the key, then what follows.
        [0x1b, ..] => Parsed::Key(Key::Escape, 1),
        [b'\r' | b'\n', ..] => Parsed::Key(Key::Enter, 1),
        [b'\t', ..] => Parsed::Key(Key::Tab, 1),
        [0x7f | 0x08, ..] => Parsed::Key(Key::Backspace, 1),
        [control @ 0x01..=0x1a, ..] => Parsed::Key(Key::Ctrl((b'a' + control - 1) as char), 1),
        [0x00..=0x1f, ..] => Parsed::Skip(1),
        [first, ..] => {
            let length = match first {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            if bytes.len() < length {
                return Parsed::Unfinished;
            }
            match std::str::from_utf8(&bytes[..length])
                .ok()
                .and_then(|text| text.chars().next())
            {
                Some(character) => Parsed::Key(Key::Char(character), length),
                None => Parsed::Skip(1),
            }
        }
    }
}

/// Reads a control sequence after its `ESC [`.
fn parse_csi(rest: &[u8]) -> Parsed {
    let Some(end) = rest.iter().position(|byte| (0x40..=0x7e).contains(byte)) else {
        return Parsed::Unfinished;
    };
    let length = 2 + end + 1;
    let params = std::str::from_utf8(&rest[..end]).unwrap_or_default();
    // Parameters are `;`-separated, and each may carry `:`-separated parts.
    let mut fields = params
        .split(';')
        .map(|field| field.split(':').next().unwrap_or_default());
    let first = fields.next().unwrap_or_default();
    let modifiers = fields.next().and_then(|field| field.parse::<u32>().ok());
    let key = match rest[end] {
        b'u' => kitty_key(first.parse().ok(), modifiers),
        b'~' => match first {
            "1" | "7" => Some(Key::Home),
            "4" | "8" => Some(Key::End),
            "5" => Some(Key::PageUp),
            "6" => Some(Key::PageDown),
            _ => None,
        },
        last => final_key(last),
    };
    match key {
        Some(key) => Parsed::Key(key, length),
        None => Parsed::Skip(length),
    }
}

/// Returns the key of a sequence that its final byte names.
fn final_key(last: u8) -> Option<Key> {
    match last {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(Key::Right),
        b'D' => Some(Key::Left),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        _ => None,
    }
}

/// Returns the key of a kitty `CSI <code> ; <modifiers> u` sequence.
/// `modifiers` is one more than the bits of the modifiers held, of which 4 is
/// Ctrl.
fn kitty_key(code: Option<u32>, modifiers: Option<u32>) -> Option<Key> {
    let ctrl = modifiers.is_some_and(|modifiers| modifiers.saturating_sub(1) & 4 != 0);
    match code? {
        27 => Some(Key::Escape),
        13 => Some(Key::Enter),
        9 => Some(Key::Tab),
        127 => Some(Key::Backspace),
        code => {
            let character = char::from_u32(code)?;
            if ctrl && character.is_ascii_alphabetic() {
                Some(Key::Ctrl(character.to_ascii_lowercase()))
            } else if character.is_control() || (0xe000..=0xf8ff).contains(&code) {
                // Kitty sends keys without a character from the private use
                // area.
                None
            } else {
                Some(Key::Char(character))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the keys of `bytes`, and checks that they took all of it.
    fn keys(bytes: &[u8]) -> Vec<Key> {
        let (keys, used) = parse_keys(bytes);
        assert_eq!(used, bytes.len());
        keys
    }

    #[test]
    fn reads_plain_keys() {
        assert_eq!(
            keys(b"aq\r\x7f\x03"),
            [
                Key::Char('a'),
                Key::Char('q'),
                Key::Enter,
                Key::Backspace,
                Key::Ctrl('c')
            ]
        );
        assert_eq!(keys("é".as_bytes()), [Key::Char('é')]);
    }

    #[test]
    fn reads_legacy_and_kitty_sequences() {
        assert_eq!(
            keys(b"\x1b[A\x1b[1;5B\x1bOC\x1b[5~\x1b[6~\x1b[H"),
            [
                Key::Up,
                Key::Down,
                Key::Right,
                Key::PageUp,
                Key::PageDown,
                Key::Home
            ]
        );
        assert_eq!(
            keys(b"\x1b[27u\x1b[99;5u\x1b[97u\x1b[113:81;2u"),
            [Key::Escape, Key::Ctrl('c'), Key::Char('a'), Key::Char('q')]
        );
    }

    #[test]
    fn leaves_unfinished_keys_for_the_next_read() {
        assert_eq!(parse_keys(b"a\x1b"), (vec![Key::Char('a')], 1));
        assert_eq!(parse_keys(b"a\x1b[1;5"), (vec![Key::Char('a')], 1));
        assert_eq!(parse_keys(&"é".as_bytes()[..1]), (vec![], 0));
    }

    #[test]
    fn skips_keys_it_does_not_name() {
        // F5, and the kitty code of a key from the private use area.
        assert_eq!(keys(b"\x1b[15~\x1b[57399ua"), [Key::Char('a')]);
    }

    #[test]
    fn writes_notifications_that_end_where_they_should() {
        assert_eq!(
            notification("a;b", "line\none; two\x1b"),
            "\x1b]777;notify;a b;line one; two \x1b\\"
        );
    }

    #[test]
    fn wraps_text_at_spaces() {
        assert_eq!(
            wrap_to_width("one two three  four", 9),
            ["one two", "three", "four"]
        );
        assert_eq!(wrap_to_width("unbreakable a", 4), ["unbreakable", "a"]);
    }

    #[test]
    fn cuts_lines_to_the_screen() {
        assert_eq!(cut_to_width("abcdef", 4), ("abcd", 4));
        assert_eq!(cut_to_width("日本語", 5), ("日本", 4));
        let lines = vec![
            vec![Span::styled("ab", Style::Bold), Span::plain("cdef")],
            vec![Span::plain("second")],
        ];
        assert_eq!(
            render_frame(&lines, 4, 1),
            "\x1b[?2026h\x1b[1;1H\x1b[1mab\x1b[0mcd\x1b[K\x1b[?2026l"
        );
        assert_eq!(
            render_frame(&lines[1..], 10, 3),
            "\x1b[?2026h\x1b[1;1Hsecond\x1b[K\x1b[2;1H\x1b[J\x1b[?2026l"
        );
    }
}
