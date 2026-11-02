//! Presentation validity: the links from notes and bar lines to the
//! timeline, and whether one playhead can meet every note endpoint
//! (design.md "Chart Model", "Timeline").

use crate::{
    Chart, EndpointLocation, EndpointPart, LANE_COUNT, Lane, NoteAnchors, NoteKind, Position,
    PresentationError, TimeUs, TimelineId,
};

impl Chart {
    /// Checks the links between the notes, the bar lines and the timeline:
    /// every endpoint names a row at its own time and position, a tap's or
    /// a mine's two anchors are one row, a long note's end row is not
    /// before its head row, each note's head row is not before the end row
    /// of the note before it in its lane, and the bar lines and their rows
    /// match one for one. Rows are checked by identity: two endpoints at
    /// one time and position may name distinct rows. It assumes
    /// [`Chart::validate`] passes and checks nothing that does.
    ///
    /// # Errors
    ///
    /// The first [`PresentationError`] other than
    /// [`EndpointConflict`](PresentationError::EndpointConflict), lane by
    /// lane, then the bar lines.
    pub fn validate_presentation_links(&self) -> Result<(), PresentationError> {
        for lane in Lane::all() {
            let mut prev_end: Option<TimelineId> = None;
            for (index, note) in self.lanes[lane.index()].iter().enumerate() {
                let NoteAnchors { head, end } = note.anchors;
                match note.kind {
                    NoteKind::Long { .. } if end < head => {
                        return Err(PresentationError::ReversedAnchors { lane, index });
                    }
                    NoteKind::Tap | NoteKind::Mine { .. } if end != head => {
                        return Err(PresentationError::SplitAnchors { lane, index });
                    }
                    _ => {}
                }
                if prev_end.is_some_and(|prev| head < prev) {
                    return Err(PresentationError::LaneOrder { lane, index });
                }
                prev_end = Some(end);
                for (part, id, at, pos) in [
                    (EndpointPart::Head, head, note.time, note.pos),
                    (EndpointPart::End, end, note.end(), note.end_pos()),
                ] {
                    let row = self.timeline.point(id);
                    if !row.is_some_and(|r| r.at == at && r.pos == pos) {
                        return Err(PresentationError::Link {
                            endpoint: EndpointLocation {
                                lane,
                                index,
                                part,
                                timeline_id: id,
                            },
                        });
                    }
                }
            }
        }
        let rows = self.timeline.bar_ids();
        if rows.len() != self.bar_lines.len() {
            return Err(PresentationError::BarCount {
                bars: self.bar_lines.len(),
                rows: rows.len(),
            });
        }
        for (index, (&pos, &id)) in self.bar_lines.iter().zip(rows).enumerate() {
            if !self.timeline.point(id).is_some_and(|r| r.pos == pos) {
                return Err(PresentationError::BarLink { index });
            }
        }
        Ok(())
    }

    /// Checks the links ([`validate_presentation_links`]), then that the
    /// note endpoints sitting at one playback µs, taps, mines and both ends
    /// of long notes in every lane, all sit at one position: a chart one
    /// playhead can present at the speed it was loaded at. Rows of BGM, bar
    /// lines, BPM changes and STOPs take no part. The endpoints are walked
    /// in identity order, lane then index then head before end among equal
    /// rows, with one cursor per lane and nothing allocated; the first
    /// endpoint of a µs is compared with each later one.
    ///
    /// [`validate_presentation_links`]: Self::validate_presentation_links
    ///
    /// # Errors
    ///
    /// The links' [`PresentationError`], or
    /// [`EndpointConflict`](PresentationError::EndpointConflict) for the
    /// first endpoint found at another position than the first endpoint of
    /// its µs.
    pub fn validate_presentation(&self) -> Result<(), PresentationError> {
        self.validate_presentation_links()?;
        let mut cursors = [Cursor::default(); LANE_COUNT];
        let mut bucket: Option<(TimeUs, EndpointLocation, Position)> = None;
        loop {
            let mut next: Option<EndpointLocation> = None;
            for lane in Lane::all() {
                let Some(e) = self.endpoint(lane, cursors[lane.index()]) else {
                    continue;
                };
                // Lanes are walked in order, so a tie keeps the lower lane.
                if next.is_none_or(|n| e.timeline_id < n.timeline_id) {
                    next = Some(e);
                }
            }
            let Some(e) = next else {
                return Ok(());
            };
            cursors[e.lane.index()] = self.advance(e);
            // The links passed, so every endpoint's row exists.
            let Some(row) = self.timeline.point(e.timeline_id) else {
                return Err(PresentationError::Link { endpoint: e });
            };
            match bucket {
                Some((at, first, first_pos)) if at == row.at => {
                    if row.pos != first_pos {
                        return Err(PresentationError::EndpointConflict {
                            at,
                            first,
                            second: e,
                            first_pos,
                            second_pos: row.pos,
                        });
                    }
                }
                _ => bucket = Some((row.at, e, row.pos)),
            }
        }
    }

