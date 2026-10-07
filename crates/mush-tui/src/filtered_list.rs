pub trait FilteredList {
    fn filter_and_selected(&mut self) -> (&mut String, &mut usize);
    fn visible_len(&self) -> usize;

    fn select_previous(&mut self) {
        let (_, selected) = self.filter_and_selected();
        *selected = selected.saturating_sub(1);
    }

    fn select_next(&mut self) {
        let visible = self.visible_len();
        let (_, selected) = self.filter_and_selected();
        if *selected + 1 < visible {
            *selected += 1;
        }
    }

    fn push_filter(&mut self, c: char) {
        let (filter, selected) = self.filter_and_selected();
        filter.push(c);
        *selected = 0;
    }

    fn pop_filter(&mut self) {
        let (filter, selected) = self.filter_and_selected();
        filter.pop();
        *selected = 0;
    }
}
