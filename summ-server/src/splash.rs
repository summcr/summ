//! The startup banner: what `summ serve` prints once it is listening, and the
//! mark beside it when a person is watching.
//!
//! Declared by `main.rs`, so it is compiled into the binary and not the
//! library: it is presentation for one command, and nothing else should grow a
//! dependency on it.
//!
//! Three layouts, chosen by where stdout goes and nothing else:
//!
//! - **Plain**, when stdout is not a terminal - `docker logs`, systemd, a pipe,
//!   a CI transcript. Byte for byte what the banner was before it had a mark:
//!   no escapes, and nothing to strip before grepping it.
//! - **Tile**, on a terminal wide enough for it: the favicon redrawn in
//!   half-blocks, white strokes on the accent, with the banner set beside it.
//!   It is the layout Redis has used for a decade, and for the same reason: a
//!   server's banner is information, so the mark should cost it no lines. The
//!   tile is drawn on its own background rather than in the accent on the
//!   terminal's, because `#0b7285` all but vanishes on a dark theme and a
//!   white-on-teal tile reads the same on every one.
//! - **Compact**, on a terminal too narrow for the tile, or one whose width
//!   cannot be read: the plain layout with a one-cell mark on its heading. A
//!   line that wraps there is untidy; beside the tile it would land under the
//!   mark and knock every row after it out of line, which reads as a bug.
//!
//! Nothing here moves the cursor, animates or asks the terminal anything. The
//! banner lives in scrollback, and a query a terminal never answers costs
//! startup a timeout on every launch.

use std::io::IsTerminal;

/// The favicon's sigma (`ui/logo.svg`), on a grid of 14 x 12 pixels.
///
/// A third copy of the mark, after the SVG and the topbar's inline path, and
/// like the topbar's it is a copy because the medium cannot load the original.
/// Keep all three in step. A terminal cell is about twice as tall as it is
/// wide, and a half-block splits it in two, so each pixel here is close to
/// square and the tile comes out about as square as the favicon. The strokes
/// are bolder than the SVG's: three columns on the diagonals, because two read
/// as a hairline at this size.
const MARK: [&str; 12] = [
    "..............",
    "..##########..",
    "..##########..",
    "..###.........",
    "....###.......",
    "......###.....",
    "......###.....",
    "....###.......",
    "..###.........",
    "..##########..",
    "..##########..",
    "..............",
];

/// Columns the tile occupies.
const MARK_WIDTH: usize = 14;

/// A field's key, padded: the values line up one column past the longest key.
const KEY_WIDTH: usize = 14;

/// Where the text starts beside the tile: margin, tile, gutter.
const TEXT_COLUMN: usize = 2 + MARK_WIDTH + 3;

/// The banner's rows, before any layout has been chosen.
pub struct Banner {
    version: String,
    rows: Vec<Row>,
}

enum Row {
    /// `key` padded to [`KEY_WIDTH`], then the value. An empty key continues
    /// the field above it.
    Field(&'static str, String),
    /// Free text, at the fields' indent.
    Note(String),
    /// A line that sets its own indent and width and must never sit beside the
    /// mark: the open-registry box, whose one job is to stand out.
    Wide(String),
}

impl Banner {
    pub fn new(version: &str) -> Self {
        Self {
            version: version.to_owned(),
            rows: Vec::new(),
        }
    }

    pub fn field(&mut self, key: &'static str, value: impl Into<String>) {
        self.rows.push(Row::Field(key, value.into()));
    }

    /// A continuation of the field above, aligned with its value.
    pub fn more(&mut self, value: impl Into<String>) {
        self.field("", value);
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.rows.push(Row::Note(text.into()));
    }

    pub fn wide(&mut self, line: impl Into<String>) {
        self.rows.push(Row::Wide(line.into()));
    }

    /// Print it, laid out for wherever stdout goes.
    pub fn print(&self) {
        for line in self.render(Term::detect()) {
            println!("{line}");
        }
    }

