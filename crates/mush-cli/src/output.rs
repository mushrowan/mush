use std::fmt::{self, Display};

#[derive(Clone, Copy)]
pub enum Style {
    Bold,
    Dim,
    Red,
    Green,
    Yellow,
    Magenta,
    Cyan,
}

#[must_use]
pub fn short_id(id: &str) -> &str {
    id.char_indices().nth(8).map_or(id, |(end, _)| &id[..end])
}

pub fn stderr_colour() -> bool {
    anstream::AutoStream::auto(std::io::stderr()).current_choice()
        == anstream::ColorChoice::AlwaysAnsi
}

impl Style {
    pub fn stdout(self, text: impl Display) -> impl Display {
        self.paint(
            text,
            anstream::AutoStream::auto(std::io::stdout()).current_choice()
                == anstream::ColorChoice::AlwaysAnsi,
        )
    }

    pub fn stderr(self, text: impl Display) -> impl Display {
        self.paint(text, stderr_colour())
    }

    fn paint(self, text: impl Display, colour: bool) -> impl Display {
        Styled {
            style: self,
            text,
            colour,
        }
    }
}

struct Styled<T> {
    style: Style,
    text: T,
    colour: bool,
}

impl<T: Display> Display for Styled<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.colour {
            f.write_str(match self.style {
                Style::Bold => "\x1b[1m",
                Style::Dim => "\x1b[2m",
                Style::Red => "\x1b[31m",
                Style::Green => "\x1b[32m",
                Style::Yellow => "\x1b[33m",
                Style::Magenta => "\x1b[35m",
                Style::Cyan => "\x1b[36m",
            })?;
        }
        self.text.fmt(f)?;
        if self.colour {
            f.write_str("\x1b[0m")?;
        }
        Ok(())
    }
}