    /// The endpoint at `cursor` in `lane`'s stream: each note's head, then
    /// a long note's end.
    fn endpoint(&self, lane: Lane, cursor: Cursor) -> Option<EndpointLocation> {
        let note = self.lanes[lane.index()].get(cursor.index)?;
        Some(EndpointLocation {
            lane,
            index: cursor.index,
            part: cursor.part,
            timeline_id: match cursor.part {
                EndpointPart::Head => note.anchors.head,
                EndpointPart::End => note.anchors.end,
            },
        })
    }

    /// The cursor after `e`.
    fn advance(&self, e: EndpointLocation) -> Cursor {
        let long = self.lanes[e.lane.index()]
            .get(e.index)
            .is_some_and(|n| matches!(n.kind, NoteKind::Long { .. }));
        match e.part {
            EndpointPart::Head if long => Cursor {
                index: e.index,
                part: EndpointPart::End,
            },
            EndpointPart::Head | EndpointPart::End => Cursor {
                index: e.index + 1,
                part: EndpointPart::Head,
            },
        }
    }
}

/// A position in a lane's endpoint stream.
#[derive(Clone, Copy)]
struct Cursor {
    index: usize,
    part: EndpointPart,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            index: 0,
            part: EndpointPart::Head,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Chart, ChartBuilder, ChartTimeline, EndpointLocation, EndpointPart, Lane, Note,
        NoteAnchors, NoteKind, Position, PresentationError, Speed, Tempo, TimeUs, TimelineId,
        TimelinePoint,
    };

    fn key(n: u8) -> Lane {
        Lane::new(n).unwrap()
    }

    fn tempo() -> Tempo {
        Tempo::from_milli(150_000).unwrap()
    }

    /// Replaces the chart's timeline with `rows` (`(µs, position)`) and
    /// drops its bar lines.
    fn with_rows(chart: &mut Chart, rows: &[(i64, i128)]) {
        let points = rows
            .iter()
            .map(|&(at, pos)| TimelinePoint {
                at: TimeUs(at),
                pos: Position(pos),
                tempo: tempo(),
            })
            .collect();
        chart.timeline = ChartTimeline::new(points, vec![], tempo(), Speed::NORMAL).unwrap();
        chart.bar_lines.clear();
    }

    /// A tap on row `id` of `rows`.
    fn tap(rows: &[(i64, i128)], id: u32) -> Note {
        let (at, pos) = rows[id as usize];
        Note {
            time: TimeUs(at),
            pos: Position(pos),
            sound: None,
            kind: NoteKind::Tap,
            anchors: NoteAnchors {
                head: TimelineId(id),
                end: TimelineId(id),
            },
        }
    }

    /// A long note from row `head` to row `end` of `rows`.
    fn long(rows: &[(i64, i128)], head: u32, end: u32) -> Note {
        let (end_at, end_pos) = rows[end as usize];
        Note {
            kind: NoteKind::Long {
                end: TimeUs(end_at),
                end_pos: Position(end_pos),
                end_sound: None,
            },
            anchors: NoteAnchors {
                head: TimelineId(head),
                end: TimelineId(end),
            },
            ..tap(rows, head)
        }
    }

    fn at(lane: u8, index: usize, part: EndpointPart, id: u32) -> EndpointLocation {
        EndpointLocation {
            lane: key(lane),
            index,
            part,
            timeline_id: TimelineId(id),
        }
    }

    #[test]
    fn built_charts_link_every_endpoint_and_bar_line() {
        let chart = ChartBuilder::new(150_000)
            .tap(key(1), TimeUs(1_000))
            .tap(key(2), TimeUs(1_000))
            .long(key(3), TimeUs(500), TimeUs(4_000))
            .stop(TimeUs(2_000), TimeUs(1_000))
            .bpm(TimeUs(2_000), 300_000)
            .bar(TimeUs(3_000))
            .bar(TimeUs(0))
            .build()
            .unwrap();
        assert_eq!(chart.validate_presentation(), Ok(()));
        let t = &chart.timeline;
        // Rows: 0, 500, 1 000, 2 000 (the change and the STOP's start),
        // 3 000 (the STOP's end and a bar line), 4 000.
        let times: Vec<i64> = t.points().iter().map(|p| p.at.0).collect();
        assert_eq!(times, [0, 500, 1_000, 2_000, 3_000, 4_000]);
        assert_eq!(
            chart.lanes[1][0].anchors.head,
            chart.lanes[2][0].anchors.head
        );
        assert_eq!(
            chart.lanes[3][0].anchors,
            NoteAnchors {
                head: TimelineId(1),
                end: TimelineId(5)
            }
        );
        assert_eq!(t.bar_ids(), [TimelineId(0), TimelineId(4)]);
        assert_eq!(
            chart.bar_lines,
            [Position(0), chart.scroll.position_at(TimeUs(3_000))]
        );
        // The STOP holds the position from its start to its end.
        assert_eq!(t.points()[3].pos, t.points()[4].pos);
        let fast = Tempo::from_milli(300_000).unwrap();
        assert_eq!((t.meta().initial, t.meta().max), (tempo(), fast));
        assert_eq!(t.tempo_at(TimeUs(2_500)), fast);
    }

    #[test]
    fn stale_links_fail_the_link_check_but_not_validate() {
        let built = ChartBuilder::new(150_000)
            .tap(key(1), TimeUs(1_000))
            .long(key(2), TimeUs(1_000), TimeUs(2_000))
            .bar(TimeUs(2_000))
            .build()
            .unwrap();
        let link = |lane, part, id| PresentationError::Link {
            endpoint: at(lane, 0, part, id),
        };

        let mut moved = built.clone();
        moved.lanes[1][0].time = TimeUs(1_500);
        moved.lanes[1][0].pos = moved.scroll.position_at(TimeUs(1_500));
        assert_eq!(moved.validate(), Ok(()));
        assert_eq!(
            moved.validate_presentation_links(),
            Err(link(1, EndpointPart::Head, 1))
        );

        let mut missing = built.clone();
        missing.lanes[1][0].anchors = NoteAnchors {
            head: TimelineId(9),
            end: TimelineId(9),
        };
        assert_eq!(
            missing.validate_presentation_links(),
            Err(link(1, EndpointPart::Head, 9))
        );

        let mut split = built.clone();
        split.lanes[1][0].anchors.end = TimelineId(2);
        assert_eq!(
            split.validate_presentation_links(),
            Err(PresentationError::SplitAnchors {
                lane: key(1),
                index: 0
            })
        );

        let mut end = built.clone();
        end.lanes[2][0].anchors.end = TimelineId(1);
        assert_eq!(
            end.validate_presentation_links(),
            Err(link(2, EndpointPart::End, 1))
        );
        end.lanes[2][0].anchors.head = TimelineId(2);
        assert_eq!(
            end.validate_presentation_links(),
            Err(PresentationError::ReversedAnchors {
                lane: key(2),
                index: 0
            })
        );

        let mut bars = built.clone();
        bars.bar_lines.push(bars.bar_lines[0]);
        assert_eq!(bars.validate(), Ok(()));
        assert_eq!(
            bars.validate_presentation_links(),
            Err(PresentationError::BarCount { bars: 2, rows: 1 })
        );
        bars.bar_lines = vec![Position(0)];
        assert_eq!(
            bars.validate_presentation_links(),
            Err(PresentationError::BarLink { index: 0 })
        );
        assert_eq!(built.validate_presentation(), Ok(()));
    }

    #[test]
    fn lane_order_follows_identity_through_aliases() {
        // Rows 1 to 3 share one µs and one position.
        let rows = [(0, 0), (1_000, 7), (1_000, 7), (1_000, 7), (2_000, 9)];
        let mut chart = ChartBuilder::new(150_000).build().unwrap();
        with_rows(&mut chart, &rows);
        let lane = key(4);
        // Alias taps on rows [2, 1]: their times and positions agree, their
        // identities run backwards.
        chart.lanes[4] = vec![tap(&rows, 2), tap(&rows, 1)];
        assert_eq!(chart.validate(), Ok(()));
        assert_eq!(
            chart.validate_presentation_links(),
            Err(PresentationError::LaneOrder { lane, index: 1 })
        );
        chart.lanes[4] = vec![tap(&rows, 1), tap(&rows, 2)];
        assert_eq!(chart.validate_presentation(), Ok(()));
        // A long note on rows [1, 3], then a head on row 2: inside it by
        // identity, though at its end's time and position.
        chart.lanes[4] = vec![long(&rows, 1, 3), tap(&rows, 2)];
        assert_eq!(chart.validate(), Ok(()));
        assert_eq!(
            chart.validate_presentation_links(),
            Err(PresentationError::LaneOrder { lane, index: 1 })
        );
        // A head on the end's own row touches it.
        chart.lanes[4] = vec![long(&rows, 1, 3), tap(&rows, 3)];
        assert_eq!(chart.validate_presentation(), Ok(()));
    }

    #[test]
    fn endpoints_at_one_us_must_share_a_position() {
        // Rows 1 and 2 share 1 000 µs at two positions; row 3 aliases row 2.
        let rows = [(0, 0), (1_000, 7), (1_000, 8), (1_000, 8), (2_000, 9)];
        let mut chart = ChartBuilder::new(150_000).build().unwrap();
        with_rows(&mut chart, &rows);

        // Distinct rows at one position are no conflict.
        chart.lanes[2] = vec![tap(&rows, 2)];
        chart.lanes[5] = vec![tap(&rows, 3)];
        assert_eq!(chart.validate_presentation(), Ok(()));
        // A row only a bar line names takes no part.
        let points = chart.timeline.points().to_vec();
        chart.timeline =
            ChartTimeline::new(points, vec![TimelineId(1)], tempo(), Speed::NORMAL).unwrap();
        chart.bar_lines = vec![Position(7)];
        assert_eq!(chart.validate_presentation(), Ok(()));

        // A key at the other position conflicts: the lower row comes first,
        // and the first endpoint found at another position is reported.
        chart.lanes[6] = vec![tap(&rows, 1)];
        assert_eq!(chart.validate(), Ok(()));
        assert_eq!(chart.validate_presentation_links(), Ok(()));
        assert_eq!(
            chart.validate_presentation(),
            Err(PresentationError::EndpointConflict {
                at: TimeUs(1_000),
                first: at(6, 0, EndpointPart::Head, 1),
                second: at(2, 0, EndpointPart::Head, 2),
                first_pos: Position(7),
                second_pos: Position(8),
            })
        );

        // A long note's end conflicts as a head does, and equal rows break
        // ties by lane.
        chart.lanes[6].clear();
        chart.lanes[1] = vec![long(&rows, 0, 1)];
        chart.lanes[3] = vec![long(&rows, 0, 1)];
        assert_eq!(
            chart.validate_presentation(),
            Err(PresentationError::EndpointConflict {
                at: TimeUs(1_000),
                first: at(1, 0, EndpointPart::End, 1),
                second: at(2, 0, EndpointPart::Head, 2),
                first_pos: Position(7),
                second_pos: Position(8),
            })
        );
        // The scratch is a lane like the others, mines included.
        chart.lanes[1].clear();
        chart.lanes[3].clear();
        chart.lanes[0] = vec![Note {
            kind: NoteKind::Mine { damage: 1 },
            ..tap(&rows, 1)
        }];
        assert_eq!(
            chart.validate_presentation(),
            Err(PresentationError::EndpointConflict {
                at: TimeUs(1_000),
                first: at(0, 0, EndpointPart::Head, 1),
                second: at(2, 0, EndpointPart::Head, 2),
                first_pos: Position(7),
                second_pos: Position(8),
            })
        );
    }
}