    /// The lines to print: plain without a terminal, otherwise the tile if it
    /// fits and the compact layout if it does not.
    fn render(&self, term: Option<Term>) -> Vec<String> {
        let Some(term) = term else {
            return self.indented(format!("summ {}", self.version), Paint(None));
        };
        let paint = Paint(term.color);
        match term.columns {
            Some(columns) if self.tile_width() <= columns => self.tile(paint),
            _ => self.indented(
                format!(
                    "{} {} {}",
                    paint.tile(" \u{03a3} "),
                    paint.bold("summ"),
                    self.version
                ),
                paint,
            ),
        }
    }

    /// A heading, then every row two columns in: the plain and compact layouts.
    fn indented(&self, heading: String, paint: Paint) -> Vec<String> {
        let mut lines = vec![heading];
        for row in &self.rows {
            lines.push(match (row, self.text(row, paint)) {
                (Row::Wide(line), _) => line.clone(),
                (_, Some(text)) => format!("  {text}"),
                (_, None) => unreachable!("only a wide row has no text"),
            });
        }
        lines
    }

    /// The mark down the left, the heading and the rows beside it.
    ///
    /// A wide row breaks out: whatever is left of the mark is drawn first, on
    /// its own, so the box starts below the tile and never beside it. Rows
    /// after the mark keep the text column, so a continuation stays aligned
    /// with the value it continues.
    fn tile(&self, paint: Paint) -> Vec<String> {
        let mark = mark();
        let mut side = mark.iter().map(|row| format!("  {}", paint.tile(row)));
        let beside = |side: Option<String>, text: String| match side {
            Some(side) => format!("{side}   {text}"),
            None => format!("{:TEXT_COLUMN$}{text}", ""),
        };

        let heading = format!("{} {}", paint.bold("summ"), self.version);
        let mut lines = vec![beside(side.next(), heading)];
        for row in &self.rows {
            match (row, self.text(row, paint)) {
                (Row::Wide(line), _) => {
                    lines.extend(side.by_ref());
                    lines.push(line.clone());
                }
                (_, Some(text)) => lines.push(beside(side.next(), text)),
                (_, None) => unreachable!("only a wide row has no text"),
            }
        }
        lines.extend(side);
        lines
    }

    /// The columns the tile layout needs, wide rows aside: they set their own
    /// width, and it is the same in every layout.
    ///
    /// Characters stand in for columns. Everything here is ASCII but the data
    /// directory, and a path in a script whose characters are two columns wide
    /// is under-measured - which costs a wrapped line, not a wrong banner.
    fn tile_width(&self) -> usize {
        let heading = "summ ".len() + self.version.chars().count();
        let rows = self
            .rows
            .iter()
            .filter_map(|row| self.text(row, Paint(None)))
            .map(|text| text.chars().count());
        TEXT_COLUMN + rows.fold(heading, usize::max)
    }

