// Copyright (C) 2024 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use once_cell::sync::Lazy;

use crate::st2038anc_utils::AncDataHeader;

#[derive(Default)]
struct State {
    downstream_framerate: Option<gst::Fraction>,
}

#[derive(Default)]
pub struct St2038AncMux {
    state: Mutex<State>,
}

pub(crate) static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "st2038ancmux",
        gst::DebugColorFlags::empty(),
        Some("ST2038 Anc Mux Element"),
    )
});

impl AggregatorImpl for St2038AncMux {
    fn aggregate(&self, timeout: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = self.state.lock().unwrap();
        let src_segment = self
            .obj()
            .src_pad()
            .segment()
            .downcast::<gst::ClockTime>()
            .expect("Non-TIME segment");

        let start_running_time =
            if src_segment.position().is_none() || src_segment.position() < src_segment.start() {
                src_segment.start().unwrap()
            } else {
                src_segment.position().unwrap()
            };

        // Only if downstream framerate provided, otherwise we output as we go
        let duration = if let Some(framerate) = state.downstream_framerate {
            gst::ClockTime::SECOND
                .nseconds()
                .mul_div_round(framerate.denom() as u64, framerate.numer() as u64)
                .unwrap()
                .nseconds()
        } else {
            gst::ClockTime::ZERO
        };
        let end_running_time = start_running_time + duration;
        drop(state);

        gst::trace!(
            CAT,
            imp = self,
            "Aggregating for start time {} end {} timeout {}",
            start_running_time.display(),
            end_running_time.display(),
            timeout
        );

        let sinkpads = self.obj().sink_pads();

        // Collect buffers from all pads. We can start outputting for this frame on timeout,
        // or otherwise all pads are either EOS or have a buffer for a future frame.
        let mut all_pads_done = true;
        let mut all_pads_eos = true;
        let mut min_next_buffer_running_time = None;

        for pad in sinkpads
            .iter()
            .map(|pad| pad.downcast_ref::<super::St2038AncMuxSinkPad>().unwrap())
        {
            let mut pad_state = pad.imp().pad_state.lock().unwrap();

            if pad.is_eos() {
                // This pad is done
                gst::trace!(CAT, obj = pad, "Pad is EOS");
                if !pad_state.queued_buffers.is_empty() {
                    all_pads_eos = false;
                }
                continue;
            }

            all_pads_eos = false;

            let buffer = if let Some(buffer) = pad.peek_buffer() {
                buffer
            } else {
                all_pads_done = false;
                continue;
            };

            let segment = pad.segment().downcast::<gst::ClockTime>().unwrap();
            let Some(buffer_start_ts) = segment.to_running_time(buffer.pts()) else {
                gst::warning!(CAT, obj = pad, "Buffer without valid PTS, dropping");
                pad.drop_buffer();
                all_pads_done = false;
                continue;
            };

            if buffer_start_ts > end_running_time
                || (end_running_time > start_running_time && buffer_start_ts == end_running_time)
            {
                gst::trace!(
                    CAT,
                    obj = pad,
                    "Buffer starting at {buffer_start_ts} >= {end_running_time}"
                );

                if min_next_buffer_running_time.map_or(true, |next_buffer_min_running_time| {
                    next_buffer_min_running_time > buffer_start_ts
                }) {
                    min_next_buffer_running_time = Some(buffer_start_ts);
                }

                // buffer is not for this frame so we're not interested in it yet
                // and this pad is done for this frame.
                continue;
            }

            // Store buffers on the pad
            gst::trace!(
                CAT,
                obj = pad,
                "Queueing buffer starting at {buffer_start_ts}"
            );
            pad_state.queued_buffers.push(buffer);
            pad.drop_buffer();

            // Check again if there's another buffer on this pad for this frame
            all_pads_done = false;
        }

        if !all_pads_done && !timeout {
            gst::trace!(CAT, imp = self, "Not all pads ready yet");
            return Err(gst_base::AGGREGATOR_FLOW_NEED_DATA);
        }

        if all_pads_eos {
            gst::debug!(CAT, imp = self, "All pads EOS");
            return Err(gst::FlowError::Eos);
        }

        gst::trace!(CAT, imp = self, "Ready for outputting");

        self.obj()
            .selected_samples(start_running_time, None, duration, None);

        // Remove all overlapping anc buffers from the queued buffers. The latest pad, latest
        // buffer of that pad wins.
        let mut lines =
            BTreeMap::<u16, BTreeMap<u16, (u16, super::St2038AncMuxSinkPad, gst::Buffer)>>::new();
        for pad in sinkpads
            .iter()
            .rev()
            .map(|pad| pad.downcast_ref::<super::St2038AncMuxSinkPad>().unwrap())
        {
            let mut pad_state = pad.imp().pad_state.lock().unwrap();

            for buffer in pad_state.queued_buffers.drain(..).rev() {
                if buffer.size() == 0
                    && buffer.flags().contains(gst::BufferFlags::GAP)
                    && gst::meta::CustomMeta::from_buffer(&buffer, "GstAggregatorMissingDataMeta")
                        .is_ok()
                {
                    gst::trace!(CAT, obj = pad, "Dropping gap buffer");
                    continue;
                }

                let header = match AncDataHeader::from_buffer(&buffer) {
                    Ok(header) => header,
                    Err(err) => {
                        gst::warning!(
                            CAT,
                            obj = pad,
                            "Dropping buffer with invalid ST2038 data ({err})"
                        );
                        continue;
                    }
                };

                gst::trace!(CAT, obj = pad, "Parsed ST2038 header {header:?}");

                // FIXME: One pixel per word of data? ADF header needs to be included in the
                // calculation? Two words per pixel because 4:2:2 YUV? Nobody knows!
                let buffer_clone = buffer.clone(); // FIXME: To appease the borrow checker
                lines
                    .entry(header.line_number)
                    .and_modify(|line| {
                        let new_offset = header.horizontal_offset;
                        let new_offset_end = header.horizontal_offset + header.data_count as u16;

                        for (offset, (offset_end, _pad, _buffer)) in &*line {
                            // If one of the range starts is between the start/end of the other
                            // then the two ranges are overlapping.
                            if (new_offset >= *offset && new_offset < *offset_end)
                                || (*offset >= new_offset && *offset < new_offset_end)
                            {
                                gst::trace!(
                                    CAT,
                                    obj = pad,
                                    "Not including ST2038 packet at {}x{}",
                                    header.line_number,
                                    header.horizontal_offset
                                );
                                return;
                            }
                        }

                        gst::trace!(
                            CAT,
                            obj = pad,
                            "Including ST2038 packet at {}x{}",
                            header.line_number,
                            header.horizontal_offset
                        );

                        line.insert(new_offset, (new_offset_end, pad.clone(), buffer));
                    })
                    .or_insert_with(|| {
                        gst::trace!(
                            CAT,
                            obj = pad,
                            "Including ST2038 packet at {}x{}",
                            header.line_number,
                            header.horizontal_offset
                        );

                        let mut line = BTreeMap::new();
                        line.insert(
                            header.horizontal_offset,
                            (
                                header.horizontal_offset + header.data_count as u16,
                                pad.clone(),
                                buffer_clone,
                            ),
                        );
                        line
                    });
            }
        }

        // Collect all anc buffers for this frame and output them as a single buffer list,
        // sorted by line. Multiple anc in a single line are merged into a single buffer.
        let ret = if !lines.is_empty() {
            let mut buffers = gst::BufferList::new();

            let buffers_ref = buffers.get_mut().unwrap();

            for (line_idx, line) in lines {
                // If there are multiple buffers for a line then merge them into a single buffer
                if line.len() == 1 {
                    for (horizontal_offset, (_, _pad, buffer)) in line {
                        gst::trace!(
                            CAT,
                            imp = self,
                            "Outputting ST2038 packet at {line_idx}x{horizontal_offset}"
                        );
                        buffers_ref.add(buffer);
                    }
                } else {
                    gst::trace!(
                        CAT,
                        imp = self,
                        "Outputting multiple ST2038 packets at line {line_idx}"
                    );
                    let mut new_buffer = gst::Buffer::new();
                    for (horizontal_offset, (_, _pad, buffer)) in line {
                        gst::trace!(CAT, imp = self, "Horizontal offset {horizontal_offset}");
                        // Copy over metadata of the first buffer for this line
                        if new_buffer.size() == 0 {
                            let new_buffer_ref = new_buffer.get_mut().unwrap();
                            let _ = buffer.copy_into(new_buffer_ref, gst::BUFFER_COPY_METADATA, ..);
                        }
                        new_buffer.append(buffer);
                    }
                }
            }

            gst::trace!(CAT, imp = self, "Outputting {} buffers", buffers_ref.len());

            // Unset marker flag on all buffers, and set PTS/duration if there is a downstream
            // framerate. Otherwise we leave them as-is.
            if duration > gst::ClockTime::ZERO {
                buffers_ref.foreach_mut(|mut buffer, _idx| {
                    let buffer_ref = buffer.make_mut();
                    buffer_ref.set_pts(start_running_time);
                    buffer_ref.set_duration(duration);
                    buffer_ref.unset_flags(gst::BufferFlags::MARKER);

                    ControlFlow::Continue(Some(buffer))
                });
            } else {
                buffers_ref.foreach_mut(|mut buffer, _idx| {
                    if buffer.flags().contains(gst::BufferFlags::MARKER) {
                        let buffer_ref = buffer.make_mut();
                        buffer_ref.unset_flags(gst::BufferFlags::MARKER);
                    }

                    ControlFlow::Continue(Some(buffer))
                });
            }

            // Set marker flag on last buffer
            {
                let last = buffers_ref.get_mut(buffers_ref.len() - 1).unwrap();
                last.set_flags(gst::BufferFlags::MARKER);
            }

            self.finish_buffer_list(buffers)
        } else {
            let mut duration = duration;

            if let Some(min_next_buffer_running_time) = min_next_buffer_running_time {
                gst::trace!(
                    CAT,
                    imp = self,
                    "Next buffer at {min_next_buffer_running_time}"
                );
                if duration == gst::ClockTime::ZERO {
                    duration = min_next_buffer_running_time - start_running_time;
                }
            }

            gst::trace!(
                CAT,
                imp = self,
                "Outputting gap event at {start_running_time} with duration {duration}"
            );

            // Nothing to be output for this frame

            #[cfg(feature = "v1_26")]
            {
                self.obj().push_src_event(
                    gst::event::Gap::builder(start_running_time)
                        .duration(duration)
                        .build(),
                );
            }

            #[cfg(not(feature = "v1_26"))]
            {
                self.obj().src_pad().push_event(
                    gst::event::Gap::builder(start_running_time)
                        .duration(duration)
                        .build(),
                );
            }

            Ok(gst::FlowSuccess::Ok)
        };

        // Advance position to the next frame if there is a downstream framerate, or otherwise
        // to the start of the next buffer.
        if duration > gst::ClockTime::ZERO {
            self.obj().set_position(end_running_time);
        } else if let Some(min_next_buffer_running_time) = min_next_buffer_running_time {
            self.obj().set_position(min_next_buffer_running_time);
        } else {
            self.obj()
                .set_position(src_segment.position().opt_add(40.mseconds()));
        }

        ret
    }

