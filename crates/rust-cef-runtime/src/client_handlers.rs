//! Native CEF DragHandler, FrameHandler, and opt-in AudioHandler adapters
//! matching CefSharp's `ClientAdapter` surface (`D:\System Dir\Downloads\CefSharp`,
//! rev `41919703`, `ClientAdapter.h:194-218`, `ClientAdapter.cpp:1490-1617`).
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::sync::{
    Arc,
    atomic::{AtomicI32, Ordering},
};

use cef::*;
use rust_cef_core::{
    contracts::{
        DraggableRegionRect, FrameLifecycleInfo, MAX_DRAGGABLE_REGIONS, MAX_FRAME_ID_BYTES,
        truncate_utf8_bytes,
    },
    host_capabilities::{AudioStreamConfig, DragEnterDecision, DragEnterInfo},
    ports::{BrowserEvent, EXT_EVENT_DRAGGABLE_REGIONS, EXT_EVENT_FRAME_LIFECYCLE},
};

use crate::client::BrowserClientState;

const MAX_AUDIO_CHANNELS: i32 = 64;
const MAX_AUDIO_FRAMES_PER_PACKET: i32 = 65_536;

fn snapshot_frame_info(frame: &Frame) -> FrameLifecycleInfo {
    let frame_id = truncate_utf8_bytes(
        &CefString::from(&frame.identifier()).to_string(),
        MAX_FRAME_ID_BYTES,
    );
    let is_main = frame.is_main() != 0;
    let (name, url) = if frame.is_valid() != 0 {
        (
            truncate_utf8_bytes(&CefString::from(&frame.name()).to_string(), 512),
            truncate_utf8_bytes(&CefString::from(&frame.url()).to_string(), 2048),
        )
    } else {
        (String::new(), String::new())
    };
    FrameLifecycleInfo {
        frame_id,
        name,
        url,
        is_main,
    }
}

fn channel_layout_to_i32(layout: ChannelLayout) -> i32 {
    cef::sys::cef_channel_layout_t::from(layout) as i32
}

fn i32_to_channel_layout(raw: i32) -> Option<ChannelLayout> {
    use cef::sys::cef_channel_layout_t as L;
    let variant = match raw {
        0 => L::CEF_CHANNEL_LAYOUT_NONE,
        1 => L::CEF_CHANNEL_LAYOUT_UNSUPPORTED,
        2 => L::CEF_CHANNEL_LAYOUT_MONO,
        3 => L::CEF_CHANNEL_LAYOUT_STEREO,
        4 => L::CEF_CHANNEL_LAYOUT_2_1,
        5 => L::CEF_CHANNEL_LAYOUT_SURROUND,
        6 => L::CEF_CHANNEL_LAYOUT_4_0,
        7 => L::CEF_CHANNEL_LAYOUT_2_2,
        8 => L::CEF_CHANNEL_LAYOUT_QUAD,
        9 => L::CEF_CHANNEL_LAYOUT_5_0,
        10 => L::CEF_CHANNEL_LAYOUT_5_1,
        11 => L::CEF_CHANNEL_LAYOUT_5_0_BACK,
        12 => L::CEF_CHANNEL_LAYOUT_5_1_BACK,
        13 => L::CEF_CHANNEL_LAYOUT_7_0,
        14 => L::CEF_CHANNEL_LAYOUT_7_1,
        15 => L::CEF_CHANNEL_LAYOUT_7_1_WIDE,
        16 => L::CEF_CHANNEL_LAYOUT_STEREO_DOWNMIX,
        17 => L::CEF_CHANNEL_LAYOUT_2POINT1,
        18 => L::CEF_CHANNEL_LAYOUT_3_1,
        19 => L::CEF_CHANNEL_LAYOUT_4_1,
        20 => L::CEF_CHANNEL_LAYOUT_6_0,
        21 => L::CEF_CHANNEL_LAYOUT_6_0_FRONT,
        22 => L::CEF_CHANNEL_LAYOUT_HEXAGONAL,
        23 => L::CEF_CHANNEL_LAYOUT_6_1,
        24 => L::CEF_CHANNEL_LAYOUT_6_1_BACK,
        25 => L::CEF_CHANNEL_LAYOUT_6_1_FRONT,
        26 => L::CEF_CHANNEL_LAYOUT_7_0_FRONT,
        27 => L::CEF_CHANNEL_LAYOUT_7_1_WIDE_BACK,
        28 => L::CEF_CHANNEL_LAYOUT_OCTAGONAL,
        29 => L::CEF_CHANNEL_LAYOUT_DISCRETE,
        30 => L::CEF_CHANNEL_LAYOUT_STEREO_AND_KEYBOARD_MIC,
        31 => L::CEF_CHANNEL_LAYOUT_4_1_QUAD_SIDE,
        32 => L::CEF_CHANNEL_LAYOUT_BITSTREAM,
        33 => L::CEF_CHANNEL_LAYOUT_5_1_4_DOWNMIX,
        34 => L::CEF_CHANNEL_LAYOUT_1_1,
        35 => L::CEF_CHANNEL_LAYOUT_3_1_BACK,
        36 => L::CEF_CHANNEL_LAYOUT_5_1_4,
        37 => L::CEF_CHANNEL_LAYOUT_7_1_4,
        _ => return None,
    };
    Some(ChannelLayout::from(variant))
}

