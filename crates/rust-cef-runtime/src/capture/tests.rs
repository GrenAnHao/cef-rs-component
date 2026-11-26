//! Targeted native primitive/endpoint tests, NOT initialized CEF UI evidence.
use super::*;
use d3d11::{D3dFaultPoint, test_texture};
use rust_cef_core::{
    contracts::{BrowserId, RuntimeGeneration},
    frame_layout::FrameFormat,
    mailbox::SurfaceBounds,
};

struct Observer;
impl FrameSink for Observer {
    fn publish(&self, _: OwnedFrame) -> PublishResult {
        PublishResult::Rejected
    }
}
impl UiDispatcher for Observer {
    fn wake(&self) -> Result<(), EngineError> {
        Ok(())
    }
}

fn source() -> Arc<FrameMailbox> {
    let source = Arc::new(FrameMailbox::with_budget(
        BrowserId::new(RuntimeGeneration::new(1), 1),
        FrameBudget::new(64),
    ));
    source
        .configure_view(
            SurfaceBounds {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            2,
            2,
        )
        .unwrap();
    source.take_update();
    source
}
fn native_endpoint(source: &FrameMailbox) -> CaptureEndpoint {
    let endpoint = CaptureEndpoint::new(source.budget());
    endpoint.prepare_adapter_test();
    let owner = crate::executor::current_thread_id();
    let gpu = D3d11Capture::new(owner).expect("real hardware receiver device; no WARP fallback");
    {
        let mut state = endpoint.lock();
        state.backend = Some(RenderPreference::D3d11Required);
        state.diagnostics.owner_thread = owner;
        state.diagnostics.requested_backend = Some("D3d11");
    }
    endpoint.commit_gpu(Some(gpu)).unwrap();
    endpoint
}
fn publish(
    endpoint: &CaptureEndpoint,
    source: &Arc<FrameMailbox>,
    input: &cef::AcceleratedPaintInfo,
) -> Result<PublishResult, EngineError> {
    let info = source
        .capture_token(SurfaceKind::View, 2, 2)
        .unwrap()
        .info();
    let frame = endpoint.accelerated(info, input)?;
    let sink = CaptureFrameSink {
        mailbox: source.clone(),
        observer: Arc::new(Observer),
        ui: Arc::new(Observer),
        recovery: None,
    };
    Ok(FrameSink::publish(&sink, frame))
}

#[test]
fn native_frame_input_failures_keep_source_good_pixels_and_budget() {
    let texture = test_texture(FrameFormat::Rgba8);
    let source = source();
    let endpoint = native_endpoint(&source);
    assert_eq!(
        publish(&endpoint, &source, &texture.info).unwrap(),
        PublishResult::Published
    );
    let good = source.latest(SurfaceKind::View).unwrap();
    assert_eq!(
        good.cpu_pixels().unwrap().as_slice(),
        texture.expected_bgra()
    );
    let mut bad = texture.info.clone();
    bad.format = cef::ColorType::NUM_VALUES;
    let result = publish(&endpoint, &source, &bad);
    assert!(
        matches!(
            result,
            Err(EngineError::Capture {
                kind: CaptureFailureKind::UnsupportedFormat,
                ..
            })
        ),
        "{result:?}"
    );
    bad = texture.info.clone();
    bad.extra.coded_size.width = 0;
    assert!(matches!(
        publish(&endpoint, &source, &bad),
        Err(EngineError::Capture {
            kind: CaptureFailureKind::InvalidDimensions,
            ..
        })
    ));
    bad = texture.info.clone();
    bad.extra.content_rect.width -= 1;
    assert!(matches!(
        publish(&endpoint, &source, &bad),
        Err(EngineError::Capture {
            kind: CaptureFailureKind::IncompleteFrame,
            ..
        })
    ));
    bad = texture.info.clone();
    bad.extra.coded_size.width += 1;
    assert!(matches!(
        publish(&endpoint, &source, &bad),
        Err(EngineError::Capture {
            kind: CaptureFailureKind::TextureMismatch,
            ..
        })
    ));
    assert!(Arc::ptr_eq(
        &good,
        &source.latest(SurfaceKind::View).unwrap()
    ));
    assert_eq!(source.budget().stats().used, 16);
    endpoint.retire().unwrap();
    assert!(endpoint.native_refs_clear());
}

#[test]
fn malformed_accelerated_geometry_does_not_enter_letterbox_or_gpu_io() {
    let texture = test_texture(FrameFormat::Bgra8);
    let source = source();
    let endpoint = native_endpoint(&source);
    publish(&endpoint, &source, &texture.info).unwrap();
    let good = source.latest(SurfaceKind::View).unwrap();
    let before = endpoint.diagnostics();
    let cases: &[(CaptureFailureKind, fn(&mut cef::AcceleratedPaintInfoCommon))] = &[
        (CaptureFailureKind::IncompleteFrame, |m| {
            m.content_rect.width -= 1
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.content_rect.width = 0
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.content_rect.height = -1
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.content_rect.x -= 1
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.content_rect.width += 1
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.content_rect.x = i32::MAX
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.visible_rect.width = 0
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.visible_rect.y = -1
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.visible_rect.x = i32::MAX
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.coded_size.width = 0;
            m.content_rect.width -= 1;
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.coded_size.width = 16385;
            m.visible_rect.width = 16385;
            m.content_rect.width = 16385;
        }),
        (CaptureFailureKind::InvalidDimensions, |m| {
            m.coded_size.height = 16385;
        }),
    ];
    for &(kind, change) in cases {
        let mut bad = texture.info.clone();
        change(&mut bad.extra);
        let error = EngineError::Capture { kind, code: None };
        assert_eq!(validate_accelerated(&bad), Err(error.clone()));
        assert_eq!(publish(&endpoint, &source, &bad), Err(error));
        let after = endpoint.diagnostics();
        assert_eq!(
            (
                after.opens,
                after.copies,
                after.maps,
                after.unmaps,
                after.live_imports
            ),
            (before.opens, before.copies, before.maps, before.unmaps, 0),
        );
        assert!(Arc::ptr_eq(
            &good,
            &source.latest(SurfaceKind::View).unwrap()
        ));
        assert_eq!(source.budget().stats().used, 16);
    }
    endpoint.retire().unwrap();
    assert!(endpoint.native_refs_clear());
}

#[test]
fn native_capture_faults_unmap_and_never_publish_partial_or_removed_device_frames() {
    let texture = test_texture(FrameFormat::Bgra8);
    let source = source();
    let endpoint = native_endpoint(&source);
    publish(&endpoint, &source, &texture.info).unwrap();
    let good = source.latest(SurfaceKind::View).unwrap();
    for point in [
        D3dFaultPoint::Open,
        D3dFaultPoint::Describe,
        D3dFaultPoint::Copy,
        D3dFaultPoint::Map,
        D3dFaultPoint::AfterMap,
        D3dFaultPoint::DeviceRemoved,
    ] {
        endpoint
            .lock()
            .gpu
            .as_mut()
            .unwrap()
            .set_next_fault(Some(point));
        let before = endpoint.diagnostics();
        let result = publish(&endpoint, &source, &texture.info);
        assert!(result.is_err(), "{point:?} did not reject: {result:?}");
        if point == D3dFaultPoint::DeviceRemoved {
            assert!(matches!(
                result,
                Err(EngineError::Capture {
                    kind: CaptureFailureKind::DeviceRemoved,
                    code: Some(_)
                })
            ));
        }
        let after = endpoint.diagnostics();
        assert_eq!(after.maps, after.unmaps, "{point:?}");
        assert_eq!(after.live_imports, 0, "{point:?}");
        if point == D3dFaultPoint::AfterMap {
            assert_eq!(after.maps, before.maps + 1);
            assert_eq!(after.unmaps, before.unmaps + 1);
        }
        assert!(
            Arc::ptr_eq(&good, &source.latest(SurfaceKind::View).unwrap()),
            "{point:?}"
        );
        assert_eq!(
            good.cpu_pixels().unwrap().as_slice(),
            texture.expected_bgra()
        );
        assert_eq!(source.budget().stats().used, 16, "{point:?}");
    }
    endpoint.retire().unwrap();
    assert!(endpoint.native_refs_clear());
}

#[test]
fn checkout_keeps_native_gate_closed_and_defers_retirement_without_a_resource_lock() {
    let source = source();
    let endpoint = Arc::new(native_endpoint(&source));
    let gpu = {
        let mut state = endpoint.lock();
        state.in_flight = true;
        state.gpu.take().unwrap()
    };
    let checkout = GpuCheckout {
        endpoint: &endpoint,
        gpu: Some(gpu),
        _ui: std::marker::PhantomData,
    };
    assert!(!endpoint.native_refs_clear());
    assert!(matches!(
        endpoint.retire(),
        Err(EngineError::Capture {
            kind: CaptureFailureKind::Busy,
            ..
        })
    ));
    let retired = Arc::new(AtomicBool::new(false));
    let observed = retired.clone();
    let ready = endpoint.clone();
    endpoint.when_idle(move || {
        ready.retire().unwrap();
        observed.store(true, Ordering::Release);
    });
    assert!(!retired.load(Ordering::Acquire));
    drop(checkout);
    assert!(retired.load(Ordering::Acquire));
    assert!(endpoint.native_refs_clear());
    assert_eq!(
        endpoint.diagnostics().retirement_thread,
        crate::executor::current_thread_id()
    );
}

#[test]
fn abandoned_preparation_cannot_commit_or_readmit_or_claim_live_native_refs_clear() {
    let gpu = D3d11Capture::new(crate::executor::current_thread_id())
        .expect("hardware device for in-flight preparation window");
    for (gpu, has_native) in [(None, false), (Some(gpu), true)] {
        let endpoint = Arc::new(CaptureEndpoint::new(FrameBudget::new(16)));
        let abandoned = endpoint.clone();
        std::thread::spawn(move || abandoned.abandon())
            .join()
            .unwrap();
        assert_eq!(endpoint.commit_gpu(gpu), Err(EngineError::Closed));
        assert!(endpoint.lock().gpu.is_none());
        assert!(endpoint.is_abandoned());
        assert_eq!(endpoint.native_refs_clear(), !has_native);
        endpoint.prepare_adapter_test();
        assert!(matches!(
            endpoint.check(&endpoint.lock(), RenderPreference::SoftwareRequired),
            Err(EngineError::Closed)
        ));
    }
    // The deliberately abandoned standalone test device stays leaked until this
    // test process exits, exactly as the unfinished-owner contract specifies.
}