    fn peek_next_sample(&self, pad: &gst_base::AggregatorPad) -> Option<gst::Sample> {
        let pad = pad.downcast_ref::<super::St2038AncMuxSinkPad>().unwrap();

        let pad_state = pad.imp().pad_state.lock().unwrap();
        let caps = pad.current_caps()?;
        if pad_state.queued_buffers.is_empty() {
            return None;
        }

        Some(
            gst::Sample::builder()
                .buffer_list(
                    &pad_state
                        .queued_buffers
                        .iter()
                        .cloned()
                        .collect::<gst::BufferList>(),
                )
                .segment(&pad.segment())
                .caps(&caps)
                .build(),
        )
    }

    fn next_time(&self) -> Option<gst::ClockTime> {
        self.obj().simple_get_next_time()
    }

    fn flush(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        *state = State::default();

        self.obj()
            .src_pad()
            .segment()
            .set_position(None::<gst::ClockTime>);

        Ok(gst::FlowSuccess::Ok)
    }

    fn negotiate(&self) -> bool {
        let templ_caps = self.obj().src_pad().pad_template_caps();
        let mut peer_caps = self.obj().src_pad().peer_query_caps(Some(&templ_caps));
        gst::debug!(CAT, imp = self, "Downstream caps {peer_caps:?}");

        if peer_caps.is_empty() {
            gst::warning!(CAT, imp = self, "Downstream returned EMPTY caps");
            return false;
        }

        peer_caps.fixate();
        let framerate = peer_caps
            .structure(0)
            .unwrap()
            .get::<gst::Fraction>("framerate")
            .ok();

        let mut state = self.state.lock().unwrap();
        if let Some(framerate) = framerate {
            gst::debug!(
                CAT,
                imp = self,
                "Configuring downstream requested framerate {framerate}"
            );
            state.downstream_framerate = Some(framerate);
            drop(state);

            let duration = gst::ClockTime::SECOND
                .nseconds()
                .mul_div_round(framerate.denom() as u64, framerate.numer() as u64)
                .unwrap()
                .nseconds();

            self.obj().set_latency(duration, duration);
        } else {
            gst::debug!(CAT, imp = self, "Downstream requested no framerate");
            state.downstream_framerate = None;
            drop(state);

            // Assume 25fps as a worst case
            self.obj().set_latency(40.mseconds(), None);
        }

        self.obj().set_src_caps(&peer_caps);

        true
    }

