use ratatui::text::{Line, Span};

use super::segments::{Segment, SegmentId, spans_width, truncate_spans};

pub(super) const SEPARATOR: usize = 2;
pub(super) const GAP: usize = 3;
pub(super) const PADDING: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Form(usize),
    Drop,
}

pub(super) const LADDER: &[(SegmentId, Step)] = &[
    (SegmentId::Cost, Step::Drop),
    (SegmentId::Tokens, Step::Drop),
    (SegmentId::Cwd, Step::Form(1)),
    (SegmentId::Quota, Step::Form(1)),
    (SegmentId::SubModel, Step::Drop),
    (SegmentId::Context, Step::Form(1)),
    (SegmentId::Ready, Step::Form(1)),
    (SegmentId::Activity, Step::Form(1)),
    (SegmentId::Cwd, Step::Drop),
    (SegmentId::Ready, Step::Drop),
    (SegmentId::Context, Step::Form(2)),
    (SegmentId::Jobs, Step::Form(1)),
    (SegmentId::Goal, Step::Form(1)),
    (SegmentId::Agents, Step::Form(1)),
    (SegmentId::Decisions, Step::Form(1)),
    (SegmentId::Activity, Step::Form(2)),
    (SegmentId::Jobs, Step::Form(2)),
    (SegmentId::Note, Step::Drop),
    (SegmentId::Quota, Step::Form(2)),
    (SegmentId::Model, Step::Form(1)),
    (SegmentId::Context, Step::Form(3)),
    (SegmentId::Auto, Step::Form(1)),
    (SegmentId::Activity, Step::Form(3)),
    (SegmentId::Quota, Step::Drop),
    (SegmentId::Model, Step::Form(2)),
    (SegmentId::Jobs, Step::Drop),
    (SegmentId::Auto, Step::Drop),
    (SegmentId::Agents, Step::Form(2)),
    (SegmentId::Decisions, Step::Form(2)),
    (SegmentId::Goal, Step::Form(2)),
    (SegmentId::Context, Step::Form(4)),
    (SegmentId::Activity, Step::Form(4)),
    (SegmentId::Agents, Step::Form(3)),
    (SegmentId::Alerts, Step::Form(1)),
    (SegmentId::Model, Step::Drop),
];

impl Segment {
    fn apply(&mut self, step: Step) {
        if self.dropped || self.forms.is_empty() {
            return;
        }
        let last = self.forms.len() - 1;
        match step {
            Step::Drop if !self.pinned => self.dropped = true,
            Step::Drop => self.form = last,
            Step::Form(form) => self.form = self.form.max(form.min(last)),
        }
    }
}

fn cluster_width(segments: &[Segment]) -> usize {
    let widths: Vec<usize> = segments
        .iter()
        .filter(|segment| !segment.dropped)
        .map(Segment::width)
        .collect();
    widths.iter().sum::<usize>() + SEPARATOR * widths.len().saturating_sub(1)
}

pub(super) fn row_width(left: &[Segment], right: &[Segment]) -> usize {
    let left = cluster_width(left);
    let right = cluster_width(right);
    let gap = if left > 0 && right > 0 { GAP } else { 0 };
    PADDING * 2 + left + gap + right
}

pub(super) fn fit(left: &mut [Segment], right: &mut [Segment], width: usize) {
    for (id, step) in LADDER {
        if row_width(left, right) <= width {
            break;
        }
        for segment in left.iter_mut().chain(right.iter_mut()) {
            if segment.id == *id {
                segment.apply(*step);
            }
        }
    }
    stretch(left, right, width);
}

fn stretch(left: &mut [Segment], right: &[Segment], width: usize) {
    let spare = width.saturating_sub(row_width(left, right));
    for segment in left.iter_mut() {
        if segment.dropped {
            continue;
        }
        let Some(full) = segment.stretch.clone() else {
            continue;
        };
        let target = spans_width(&full).min(segment.width() + spare);
        let spans = truncate_spans(&full, target);
        let form = &mut segment.forms[segment.form];
        form.width = spans_width(&spans);
        form.spans = spans;
        return;
    }
}

fn cluster(segments: &[Segment]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for segment in segments.iter().filter(|segment| !segment.dropped) {
        if !spans.is_empty() {
            spans.push(Span::raw(" ".repeat(SEPARATOR)));
        }
        spans.extend(segment.spans().iter().cloned());
    }
    spans
}

pub(super) fn compose(left: &[Segment], right: &[Segment], width: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(PADDING))];
    spans.extend(cluster(left));
    let right = cluster(right);
    if !right.is_empty() {
        let used = spans_width(&spans) + spans_width(&right) + PADDING;
        spans.push(Span::raw(" ".repeat(width.saturating_sub(used).max(1))));
        spans.extend(right);
    }
    spans.push(Span::raw(" ".repeat(PADDING)));
    if spans_width(&spans) > width {
        spans = truncate_spans(&spans, width);
    }
    Line::from(spans)
}
