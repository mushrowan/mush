use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell as BufferCell;
use ratatui::layout::{Position, Size};

const SIZE_CACHE_TTL: Duration = Duration::from_millis(250);

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

#[derive(Debug, Default)]
pub struct CachedSizeState {
    size: Cell<Option<Size>>,
    window_size: Cell<Option<WindowSize>>,
    last_probe: Cell<Option<Instant>>,
    resize_pending: Arc<AtomicBool>,
}

impl CachedSizeState {
    pub fn invalidate(&self) {
        self.size.set(None);
        self.window_size.set(None);
        self.last_probe.set(None);
    }

    pub fn resize_pending_flag(&self) -> Arc<AtomicBool> {
        self.resize_pending.clone()
    }

    fn consume_resize_pending(&self) -> bool {
        self.resize_pending.swap(false, Ordering::AcqRel)
    }

    fn cached_size_is_fresh(&self) -> bool {
        self.last_probe
            .get()
            .is_some_and(|last_probe| last_probe.elapsed() < SIZE_CACHE_TTL)
    }

    fn mark_probed_now(&self) {
        self.last_probe.set(Some(Instant::now()));
    }

    #[cfg(test)]
    pub(crate) fn has_cached_size(&self) -> bool {
        self.size.get().is_some() || self.window_size.get().is_some()
    }

    #[cfg(test)]
    pub(crate) fn set_size_for_test(&self, size: Size) {
        self.size.set(Some(size));
    }
}

pub struct SizeCachedBackend<B> {
    inner: B,
    state: Rc<CachedSizeState>,
}

impl<B> SizeCachedBackend<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            state: Rc::new(CachedSizeState::default()),
        }
    }

    pub fn cache_handle(&self) -> Rc<CachedSizeState> {
        self.state.clone()
    }
}

impl<B: Backend> Backend for SizeCachedBackend<B> {
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
        if self.state.consume_resize_pending() || !self.state.cached_size_is_fresh() {
            let s = self.inner.size()?;
            self.state.size.set(Some(s));
            self.state.window_size.set(None);
            self.state.mark_probed_now();
            return Ok(s);
        }
        if let Some(s) = self.state.size.get() {
            return Ok(s);
        }
        let s = self.inner.size()?;
        self.state.size.set(Some(s));
        self.state.mark_probed_now();
        Ok(s)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        if self.state.consume_resize_pending() || !self.state.cached_size_is_fresh() {
            let s = self.inner.window_size()?;
            self.state.window_size.set(Some(s));
            self.state.size.set(Some(s.columns_rows));
            self.state.mark_probed_now();
            return Ok(s);
        }
        if let Some(s) = self.state.window_size.get() {
            return Ok(s);
        }
        let s = self.inner.window_size()?;
        self.state.window_size.set(Some(s));
        self.state.size.set(Some(s.columns_rows));
        self.state.mark_probed_now();
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
        let backend = SizeCachedBackend::new(inner);

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
    fn stale_size_cache_forces_refresh_without_resize_event() {
        let inner = CountingBackend::new(80, 25);
        let backend = SizeCachedBackend::new(inner);
        let handle = backend.cache_handle();

        let _ = backend.size().unwrap();
        assert_eq!(backend.inner.size_calls.get(), 1);

        handle.last_probe.set(Some(Instant::now() - SIZE_CACHE_TTL));
        let _ = backend.size().unwrap();

        assert_eq!(
            backend.inner.size_calls.get(),
            2,
            "stale cache should probe the terminal again even without Event::Resize"
        );
    }

    #[test]
    fn invalidate_forces_refresh() {
        let inner = CountingBackend::new(80, 25);
        let backend = SizeCachedBackend::new(inner);
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

        let _ = backend.size().unwrap();
        assert_eq!(backend.inner.size_calls.get(), 2);
    }

    #[test]
    fn window_size_caches_and_syncs_size() {
        let inner = CountingBackend::new(80, 25);
        let mut backend = SizeCachedBackend::new(inner);

        let _ = backend.window_size().unwrap();
        let _ = backend.window_size().unwrap();
        assert_eq!(backend.inner.window_calls.get(), 1);

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
        let backend = SizeCachedBackend::new(inner);
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

        let _ = backend.size().unwrap();
        assert_eq!(backend.inner.size_calls.get(), 2);
    }

    #[test]
    fn mark_resize_pending_forces_window_size_refresh() {
        let inner = CountingBackend::new(80, 25);
        let mut backend = SizeCachedBackend::new(inner);
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