wrap_drag_handler! {
    pub(crate) struct RuntimeDragHandler {
        state: Arc<BrowserClientState>,
    }

    impl DragHandler {
        fn on_drag_enter(
            &self,
            _browser: Option<&mut Browser>,
            drag_data: Option<&mut DragData>,
            mask: DragOperationsMask,
        ) -> ::std::os::raw::c_int {
            let Some(filter) = self.state.routing.drag_enter_filter() else {
                return 0;
            };
            let is_link = drag_data.as_ref().is_some_and(|d| d.is_link() != 0);
            let is_fragment = drag_data.as_ref().is_some_and(|d| d.is_fragment() != 0);
            let is_file = drag_data.as_ref().is_some_and(|d| d.is_file() != 0);
            let link_url = if is_link {
                drag_data
                    .as_ref()
                    .map(|d| truncate_utf8_bytes(&CefString::from(&d.link_url()).to_string(), 2048))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let info = DragEnterInfo {
                browser: self.state.browser,
                is_link,
                is_fragment,
                is_file,
                link_url,
                mask: mask.as_ref().0 as u32,
            };
            i32::from(filter(&info) == DragEnterDecision::Cancel)
        }

        fn on_draggable_regions_changed(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            regions: Option<&[DraggableRegion]>,
        ) {
            if !self.state.wants_extended_event(EXT_EVENT_DRAGGABLE_REGIONS) {
                return;
            }
            let mapped: Vec<DraggableRegionRect> = regions
                .unwrap_or(&[])
                .iter()
                .take(MAX_DRAGGABLE_REGIONS)
                .map(|r| DraggableRegionRect {
                    x: r.bounds.x,
                    y: r.bounds.y,
                    width: r.bounds.width,
                    height: r.bounds.height,
                    draggable: r.draggable != 0,
                })
                .collect();
            self.state
                .navigation_event(BrowserEvent::DraggableRegionsChanged(mapped));
        }
    }
}

wrap_frame_handler! {
    pub(crate) struct RuntimeFrameHandler {
        state: Arc<BrowserClientState>,
    }

    impl FrameHandler {
        fn on_frame_created(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>) {
            if !self.state.wants_extended_event(EXT_EVENT_FRAME_LIFECYCLE) {
                return;
            }
            if let Some(frame) = frame {
                self.state
                    .navigation_event(BrowserEvent::FrameCreated(snapshot_frame_info(frame)));
            }
        }

        fn on_frame_attached(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            reattached: ::std::os::raw::c_int,
        ) {
            if !self.state.wants_extended_event(EXT_EVENT_FRAME_LIFECYCLE) {
                return;
            }
            if let Some(frame) = frame {
                self.state.navigation_event(BrowserEvent::FrameAttached {
                    frame: snapshot_frame_info(frame),
                    reattached: reattached != 0,
                });
            }
        }

        fn on_frame_detached(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>) {
            if !self.state.wants_extended_event(EXT_EVENT_FRAME_LIFECYCLE) {
                return;
            }
            if let Some(frame) = frame {
                self.state.navigation_event(BrowserEvent::FrameDetached {
                    frame_id: truncate_utf8_bytes(
                        &CefString::from(&frame.identifier()).to_string(),
                        MAX_FRAME_ID_BYTES,
                    ),
                    is_main: frame.is_main() != 0,
                });
            }
        }

        fn on_main_frame_changed(
            &self,
            _browser: Option<&mut Browser>,
            old_frame: Option<&mut Frame>,
            new_frame: Option<&mut Frame>,
        ) {
            if !self.state.wants_extended_event(EXT_EVENT_FRAME_LIFECYCLE) {
                return;
            }
            let old_frame_id = old_frame.map(|f| {
                truncate_utf8_bytes(
                    &CefString::from(&f.identifier()).to_string(),
                    MAX_FRAME_ID_BYTES,
                )
            });
            let new_frame = new_frame.map(|f| snapshot_frame_info(f));
            self.state.navigation_event(BrowserEvent::MainFrameChanged {
                old_frame_id,
                new_frame,
            });
        }
    }
}

wrap_audio_handler! {
    pub(crate) struct RuntimeAudioHandler {
        state: Arc<BrowserClientState>,
        active_channels: Arc<AtomicI32>,
    }

    impl AudioHandler {
        fn audio_parameters(
            &self,
            _browser: Option<&mut Browser>,
            params: Option<&mut AudioParameters>,
        ) -> ::std::os::raw::c_int {
            let Some(svc) = self.state.effective_host_capabilities().audio else {
                return 0;
            };
            let Some(params) = params else {
                return 0;
            };
            let defaults = AudioStreamConfig {
                channel_layout: channel_layout_to_i32(params.channel_layout),
                sample_rate: params.sample_rate,
                frames_per_buffer: params.frames_per_buffer,
            };
            let Some(chosen) = svc.configure_stream(self.state.browser, defaults) else {
                return 0;
            };
            if chosen.sample_rate <= 0 || chosen.frames_per_buffer <= 0 {
                return 0;
            }
            if let Some(layout) = i32_to_channel_layout(chosen.channel_layout) {
                params.channel_layout = layout;
            }
            params.sample_rate = chosen.sample_rate;
            params.frames_per_buffer = chosen.frames_per_buffer;
            1
        }

        fn on_audio_stream_started(
            &self,
            _browser: Option<&mut Browser>,
            params: Option<&AudioParameters>,
            channels: ::std::os::raw::c_int,
        ) {
            let clamped_channels = channels.clamp(0, MAX_AUDIO_CHANNELS);
            self.active_channels
                .store(clamped_channels, Ordering::Release);
            let Some(svc) = self.state.effective_host_capabilities().audio else {
                return;
            };
            let config = params.map_or(
                AudioStreamConfig {
                    channel_layout: 0,
                    sample_rate: 0,
                    frames_per_buffer: 0,
                },
                |p| AudioStreamConfig {
                    channel_layout: channel_layout_to_i32(p.channel_layout),
                    sample_rate: p.sample_rate,
                    frames_per_buffer: p.frames_per_buffer,
                },
            );
            svc.on_stream_started(self.state.browser, config, clamped_channels as usize);
        }

        fn on_audio_stream_packet(
            &self,
            _browser: Option<&mut Browser>,
            data: *mut *const f32,
            frames: ::std::os::raw::c_int,
            pts: i64,
        ) {
            let channel_count = self.active_channels.load(Ordering::Acquire);
            if data.is_null()
                || channel_count <= 0
                || frames <= 0
                || frames > MAX_AUDIO_FRAMES_PER_PACKET
            {
                return;
            }
            let Some(svc) = self.state.effective_host_capabilities().audio else {
                return;
            };
            let ch_len = channel_count as usize;
            let frame_len = frames as usize;
            let mut packet = Vec::with_capacity(ch_len);
            for idx in 0..ch_len {
                // SAFETY: CEF guarantees `data` points to `channels` planar buffers each with
                // `frames` valid `f32` samples for the duration of `OnAudioStreamPacket`.
                let ptr = unsafe { *data.add(idx) };
                if ptr.is_null() {
                    return;
                }
                let slice = unsafe { std::slice::from_raw_parts(ptr, frame_len) };
                packet.push(slice.to_vec());
            }
            svc.on_stream_packet(self.state.browser, &packet, pts);
        }

        fn on_audio_stream_stopped(&self, _browser: Option<&mut Browser>) {
            self.active_channels.store(0, Ordering::Release);
            if let Some(svc) = self.state.effective_host_capabilities().audio {
                svc.on_stream_stopped(self.state.browser);
            }
        }

        fn on_audio_stream_error(
            &self,
            _browser: Option<&mut Browser>,
            message: Option<&CefString>,
        ) {
            if let Some(svc) = self.state.effective_host_capabilities().audio {
                let text = truncate_utf8_bytes(
                    &message.map(ToString::to_string).unwrap_or_default(),
                    1024,
                );
                svc.on_stream_error(self.state.browser, text);
            }
        }
    }
}

pub(crate) fn new_audio_handler(state: Arc<BrowserClientState>) -> AudioHandler {
    RuntimeAudioHandler::new(state, Arc::new(AtomicI32::new(0)))
}
