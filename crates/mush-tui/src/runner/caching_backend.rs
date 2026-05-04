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
//! [`CachingBackend::cache_handle`]. there are two invalidation paths:
//!
//! 1. the `Event::Resize` arm of the main event loop calls
//!    [`CachedSizeState::invalidate`] directly. this is enough when the
//!    loop drains terminal events promptly.
//! 2. a sigwinch-driven async task flips an `Arc<AtomicBool>` obtained
//!    via [`CachedSizeState::resize_pending_flag`]. `size()` /
//!    `window_size()` consume the flag at the top of every call, so a
//!    real os-level resize can never be hidden by a streaming burst
//!    starving the event loop's resize-event processing.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell as BufferCell;
use ratatui::layout::{Position, Size};

/// spawn a background task that flips `flag` to true on every
/// `SIGWINCH`. wired into [`CachingBackend`] so a real os-level resize
/// can never be hidden by a streaming burst starving the event loop's
/// `Event::Resize` processing.
///
/// the returned [`tokio::task::JoinHandle`] should be aborted at
/// shutdown so the listener doesn't outlive `run_tui`. on non-unix
/// platforms the task is a no-op (no SIGWINCH equivalent); cache
/// freshness then relies entirely on `Event::Resize` draining via the
/// main event loop.
#[cfg(unix)]
pub(super) fn spawn_sigwinch_listener(flag: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut signal =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) {
                Ok(sig) => sig,
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        "failed to install sigwinch listener; size cache will only \
                     refresh via Event::Resize draining"
                    );
                    return;
                }
            };
        while signal.recv().await.is_some() {
            flag.store(true, Ordering::Release);
        }
    })
}

#[cfg(not(unix))]
pub(super) fn spawn_sigwinch_listener(_flag: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {})
}

/// shared cache slot for [`CachingBackend`].
///
/// held by the backend itself and by any invalidation site (typically
/// the event loop). interior mutability is enough because all access
/// happens on the single UI thread.
#[derive(Debug, Default)]
pub struct CachedSizeState {
    size: Cell<Option<Size>>,
    window_size: Cell<Option<WindowSize>>,
    /// flipped to true by the sigwinch listener task (or any other
    /// off-thread observer) when the os reports a terminal resize.
    /// consumed at the top of `size()` / `window_size()` so a stale
    /// cache can never outlive a real resize even if `Event::Resize`
    /// is still queued
    resize_pending: Arc<AtomicBool>,
}

impl CachedSizeState {
    /// clear cached dimensions so the next `size()` / `window_size()`
    /// call refetches from the underlying backend. should be invoked
    /// on receiving a terminal resize event
    pub fn invalidate(&self) {
        self.size.set(None);
        self.window_size.set(None);
    }

    /// shareable handle to the resize-pending flag. callers that
    /// observe os-level resize signals (e.g. sigwinch) can flip this
    /// from any thread/task and the next backend size query will
    /// refresh
    pub fn resize_pending_flag(&self) -> Arc<AtomicBool> {
        self.resize_pending.clone()
    }

    /// atomically clear and return whether a resize was pending. on
    /// `true`, callers should treat both cached values as stale and
    /// refetch
    fn consume_resize_pending(&self) -> bool {
        self.resize_pending.swap(false, Ordering::AcqRel)
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
        if self.state.consume_resize_pending() {
            // os reported a resize since the last query; bypass both
            // caches so a follow-up window_size() in the same frame
            // also refreshes
            let s = self.inner.size()?;
            self.state.size.set(Some(s));
            self.state.window_size.set(None);
            return Ok(s);
        }
        if let Some(s) = self.state.size.get() {
            return Ok(s);
        }
        let s = self.inner.size()?;
        self.state.size.set(Some(s));
        Ok(s)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        if self.state.consume_resize_pending() {
            let s = self.inner.window_size()?;
            self.state.window_size.set(Some(s));
            self.state.size.set(Some(s.columns_rows));
            return Ok(s);
        }
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

    #[test]
    fn mark_resize_pending_forces_size_refresh() {
        let inner = CountingBackend::new(80, 25);
        let backend = CachingBackend::new(inner);
        let flag = backend.cache_handle().resize_pending_flag();

        let _ = backend.size().unwrap();
        let _ = backend.size().unwrap();
        assert_eq!(backend.inner.size_calls.get(), 1);

        flag.store(true, Ordering::Release);
        let _ = backend.size().unwrap();
        assert_eq!(
            backend.inner.size_calls.get(),
            2,
            "flipping the resize_pending flag should force the next size() to refetch"
        );

        // subsequent calls come back from cache until the next flip
        let _ = backend.size().unwrap();
        assert_eq!(backend.inner.size_calls.get(), 2);
    }

    #[test]
    fn mark_resize_pending_forces_window_size_refresh() {
        let inner = CountingBackend::new(80, 25);
        let mut backend = CachingBackend::new(inner);
        let flag = backend.cache_handle().resize_pending_flag();

        let _ = backend.window_size().unwrap();
        let _ = backend.window_size().unwrap();
        assert_eq!(backend.inner.window_calls.get(), 1);

        flag.store(true, Ordering::Release);
        let _ = backend.window_size().unwrap();
        assert_eq!(
            backend.inner.window_calls.get(),
            2,
            "flipping the resize_pending flag should force window_size() to refetch"
        );
    }
}