    fn sink_event(&self, aggregator_pad: &gst_base::AggregatorPad, event: gst::Event) -> bool {
        #[allow(clippy::single_match)]
        match event.view() {
            gst::EventView::Segment(ev) => {
                let segment = ev.segment();
                if segment.format() != gst::Format::Time {
                    gst::error!(CAT, imp = self, "Non-TIME segments not supported");
                    return false;
                }
            }
            _ => (),
        }

        self.parent_sink_event(aggregator_pad, event)
    }

    fn clip(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        buffer: gst::Buffer,
    ) -> Option<gst::Buffer> {
        let Some(pts) = buffer.pts() else {
            return Some(buffer);
        };
        let segment = aggregator_pad.segment();
        segment
            .downcast_ref::<gst::ClockTime>()
            .map(|segment| segment.clip(pts, pts))
            .map(|_| buffer)
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::trace!(CAT, imp = self, "Starting");
        let mut state = self.state.lock().unwrap();
        *state = State::default();

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::trace!(CAT, imp = self, "Stopping");
        let mut state = self.state.lock().unwrap();
        *state = State::default();

        Ok(())
    }
}

impl ElementImpl for St2038AncMux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ST2038 Anc Mux",
                "Muxer",
                "Combines multiple ST2038 Anc streams",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_empty_simple("meta/x-st-2038");
            let src_pad_template = gst::PadTemplate::builder(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .gtype(gst_base::AggregatorPad::static_type())
            .build()
            .unwrap();

            let sink_pad_template = gst::PadTemplate::builder(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
            )
            .gtype(super::St2038AncMuxSinkPad::static_type())
            .build()
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl GstObjectImpl for St2038AncMux {}

impl ObjectImpl for St2038AncMux {}

#[glib::object_subclass]
impl ObjectSubclass for St2038AncMux {
    const NAME: &'static str = "GstSt2038AncMux";
    type Type = super::St2038AncMux;
    type ParentType = gst_base::Aggregator;
}

#[derive(Default)]
struct PadState {
    queued_buffers: Vec<gst::Buffer>,
}

#[derive(Default)]
pub struct St2038AncMuxSinkPad {
    pad_state: Mutex<PadState>,
}

impl St2038AncMuxSinkPad {}

impl AggregatorPadImpl for St2038AncMuxSinkPad {
    fn flush(
        &self,
        _aggregator: &gst_base::Aggregator,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.pad_state.lock().unwrap();
        state.queued_buffers.clear();
        Ok(gst::FlowSuccess::Ok)
    }
}

impl PadImpl for St2038AncMuxSinkPad {}

impl GstObjectImpl for St2038AncMuxSinkPad {}

impl ObjectImpl for St2038AncMuxSinkPad {}

#[glib::object_subclass]
impl ObjectSubclass for St2038AncMuxSinkPad {
    const NAME: &'static str = "GstSt2038AncMuxSinkPad";
    type Type = super::St2038AncMuxSinkPad;
    type ParentType = gst_base::AggregatorPad;
}