    /// A row's text with no indent, or `None` for a wide row.
    fn text(&self, row: &Row, paint: Paint) -> Option<String> {
        match row {
            Row::Field("", value) => Some(format!("{:KEY_WIDTH$}{value}", "")),
            Row::Field(key, value) => Some(format!(
                "{}{value}",
                paint.dim(&format!("{key:<KEY_WIDTH$}"))
            )),
            Row::Note(text) => Some(text.clone()),
            Row::Wide(_) => None,
        }
    }
}

/// [`MARK`] in half-blocks: two pixel rows to a line of text.
fn mark() -> Vec<String> {
    MARK.chunks(2)
        .map(|pair| {
            pair[0]
                .bytes()
                .zip(pair[1].bytes())
                .map(|cell| match cell {
                    (b'#', b'#') => '\u{2588}', // █
                    (b'#', _) => '\u{2580}',    // ▀
                    (_, b'#') => '\u{2584}',    // ▄
                    _ => ' ',
                })
                .collect()
        })
        .collect()
}

/// What the terminal on stdout can show, or `None` when there is not one.
#[derive(Clone, Copy)]
struct Term {
    /// `None` when the size cannot be read, which takes the compact layout:
    /// the tile is only drawn where it is known to fit.
    columns: Option<usize>,
    /// `None` under `NO_COLOR`. The layout is unaffected - the mark is still a
    /// shape - only the escapes go.
    color: Option<Depth>,
}

impl Term {
    fn detect() -> Option<Term> {
        if !std::io::stdout().is_terminal() {
            return None;
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term == "dumb" {
            return None;
        }
        // https://no-color.org: present and not empty.
        let color = if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            None
        } else if matches!(
            std::env::var("COLORTERM").as_deref(),
            Ok("truecolor" | "24bit")
        ) {
            Some(Depth::True)
        } else if term.contains("256") {
            Some(Depth::Ansi256)
        } else {
            Some(Depth::Basic)
        };
        Some(Term {
            columns: columns(),
            color,
        })
    }
}

/// The width of the terminal on stdout.
///
/// The standard library has no way to ask, and `COLUMNS` is a shell variable
/// that is rarely exported, so this is the ioctl. A size of zero is what a
/// pseudo-terminal reports before anything has set it - `docker run -t`, for
/// one, for a moment after start - and it is treated as unknown.
#[cfg(unix)]
fn columns() -> Option<usize> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCGWINSZ` writes one `winsize` through the pointer, which
    // points at a live, correctly typed local. A bad descriptor or one that is
    // not a terminal is an error return, not undefined behaviour.
    let status = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    (status == 0 && size.ws_col > 0).then_some(usize::from(size.ws_col))
}

#[cfg(not(unix))]
fn columns() -> Option<usize> {
    None
}

/// How many colours to ask for. The tile is the accent, `#0b7285`, where the
/// terminal says it can show it; index 30 (`#008787`) is the nearest of the
/// 256; and cyan is all an eight-colour console has.
#[derive(Clone, Copy)]
enum Depth {
    True,
    Ansi256,
    Basic,
}

impl Depth {
    fn tile(self) -> &'static str {
        match self {
            Depth::True => "\x1b[48;2;11;114;133m\x1b[38;2;255;255;255m",
            Depth::Ansi256 => "\x1b[48;5;30m\x1b[38;5;231m",
            Depth::Basic => "\x1b[46m\x1b[97m",
        }
    }
}

/// Styling that is a no-op without colour, so every layout is written once.
#[derive(Clone, Copy)]
struct Paint(Option<Depth>);

impl Paint {
    fn tile(self, text: &str) -> String {
        self.wrap(self.0.map(Depth::tile), text)
    }

    fn bold(self, text: &str) -> String {
        self.wrap(Some("\x1b[1m"), text)
    }

    fn dim(self, text: &str) -> String {
        self.wrap(Some("\x1b[2m"), text)
    }

