//! caching wrapper around a ratatui [`Backend`] that avoids per-frame
//! `size()` / `window_size()` syscalls.
//!
//! `CrosstermBackend`'s `size()` implementation opens `/dev/tty` and
//! runs `ioctl(TIOCGWINSZ)` on every invocation. ratatui's
//! `Terminal::autoresize` calls it once per frame to detect resizes.
//! profiling showed this accounted for ~3% of main-thread samples
//! with the `File::open` alone at ~2.4%.
//!
//! [`CachingBackend`] forwards every call to its inner backend except
//! `size()` and `window_size()`, which are memoised. actual terminal
//! resizes invalidate the cache via the handle returned by
//! [`CachingBackend::cache_handle`] (called from the `Event::Resize`
//! branch of the event loop).

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell as BufferCell;
use ratatui::layout::{Position, Size};

/// shared cache slot for [`CachingBackend`].
///
/// held by the backend itself and by any invalidation site (typically
/// the event loop). interior mutability is enough because all access
/// happens on the single UI thread.
#[derive(Debug, Default)]
pub struct CachedSizeState {
    size: Cell<Option<Size>>,
    window_size: Cell<Option<WindowSize>>,
}

impl CachedSizeState {
    /// clear cached dimensions so the next `size()` / `window_size()`
    /// call refetches from the underlying backend. should be invoked
    /// on receiving a terminal resize event
    pub fn invalidate(&self) {
        self.size.set(None);
        self.window_size.set(None);
    }
}

/// wrapper around a ratatui [`Backend`] that memoises `size()` and
/// `window_size()` until explicitly invalidated.
pub struct CachingBackend<B> {
    inner: B,
    state: Rc<CachedSizeState>,
}

impl<B> CachingBackend<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            state: Rc::new(CachedSizeState::default()),
        }
    }

    /// handle for invalidating the cache from elsewhere (event loop)
    pub fn cache_handle(&self) -> Rc<CachedSizeState> {
        self.state.clone()
    }
}

impl<B: Backend> Backend for CachingBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a BufferCell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        if let Some(s) = self.state.size.get() {
            return Ok(s);
        }
        let s = self.inner.size()?;
        self.state.size.set(Some(s));
        Ok(s)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        if let Some(s) = self.state.window_size.get() {
            return Ok(s);
        }
        let s = self.inner.window_size()?;
        self.state.window_size.set(Some(s));
        // keep `size` cache consistent with the fresh window_size
        self.state.size.set(Some(s.columns_rows));
        Ok(s)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }

    fn scroll_region_up(&mut self, region: Range<u16>, line_count: u16) -> Result<(), Self::Error> {
        self.inner.scroll_region_up(region, line_count)
    }

    fn scroll_region_down(
        &mut self,
        region: Range<u16>,
        line_count: u16,
    ) -> Result<(), Self::Error> {
        self.inner.scroll_region_down(region, line_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// counts how many times `size()` / `window_size()` was actually
    /// invoked on the inner backend, so we can verify the cache
    struct CountingBackend {
        inner: TestBackend,
        size_calls: Cell<u32>,
        window_calls: Cell<u32>,
    }

    impl CountingBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: TestBackend::new(width, height),
                size_calls: Cell::new(0),
                window_calls: Cell::new(0),
            }
        }
    }

    impl Backend for CountingBackend {
        type Error = <TestBackend as Backend>::Error;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a BufferCell)>,
        {
            self.inner.draw(content)
        }
        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.hide_cursor()
        }
        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.show_cursor()
        }
        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.inner.get_cursor_position()
        }
        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.inner.set_cursor_position(position)
        }
        fn clear(&mut self) -> Result<(), Self::Error> {
            self.inner.clear()
        }
        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.inner.clear_region(clear_type)
        }
        fn size(&self) -> Result<Size, Self::Error> {
            self.size_calls.set(self.size_calls.get() + 1);
            self.inner.size()
        }
        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.window_calls.set(self.window_calls.get() + 1);
            self.inner.window_size()
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            self.inner.flush()
        }
        fn scroll_region_up(
            &mut self,
            region: Range<u16>,
            line_count: u16,
        ) -> Result<(), Self::Error> {
            self.inner.scroll_region_up(region, line_count)
        }
        fn scroll_region_down(
            &mut self,
            region: Range<u16>,
            line_count: u16,
        ) -> Result<(), Self::Error> {
            self.inner.scroll_region_down(region, line_count)
        }
    }

    #[test]
    fn size_cached_across_calls() {
        let inner = CountingBackend::new(80, 25);
        let backend = CachingBackend::new(inner);

        let first = backend.size().unwrap();
        let second = backend.size().unwrap();
        let third = backend.size().unwrap();

        assert_eq!(first, Size::new(80, 25));
        assert_eq!(first, second);
        assert_eq!(first, third);
        assert_eq!(
            backend.inner.size_calls.get(),
            1,
            "inner size() should be called exactly once"
        );
    }

    #[test]
    fn invalidate_forces_refresh() {
        let inner = CountingBackend::new(80, 25);
        let backend = CachingBackend::new(inner);
        let handle = backend.cache_handle();

        let _ = backend.size().unwrap();
        let _ = backend.size().unwrap();
        assert_eq!(backend.inner.size_calls.get(), 1);

        handle.invalidate();
        let _ = backend.size().unwrap();
        assert_eq!(
            backend.inner.size_calls.get(),
            2,
            "invalidate should force the next call to hit the inner backend"
        );

        // subsequent calls come back from cache
        let _ = backend.size().unwrap();
        assert_eq!(backend.inner.size_calls.get(), 2);
    }

    #[test]
    fn window_size_caches_and_syncs_size() {
        let inner = CountingBackend::new(80, 25);
        let mut backend = CachingBackend::new(inner);

        let _ = backend.window_size().unwrap();
        let _ = backend.window_size().unwrap();
        assert_eq!(backend.inner.window_calls.get(), 1);

        // size() should now read from the same cache (window_size stores it)
        let s = backend.size().unwrap();
        assert_eq!(s, Size::new(80, 25));
        assert_eq!(
            backend.inner.size_calls.get(),
            0,
            "size() should hit the cache populated by window_size()"
        );
    }
}