    fn wrap(self, on: Option<&str>, text: &str) -> String {
        match (self.0, on) {
            (Some(_), Some(on)) => format!("{on}{text}\x1b[0m"),
            _ => text.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(data_dir: &str) -> Banner {
        let mut banner = Banner::new("0.1.0-rc.4");
        banner.field("listening on", "127.0.0.1:3110");
        banner.field("registry", "http://127.0.0.1:3110/v2/");
        banner.field("data dir", data_dir);
        banner.field("purge", "every 1h after 1d unreferenced");
        banner.field("auth mode", "open - no credential is required");
        banner.more("any process on this machine may pull");
        banner.note("!! a note");
        banner
    }

    fn term(columns: Option<usize>, color: Option<Depth>) -> Option<Term> {
        Some(Term { columns, color })
    }

    /// The width a line takes on screen, escapes removed.
    fn visible(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.by_ref().find(|&c| c == 'm');
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Off a terminal the banner is what it was before it had a mark, to the
    /// byte, because that is what a log pipeline and a grep were written
    /// against.
    #[test]
    fn without_a_terminal_the_banner_is_plain_text() {
        assert_eq!(
            sample("/data").render(None),
            [
                "summ 0.1.0-rc.4",
                "  listening on  127.0.0.1:3110",
                "  registry      http://127.0.0.1:3110/v2/",
                "  data dir      /data",
                "  purge         every 1h after 1d unreferenced",
                "  auth mode     open - no credential is required",
                "                any process on this machine may pull",
                "  !! a note",
            ]
        );
    }

    /// Every half-block line is the tile's width, or the text beside it would
    /// start in a different column on different rows.
    #[test]
    fn the_mark_is_a_rectangle() {
        assert_eq!(MARK.len() % 2, 0, "half-blocks pair the pixel rows");
        for row in MARK {
            assert_eq!(row.len(), MARK_WIDTH, "{row:?}");
            assert!(row.bytes().all(|b| b == b'#' || b == b'.'), "{row:?}");
        }
        let mark = mark();
        assert_eq!(mark.len(), MARK.len() / 2);
        for row in &mark {
            assert_eq!(row.chars().count(), MARK_WIDTH, "{row:?}");
        }
    }

    /// The tile is only drawn where it fits, and the fit is measured on what
    /// is actually printed: at the width it claims to need, no line wraps.
    #[test]
    fn the_tile_is_drawn_only_where_it_fits() {
        let banner = sample("/data");
        let need = banner.tile_width();

        let tiled = banner.render(term(Some(need), Some(Depth::True)));
        let mark = mark();
        for (line, row) in tiled.iter().zip(&mark) {
            assert!(
                visible(line).starts_with(&format!("  {row}   ")),
                "{line:?}"
            );
        }
        assert!(tiled.iter().all(|l| visible(l).chars().count() <= need));
        assert!(tiled.iter().any(|l| visible(l).chars().count() == need));

        let narrow = banner.render(term(Some(need - 1), Some(Depth::True)));
        assert!(
            visible(&narrow[0]).starts_with(" \u{03a3}  summ"),
            "{:?}",
            narrow[0]
        );
        assert_eq!(narrow.len(), 1 + banner.rows.len());

        let unknown = banner.render(term(None, Some(Depth::True)));
        assert_eq!(unknown, narrow, "an unreadable size is not room for it");
    }

    /// Beside the tile every row starts in one column, and a continuation
    /// still lines up under the value it continues.
    #[test]
    fn beside_the_tile_the_text_keeps_one_column() {
        let lines = sample("/data").render(term(Some(200), None));
        // Characters, not bytes: a half-block is three bytes of UTF-8.
        let at = |line: &str| line.chars().skip(TEXT_COLUMN).collect::<String>();
        assert_eq!(at(&lines[0]), "summ 0.1.0-rc.4");
        assert_eq!(at(&lines[1]), "listening on  127.0.0.1:3110");
        assert_eq!(
            at(&lines[6]).find("any"),
            at(&lines[5]).find("open"),
            "{lines:#?}"
        );
        assert_eq!(at(&lines[7]), "!! a note", "past the mark, same column");
    }

    /// The open-registry box never sits beside the mark: the rest of the tile
    /// is drawn first, on its own, and the box keeps its own indent.
    #[test]
    fn a_wide_row_starts_below_the_tile() {
        let mut banner = Banner::new("0.1.0");
        banner.field("listening on", "0.0.0.0:3110");
        banner.wide("  ******");
        banner.wide("  *  OPEN  *");

        let lines = banner.render(term(Some(200), None));
        let mark = mark();
        assert_eq!(lines.len(), mark.len() + 2, "{lines:#?}");
        for (line, row) in lines.iter().zip(&mark).skip(2) {
            assert_eq!(line, &format!("  {row}"), "the tile, alone");
        }
        assert_eq!(&lines[mark.len()..], ["  ******", "  *  OPEN  *"]);
    }

    /// `NO_COLOR` takes the escapes and leaves the layout: the same lines, to
    /// the character, as the coloured ones once their escapes are removed.
    #[test]
    fn no_color_changes_nothing_but_the_escapes() {
        let banner = sample("/data");
        for columns in [Some(200), Some(20), None] {
            let bare = banner.render(term(columns, None));
            assert!(bare.iter().all(|l| !l.contains('\x1b')), "{bare:#?}");
            for depth in [Depth::True, Depth::Ansi256, Depth::Basic] {
                let painted = banner.render(term(columns, Some(depth)));
                let stripped: Vec<_> = painted.iter().map(|l| visible(l)).collect();
                assert_eq!(stripped, bare);
            }
        }
    }

    /// A long data directory is the realistic way a banner stops fitting.
    #[test]
    fn a_long_row_takes_the_compact_layout_at_eighty_columns() {
        let short = sample("/data").render(term(Some(80), None));
        assert!(short[0].starts_with("  "), "tile: {short:#?}");

        let long = sample(&format!("/{}", "deep/".repeat(12))).render(term(Some(80), None));
        assert!(long[0].starts_with(" \u{03a3}  summ"), "compact: {long:#?}");
    }
}
